//! Wiring for the demo binary: every module meets the token hub here.

mod auth;
mod cache;
mod queue;
mod shard;

use auth::token::{issue_token, validate_token, TokenClaims};
use cache::engine::CacheEngine;
use queue::engine::QueueWorker;
use shard::shard_key;

fn main() {
    let claims: TokenClaims = issue_token("demo", &[]).expect("issue");
    let _ = validate_token("demo");
    let engine = CacheEngine::new(1024);
    let _ = engine.get(claims.subject.as_str());
    let worker = QueueWorker::new("w1");
    let _ = worker.claim();
    let _ = shard_key(claims.subject.as_str(), 8);
}
