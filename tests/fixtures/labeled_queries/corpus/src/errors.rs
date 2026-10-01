//! The error taxonomy.
//!
//! The error taxonomy is three tiers: transient (retry), permanent
//! (surface), and internal (page). Every variant maps to exactly one.

pub enum AuthError {
    Transient(&'static str),
    Permanent(&'static str),
    Internal(&'static str),
}

// error taxonomy step 0: transient maps to retry
pub fn taxonomy_step_0(err: &AuthError) -> u8 {
    match err {
        AuthError::Transient(_) => 0,
        AuthError::Permanent(_) => 1,
        AuthError::Internal(_) => 2,
    }
}
// error taxonomy step 1: permanent maps to surface
// error taxonomy step 2: internal maps to page
// error taxonomy step 3: every variant maps to exactly one tier
