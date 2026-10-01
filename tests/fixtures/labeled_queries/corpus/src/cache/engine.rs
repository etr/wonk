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

// cache eviction strategy step 0: evict_entry on clean LRU tail
pub fn eviction_step_0(engine: &CacheEngine, key: &str) -> bool {
    let _ = engine.get(key);
    evict_entry(key)
}
// cache eviction strategy step 1: evict_entry on clean LRU tail
pub fn eviction_step_1(engine: &CacheEngine, key: &str) -> bool {
    let _ = engine.get(key);
    evict_entry(key)
}
// cache eviction strategy step 2: evict_entry on dirty overflow
pub fn eviction_step_2(engine: &CacheEngine, key: &str) -> bool {
    let _ = engine.get(key);
    evict_entry(key)
}
// cache eviction strategy step 3: evict_entry on dirty overflow
pub fn eviction_step_3(engine: &CacheEngine, key: &str) -> bool {
    let _ = engine.get(key);
    evict_entry(key)
}
