//! The error taxonomy.
//!
//! The error taxonomy is three tiers: transient (retry), permanent
//! (surface), and internal (page). Every variant maps to exactly one.

pub enum AuthError {
    Transient(&'static str),
    Permanent(&'static str),
    Internal(&'static str),
}
