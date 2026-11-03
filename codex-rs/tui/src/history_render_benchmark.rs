//! Offline rendering benchmark. Reads saved history; never connects to an app server or executes
//! a recorded command. Both modes use production command, patch, message, and highlighting code.

use std::collections::HashSet;
use std::hash::Hash;
use std::hash::Hasher;
use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::exec_cell::CommandOutput;
use crate::exec_cell::new_active_exec_command;
use crate::history_cell::HistoryCell;
use crate::history_render;
use crate::render::highlight;
use crate::terminal_hyperlinks::HyperlinkLine;

pub fn run() -> anyhow::Result<serde_json::Value> {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    anyhow::ensure!(
        (2..=3).contains(&arguments.len()),
        "usage: history-render-bench baseline|parallel ROLLOUT.jsonl [WORKERS]"
    );
    let parallel = match arguments[0].as_str() {
        "baseline" => false,
        "parallel" => true,
        _ => anyhow::bail!("mode must be baseline or parallel"),
    };
    if let Some(workers) = arguments.get(2) {
        let workers: usize = workers.parse()?;
        anyhow::ensure!(workers > 0, "worker count must be positive");
        history_render::set_benchmark_workers(workers);
    }
    highlight::BENCHMARK_CACHE_ENABLED.store(parallel, Ordering::Relaxed);
    // The benchmark is headless: mark terminal probing complete without querying or consuming
    // terminal input. Every variant uses the same deterministic unknown-color fallback.
    #[cfg(any(unix, windows))]
    crate::terminal_palette::set_default_colors_from_startup_probe(None);
    highlight::set_theme_override(Some("gruvbox-light".to_owned()), None);
    let mut cells: Vec<Arc<dyn HistoryCell>> = Vec::new();
    let mut seen = HashSet::new();
    let mut command_bytes = 0usize;
    let mut commands = 0usize;
    let mut patches = 0usize;
    let mut messages = 0usize;
    let mut cwd = std::path::PathBuf::from(".");
    for line in std::io::BufReader::new(std::fs::File::open(&arguments[1])?).lines() {
        let record: serde_json::Value = serde_json::from_str(&line?)?;
        let payload = &record["payload"];
        if record["type"] == "session_meta" {
            if let Some(directory) = payload["cwd"].as_str() {
                cwd = directory.into();
            }
            continue;
        }
        let item = &payload["item"];
        if record["type"] != "event_msg"
            || payload["type"] != "item_completed"
            || !matches!(
                item["type"].as_str(),
                Some("CommandExecution" | "FileChange" | "AgentMessage" | "UserMessage")
            )
        {
            continue;
        }
        let id = item["id"].as_str().unwrap_or_default().to_owned();
        if !seen.insert(id.clone()) {
            continue;
        }
        match item["type"].as_str() {
            Some("FileChange") => {
                let changes = serde_json::from_value(item["changes"].clone())?;
                cells.push(Arc::new(crate::history_cell::new_patch_event(
                    changes, &cwd,
                )));
                patches += 1;
                continue;
            }
            Some("AgentMessage" | "UserMessage") => {
                let text = item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                if item["type"] == "AgentMessage" {
                    cells.push(Arc::new(
                        crate::history_cell::AgentMarkdownCell::new_with_inline_visualizations(
                            text, &cwd, None,
                        ),
                    ));
                } else {
                    cells.push(Arc::new(crate::history_cell::UserHistoryCell {
                        item_id: Default::default(),
                        message: text,
                        text_elements: Vec::new(),
                        local_image_paths: Vec::new(),
                        remote_image_urls: Vec::new(),
                        spoken: false,
                    }));
                }
                messages += 1;
                continue;
            }
            _ => {}
        }
        commands += 1;
        let command: Vec<String> = serde_json::from_value(item["command"].clone())?;
        command_bytes += command.iter().map(String::len).sum::<usize>();
        let source: codex_protocol::protocol::ExecCommandSource =
            serde_json::from_value(item["source"].clone())?;
        let parsed = serde_json::from_value(item["parsed_cmd"].clone())?;
        // Keep commands as separate finalized cells. Grouping changes UI presentation and is
        // deliberately excluded so both modes perform exactly the same command-render workload.
        let mut cell =
            new_active_exec_command(id.clone(), command, parsed, source.into(), None, false);
        cell.complete_call(
            &id,
            CommandOutput::new(
                item["exit_code"].as_i64().unwrap_or(0) as i32,
                item["aggregated_output"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            ),
            Duration::ZERO,
        );
        cells.push(Arc::new(cell));
    }
    anyhow::ensure!(
        !cells.is_empty(),
        "rollout has no completed command execution records"
    );
    let mut measurements = Vec::new();
    for (label, width) in [("open", 222u16), ("resize", 74), ("restore", 222)] {
        let (cpu_started, _) = process_metrics();
        let started = Instant::now();
        let (rendered, max_heartbeat_gap) = if parallel {
            let (sender, receiver) = std::sync::mpsc::channel();
            let sources = cells.clone();
            anyhow::ensure!(
                history_render::spawn(move || {
                    let result = history_render::render_tail(
                        &sources,
                        None,
                        &AtomicBool::new(false),
                        |cell| {
                            let lines = cell.display_hyperlink_lines(width);
                            history_render::Rendered {
                                rows: lines.len(),
                                continuation: false,
                                value: lines,
                            }
                        },
                    );
                    let _ = sender.send(result);
                }),
                "unable to create render workers"
            );
            // Include foreground snapshotting and first-use pool creation in the responsiveness
            // measurement, rather than starting the clock only after workers have been spawned.
            let mut last_heartbeat = started;
            let mut max_gap = Duration::ZERO;
            let rendered = loop {
                match receiver.recv_timeout(Duration::from_millis(1)) {
                    Ok(Some(result)) => break result.cells,
                    Ok(None) => anyhow::bail!("benchmark render unexpectedly cancelled"),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        let now = Instant::now();
                        max_gap = max_gap.max(now.duration_since(last_heartbeat));
                        last_heartbeat = now;
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            max_gap = max_gap.max(last_heartbeat.elapsed());
            (rendered, max_gap)
        } else {
            let rendered = cells
                .iter()
                .map(|cell| cell.display_hyperlink_lines(width))
                .collect();
            (rendered, started.elapsed())
        };
        let elapsed = started.elapsed();
        let (cpu_finished, peak_rss_bytes) = process_metrics();
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        let mut rows = 0usize;
        for lines in &rendered {
            rows += lines.len();
            for HyperlinkLine {
                line, hyperlinks, ..
            } in lines
            {
                // Include colors/modifiers and hyperlink metadata, not just visible text.
                format!("{line:?}{hyperlinks:?}").hash(&mut hash);
            }
        }
        measurements.push(serde_json::json!({
            "case": label, "width": width, "elapsed_ms": elapsed.as_secs_f64() * 1000.0,
            "max_heartbeat_gap_ms": max_heartbeat_gap.as_secs_f64() * 1000.0,
            "cpu_ms": (cpu_finished - cpu_started) * 1000.0,
            "peak_rss_mib": peak_rss_bytes as f64 / (1024.0 * 1024.0),
            "rows": rows, "output_hash": format!("{:016x}", hash.finish()),
            "cache_hits_misses_bytes_entries": highlight::benchmark_cache_stats(),
        }));
    }
    Ok(serde_json::json!({
        "mode": arguments[0], "commands": commands, "patches": patches, "messages": messages, "cells": cells.len(), "command_bytes": command_bytes,
        "theme": "gruvbox-light", "measurements": measurements,
        "workers": if parallel { history_render::worker_count() } else { 1 },
        "scope": "offline saved command, patch and message cells; excludes terminal writes and history grouping",
    }))
}

#[cfg(unix)]
fn process_metrics() -> (f64, i64) {
    // getrusage fills a caller-owned C structure. Zero is a valid initial value for its fields.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return (0.0, 0);
    }
    let cpu = usage.ru_utime.tv_sec as f64
        + usage.ru_stime.tv_sec as f64
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as f64 / 1_000_000.0;
    #[cfg(target_os = "macos")]
    let rss = usage.ru_maxrss;
    #[cfg(not(target_os = "macos"))]
    let rss = usage.ru_maxrss * 1024;
    (cpu, rss)
}

#[cfg(not(unix))]
fn process_metrics() -> (f64, i64) {
    (0.0, 0)
}
