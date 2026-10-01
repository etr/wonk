//! Design notes on credential lifecycle.
//!
//! One page on the credential renewal policy: the signer swaps secret material
//! on a staggered calendar, publishes the successor, and lets callers
//! converge. The question of how does auth refresh without downtime: the answer is the
//! overlap window, where both secrets validate and no request is denied.

/// Marker symbol so these notes are indexed and embedded.
pub fn renewal_note_anchor() {}
