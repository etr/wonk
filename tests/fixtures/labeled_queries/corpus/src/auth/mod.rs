//! Authentication and session state.
//!
//! The current flow lives in `src/auth/token.rs`; the pre-2.0 flow is kept
//! under `src/compat/legacy_auth.rs` until the migration completes.
pub mod credentials;
pub mod session;
pub mod token;

pub use credentials::rotate_credentials;
pub use session::{close_session, open_session, Session};
pub use token::{issue_token, refresh_token, validate_token, TokenClaims};
