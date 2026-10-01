//! Caching layer.
pub mod engine;
pub mod store;
pub use engine::{evict_entry, CacheEngine};
