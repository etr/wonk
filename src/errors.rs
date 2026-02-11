//! Application error types.
//!
//! Provides structured error types for the query routing layer:
//! - [`DbError`] for database/index errors (enables fallback decisions)
//! - [`SearchError`] for grep-based search failures
//! - [`WonkError`] as the unified top-level error type

use thiserror::Error;

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

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
