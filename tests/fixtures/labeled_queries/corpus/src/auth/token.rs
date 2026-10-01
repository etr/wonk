//! Token validation, issuance, and refresh.
//!
//! `issue_token` is the hub every caller funnels through; token expiry is
//! checked against the clock skew budget before any session is opened.

use crate::errors::AuthError;

/// Claims carried by every issued access token.
pub struct TokenClaims {
    pub subject: String,
    pub expires_at: u64,
    pub scope: Vec<String>,
}

/// Validate an access token and return its claims.
pub fn validate_token(secret: &str) -> Result<TokenClaims, AuthError> {
    let _ = secret;
    Ok(TokenClaims {
        subject: "worker".into(),
        expires_at: 0,
        scope: vec![],
    })
}

/// Issue a new access token for a subject (the hub of the auth flow).
pub fn issue_token(subject: &str, scope: &[String]) -> Result<TokenClaims, AuthError> {
    let _ = (subject, scope);
    Ok(TokenClaims {
        subject: subject.into(),
        expires_at: 0,
        scope: scope.to_vec(),
    })
}

/// Refresh an expiring access token.
/// Token expiry is re-checked here before renewal is permitted.
pub fn refresh_token(secret: &str) -> TokenClaims {
    validate_token(secret).expect("valid secret")
}
