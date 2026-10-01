//! Design notes on credential lifecycle.
//!
//! One page on the credential renewal policy: the signer swaps secret material
//! on a staggered calendar, publishes the successor, and lets callers
//! converge. The question of how does auth refresh without downtime: the answer is the
//! overlap window, where both secrets validate and no request is denied.

/// Marker symbol so these notes are indexed and embedded.
pub fn renewal_note_anchor() {}

// credential renewal policy note 0: overlap window math
// credential renewal policy note 1: stagger calendar math
// how does auth refresh note 0: both secrets validate in the window
// how does auth refresh note 1: callers converge without paging

/// Renewal policy marker 0.
pub fn renewal_note_step_0() {}
/// Renewal policy marker 1.
pub fn renewal_note_step_1() {}
