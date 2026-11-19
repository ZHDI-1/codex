//! Bounded CPU workers for independent history cells, with ordered tail selection.
//!
//! The UI owns terminal output. Workers only prepare owned render products; a cancelled request
//! stops between batches, while the highlighting cache can retain useful work from that request.

use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use rayon::ThreadPool;
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;

// The long-session worker sweep reached its latency plateau at 12–24 workers. Bound CPU
// preparation at 24 while still leaving a CPU available for the UI on smaller machines.
const MAX_HISTORY_WORKERS: usize = 24;

#[cfg(feature = "history-render-bench")]
static BENCHMARK_WORKERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(feature = "history-render-bench")]
pub(crate) fn set_benchmark_workers(workers: usize) {
    BENCHMARK_WORKERS.store(workers, Ordering::Relaxed);
}

pub(crate) fn worker_count() -> usize {
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    #[cfg(feature = "history-render-bench")]
    {
        let requested = BENCHMARK_WORKERS.load(Ordering::Relaxed);
        if requested > 0 {
            return requested.min(available);
        }
    }
    available.saturating_sub(1).clamp(1, MAX_HISTORY_WORKERS)
}

fn pool() -> Option<&'static ThreadPool> {
    static POOL: OnceLock<Option<ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        ThreadPoolBuilder::new()
            .num_threads(worker_count())
            .thread_name(|index| format!("codex-history-{index}"))
            .build()
            .ok()
    })
    .as_ref()
}

pub(crate) fn spawn(work: impl FnOnce() + Send + 'static) -> bool {
    let Some(pool) = pool() else {
        return false;
    };
    pool.spawn(work);
    true
}

/// Measure independent cells on the same bounded pool as replay, retaining source order.
/// Capped hydration needs these heights before requesting the next page; highlighting here
/// must not become a serial prepass that warms the cache before replay can use its workers.
pub(crate) fn map_cells<T: Sync, R: Send>(sources: &[T], map: impl Fn(&T) -> R + Sync) -> Vec<R> {
    if sources.len() >= 32
        && let Some(pool) = pool()
    {
        pool.install(|| sources.par_iter().map(&map).collect())
    } else {
        sources.iter().map(map).collect()
    }
}

/// Coarse independent chunks avoid a scheduler rendezvous for every rendered line.
pub(crate) fn map_chunks<T: Sync, R: Send>(
    sources: &[T],
    chunk_size: usize,
    map: impl Fn(&[T]) -> R + Sync,
) -> Vec<R> {
    if sources.len() > chunk_size.saturating_mul(2)
        && let Some(pool) = pool()
    {
        pool.install(|| sources.par_chunks(chunk_size).map(&map).collect())
    } else {
        sources.chunks(chunk_size).map(map).collect()
    }
}

pub(crate) struct Rendered<T> {
    pub(crate) value: T,
    pub(crate) rows: usize,
    pub(crate) continuation: bool,
}

pub(crate) struct RenderedTail<T> {
    /// Source order, even when individual jobs finish in a different order.
    pub(crate) cells: Vec<T>,
    pub(crate) truncated: bool,
}

/// Render newest cells first in small parallel batches. A row cap stops further batches rather
/// than eagerly rendering the entire transcript and trimming it afterwards.
pub(crate) fn render_tail<T: Sync, R: Send>(
    sources: &[T],
    row_cap: Option<usize>,
    cancelled: &AtomicBool,
    render: impl Fn(&T) -> Rendered<R> + Sync,
) -> Option<RenderedTail<R>> {
    // Small transcripts are cheaper to render directly, and keep the synchronous fast path.
    let worker_pool = (sources.len() >= 32).then(pool).flatten();
    // An uncapped rebuild has no early-stop boundary. Submit the whole slice once so Rayon can
    // split useful chunks instead of making every worker rendezvous after each tiny batch.
    let batch_size = row_cap.map_or(sources.len().max(1), |cap| {
        worker_pool
            .map_or(1, ThreadPool::current_num_threads)
            .min(cap.saturating_add(1).max(1))
    });
    let mut newest_first = Vec::new();
    let mut start = sources.len();
    let mut rows = 0usize;
    let mut truncated = false;
    'batches: while start > 0 {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let end = start;
        let batch_start = end.saturating_sub(batch_size);
        let render_one = |source: &T| (!cancelled.load(Ordering::Relaxed)).then(|| render(source));
        let batch: Vec<_> = if let Some(pool) = worker_pool {
            pool.install(|| {
                sources[batch_start..end]
                    .par_iter()
                    .rev()
                    .map(render_one)
                    .collect()
            })
        } else {
            sources[batch_start..end]
                .iter()
                .rev()
                .map(render_one)
                .collect()
        };
        for cell in batch {
            let cell = cell?;
            start -= 1;
            rows += cell.rows;
            newest_first.push(cell);
            if row_cap.is_some_and(|cap| rows > cap) {
                truncated = true;
                break 'batches;
            }
        }
    }
    // A retained stream-continuation suffix needs its first cell for correct separators.
    while start > 0 && newest_first.last().is_some_and(|cell| cell.continuation) {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        start -= 1;
        newest_first.push(render(&sources[start]));
    }
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    Some(RenderedTail {
        cells: newest_first
            .into_iter()
            .rev()
            .map(|cell| cell.value)
            .collect(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn parallel_results_keep_source_order() {
        let sources: Vec<_> = (0..100).collect();
        let result = render_tail(&sources, None, &AtomicBool::new(false), |value| Rendered {
            value: *value,
            rows: 1,
            continuation: false,
        })
        .unwrap();
        assert_eq!(result.cells, sources);
        assert!(!result.truncated);
    }

    #[test]
    fn cap_bounds_work_and_preserves_continuation_start() {
        let sources: Vec<_> = (0..100).collect();
        let calls = AtomicUsize::new(0);
        let result = render_tail(&sources, Some(2), &AtomicBool::new(false), |value| {
            calls.fetch_add(1, Ordering::Relaxed);
            Rendered {
                value: *value,
                rows: 1,
                continuation: *value > 95,
            }
        })
        .unwrap();
        assert_eq!(result.cells, vec![95, 96, 97, 98, 99]);
        assert!(result.truncated);
        assert!(calls.load(Ordering::Relaxed) <= worker_count() + 5);
    }

    #[test]
    fn cancelled_request_does_no_rendering() {
        let result: Option<RenderedTail<()>> =
            render_tail(&[1, 2], None, &AtomicBool::new(true), |_| {
                panic!("cancelled request should not render")
            });
        assert!(result.is_none());
    }
}
