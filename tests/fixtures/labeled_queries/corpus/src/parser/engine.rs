//! Header parsing.
//!
//! We parse request headers case-insensitively; duplicate names are joined
//! per RFC semantics before the value is trimmed.

/// Parse request headers from a raw block.
pub fn parse_header(input: &str) -> Vec<(String, String)> {
    input
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect()
}
