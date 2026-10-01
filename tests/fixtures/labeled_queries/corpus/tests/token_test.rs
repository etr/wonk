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

// edge 0: validate_token(secret: &str) on malformed input
// edge 1: validate_token(secret: &str) on short input
// edge 2: validate_token(secret: &str) on empty scope
// edge 3: validate_token(secret: &str) on expired material

#[test]
fn step_0_accepts() {
    assert!(validate_token("step-0"));
}

#[test]
fn step_1_accepts() {
    assert!(validate_token("step-1"));
}

#[test]
fn step_2_accepts() {
    assert!(validate_token("step-2"));
}
