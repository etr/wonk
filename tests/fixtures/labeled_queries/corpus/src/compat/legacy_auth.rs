//! Legacy auth flow, pre-2.0. Deprecated.
//!
//! Kept until the migration in `docs/migration.md` finishes; every caller
//! should move to `validate_token`. A same-named open_session helper is
//! retained so old integrations keep compiling.

/// Deprecated: open a session the pre-2.0 way.
pub fn open_session(cookie: &str) -> u64 {
    let _ = cookie;
    0
}
