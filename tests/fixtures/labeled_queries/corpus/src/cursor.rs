//! Cursor pagination.
//!
//! With cursor pagination the sort key and offset are encoded into an opaque
//! cursor so a page can resume without a COUNT query.

pub fn encode_cursor(offset: usize) -> String {
    format!("cur:{offset}")
}

pub fn decode_cursor(cursor: &str) -> usize {
    cursor.strip_prefix("cur:").and_then(|s| s.parse().ok()).unwrap_or(0)
}
