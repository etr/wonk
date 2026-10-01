//! Rate limiting.
//!
//! Here rate limiting is a token bucket per subject with a shared refill
//! clock; bursts drain the bucket and steady traffic refills it.

pub struct TokenBucket {
    pub capacity: u32,
    pub tokens: u32,
}

pub fn allow(bucket: &mut TokenBucket) -> bool {
    if bucket.tokens > 0 {
        bucket.tokens -= 1;
        true
    } else {
        false
    }
}
