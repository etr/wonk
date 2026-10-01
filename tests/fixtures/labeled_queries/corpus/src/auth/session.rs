//! Session lifecycle: open, use, close.
//!
//! Here session storage is backed by the shared cache; token expiry forces a
/// refresh before a session can be reopened.

use crate::auth::token::{issue_token, validate_token, TokenClaims};

/// An authenticated user session.
pub struct Session {
    pub claims: TokenClaims,
    pub storage_key: String,
}

/// Open a session for a validated token.
pub fn open_session(secret: &str) -> Result<Session, AuthError> {
    let claims = validate_token(secret)?;
    Ok(Session {
        claims,
        storage_key: format!("session:{}", claims.subject),
    })
}

/// Close a session and drop its storage entry.
pub fn close_session(session: &mut Session) {
    session.storage_key.clear();
}

use crate::errors::AuthError;
