//! Width-independent, bounded cache of completed syntax highlighting.
//!
//! Reserve an entry under the map lock, then perform highlighting outside it. The entry's
//! OnceLock coalesces concurrent requests for the same source without serializing other jobs.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::hash::Hash;
use std::hash::Hasher;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use ratatui::text::Line;

const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;
const SHARDS: usize = 32;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Key {
    code: Arc<str>,
    language: Arc<str>,
    theme_revision: u64,
}

type Entry = Arc<OnceLock<Option<Arc<Vec<Line<'static>>>>>>;

struct CachedEntry {
    value: Entry,
    bytes: usize,
}

#[derive(Default)]
struct Entries {
    values: HashMap<Key, CachedEntry>,
    order: VecDeque<Key>,
    bytes: usize,
    #[cfg(feature = "history-render-bench")]
    hits: usize,
    #[cfg(feature = "history-render-bench")]
    misses: usize,
}

pub(super) struct HighlightCache {
    entries: [Mutex<Entries>; SHARDS],
    max_bytes: usize,
    total_bytes: AtomicUsize,
    eviction: Mutex<()>,
}

impl Default for HighlightCache {
    fn default() -> Self {
        Self::new(MAX_CACHE_BYTES)
    }
}

impl HighlightCache {
    fn new(max_bytes: usize) -> Self {
        Self {
            entries: std::array::from_fn(|_| Mutex::new(Entries::default())),
            max_bytes,
            total_bytes: AtomicUsize::new(0),
            eviction: Mutex::new(()),
        }
    }

    pub(super) fn get_or_init(
        &self,
        code: &str,
        language: &str,
        theme_revision: u64,
        render: impl FnOnce() -> Option<Vec<Line<'static>>>,
    ) -> Option<Arc<Vec<Line<'static>>>> {
        let key = Key {
            code: Arc::from(code),
            language: Arc::from(language),
            theme_revision,
        };
        // Hash outside the locks; independent jobs should not queue behind one global cache lock.
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hash);
        let shard = &self.entries[hash.finish() as usize % SHARDS];
        let entry = {
            let mut entries = shard.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = entries.values.get(&key).map(|entry| entry.value.clone()) {
                #[cfg(feature = "history-render-bench")]
                {
                    entries.hits += 1;
                }
                entry
            } else {
                #[cfg(feature = "history-render-bench")]
                {
                    entries.misses += 1;
                }
                let entry = Arc::new(OnceLock::new());
                let bytes = key.code.len()
                    + key.language.len()
                    + 2 * std::mem::size_of::<Key>()
                    + std::mem::size_of::<CachedEntry>()
                    + std::mem::size_of::<OnceLock<Option<Arc<Vec<Line<'static>>>>>>()
                    + 4 * std::mem::size_of::<usize>();
                entries.bytes += bytes;
                self.total_bytes.fetch_add(bytes, Ordering::Relaxed);
                entries.order.push_back(key.clone());
                entries.values.insert(
                    key.clone(),
                    CachedEntry {
                        value: entry.clone(),
                        bytes,
                    },
                );
                entry
            }
        };
        let mut initialized = false;
        let lines = entry.get_or_init(|| {
            initialized = true;
            render().map(Arc::new)
        });
        if initialized {
            {
                let mut entries = shard.lock().unwrap_or_else(|e| e.into_inner());
                // An evicted in-flight reservation may have been replaced by a newer request.
                if let Some(current) = entries.values.get_mut(&key)
                    && Arc::ptr_eq(&current.value, &entry)
                {
                    let bytes = lines.as_ref().map_or(0, |lines| rendered_bytes(lines));
                    current.bytes += bytes;
                    entries.bytes += bytes;
                    self.total_bytes.fetch_add(bytes, Ordering::Relaxed);
                }
            }
            self.trim();
        }
        lines.clone()
    }

    fn trim(&self) {
        if self.total_bytes.load(Ordering::Relaxed) <= self.max_bytes {
            return;
        }
        // Eviction is rare and never holds a shard lock while waiting for this mutex. Cache hits
        // and normal insertions remain independent, and a large entry can use the shared budget.
        let _eviction = self.eviction.lock().unwrap_or_else(|e| e.into_inner());
        while self.total_bytes.load(Ordering::Relaxed) > self.max_bytes {
            let mut removed = false;
            for shard in &self.entries {
                if self.total_bytes.load(Ordering::Relaxed) <= self.max_bytes {
                    break;
                }
                let mut entries = shard.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(oldest) = entries.order.pop_front()
                    && let Some(entry) = entries.values.remove(&oldest)
                {
                    entries.bytes -= entry.bytes;
                    self.total_bytes.fetch_sub(entry.bytes, Ordering::Relaxed);
                    removed = true;
                }
            }
            if !removed {
                break;
            }
        }
    }

    #[cfg(feature = "history-render-bench")]
    pub(super) fn stats(&self) -> [usize; 4] {
        let mut totals = [0; 4];
        for shard in &self.entries {
            let entries = shard.lock().unwrap_or_else(|e| e.into_inner());
            totals[0] += entries.hits;
            totals[1] += entries.misses;
            totals[2] += entries.bytes;
            totals[3] += entries.values.len();
        }
        totals
    }
}

fn rendered_bytes(lines: &[Line<'static>]) -> usize {
    std::mem::size_of_val(lines)
        + lines
            .iter()
            .map(|line| {
                line.spans.capacity() * std::mem::size_of::<ratatui::text::Span<'static>>()
                    + line
                        .spans
                        .iter()
                        .map(|span| span.content.len())
                        .sum::<usize>()
            })
            .sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn concurrent_identical_requests_share_one_render() {
        let cache = HighlightCache::default();
        let barrier = Barrier::new(4);
        let renders = AtomicUsize::new(0);
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    barrier.wait();
                    let lines = cache.get_or_init("echo hello", "bash", 0, || {
                        renders.fetch_add(1, Ordering::Relaxed);
                        Some(vec![Line::from("echo hello")])
                    });
                    assert_eq!(lines.unwrap()[0], Line::from("echo hello"));
                });
            }
        });
        assert_eq!(renders.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn source_language_and_theme_are_part_of_the_key() {
        let cache = HighlightCache::default();
        let renders = AtomicUsize::new(0);
        for (source, language, theme) in [
            ("same", "bash", 0),
            ("same", "bash", 0),
            ("other", "bash", 0),
            ("same", "rust", 0),
            ("same", "bash", 1),
        ] {
            cache.get_or_init(source, language, theme, || {
                renders.fetch_add(1, Ordering::Relaxed);
                Some(vec![])
            });
        }
        assert_eq!(renders.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn one_entry_can_use_more_than_a_single_shards_share() {
        let cache = HighlightCache::new(64 * 1024);
        let renders = AtomicUsize::new(0);
        for _ in 0..2 {
            cache.get_or_init("large", "bash", 0, || {
                renders.fetch_add(1, Ordering::Relaxed);
                Some(vec![Line::from("x".repeat(4096))])
            });
        }
        assert_eq!(renders.load(Ordering::Relaxed), 1);
        assert!(cache.total_bytes.load(Ordering::Relaxed) <= cache.max_bytes);
    }

    #[test]
    fn oversized_entries_do_not_stay_in_the_cache() {
        let cache = HighlightCache::new(8);
        let renders = AtomicUsize::new(0);
        for _ in 0..2 {
            cache.get_or_init("echo hello", "bash", 0, || {
                renders.fetch_add(1, Ordering::Relaxed);
                Some(vec![Line::from("echo hello")])
            });
        }
        assert_eq!(renders.load(Ordering::Relaxed), 2);
        assert!(
            cache
                .entries
                .iter()
                .all(|shard| shard.lock().unwrap().values.is_empty())
        );
    }
}
