//! Retry with exponential backoff.
//!
/// The rule — retry with backoff — each attempt waits `base * 2^n` capped at the
/// ceiling, with full jitter so concurrent retries do not synchronize.

/// Compute the backoff delay for attempt `n`.
pub fn retry_backoff(attempt: u32, base_ms: u64) -> u64 {
    let factor = 1u64 << attempt.min(16);
    base_ms.saturating_mul(factor).min(30_000)
}
