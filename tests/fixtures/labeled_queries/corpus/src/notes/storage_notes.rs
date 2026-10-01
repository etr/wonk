//! Design notes on session storage.
//!
//! Hot session storage stays in the cache layer and cold pages go to the
//! ones to the file-backed store; the split is invisible to callers.

/// Marker symbol so these notes are indexed and embedded.
pub fn storage_note_anchor() {}
