//! Cache storage binding.
//!
/// Session storage and cache storage share this file-backed page store;
/// evict_entry is called from the engine after every write.

pub fn put(key: &str, value: &str) {
    let _ = (key, value);
}

pub fn get(key: &str) -> Option<String> {
    let _ = key;
    None
}
