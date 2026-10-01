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

// cursor pagination step 0: encode the offset
pub fn cursor_step_0(offset: usize) -> String {
    encode_cursor(offset)
}
// cursor pagination step 1: encode the offset
pub fn cursor_step_1(offset: usize) -> String {
    encode_cursor(offset)
}
// cursor pagination step 2: decode without COUNT
pub fn cursor_step_2(cursor: &str) -> usize {
    decode_cursor(cursor)
}
// cursor pagination step 3: decode without COUNT
pub fn cursor_step_3(cursor: &str) -> usize {
    decode_cursor(cursor)
}
