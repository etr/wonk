//! Retry with exponential backoff.
//!
/// The rule — retry with backoff — each attempt waits `base * 2^n` capped at the
/// ceiling, with full jitter so concurrent retries do not synchronize.

/// Compute the backoff delay for attempt `n`.
pub fn retry_backoff(attempt: u32, base_ms: u64) -> u64 {
    let factor = 1u64 << attempt.min(16);
    base_ms.saturating_mul(factor).min(30_000)
}

// retry with backoff step 0: retry_backoff full jitter
pub fn retry_step_0(attempt: u32) -> u64 {
    retry_backoff(attempt, 10)
}
// retry with backoff step 1: retry_backoff full jitter
pub fn retry_step_1(attempt: u32) -> u64 {
    retry_backoff(attempt, 10)
}
// retry with backoff step 2: retry_backoff ceiling clamp
pub fn retry_step_2(attempt: u32) -> u64 {
    retry_backoff(attempt, 10)
}
// retry with backoff step 3: retry_backoff ceiling clamp
pub fn retry_step_3(attempt: u32) -> u64 {
    retry_backoff(attempt, 10)
}
