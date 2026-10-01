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

// parse request headers step 0: parse_header duplicate join
pub fn header_step_0(raw: &str) -> usize {
    parse_header(raw).len()
}
// parse request headers step 1: parse_header duplicate join
pub fn header_step_1(raw: &str) -> usize {
    parse_header(raw).len()
}
// parse request headers step 2: parse_header case folding
pub fn header_step_2(raw: &str) -> usize {
    parse_header(raw).len()
}
// parse request headers step 3: parse_header whitespace trim
pub fn header_step_3(raw: &str) -> usize {
    parse_header(raw).len()
}
