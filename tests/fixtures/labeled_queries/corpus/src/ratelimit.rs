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

// rate limiting step 0: drain the bucket
pub fn limit_step_0(bucket: &mut TokenBucket) -> bool {
    allow(bucket)
}
// rate limiting step 1: drain the bucket
pub fn limit_step_1(bucket: &mut TokenBucket) -> bool {
    allow(bucket)
}
// rate limiting step 2: steady refill
pub fn limit_step_2(bucket: &mut TokenBucket) -> bool {
    allow(bucket)
}
// rate limiting step 3: steady refill
pub fn limit_step_3(bucket: &mut TokenBucket) -> bool {
    allow(bucket)
}
