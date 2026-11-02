use super::HistoryHydrationScope;
use super::HistoryLoadBudget;
use super::advancing_cursor;
use super::rendered_cell_rows;
use crate::history_cell::HistoryCell;
use crate::history_cell::HistoryRenderMode;
use crate::legacy_core::config::ConfigBuilder;
use crate::legacy_core::config::TerminalResizeReflowMaxRows;
use crate::local_settings::LocalSettings;
use codex_config::types::AltScreenMode;
use pretty_assertions::assert_eq;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Debug)]
struct HeightMeasuredCell {
    height: u16,
    continuation: bool,
    calls: Arc<Mutex<Vec<(u16, HistoryRenderMode, String)>>>,
}

impl HistoryCell for HeightMeasuredCell {
    fn raw_lines(&self) -> Vec<ratatui::text::Line<'static>> {
        unreachable!("height measurement should use the cell's mode-aware height")
    }

    fn display_lines(&self, _width: u16) -> Vec<ratatui::text::Line<'static>> {
        unreachable!("height measurement should use the cell's mode-aware height")
    }

    fn desired_height_for_mode(&self, width: u16, mode: HistoryRenderMode) -> u16 {
        self.calls.lock().unwrap().push((
            width,
            mode,
            std::thread::current().name().unwrap_or("").to_owned(),
        ));
        self.height
    }

    fn is_stream_continuation(&self) -> bool {
        self.continuation
    }
}

#[test]
fn parallel_page_heights_preserve_empty_cells_continuations_and_prior_rows() {
    for mode in [HistoryRenderMode::Rich, HistoryRenderMode::Raw] {
        for prior_rows in [0, 11] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let cells: Vec<Arc<dyn HistoryCell>> = (0..8)
                .flat_map(|_| {
                    [
                        (0, false),
                        (0, true),
                        (2, false),
                        (0, false),
                        (3, true),
                        (1, false),
                        (0, true),
                        (4, false),
                    ]
                })
                .map(|(height, continuation)| {
                    Arc::new(HeightMeasuredCell {
                        height,
                        continuation,
                        calls: calls.clone(),
                    }) as Arc<dyn HistoryCell>
                })
                .collect();
            let rows = rendered_cell_rows(&cells, 74, mode, prior_rows);
            // Eight groups each add ten content rows and three block separators;
            // only an initially empty transcript omits its first separator.
            assert_eq!(rows, prior_rows + 80 + 24 - usize::from(prior_rows == 0));
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 64);
            assert!(calls.iter().all(|(width, actual_mode, thread)| {
                *width == 74 && *actual_mode == mode && thread.starts_with("codex-history-")
            }));
        }
    }
}

#[test]
fn empty_and_small_pages_keep_height_accounting_without_worker_dispatch() {
    assert_eq!(rendered_cell_rows(&[], 80, HistoryRenderMode::Rich, 11), 11);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let cells: Vec<Arc<dyn HistoryCell>> = vec![Arc::new(HeightMeasuredCell {
        height: 3,
        continuation: false,
        calls: calls.clone(),
    })];
    assert_eq!(
        rendered_cell_rows(&cells, 80, HistoryRenderMode::Raw, 11),
        15
    );
    let current = std::thread::current();
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].2, current.name().unwrap_or(""));
}

#[test]
fn advancing_cursor_rejects_repeated_cursors() {
    let mut seen_cursors = HashSet::new();
    assert_eq!(
        advancing_cursor(
            /*current*/ None,
            Some("first".to_string()),
            &mut seen_cursors,
        ),
        Some("first".to_string())
    );
    assert_eq!(
        advancing_cursor(Some("first"), Some("second".to_string()), &mut seen_cursors,),
        Some("second".to_string())
    );
    assert_eq!(
        advancing_cursor(Some("second"), Some("first".to_string()), &mut seen_cursors,),
        None
    );
    assert_eq!(
        advancing_cursor(Some("second"), /*next*/ None, &mut seen_cursors),
        None
    );
}

#[tokio::test]
async fn owned_initial_history_stops_after_viewport_or_scan_budget() {
    let codex_home = tempfile::tempdir().expect("temporary codex home");
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("config");
    config.tui_fullscreen_transcript = true;
    config.tui_alternate_screen = AltScreenMode::Always;
    config.terminal_resize_reflow.max_rows = TerminalResizeReflowMaxRows::Disabled;
    let local_settings = LocalSettings::from(&config);
    let budget = HistoryLoadBudget::new(
        HistoryHydrationScope::Initial,
        Some(&config),
        Some(&local_settings),
        /*terminal_height*/ 10,
    );

    assert_eq!(
        [
            budget.next_page_size(/*rendered_rows*/ 0, /*scanned_items*/ 0),
            budget.next_page_size(/*rendered_rows*/ 29, /*scanned_items*/ 10),
            budget.next_page_size(/*rendered_rows*/ 30, /*scanned_items*/ 11),
            budget.next_page_size(/*rendered_rows*/ 0, /*scanned_items*/ 399),
            budget.next_page_size(/*rendered_rows*/ 0, /*scanned_items*/ 400),
        ],
        [Some(30), Some(100), None, Some(1), None],
    );

    let complete = HistoryLoadBudget::new(
        HistoryHydrationScope::Complete,
        Some(&config),
        /*local_settings*/ None,
        /*terminal_height*/ 10,
    );
    assert_eq!(
        complete.next_page_size(/*rendered_rows*/ 10_000, /*scanned_items*/ 10_000),
        Some(100),
    );

    config.tui_fullscreen_transcript = false;
    // The mode selected at launch wins even if thread config changes or is unavailable.
    for config in [Some(&config), None] {
        let budget = HistoryLoadBudget::new(
            HistoryHydrationScope::Initial,
            config,
            Some(&local_settings),
            /*terminal_height*/ 10,
        );
        assert_eq!(
            [
                budget.next_page_size(/*rendered_rows*/ 0, /*scanned_items*/ 0),
                budget.next_page_size(/*rendered_rows*/ 30, /*scanned_items*/ 11),
            ],
            [Some(30), None],
        );
    }
}

#[tokio::test]
async fn uncapped_native_history_has_no_height_measurement_budget() {
    let codex_home = tempfile::tempdir().unwrap();
    let mut config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .unwrap();
    config.tui_fullscreen_transcript = false;
    config.terminal_resize_reflow.max_rows = TerminalResizeReflowMaxRows::Disabled;
    let local_settings = LocalSettings::from(&config);
    let budget = HistoryLoadBudget::new(
        HistoryHydrationScope::Initial,
        Some(&config),
        Some(&local_settings),
        24,
    );
    assert_eq!(budget.rows, None);
    assert_eq!(budget.items, None);
    assert_eq!(budget.next_page_size(100_000, 100_000), Some(100));
}
