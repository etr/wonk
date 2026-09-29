//! Canonical lexical tokenizer shared by index-time term statistics
//! (TASK-078) and query-time BM25 scoring (TASK-079).
//!
//! A token is a maximal run of Unicode alphanumeric characters, lowercased.
//! Everything else — including `_`, `-`, `.`, and `::` — is a separator, so
//! `snake_case` yields `["snake", "case"]` and `x.map()` yields
//! `["x", "map"]`. There is no sub-word splitting, no stopword list, and no
//! stemming: grep candidates are matched against raw text, so the corpus
//! BM25 describes must be tokenized the same way.

use std::collections::HashMap;

/// Terms longer than this many characters are discarded, bounding index
/// size against base64 blobs in minified or generated files.
pub const MAX_TERM_LEN: usize = 64;

/// Split `text` into lowercased alphanumeric tokens.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                current.push(lower);
            }
        } else if !current.is_empty() {
            push_token(&mut tokens, &mut current);
        }
    }
    push_token(&mut tokens, &mut current);
    tokens
}

/// Flush `current` into `tokens`, dropping it when empty or over
/// [`MAX_TERM_LEN`].
fn push_token(tokens: &mut Vec<String>, current: &mut String) {
    if current.chars().count() <= MAX_TERM_LEN && !current.is_empty() {
        tokens.push(std::mem::take(current));
    } else {
        current.clear();
    }
}

/// Tokenize `text` and count occurrences per term.
pub fn term_frequencies(text: &str) -> HashMap<String, u32> {
    let mut freqs = HashMap::new();
    for token in tokenize(text) {
        *freqs.entry(token).or_insert(0u32) += 1;
    }
    freqs
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_splits_on_non_alphanumeric() {
        // Underscores, punctuation, and :: are separators; digits stay
        // attached to letters.
        let tokens = tokenize("fn foo_bar(x: &str) { x.len(); }");
        assert_eq!(tokens, vec!["fn", "foo", "bar", "x", "str", "x", "len"]);
    }

    #[test]
    fn test_lowercases_tokens() {
        let tokens = tokenize("ParseHTTPServer SQLEngine");
        assert_eq!(tokens, vec!["parsehttpserver", "sqlengine"]);
    }

    #[test]
    fn test_keeps_digits_and_unicode() {
        let tokens = tokenize("foo2 3rd über café");
        assert_eq!(tokens, vec!["foo2", "3rd", "über", "café"]);
    }

    #[test]
    fn test_drops_tokens_over_max_term_len() {
        let ok = "a".repeat(MAX_TERM_LEN);
        let too_long = "b".repeat(MAX_TERM_LEN + 1);
        let tokens = tokenize(&format!("{ok} {too_long}"));
        assert_eq!(tokens, vec![ok]);
    }

    #[test]
    fn test_empty_and_punctuation_only_input() {
        assert!(tokenize("").is_empty());
        assert!(tokenize("... :: --- _ _||| ///").is_empty());
    }

    #[test]
    fn test_term_frequencies_counts_occurrences() {
        let mut expected = HashMap::new();
        expected.insert("x".to_string(), 3);
        expected.insert("map".to_string(), 1);
        expected.insert("rs".to_string(), 1);
        assert_eq!(term_frequencies("x.map() x_rs x"), expected);
    }

    #[test]
    fn test_term_frequencies_empty_input() {
        assert!(term_frequencies("").is_empty());
    }
}
