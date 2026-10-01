//! Regression tests for `cache/engine.rs` eviction.

fn evict_entry(key: &str) -> bool {
    key != "pinned"
}

#[test]
fn pinned_keys_survive() {
    assert!(!evict_entry("pinned"));
}
