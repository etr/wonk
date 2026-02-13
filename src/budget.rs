//! Token budget tracking for output limiting.
//!
//! Provides a simple heuristic (~4 characters per token) for estimating token
//! counts and a [`TokenBudget`] struct that tracks cumulative consumption,
//! allowing callers to stop emitting results once a budget is exhausted.

/// Estimate the number of tokens in `text` using the ~4 chars/token heuristic.
///
/// Returns `(text.len() + 3) / 4` (ceiling division by 4).
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Estimate the number of tokens from a byte length using the ~4 bytes/token
/// heuristic. Equivalent to `estimate_tokens` but avoids requiring a `&str`.
pub fn estimate_tokens_from_len(byte_len: usize) -> usize {
    byte_len.div_ceil(4)
}

/// Tracks cumulative token consumption against a fixed limit.
pub struct TokenBudget {
    limit: usize,
    used: usize,
}

impl TokenBudget {
    /// Create a new budget with the given token limit.
    pub fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    /// How many tokens remain before the budget is exhausted.
    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.used)
    }

    /// How many tokens have been consumed so far.
    pub fn used(&self) -> usize {
        self.used
    }

    /// The total token limit.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Try to consume tokens for `text`. Returns `true` if the text fits
    /// within the remaining budget (and records the consumption), or `false`
    /// if it would exceed the budget (leaving the budget unchanged).
    pub fn try_consume(&mut self, text: &str) -> bool {
        let tokens = estimate_tokens(text);
        if tokens + self.used <= self.limit {
            self.used += tokens;
            true
        } else {
            false
        }
    }

    /// Try to consume tokens estimated from a byte length. Same semantics as
    /// [`try_consume`](Self::try_consume) but avoids requiring a `&str`,
    /// eliminating the need for `String::from_utf8_lossy` when working with
    /// raw byte buffers.
    pub fn try_consume_bytes(&mut self, byte_len: usize) -> bool {
        let tokens = estimate_tokens_from_len(byte_len);
        if tokens + self.used <= self.limit {
            self.used += tokens;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- estimate_tokens -----------------------------------------------------

    #[test]
    fn estimate_tokens_empty_string() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_short_string() {
        // "hi" = 2 chars -> (2 + 3) / 4 = 1
        assert_eq!(estimate_tokens("hi"), 1);
    }

    #[test]
    fn estimate_tokens_exact_multiple() {
        // 8 chars -> (8 + 3) / 4 = 2
        assert_eq!(estimate_tokens("abcdefgh"), 2);
    }

    #[test]
    fn estimate_tokens_longer_string() {
        // 100 chars -> (100 + 3) / 4 = 25
        let text = "a".repeat(100);
        assert_eq!(estimate_tokens(&text), 25);
    }

    #[test]
    fn estimate_tokens_one_char() {
        // 1 char -> (1 + 3) / 4 = 1
        assert_eq!(estimate_tokens("x"), 1);
    }

    // -- TokenBudget ---------------------------------------------------------

    #[test]
    fn token_budget_new_has_full_remaining() {
        let budget = TokenBudget::new(500);
        assert_eq!(budget.remaining(), 500);
        assert_eq!(budget.used(), 0);
        assert_eq!(budget.limit(), 500);
    }

    #[test]
    fn try_consume_returns_true_when_within_budget() {
        let mut budget = TokenBudget::new(100);
        // "hello world" = 11 chars -> (11+3)/4 = 3 tokens
        assert!(budget.try_consume("hello world"));
        assert_eq!(budget.used(), 3);
        assert_eq!(budget.remaining(), 97);
    }

    #[test]
    fn try_consume_returns_false_when_budget_exhausted() {
        let mut budget = TokenBudget::new(5);
        // 20 chars -> (20+3)/4 = 5 tokens. Should fit exactly.
        assert!(budget.try_consume("aaaaaaaaaaaaaaaaaaaa"));
        assert_eq!(budget.used(), 5);
        // Now any more should fail.
        assert!(!budget.try_consume("extra text"));
    }

    #[test]
    fn try_consume_rejects_when_text_exceeds_remaining() {
        let mut budget = TokenBudget::new(2);
        // "a long string here!!" = 20 chars -> 5 tokens, exceeds budget of 2
        assert!(!budget.try_consume("a long string here!!"));
        // Used should still be 0 since it was rejected.
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn try_consume_accumulates_across_calls() {
        let mut budget = TokenBudget::new(10);
        assert!(budget.try_consume("abcd")); // 4 chars -> 1 token
        assert!(budget.try_consume("abcdefgh")); // 8 chars -> 2 tokens
        assert_eq!(budget.used(), 3);
        assert_eq!(budget.remaining(), 7);
    }

    #[test]
    fn try_consume_empty_string_always_succeeds() {
        let mut budget = TokenBudget::new(0);
        // Empty string = 0 tokens, should succeed even with 0 budget.
        assert!(budget.try_consume(""));
        assert_eq!(budget.used(), 0);
    }
}
