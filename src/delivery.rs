//! Search delivery planning before feedback capture.

use crate::budget::TokenBudget;
use crate::feedback::StoredSlate;
use crate::output::{SearchOutput, WhyOutput};
use crate::rerank::{RankedSearch, ScoredResult};

/// One row selected for emission, with its original flattened 1-based rank.
pub struct DeliveredSearchRow<'a> {
    pub item: &'a ScoredResult,
    pub rank: usize,
    pub output: SearchOutput,
}

/// The budget/page decision reused by feedback storage and rendering.
pub struct DeliveredSearchPage<'a> {
    pub rows: Vec<DeliveredSearchRow<'a>>,
    pub truncated: usize,
}

impl<'a> DeliveredSearchPage<'a> {
    pub fn feedback_members(&self) -> Vec<(&'a ScoredResult, usize)> {
        self.rows.iter().map(|row| (row.item, row.rank)).collect()
    }

    /// Corresponding stored members avoid scanning the slate for each row.
    pub fn stamp(&mut self, slate: &StoredSlate) {
        for (row, member) in self.rows.iter_mut().zip(&slate.members) {
            row.output.slate = Some(slate.token.clone());
            row.output.identity = Some(member.identity.clone());
        }
    }

    pub fn clear_feedback(&mut self) {
        for row in &mut self.rows {
            row.output.slate = None;
            row.output.identity = None;
        }
    }
}

/// Select a search page using a surface's existing accounting rule, before
/// feedback slate is persisted. Fixed-width metadata reserves its rendered cost.
/// Numeric hex placeholders reserve TOON's maximum quoting cost (up to four
/// extra bytes per row); JSON is exact. Emit selected rows without rechecking.
pub fn select_search_page(
    ranked: &RankedSearch,
    why: bool,
    feedback: bool,
    mut accepts: impl FnMut(&SearchOutput) -> std::io::Result<bool>,
) -> std::io::Result<DeliveredSearchPage<'_>> {
    let mut rows = Vec::new();
    let mut truncated = 0;
    for (index, item) in ranked
        .groups
        .iter()
        .flat_map(|(_, items)| items)
        .enumerate()
    {
        let result = &item.classified.result;
        let mut output = SearchOutput::from_search_result(
            &result.file,
            result.line,
            result.col,
            &result.content,
        );
        output.annotation = item.classified.annotation.clone();
        output.query_class = ranked.query_class.map(|class| class.as_str().to_string());
        if feedback {
            output.slate = Some("0".repeat(16));
            output.identity = Some("0".repeat(64));
        }
        if why {
            output.why = Some(WhyOutput::from_contributions(
                item.score,
                &item.contributions,
            ));
        }
        if accepts(&output)? {
            rows.push(DeliveredSearchRow {
                item,
                rank: index + 1,
                output,
            });
        } else {
            truncated += 1;
        }
    }
    Ok(DeliveredSearchPage { rows, truncated })
}

/// MCP's established per-row JSON cost and quick size rejection. Feedback
/// identities use fixed-width placeholders until the selected slate is stored.
pub fn select_mcp_search_page(
    ranked: &RankedSearch,
    limit: Option<usize>,
    page: Option<usize>,
    feedback: bool,
) -> std::io::Result<DeliveredSearchPage<'_>> {
    let mut budget = limit.map(|limit| {
        TokenBudget::new_with_skip(
            limit,
            page.unwrap_or(1).saturating_sub(1).saturating_mul(limit),
        )
    });
    select_search_page(ranked, false, feedback, |out| {
        let Some(budget) = budget.as_mut() else {
            return Ok(true);
        };
        let estimate = (out.file.len() + out.content.len() + 20) / 4;
        if budget.remaining() < estimate {
            return Ok(false);
        }
        let serialized = serde_json::to_string(out).map_err(std::io::Error::other)?;
        Ok(budget.try_consume(&serialized))
    })
}
