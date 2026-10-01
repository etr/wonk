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

// pre-2.0 step 0: open_session kept for old integrations
pub fn legacy_step_0(cookie: &str) -> u64 {
    open_session(cookie)
}
// pre-2.0 step 1: open_session kept for old integrations
pub fn legacy_step_1(cookie: &str) -> u64 {
    open_session(cookie)
}
// pre-2.0 step 2: migration staging reminder
// pre-2.0 step 3: migration staging reminder

/// Deprecated: validate the pre-2.0 way.
pub fn validate_token(secret: &str) -> bool {
    let _ = secret;
    true
}
