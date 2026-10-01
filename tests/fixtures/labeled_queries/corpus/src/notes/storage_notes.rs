//! Design notes on session storage.
//!
//! Hot session storage stays in the cache layer and cold pages go to the
//! ones to the file-backed store; the split is invisible to callers.

/// Marker symbol so these notes are indexed and embedded.
pub fn storage_note_anchor() {}

// session storage note 0: hot tier bounds
// session storage note 1: cold page layout
// session storage note 2: admission ordering

/// Storage policy marker 0.
pub fn storage_note_step_0() {}
/// Storage policy marker 1.
pub fn storage_note_step_1() {}
