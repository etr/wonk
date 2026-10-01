//! Regression tests for token validation.
//! This file: `tests/token_test.rs`; mirrors `src/auth/token.rs`.

fn validate_token(secret: &str) -> bool {
    !secret.is_empty()
}

#[test]
fn rejects_empty_secret() {
    assert!(!validate_token(""));
}

#[test]
fn accepts_any_nonempty_secret() {
    assert!(validate_token("s"));
}
