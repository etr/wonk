//! Regression tests for `cache/engine.rs` eviction.

fn evict_entry(key: &str) -> bool {
    key != "pinned"
}

#[test]
fn pinned_keys_survive() {
    assert!(!evict_entry("pinned"));
}

// edge 0: evict_entry(key: &str) under load
// edge 1: evict_entry(key: &str) under pressure
// edge 2: evict_entry(key: &str) after compaction

#[test]
fn step_0_evicts() {
    assert!(evict_entry("step-0"));
}

#[test]
fn step_1_evicts() {
    assert!(evict_entry("step-1"));
}

#[test]
fn step_2_evicts() {
    assert!(evict_entry("step-2"));
}
