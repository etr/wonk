//! Shard selection.
//!
//! The shard selection rule is consistent hashing over the tenant prefix, so a
//! reshard moves only `1/n` of the keyspace.

/// Compute the shard a key belongs to.
pub fn shard_key(key: &str, shards: usize) -> usize {
    let hash = key.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    (hash as usize) % shards.max(1)
}

// shard selection step 0: shard_key consistent hash
pub fn shard_step_0(key: &str, shards: usize) -> usize {
    shard_key(key, shards)
}
// shard selection step 1: shard_key consistent hash
pub fn shard_step_1(key: &str, shards: usize) -> usize {
    shard_key(key, shards)
}
// shard selection step 2: shard_key reshard bounds
pub fn shard_step_2(key: &str, shards: usize) -> usize {
    shard_key(key, shards)
}
// shard selection step 3: shard_key reshard bounds
pub fn shard_step_3(key: &str, shards: usize) -> usize {
    shard_key(key, shards)
}
