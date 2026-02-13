//! Application error types and user-facing error formatting.
//!
//! Provides structured error types for the query routing layer:
//! - [`DbError`] for database/index errors (enables fallback decisions)
//! - [`SearchError`] for grep-based search failures
//! - [`WonkError`] as the unified top-level error type
//!
//! The [`WonkError`] type carries contextual hints and exit codes so that
//! `main()` can present human-readable diagnostics on stderr without ever
//! exposing raw panics or debug formatting.

use thiserror::Error;

// ---------------------------------------------------------------------------
// Exit codes
// ---------------------------------------------------------------------------

/// Process exit codes.
///
/// * `0` - success
/// * `1` - general runtime error
/// * `2` - usage / argument error (bad CLI invocation)
pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;

// ---------------------------------------------------------------------------
// Layer-specific error types
// ---------------------------------------------------------------------------

/// Errors arising from the SQLite index layer.
///
/// Pattern-matching on these variants drives the fallback logic in
/// [`crate::router::QueryRouter`]: when the index is missing or a query
/// returns no results, the router falls back to grep-based heuristics.
#[derive(Error, Debug)]
pub enum DbError {
    /// No index database exists for the current repository.
    #[error("no index found for this repository")]
    NoIndex,

    /// A SQL query failed at the rusqlite level.
    #[error("query failed: {0}")]
    QueryFailed(#[from] rusqlite::Error),
}

/// Errors arising from the grep-based text-search fallback.
#[derive(Error, Debug)]
pub enum SearchError {
    /// The grep search itself failed (e.g. bad pattern, I/O error).
    #[error("search failed: {0}")]
    SearchFailed(String),
}

// ---------------------------------------------------------------------------
// Unified application error
// ---------------------------------------------------------------------------

/// Unified error type for the entire application.
///
/// Allows callers to propagate any layer's error through a single `Result`
/// type while still enabling pattern matching on the specific variant.
#[derive(Error, Debug)]
pub enum WonkError {
    #[error(transparent)]
    Db(#[from] DbError),

    #[error(transparent)]
    Search(#[from] SearchError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A usage / argument error (exit code 2).
    #[error("{0}")]
    Usage(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl WonkError {
    /// Return the appropriate process exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            WonkError::Usage(_) => EXIT_USAGE,
            _ => EXIT_ERROR,
        }
    }

    /// Return an optional human-readable hint that may help the user fix
    /// the problem.  Returns `None` when no specific guidance applies.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            WonkError::Db(DbError::NoIndex) => {
                Some("run `wonk init` to build an index for this repository")
            }
            WonkError::Db(DbError::QueryFailed(_)) => {
                Some("the index may be corrupt; try `wonk init` to rebuild it")
            }
            WonkError::Search(SearchError::SearchFailed(_)) => {
                Some("check your search pattern for syntax errors")
            }
            WonkError::Io(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Some("verify the file or directory exists")
            }
            WonkError::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                Some("check file permissions")
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Conversion: anyhow::Error -> WonkError  (for ? in dispatch)
// ---------------------------------------------------------------------------

// The `#[from] anyhow::Error` on `Other` already provides this via thiserror.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_usage() {
        let err = WonkError::Usage("bad flag".into());
        assert_eq!(err.exit_code(), EXIT_USAGE);
    }

    #[test]
    fn exit_code_general() {
        let err = WonkError::Db(DbError::NoIndex);
        assert_eq!(err.exit_code(), EXIT_ERROR);
    }

    #[test]
    fn exit_code_io() {
        let err = WonkError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert_eq!(err.exit_code(), EXIT_ERROR);
    }

    #[test]
    fn hint_no_index() {
        let err = WonkError::Db(DbError::NoIndex);
        assert!(err.hint().unwrap().contains("wonk init"));
    }

    #[test]
    fn hint_search_failed() {
        let err = WonkError::Search(SearchError::SearchFailed("oops".into()));
        assert!(err.hint().unwrap().contains("pattern"));
    }

    #[test]
    fn hint_io_not_found() {
        let err = WonkError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "gone"));
        assert!(err.hint().unwrap().contains("exists"));
    }

    #[test]
    fn hint_io_permission() {
        let err = WonkError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "nope",
        ));
        assert!(err.hint().unwrap().contains("permissions"));
    }

    #[test]
    fn hint_none_for_other() {
        let err = WonkError::Other(anyhow::anyhow!("something went wrong"));
        assert!(err.hint().is_none());
    }

    #[test]
    fn display_no_debug_formatting() {
        let err = WonkError::Db(DbError::NoIndex);
        let msg = format!("{err}");
        // Should be the human-readable message, not Debug output
        assert_eq!(msg, "no index found for this repository");
        assert!(!msg.contains("DbError"));
        assert!(!msg.contains("NoIndex"));
    }

    #[test]
    fn display_usage_error() {
        let err = WonkError::Usage("missing required argument: name".into());
        assert_eq!(format!("{err}"), "missing required argument: name");
    }

    #[test]
    fn db_error_display() {
        let err = DbError::NoIndex;
        assert_eq!(format!("{err}"), "no index found for this repository");
    }

    #[test]
    fn search_error_display() {
        let err = SearchError::SearchFailed("bad pattern".to_string());
        assert_eq!(format!("{err}"), "search failed: bad pattern");
    }

    #[test]
    fn wonk_error_from_db_error() {
        let db_err = DbError::NoIndex;
        let wonk_err: WonkError = db_err.into();
        assert!(matches!(wonk_err, WonkError::Db(DbError::NoIndex)));
    }

    #[test]
    fn wonk_error_from_search_error() {
        let search_err = SearchError::SearchFailed("oops".to_string());
        let wonk_err: WonkError = search_err.into();
        assert!(matches!(wonk_err, WonkError::Search(_)));
    }

    #[test]
    fn wonk_error_from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let wonk_err: WonkError = io_err.into();
        assert!(matches!(wonk_err, WonkError::Io(_)));
    }

    #[test]
    fn hint_query_failed() {
        let inner =
            rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(1), Some("test".into()));
        let err = WonkError::Db(DbError::QueryFailed(inner));
        assert!(err.hint().unwrap().contains("rebuild"));
    }
}
