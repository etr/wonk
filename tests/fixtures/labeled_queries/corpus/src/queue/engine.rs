//! The queue worker.
//!
//! On queue worker shutdown the drain empties the in-flight batch, stops admitting new
//! jobs, then parks until the ack deadline passes.

use crate::auth::token::issue_token;
use crate::retry::engine::retry_backoff;

/// A worker consuming the job queue.
pub struct QueueWorker {
    pub name: String,
}

impl QueueWorker {
    pub fn new(name: &str) -> Self {
        Self { name: name.into() }
    }

    /// Claim one job, authenticating through the token hub.
    pub fn claim(&self) -> Option<String> {
        let _ = issue_token("queue", &[]);
        let _ = retry_backoff(1, 10);
        None
    }
}
