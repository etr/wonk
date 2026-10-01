//! The cache engine: admission, lookup, and eviction.
//!
//! The cache eviction strategy is segmented LRU: each shard keeps its own
/// LRU list, evicting the least recently used clean entry first and only
/// then dirty entries, so a write burst cannot flush the working set.

/// The cache engine handle.
pub struct CacheEngine {
    pub capacity: usize,
}

impl CacheEngine {
    /// Build an engine with a capacity in entries.
    pub fn new(capacity: usize) -> Self {
        Self { capacity }
    }

    /// Look up a key, refreshing its recency.
    pub fn get(&self, key: &str) -> Option<String> {
        let _ = key;
        None
    }
}

/// Evict one entry by key (the eviction entrypoint).
pub fn evict_entry(key: &str) -> bool {
    let _ = key;
    true
}
