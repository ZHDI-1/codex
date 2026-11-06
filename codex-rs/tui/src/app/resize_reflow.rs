//! Connects terminal resize events to source-backed transcript scrollback rebuilds.
//!
//! The app stores conversation history as `HistoryCell`s, but it also writes finalized history into
//! terminal scrollback for the normal chat view. When the terminal width changes, this module uses
//! the stored cells as source, clears the Codex-owned terminal history, and re-emits the transcript
//! for the new terminal size.
//!
//! Streaming output is the fragile part of this lifecycle. Active streams first appear as transient
//! stream cells, then consolidate into source-backed finalized cells. Resize work that happens
//! before consolidation is marked as stream-time work so consolidation can force one final rebuild
//! from the finalized source.
//!
//! Initial replay defers formatting until its cells are collected. Large snapshots render on a
//! bounded CPU pool; only the UI validates and emits their results. Width, source identity, theme,
//! and terminal colors must still match when work finishes. A row cap bounds the tail being rebuilt.
//!
//! Owned-screen sessions lay out their retained transcript during drawing. Their entry points drop
//! queued scrollback work and request a frame instead of clearing or replaying terminal history.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::time::Instant;

use color_eyre::eyre::Result;
use ratatui::layout::Size;
use ratatui::style::Stylize;
use ratatui::text::Line;

use super::App;
use super::InitialHistoryReplayBuffer;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use crate::history_cell::HistoryRenderMode;
use crate::insert_history::HistoryLineWrapPolicy;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::transcript_reflow::TRANSCRIPT_REFLOW_DEBOUNCE;
use crate::tui;

/// Full terminal width before transcript-specific layout reservations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TerminalWidth(u16);

impl From<ratatui::layout::Size> for TerminalWidth {
    fn from(size: ratatui::layout::Size) -> Self {
        Self(size.width)
    }
}

struct ReflowCellDisplay {
    lines: Vec<HyperlinkLine>,
    is_stream_continuation: bool,
}

/// Rendered transcript lines ready to be replayed into terminal scrollback.
///
/// This is intentionally line-oriented rather than cell-oriented because the terminal only accepts
/// already-wrapped rows. Callers should keep treating `transcript_cells` as the source of truth; the
/// rows here are a transient render product for a single terminal width.
#[derive(Default)]
pub(super) struct ReflowRenderResult {
    pub(super) lines: Vec<HyperlinkLine>,
    history_was_truncated: bool,
    emitted_history_cell: bool,
    prepared: Option<crate::insert_history::PreparedHistory>,
}

const BACKGROUND_REFLOW_MIN_CELLS: usize = 32;

#[derive(Clone, Eq, PartialEq)]
struct ReflowKey {
    width: u16,
    terminal_width: u16,
    row_cap: Option<usize>,
    mode: HistoryRenderMode,
    theme_revision: u64,
    terminal_fg: Option<(u8, u8, u8)>,
    terminal_bg: Option<(u8, u8, u8)>,
    color_level: crate::terminal_palette::StdoutColorLevel,
    wrap_policy: HistoryLineWrapPolicy,
    older_history: bool,
    notice: Option<Arc<str>>,
}

struct ReflowJob {
    id: u64,
    key: ReflowKey,
    sources: Vec<Weak<dyn HistoryCell>>,
    cancelled: Arc<AtomicBool>,
    result: mpsc::Receiver<std::thread::Result<std::io::Result<Option<ReflowRenderResult>>>>,
}

#[derive(Default)]
pub(super) struct BackgroundReflow {
    next_id: u64,
    job: Option<ReflowJob>,
    unavailable: bool,
}

enum PreparedReflow {
    Pending,
    Ready(ReflowRenderResult),
    Synchronous,
}

impl Drop for BackgroundReflow {
    fn drop(&mut self) {
        self.clear();
    }
}

impl BackgroundReflow {
    pub(super) fn invalidate(&mut self) {
        if let Some(job) = &self.job {
            job.cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn clear(&mut self) {
        if let Some(job) = self.job.take() {
            job.cancelled.store(true, Ordering::Relaxed);
        }
    }

    pub(super) fn is_current(&self, id: u64) -> bool {
        self.job.as_ref().is_some_and(|job| job.id == id)
    }

    fn prepare(
        &mut self,
        sources: &[Arc<dyn HistoryCell>],
        key: ReflowKey,
        events: &crate::app_event_sender::AppEventSender,
    ) -> PreparedReflow {
        // Select the visible suffix before comparing identities. A newly appended clear header
        // replaces the old render's source rather than extending it with stale visible history.
        let sources = cells_since_last_clear(sources);
        if sources.len() < BACKGROUND_REFLOW_MIN_CELLS || self.unavailable {
            self.clear();
            return PreparedReflow::Synchronous;
        }
        let mut prefix = None;
        if let Some(job) = &self.job {
            // New warnings and other finalized cells can arrive during initial replay. An
            // unchanged, uncapped prefix remains useful; retain it instead of rendering it again.
            // Capped histories and history notices depend on the whole tail, so keep their normal
            // invalidation behavior. Explicit invalidation still covers cells mutated in place.
            let append_only =
                key.row_cap.is_none() && !key.older_history && job.sources.len() <= sources.len();
            let matches = !job.cancelled.load(Ordering::Relaxed)
                && job.key == key
                && (job.sources.len() == sources.len() || append_only)
                && job
                    .sources
                    .iter()
                    .zip(sources)
                    .all(|(old, current)| old.ptr_eq(&Arc::downgrade(current)));
            if !matches {
                job.cancelled.store(true, Ordering::Relaxed);
            }
            // Keep at most one in-flight request per app. A later resize supersedes it rather
            // than queueing another full copy of the transcript on every draw.
            match job.result.try_recv() {
                Ok(Ok(Ok(Some(result)))) if matches => {
                    let rendered_count = job.sources.len();
                    self.job = None;
                    if rendered_count == sources.len() {
                        return PreparedReflow::Ready(result);
                    }
                    prefix = Some((rendered_count, result));
                }
                Ok(Ok(Ok(_))) => self.job = None,
                Ok(Err(_)) | Ok(Ok(Err(_))) | Err(mpsc::TryRecvError::Disconnected) => {
                    self.job = None;
                    self.unavailable = true;
                    tracing::warn!(
                        "background history rendering failed; using synchronous rendering"
                    );
                    return PreparedReflow::Synchronous;
                }
                Err(mpsc::TryRecvError::Empty) => return PreparedReflow::Pending,
            }
        }
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        let cancelled = Arc::new(AtomicBool::new(false));
        let (sender, result) = mpsc::channel();
        let weak_sources = sources.iter().map(Arc::downgrade).collect();
        let rendered_count = prefix.as_ref().map_or(0, |(count, _)| *count);
        let sources = sources[rendered_count..].to_vec();
        let worker_cancelled = cancelled.clone();
        let events = events.clone();
        let worker_key = key.clone();
        let spawned = crate::history_render::spawn(move || {
            let rendered =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> std::io::Result<_> {
                    let mut prefix = prefix.map(|(_, mut result)| {
                        if let Some(prepared) = result.prepared.take() {
                            result.lines = prepared.source;
                            // The old ANSI buffer is released here, off the UI thread.
                        }
                        result
                    });
                    let Some(mut result) = render_reflow_cells_after(
                        &sources,
                        worker_key.width,
                        worker_key.mode,
                        worker_key.row_cap,
                        &worker_cancelled,
                        prefix
                            .as_ref()
                            .is_some_and(|result| result.emitted_history_cell),
                    ) else {
                        return Ok(None);
                    };
                    if let Some(mut prefix) = prefix.take() {
                        prefix.lines.append(&mut result.lines);
                        result.lines = prefix.lines;
                    }
                    prepend_history_notice(
                        &mut result.lines,
                        result.history_was_truncated,
                        worker_key.width,
                        worker_key.row_cap,
                        worker_key.older_history,
                        worker_key.notice.as_deref(),
                    );
                    if worker_cancelled.load(Ordering::Relaxed) {
                        return Ok(None);
                    }
                    result.prepared = Some(crate::insert_history::prepare_history_hyperlink_lines(
                        std::mem::take(&mut result.lines),
                        worker_key.terminal_width,
                        worker_key.wrap_policy,
                    )?);
                    Ok((!worker_cancelled.load(Ordering::Relaxed)).then_some(result))
                }));
            let _ = sender.send(rendered);
            events.send(crate::app_event::AppEvent::HistoryReflowReady { id });
        });
        if !spawned {
            self.unavailable = true;
            return PreparedReflow::Synchronous;
        }
        self.job = Some(ReflowJob {
            id,
            key,
            sources: weak_sources,
            cancelled,
            result,
        });
        PreparedReflow::Pending
    }
}

fn render_reflow_cells(
    cells: &[Arc<dyn HistoryCell>],
    width: u16,
    mode: HistoryRenderMode,
    row_cap: Option<usize>,
    cancelled: &AtomicBool,
) -> Option<ReflowRenderResult> {
    render_reflow_cells_after(
        cells_since_last_clear(cells),
        width,
        mode,
        row_cap,
        cancelled,
        false,
    )
}

/// Ctrl+L retains the transcript for browsing while starting a new native scrollback view.
/// Limit rendering before dispatch so parallel workers also skip the cleared prefix.
fn cells_since_last_clear(cells: &[Arc<dyn HistoryCell>]) -> &[Arc<dyn HistoryCell>] {
    let start = cells
        .iter()
        .rposition(|cell| cell.as_any().is::<history_cell::ClearScreenHeader>())
        .unwrap_or(0);
    &cells[start..]
}

fn render_reflow_cells_after(
    cells: &[Arc<dyn HistoryCell>],
    width: u16,
    mode: HistoryRenderMode,
    row_cap: Option<usize>,
    cancelled: &AtomicBool,
    emitted_history_cell: bool,
) -> Option<ReflowRenderResult> {
    let rendered = crate::history_render::render_tail(cells, row_cap, cancelled, |cell| {
        let lines = cell.display_hyperlink_lines_for_mode(width, mode);
        crate::history_render::Rendered {
            rows: lines.len(),
            continuation: cell.is_stream_continuation(),
            value: ReflowCellDisplay {
                lines,
                is_stream_continuation: cell.is_stream_continuation(),
            },
        }
    })?;
    let mut lines = Vec::new();
    let mut emitted = emitted_history_cell;
    for display in rendered.cells {
        if !display.lines.is_empty() && !display.is_stream_continuation {
            if emitted {
                lines.push(HyperlinkLine::new(Line::from("")));
            }
            emitted = true;
        }
        lines.extend(display.lines);
    }
    let mut history_was_truncated = rendered.truncated;
    if let Some(cap) = row_cap
        && lines.len() > cap
    {
        history_was_truncated = true;
        lines = lines.split_off(lines.len() - cap);
    }
    Some(ReflowRenderResult {
        lines,
        history_was_truncated,
        emitted_history_cell: emitted,
        prepared: None,
    })
}

fn prepend_history_notice(
    lines: &mut Vec<HyperlinkLine>,
    history_was_truncated: bool,
    width: u16,
    max_rows: Option<usize>,
    older_history: bool,
    notice: Option<&str>,
) {
    if lines.is_empty() || (!history_was_truncated && !older_history) {
        return;
    }
    let Some(notice) = notice else {
        return;
    };
    let notice_lines = crate::wrapping::word_wrap_lines(
        [Line::from(notice.to_owned()).dim()],
        usize::from(width.max(1)),
    );
    if let Some(max_rows) = max_rows {
        let available_history_rows = max_rows.saturating_sub(notice_lines.len());
        if available_history_rows == 0 {
            return;
        }
        if lines.len() > available_history_rows {
            lines.drain(..lines.len() - available_history_rows);
        }
    }
    lines.splice(0..0, notice_lines.into_iter().map(HyperlinkLine::new));
}

pub(super) fn trailing_run_start<T: 'static>(transcript_cells: &[Arc<dyn HistoryCell>]) -> usize {
    let end = transcript_cells.len();
    let mut start = end;

    while start > 0
        && transcript_cells[start - 1].is_stream_continuation()
        && transcript_cells[start - 1].as_any().is::<T>()
    {
        start -= 1;
    }

    if start > 0
        && transcript_cells[start - 1].as_any().is::<T>()
        && !transcript_cells[start - 1].is_stream_continuation()
    {
        start -= 1;
    }

    start
}

impl App {
    pub(super) fn reset_history_emission_state(&mut self) {
        self.has_emitted_history_lines = false;
        self.deferred_history_lines.clear();
        self.last_rendered_history_tail = None;
    }

    fn display_lines_for_history_insert(
        &mut self,
        cell: &dyn HistoryCell,
        width: u16,
    ) -> Vec<HyperlinkLine> {
        let mut display =
            cell.display_hyperlink_lines_for_mode(width, self.chat_widget.history_render_mode());
        if !display.is_empty() && !cell.is_stream_continuation() {
            if self.has_emitted_history_lines {
                display.insert(/*index*/ 0, HyperlinkLine::new(Line::from("")));
            } else {
                self.has_emitted_history_lines = true;
            }
        }
        display
    }

    pub(super) fn insert_history_cell_lines(
        &mut self,
        tui: &mut tui::Tui,
        cell: &dyn HistoryCell,
        width: u16,
    ) {
        if tui.is_owned_screen() {
            tui.frame_requester().schedule_frame();
            return;
        }
        let display = self.display_lines_for_history_insert(cell, width);
        if display.is_empty() {
            return;
        }
        if self.overlay.is_some() {
            self.deferred_history_lines.extend(display);
        } else {
            tui.insert_history_hyperlink_lines_with_wrap_policy(
                display,
                self.history_line_wrap_policy(),
            );
        }
    }

    /// Defer initial resume formatting until all replay cells have been collected.
    ///
    /// Resume replay can insert thousands of already-finalized history cells before the first draw.
    /// Deferring lets startup use the parallel resize renderer and its row cap.
    /// Starting this buffer while an overlay owns rendering would split transcript ownership, so
    /// overlay replay continues through the normal deferred-history path.
    pub(super) fn begin_initial_history_replay_buffer(&mut self) {
        if self.overlay.is_none() {
            self.initial_history_replay_buffer = Some(InitialHistoryReplayBuffer {
                render_from_transcript_tail: true,
                ..Default::default()
            });
        }
    }

    /// Start retaining a thread-switch transcript replay without rendering each historical cell.
    ///
    /// Defer formatting until replay completes so large uncapped histories also use the background
    /// renderer. With a row cap, the same path additionally avoids formatting the full backlog.
    pub(super) fn begin_thread_switch_history_replay_buffer(&mut self) {
        if self.overlay.is_none() {
            self.initial_history_replay_buffer = Some(InitialHistoryReplayBuffer {
                retained_lines: VecDeque::new(),
                render_from_transcript_tail: true,
                was_truncated: false,
            });
        }
    }

    /// Flush retained initial resume replay rows into terminal scrollback.
    ///
    /// Source-backed replay schedules one reflow after the replay barrier. Any already-retained
    /// display rows are otherwise flushed with the same terminal row limit.
    pub(super) fn finish_initial_history_replay_buffer(&mut self, tui: &mut tui::Tui) {
        let Some(buffer) = self.initial_history_replay_buffer.take() else {
            return;
        };
        if tui.is_owned_screen() {
            self.discard_scrollback_render_work(tui);
            tui.frame_requester().schedule_frame();
            return;
        }

        if buffer.render_from_transcript_tail || self.overlay.is_some() {
            // Reflow clears any pre-replay or partially emitted history and applies the reserved
            // history width. It also waits for an active overlay to close before rebuilding.
            self.schedule_immediate_resize_reflow(tui);
            return;
        }

        if buffer.retained_lines.is_empty() {
            self.request_scrollback_history_top_up(/*rendered_rows*/ 0);
            return;
        }

        let mut retained_lines = buffer.retained_lines.into_iter().collect::<Vec<_>>();
        let width = self
            .chat_widget
            .history_wrap_width(tui.terminal.last_known_screen_size.width);
        self.prepend_scrollback_history_notice(&mut retained_lines, buffer.was_truncated, width);
        let retained_rows = retained_lines.len();
        tui.insert_history_hyperlink_lines_with_wrap_policy(
            retained_lines,
            self.history_line_wrap_policy(),
        );
        if self.pending_thread_usage_history_refresh
            && let Err(err) = self.refresh_thread_usage_history_tail(tui)
        {
            tracing::warn!(error = %err, "failed to refresh thread usage after initial replay");
        }
        self.request_scrollback_history_top_up(retained_rows);
    }

    pub(super) fn insert_history_cell_lines_with_initial_replay_buffer(
        &mut self,
        tui: &mut tui::Tui,
        cell: &dyn HistoryCell,
        width: u16,
    ) {
        if tui.is_owned_screen() {
            tui.frame_requester().schedule_frame();
            return;
        }
        if self
            .initial_history_replay_buffer
            .as_ref()
            .is_some_and(|buffer| buffer.render_from_transcript_tail)
        {
            return;
        }

        let display = self.display_lines_for_history_insert(cell, width);

        if display.is_empty() {
            return;
        }

        let max_rows = crate::resize_reflow_cap::resize_reflow_max_rows(
            self.local_settings.terminal_resize_reflow(),
        );
        if let Some(buffer) = &mut self.initial_history_replay_buffer {
            if let Some(max_rows) = max_rows {
                Self::buffer_initial_history_replay_display_lines(buffer, display, max_rows);
            } else if self.overlay.is_some() {
                self.deferred_history_lines.extend(display);
            } else {
                tui.insert_history_hyperlink_lines_with_wrap_policy(
                    display,
                    self.history_line_wrap_policy(),
                );
            }
        }
    }

    pub(crate) fn history_line_wrap_policy(&self) -> HistoryLineWrapPolicy {
        if self.chat_widget.raw_output_mode() {
            HistoryLineWrapPolicy::Terminal
        } else {
            HistoryLineWrapPolicy::PreWrap
        }
    }

    /// Retain only the newest rendered rows for initial resume replay.
    ///
    /// The oldest rows are dropped first because terminal scrollback caps preserve the tail of the
    /// transcript. Keeping this policy local to display lines is important: trimming source cells
    /// here would make copy, transcript overlay, and future replay paths disagree about history.
    pub(super) fn buffer_initial_history_replay_display_lines(
        buffer: &mut InitialHistoryReplayBuffer,
        display: Vec<HyperlinkLine>,
        max_rows: usize,
    ) {
        buffer.retained_lines.extend(display);
        while buffer.retained_lines.len() > max_rows {
            buffer.retained_lines.pop_front();
            buffer.was_truncated = true;
        }
    }

    fn schedule_resize_reflow(&mut self, target_width: Option<u16>) -> bool {
        self.background_reflow.invalidate();
        self.transcript_reflow.schedule_debounced(target_width)
    }

    fn resize_reflow_max_rows(&self) -> Option<usize> {
        crate::resize_reflow_cap::resize_reflow_max_rows(
            self.local_settings.terminal_resize_reflow(),
        )
    }

    pub(super) fn update_visible_history_rows(&mut self, screen_size: Size) {
        let width = screen_size.width.max(/*other*/ 1);
        let viewport_height = self
            .with_chat_widget_frame(width, |desired_height, _| desired_height)
            .min(screen_size.height);
        self.transcript_reflow.set_visible_history_rows(
            screen_size
                .height
                .saturating_sub(viewport_height)
                .max(/*other*/ 1),
        );
    }

    fn clear_terminal_for_resize_replay(&mut self, tui: &mut tui::Tui) -> Result<()> {
        if tui.is_owned_screen() || tui.has_deferred_thread_switch_clear() {
            return Ok(());
        }
        if tui.is_alt_screen_active() {
            tui.terminal.clear_visible_screen()?;
        } else {
            tui.terminal.clear_scrollback_and_visible_screen_ansi()?;
        }
        let mut area = tui.terminal.viewport_area;
        if area.y > 0 {
            area.y = 0;
            tui.terminal.set_viewport_area(area);
        }
        Ok(())
    }

    /// Finish stream consolidation by repairing any resize work that happened during streaming.
    ///
    /// This is called after agent-message stream cells have either been replaced by an
    /// `AgentMarkdownCell` or found to need no replacement. If a resize happened while the stream
    /// was active or while its transient cells were still present, this method runs an immediate
    /// source-backed reflow so terminal scrollback reflects the finalized cell instead of the
    /// transient stream rows.
    pub(super) fn maybe_finish_stream_reflow(&mut self, tui: &mut tui::Tui) -> Result<()> {
        if tui.is_owned_screen() {
            self.discard_scrollback_render_work(tui);
            tui.frame_requester().schedule_frame();
            return Ok(());
        }
        if self.transcript_reflow.take_stream_finish_reflow_needed() {
            self.schedule_immediate_resize_reflow(tui);
            let screen_size = tui.terminal.last_known_screen_size;
            self.maybe_run_resize_reflow(tui, screen_size)?;
        } else if self.transcript_reflow.pending_is_due(Instant::now()) {
            tui.frame_requester().schedule_frame();
        }
        Ok(())
    }

    pub(super) fn schedule_immediate_resize_reflow(&mut self, tui: &mut tui::Tui) {
        self.background_reflow.invalidate();
        if tui.is_owned_screen() {
            self.discard_scrollback_render_work(tui);
        } else {
            self.transcript_reflow.schedule_immediate();
        }
        tui.frame_requester().schedule_frame();
    }

    /// Force stream-finalized output through the resize reflow path.
    ///
    /// Proposed plan consolidation uses this stricter path because a completed plan is inserted or
    /// replaced as one styled source-backed cell. If this reflow is skipped after a stream-time
    /// resize, the visible scrollback can keep the pre-consolidation wrapping.
    pub(super) fn finish_required_stream_reflow(&mut self, tui: &mut tui::Tui) -> Result<()> {
        if tui.is_owned_screen() {
            self.discard_scrollback_render_work(tui);
            tui.frame_requester().schedule_frame();
            return Ok(());
        }
        // Capped initial replay normally buffers per-cell display rows. A live stream tail is
        // consolidated directly into `transcript_cells`, so any retained rows no longer describe
        // the canonical transcript. Let the replay-end event render the capped transcript tail
        // once all consolidation events have been processed.
        if self.resize_reflow_max_rows().is_some()
            && let Some(buffer) = self.initial_history_replay_buffer.as_mut()
        {
            buffer.retained_lines.clear();
            buffer.render_from_transcript_tail = true;
            self.transcript_reflow.clear_stream_flags();
            return Ok(());
        }

        self.schedule_immediate_resize_reflow(tui);
        let screen_size = tui.terminal.last_known_screen_size;
        self.maybe_run_resize_reflow(tui, screen_size)?;
        if !self.transcript_reflow.has_pending_reflow() {
            self.transcript_reflow.clear_stream_flags();
        }
        Ok(())
    }

    /// Record terminal size changes and schedule any resize-sensitive transcript work.
    ///
    /// Width changes need a rebuild because transcript wrapping changes. Height changes can expose,
    /// hide, or shift rows around the inline viewport, so they also rebuild from source-backed
    /// cells. The first observed width initializes resize tracking without scheduling a rebuild,
    /// because there is no previously emitted width to repair yet.
    pub(super) fn handle_draw_size_change(
        &mut self,
        size: ratatui::layout::Size,
        last_known_screen_size: ratatui::layout::Size,
        frame_requester: &tui::FrameRequester,
    ) -> bool {
        if size != last_known_screen_size || self.transcript_reflow.visible_history_rows().is_none()
        {
            self.update_visible_history_rows(size);
        }
        let width = self.transcript_reflow.note_width(size.width);
        let reflow_needed = self.transcript_reflow.reflow_needed_for_width(size.width);
        let height_changed = size.height != last_known_screen_size.height;
        let should_rebuild_transcript = reflow_needed || height_changed;
        if width.changed || width.initialized {
            self.chat_widget.on_terminal_resize(size.width);
        }
        if should_rebuild_transcript {
            if reflow_needed && self.should_mark_reflow_as_stream_time() {
                self.transcript_reflow.mark_resize_requested_during_stream();
            }
            let target_width = reflow_needed.then_some(size.width);
            if self.schedule_resize_reflow(target_width) {
                frame_requester.schedule_frame();
            } else {
                frame_requester.schedule_frame_in(TRANSCRIPT_REFLOW_DEBOUNCE);
            }
        }
        if size != last_known_screen_size {
            self.refresh_status_line();
        }
        self.maybe_clear_resize_reflow_without_terminal();
        should_rebuild_transcript
    }

    fn maybe_clear_resize_reflow_without_terminal(&mut self) {
        let Some(deadline) = self.transcript_reflow.pending_until() else {
            return;
        };
        if Instant::now() < deadline || self.overlay.is_some() || !self.transcript_cells.is_empty()
        {
            return;
        }

        self.transcript_reflow.clear_pending_reflow();
        self.reset_history_emission_state();
    }

    pub(super) fn handle_draw_pre_render(
        &mut self,
        tui: &mut tui::Tui,
        size: ratatui::layout::Size,
    ) -> Result<()> {
        self.flush_native_history(tui);
        if tui.is_owned_screen() {
            let width = self.transcript_reflow.note_width(size.width);
            if width.changed || width.initialized {
                self.chat_widget.on_terminal_resize(size.width);
            }
            if size != tui.terminal.last_known_screen_size {
                self.refresh_status_line();
            }
            self.discard_scrollback_render_work(tui);
            return Ok(());
        }
        let should_rebuild_transcript = self.handle_draw_size_change(
            size,
            tui.terminal.last_known_screen_size,
            &tui.frame_requester(),
        );
        if should_rebuild_transcript {
            // Resize-sensitive history inserts queued before this frame may be wrapped for the old
            // viewport or targeted at rows no longer visible. Drop them and let resize reflow
            // rebuild from transcript cells.
            tui.clear_pending_history_lines();
        }
        self.maybe_run_resize_reflow(tui, size)?;
        Ok(())
    }

    /// Run a pending transcript reflow when its debounce deadline has arrived.
    ///
    /// Reflow is deferred while an overlay is active because the overlay owns the current draw
    /// surface. Callers must keep using `HistoryCell` source as the rebuild input; attempting to
    /// reuse terminal-wrapped output here would preserve exactly the stale wrapping this feature is
    /// meant to remove.
    pub(super) fn maybe_run_resize_reflow(
        &mut self,
        tui: &mut tui::Tui,
        screen_size: ratatui::layout::Size,
    ) -> Result<()> {
        if tui.is_owned_screen() {
            self.discard_scrollback_render_work(tui);
            return Ok(());
        }
        let Some(deadline) = self.transcript_reflow.pending_until() else {
            return Ok(());
        };
        let now = Instant::now();
        if now < deadline {
            // Later resize events push the reflow deadline out, while the frame scheduler coalesces
            // delayed draws to the earliest requested instant. If an early draw arrives before the
            // latest quiet-period deadline, re-arm the draw so the pending reflow cannot get stuck
            // until the next keypress.
            tui.frame_requester().schedule_frame_in(deadline - now);
            return Ok(());
        }
        if self.overlay.is_some() {
            return Ok(());
        }

        let key = self.reflow_key(screen_size.into());
        let prepared = self.background_reflow.prepare(
            self.native_history.replayable_cells(&self.transcript_cells),
            key,
            &self.app_event_tx,
        );
        if matches!(&prepared, PreparedReflow::Pending) {
            return Ok(());
        }

        self.transcript_reflow.clear_pending_reflow();

        // Track that a reflow happened during an active stream or while trailing
        // unconsolidated AgentMessageCells are still pending consolidation so
        // ConsolidateAgentMessage can schedule a follow-up reflow.
        let reflow_ran_during_stream =
            !self.transcript_cells.is_empty() && self.should_mark_reflow_as_stream_time();

        let width = match prepared {
            PreparedReflow::Ready(result) => {
                self.has_emitted_history_lines = result
                    .prepared
                    .as_ref()
                    .is_some_and(|prepared| !prepared.source.is_empty());
                self.apply_reflow_render(tui, screen_size.into(), result)?
            }
            PreparedReflow::Synchronous => self.reflow_transcript_now(tui, screen_size.into())?,
            PreparedReflow::Pending => unreachable!(),
        };
        self.transcript_reflow.mark_reflowed_width(width.0);

        if reflow_ran_during_stream {
            self.transcript_reflow.mark_ran_during_stream();
        }
        // Some terminals settle their final reported width after the repaint that handled the
        // last resize event. Request one cheap follow-up draw so `handle_draw_pre_render` can
        // sample that width and schedule a final reflow if needed.
        tui.schedule_screen_size_recheck(TRANSCRIPT_REFLOW_DEBOUNCE);

        Ok(())
    }

    fn reflow_key(&self, terminal_width: TerminalWidth) -> ReflowKey {
        ReflowKey {
            width: self.chat_widget.history_wrap_width(terminal_width.0),
            terminal_width: terminal_width.0,
            row_cap: self.resize_reflow_max_rows(),
            mode: self.chat_widget.history_render_mode(),
            theme_revision: crate::render::highlight::syntax_theme_revision(),
            terminal_fg: crate::terminal_palette::default_fg(),
            terminal_bg: crate::terminal_palette::default_bg(),
            color_level: crate::terminal_palette::stdout_color_level(),
            wrap_policy: self.history_line_wrap_policy(),
            older_history: self.scrollback_has_older_history,
            notice: self.reflow_notice_text(),
        }
    }

    pub(super) fn reflow_transcript_now(
        &mut self,
        tui: &mut tui::Tui,
        terminal_width: TerminalWidth,
    ) -> Result<TerminalWidth> {
        if tui.is_owned_screen() {
            self.discard_scrollback_render_work(tui);
            tui.frame_requester().schedule_frame();
            return Ok(terminal_width);
        }
        let width = self.chat_widget.history_wrap_width(terminal_width.0);
        if self.transcript_cells.is_empty() {
            // Drop any queued pre-resize/pre-consolidation inserts before rebuilding from cells.
            tui.clear_pending_history_lines();
            self.reset_history_emission_state();
            return Ok(terminal_width);
        }

        self.native_history.replayed();
        let reflow_result = self.render_transcript_lines_for_reflow(width);
        self.apply_reflow_render(tui, terminal_width, reflow_result)
    }

    fn apply_reflow_render(
        &mut self,
        tui: &mut tui::Tui,
        terminal_width: TerminalWidth,
        reflow_result: ReflowRenderResult,
    ) -> Result<TerminalWidth> {
        self.native_history.replayed();
        let width = self.chat_widget.history_wrap_width(terminal_width.0);
        let reflowed_rows = reflow_result.prepared.as_ref().map_or_else(
            || reflow_result.lines.len(),
            |prepared| prepared.source.len(),
        );

        // Drop any queued pre-resize/pre-consolidation inserts before rebuilding from cells.
        tui.clear_pending_history_lines();
        self.clear_terminal_for_resize_replay(tui)?;

        self.deferred_history_lines.clear();
        if let Some(prepared) = reflow_result.prepared {
            tui.insert_prepared_history(prepared, self.history_line_wrap_policy());
        } else if !reflow_result.lines.is_empty() {
            tui.insert_history_hyperlink_lines_with_wrap_policy(
                reflow_result.lines,
                self.history_line_wrap_policy(),
            );
        }
        self.last_rendered_history_tail = self
            .native_history
            .replayable_cells(&self.transcript_cells)
            .iter()
            .rev()
            .find_map(|cell| {
                let lines = cell.display_hyperlink_lines_for_mode(
                    width,
                    self.chat_widget.history_render_mode(),
                );
                (!lines.is_empty()).then(|| super::history_ui::RenderedHistoryTail {
                    cell: Arc::downgrade(cell),
                    lines,
                })
            });
        if let Some(status_history) = self.last_thread_usage_status_cell.as_mut()
            && let Some(cell) = status_history.cell.upgrade()
        {
            status_history.lines = cell
                .display_hyperlink_lines_for_mode(width, self.chat_widget.history_render_mode());
        }
        if self.pending_thread_usage_history_refresh {
            self.refresh_thread_usage_history_tail(tui)?;
        }
        self.request_scrollback_history_top_up(reflowed_rows);

        Ok(terminal_width)
    }

    /// Return whether older paginated source can fill unused configured scrollback rows.
    pub(super) fn scrollback_history_needs_top_up(&self, rendered_rows: usize) -> bool {
        self.overlay.is_none()
            && self.scrollback_has_older_history
            && self
                .resize_reflow_max_rows()
                .is_some_and(|max_rows| rendered_rows < max_rows)
            && !self
                .transcript_cells
                .iter()
                .any(|cell| cell.as_any().is::<history_cell::ClearScreenHeader>())
    }

    fn request_scrollback_history_top_up(&self, rendered_rows: usize) {
        if self.scrollback_history_needs_top_up(rendered_rows)
            && let Some(thread_id) = self.chat_widget.thread_id()
        {
            tracing::debug!(
                %thread_id,
                rendered_rows,
                max_rows = self.resize_reflow_max_rows(),
                "refilling underfilled terminal scrollback from paginated history"
            );
            self.app_event_tx
                .send(crate::app_event::AppEvent::RequestOlderScrollbackHistory { thread_id });
        }
    }

    /// Rebuild scrollback after rollback removes transcript cells.
    ///
    /// Unlike resize reflow, rollback must clear the terminal even when no cells remain. Otherwise
    /// the cancelled user prompt stays visible in scrollback despite being removed from the source
    /// transcript.
    pub(super) fn rebuild_transcript_after_backtrack(
        &mut self,
        tui: &mut tui::Tui,
        terminal_width: TerminalWidth,
    ) -> Result<bool> {
        if tui.is_owned_screen() {
            self.discard_scrollback_render_work(tui);
            tui.frame_requester().schedule_frame();
            return Ok(true);
        }
        self.native_history.replayed();
        let result = if self.transcript_cells.is_empty() {
            self.reset_history_emission_state();
            ReflowRenderResult::default()
        } else {
            match self.background_reflow.prepare(
                self.native_history.replayable_cells(&self.transcript_cells),
                self.reflow_key(terminal_width),
                &self.app_event_tx,
            ) {
                PreparedReflow::Pending => return Ok(false),
                PreparedReflow::Ready(result) => result,
                PreparedReflow::Synchronous => self.render_transcript_lines_for_reflow(
                    self.chat_widget.history_wrap_width(terminal_width.0),
                ),
            }
        };
        self.apply_reflow_render(tui, terminal_width, result)?;
        self.transcript_reflow.clear_pending_reflow();
        self.transcript_reflow.mark_reflowed_width(terminal_width.0);
        Ok(true)
    }

    /// Discard terminal render products while preserving the canonical cells and view position.
    pub(super) fn discard_scrollback_render_work(&mut self, tui: &mut tui::Tui) {
        self.background_reflow.clear();
        self.transcript_reflow.clear_pending_reflow();
        self.transcript_reflow.clear_stream_flags();
        self.reset_history_emission_state();
        tui.clear_pending_history_lines();
    }

    /// Render transcript cells for the current resize rebuild.
    ///
    /// Rendering walks backward from the transcript tail so row-capped sessions avoid formatting the
    /// full backlog. If the retained suffix begins inside a stream-continuation run, the walk extends
    /// to include the run's first cell; otherwise separators would be inserted as if the continuation
    /// were a new top-level history item. The final row trim happens after separators are restored,
    /// so the returned rows obey the cap exactly.
    pub(super) fn render_transcript_lines_for_reflow(&mut self, width: u16) -> ReflowRenderResult {
        let result = render_reflow_cells(
            self.native_history.replayable_cells(&self.transcript_cells),
            width,
            self.chat_widget.history_render_mode(),
            self.resize_reflow_max_rows(),
            &AtomicBool::new(false),
        )
        .unwrap_or_default();
        self.finish_reflow_render(width, result)
    }

    fn finish_reflow_render(
        &mut self,
        width: u16,
        mut result: ReflowRenderResult,
    ) -> ReflowRenderResult {
        self.prepend_scrollback_history_notice(
            &mut result.lines,
            result.history_was_truncated,
            width,
        );
        self.has_emitted_history_lines = !result.lines.is_empty();
        result
    }

    fn prepend_scrollback_history_notice(
        &self,
        lines: &mut Vec<HyperlinkLine>,
        history_was_truncated: bool,
        width: u16,
    ) {
        prepend_history_notice(
            lines,
            history_was_truncated,
            width,
            self.resize_reflow_max_rows(),
            self.scrollback_has_older_history,
            self.reflow_notice_text().as_deref(),
        );
    }

    fn reflow_notice_text(&self) -> Option<Arc<str>> {
        crate::keymap::primary_binding(&self.keymap.app.open_transcript).map(|binding| {
            format!(
                "Earlier messages are available — press {} to view the full transcript",
                binding.display_label()
            )
            .into()
        })
    }

    /// Return whether current transcript state should be treated as stream-time resize state.
    ///
    /// The active stream controllers cover normal streaming. The trailing-cell checks cover the
    /// narrow window after a controller has stopped but before the app has processed the
    /// consolidation event that replaces transient stream cells with source-backed cells.
    pub(super) fn should_mark_reflow_as_stream_time(&self) -> bool {
        self.chat_widget.has_active_agent_stream()
            || self.chat_widget.has_active_plan_stream()
            || trailing_run_start::<history_cell::AgentMessageCell>(&self.transcript_cells)
                < self.transcript_cells.len()
            || trailing_run_start::<history_cell::ProposedPlanStreamCell>(&self.transcript_cells)
                < self.transcript_cells.len()
    }
}

#[cfg(test)]
mod background_reflow_tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicUsize;

    #[derive(Debug)]
    struct GatedCell {
        first: AtomicBool,
        renders: AtomicUsize,
        entered: Barrier,
        released: Barrier,
    }

    impl HistoryCell for GatedCell {
        fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
            self.renders.fetch_add(1, Ordering::Relaxed);
            if self.first.swap(false, Ordering::Relaxed) {
                self.entered.wait();
                self.released.wait();
            }
            vec![Line::from(format!("width={width}"))]
        }

        fn raw_lines(&self) -> Vec<Line<'static>> {
            self.display_lines(0)
        }
    }

    #[derive(Debug)]
    struct TailCell {
        lines: Vec<HyperlinkLine>,
        continuation: bool,
    }

    impl HistoryCell for TailCell {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.lines.iter().map(|line| line.line.clone()).collect()
        }

        fn raw_lines(&self) -> Vec<Line<'static>> {
            self.display_lines(0)
        }

        fn display_hyperlink_lines(&self, _width: u16) -> Vec<HyperlinkLine> {
            self.lines.clone()
        }

        fn is_stream_continuation(&self) -> bool {
            self.continuation
        }
    }

    fn key(width: u16) -> ReflowKey {
        ReflowKey {
            width,
            terminal_width: width,
            row_cap: None,
            mode: HistoryRenderMode::Rich,
            theme_revision: 0,
            terminal_fg: None,
            terminal_bg: None,
            color_level: crate::terminal_palette::StdoutColorLevel::Unknown,
            wrap_policy: HistoryLineWrapPolicy::PreWrap,
            older_history: false,
            notice: None,
        }
    }

    #[tokio::test]
    async fn preparing_returns_before_render_and_discards_an_obsolete_width() {
        let cell = Arc::new(GatedCell {
            first: AtomicBool::new(true),
            renders: AtomicUsize::new(0),
            entered: Barrier::new(2),
            released: Barrier::new(2),
        });
        let sources: Vec<Arc<dyn HistoryCell>> = (0..32).map(|_| cell.clone() as _).collect();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let sender = crate::app_event_sender::AppEventSender::new(sender);
        let mut state = BackgroundReflow::default();
        assert!(matches!(
            state.prepare(&sources, key(222), &sender),
            PreparedReflow::Pending
        ));
        // The foreground got control back even though rendering cannot finish yet.
        cell.entered.wait();
        let first_id = state.job.as_ref().unwrap().id;
        assert!(matches!(
            state.prepare(&sources, key(74), &sender),
            PreparedReflow::Pending
        ));
        cell.released.wait();
        tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap();
        assert!(matches!(
            state.prepare(&sources, key(74), &sender),
            PreparedReflow::Pending
        ));
        assert!(!state.is_current(first_id));
        tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap();
        let PreparedReflow::Ready(result) = state.prepare(&sources, key(74), &sender) else {
            panic!("expected the current-width result");
        };
        let prepared = result.prepared.expect("worker prepared terminal output");
        assert!(!prepared.source.is_empty());
        assert!(
            prepared
                .source
                .iter()
                .filter(|line| !line.line.spans.is_empty())
                .all(|line| {
                    line.line.to_string().is_empty() || line.line.to_string() == "width=74"
                })
        );
    }

    #[tokio::test]
    async fn clearing_transcript_cancels_work_and_keeps_request_ids_unique() {
        let cell = Arc::new(GatedCell {
            first: AtomicBool::new(true),
            renders: AtomicUsize::new(0),
            entered: Barrier::new(2),
            released: Barrier::new(2),
        });
        let sources: Vec<Arc<dyn HistoryCell>> = (0..32).map(|_| cell.clone() as _).collect();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let sender = crate::app_event_sender::AppEventSender::new(sender);
        let mut state = BackgroundReflow::default();
        state.prepare(&sources, key(222), &sender);
        cell.entered.wait();
        let old_id = state.job.as_ref().unwrap().id;
        state.clear();
        assert!(!state.is_current(old_id));
        cell.released.wait();
        tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap();
        state.prepare(&sources, key(222), &sender);
        assert_ne!(state.job.as_ref().unwrap().id, old_id);
        state.clear();
    }

    #[tokio::test]
    async fn clear_header_cancels_in_flight_prefix_instead_of_extending_it() {
        let cell = Arc::new(GatedCell {
            first: AtomicBool::new(true),
            renders: AtomicUsize::new(0),
            entered: Barrier::new(2),
            released: Barrier::new(2),
        });
        let mut sources: Vec<Arc<dyn HistoryCell>> = (0..32).map(|_| cell.clone() as _).collect();
        let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
        let sender = crate::app_event_sender::AppEventSender::new(sender);
        let mut state = BackgroundReflow::default();
        state.prepare(&sources, key(74), &sender);
        cell.entered.wait();
        let old_id = state.job.as_ref().unwrap().id;
        sources.push(Arc::new(history_cell::ClearScreenHeader(
            history_cell::SessionHeaderHistoryCell::new(
                "gpt-test".to_string(),
                None,
                std::path::PathBuf::from("/tmp"),
                "test",
            ),
        )));
        sources.extend((0..32).map(|_| {
            Arc::new(TailCell {
                lines: vec![HyperlinkLine::from("after clear")],
                continuation: false,
            }) as Arc<dyn HistoryCell>
        }));
        assert!(matches!(
            state.prepare(&sources, key(74), &sender),
            PreparedReflow::Pending
        ));
        let cancelled = state
            .job
            .as_ref()
            .unwrap()
            .cancelled
            .load(Ordering::Relaxed);
        cell.released.wait();
        assert!(cancelled);
        tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap();
        assert!(matches!(
            state.prepare(&sources, key(74), &sender),
            PreparedReflow::Pending
        ));
        assert!(!state.is_current(old_id));
        tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .unwrap();
        let PreparedReflow::Ready(result) = state.prepare(&sources, key(74), &sender) else {
            panic!("expected the post-clear render");
        };
        let text = result
            .prepared
            .unwrap()
            .source
            .iter()
            .map(|line| line.line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("OpenAI Codex"));
        assert_eq!(text.matches("after clear").count(), 32);
        assert!(!text.contains("width="), "cleared prefix must stay hidden");
        assert_eq!(sources.len(), 65, "clearing must retain source history");
    }

    #[tokio::test]
    async fn appending_cells_reuses_uncapped_work_but_invalidates_capped_work() {
        for row_cap in [None, Some(10_000)] {
            let cell = Arc::new(GatedCell {
                first: AtomicBool::new(true),
                renders: AtomicUsize::new(0),
                entered: Barrier::new(2),
                released: Barrier::new(2),
            });
            let mut sources: Vec<Arc<dyn HistoryCell>> =
                (0..32).map(|_| cell.clone() as _).collect();
            let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
            let sender = crate::app_event_sender::AppEventSender::new(sender);
            let mut state = BackgroundReflow::default();
            let mut key = key(74);
            key.row_cap = row_cap;
            state.prepare(&sources, key.clone(), &sender);
            cell.entered.wait();
            sources.push(Arc::new(TailCell {
                lines: Vec::new(),
                continuation: false,
            }));
            for continuation in [true, false] {
                sources.push(Arc::new(TailCell {
                    lines: vec![crate::terminal_hyperlinks::annotate_web_urls_in_line(
                        Line::from("https://example.com/appended").bold(),
                    )],
                    continuation,
                }));
            }
            assert!(matches!(
                state.prepare(&sources, key.clone(), &sender),
                PreparedReflow::Pending
            ));
            assert_eq!(
                state
                    .job
                    .as_ref()
                    .unwrap()
                    .cancelled
                    .load(Ordering::Relaxed),
                row_cap.is_some()
            );
            cell.released.wait();
            tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .unwrap();
            assert!(matches!(
                state.prepare(&sources, key.clone(), &sender),
                PreparedReflow::Pending
            ));
            tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
                .await
                .unwrap();
            let PreparedReflow::Ready(result) = state.prepare(&sources, key, &sender) else {
                panic!("expected the extended result");
            };
            let renders = cell.renders.load(Ordering::Relaxed);
            if row_cap.is_none() {
                assert_eq!(renders, 32);
            } else {
                // Cancellation may stop the first job before all its cells start.
                assert!((33..=64).contains(&renders));
            }
            let expected = render_reflow_cells(
                &sources,
                74,
                HistoryRenderMode::Rich,
                row_cap,
                &AtomicBool::new(false),
            )
            .unwrap();
            assert_eq!(result.prepared.unwrap().source, expected.lines);
        }
    }

    #[test]
    fn extending_empty_and_continuation_prefixes_preserves_separators() {
        for prefix_lines in [Vec::new(), vec![HyperlinkLine::from("continued")]] {
            for continuation in [false, true] {
                let sources: Vec<Arc<dyn HistoryCell>> = vec![
                    Arc::new(TailCell {
                        lines: prefix_lines.clone(),
                        continuation,
                    }),
                    Arc::new(TailCell {
                        lines: vec![HyperlinkLine::from("appended")],
                        continuation: false,
                    }),
                ];
                let cancelled = AtomicBool::new(false);
                let mut prefix = render_reflow_cells(
                    &sources[..1],
                    74,
                    HistoryRenderMode::Rich,
                    None,
                    &cancelled,
                )
                .unwrap();
                let suffix = render_reflow_cells_after(
                    &sources[1..],
                    74,
                    HistoryRenderMode::Rich,
                    None,
                    &cancelled,
                    prefix.emitted_history_cell,
                )
                .unwrap();
                prefix.lines.extend(suffix.lines);
                let expected =
                    render_reflow_cells(&sources, 74, HistoryRenderMode::Rich, None, &cancelled)
                        .unwrap();
                assert_eq!(prefix.lines, expected.lines);
            }
        }
    }
}

#[cfg(test)]
#[path = "resize_reflow_tests.rs"]
mod tests;
