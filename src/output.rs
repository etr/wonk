//! Output formatting: grep-compatible (default), JSON Lines, or TOON.
//!
//! All result data flows through a [`Formatter`] which writes to an
//! arbitrary [`std::io::Write`] destination (typically stdout).
//! Hints and errors always go to stderr via [`print_hint`] and [`print_error`].

use std::fmt::Display;
use std::io::Write;
use std::path::Path;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::budget::TokenBudget;
use crate::color;
use crate::types::ShowResult;

// ---------------------------------------------------------------------------
// Output format
// ---------------------------------------------------------------------------

/// Output format selection for CLI and MCP results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Grep,
    Json,
    Toon,
}

impl OutputFormat {
    /// Returns `true` for structured (non-grep) formats that should suppress
    /// stderr hints and disable color.
    pub fn is_structured(&self) -> bool {
        matches!(self, OutputFormat::Json | OutputFormat::Toon)
    }
}

impl std::str::FromStr for OutputFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "grep" => Ok(Self::Grep),
            "json" => Ok(Self::Json),
            "toon" => Ok(Self::Toon),
            _ => Err(format!("unknown format '{s}' (expected: grep, json, toon)")),
        }
    }
}

// ---------------------------------------------------------------------------
// Serializable output types
// ---------------------------------------------------------------------------

/// A single text search match (corresponds to `SearchResult` in `search.rs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchOutput {
    pub file: String,
    pub line: u64,
    pub col: u64,
    pub content: String,
    /// Optional annotation from ranking/dedup (e.g. "(+3 other locations)").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotation: Option<String>,
    /// Optional source indicator for blended search ("structural" or "semantic").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// A symbol definition result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolOutput {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub col: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub signature: String,
    pub language: String,
}

/// A reference (usage site) result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefOutput {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub context: String,
    /// Name of the enclosing function/method (from call graph).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller_name: Option<String>,
    pub confidence: f64,
}

/// A function/method signature result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureOutput {
    pub name: String,
    pub file: String,
    pub line: usize,
    pub signature: String,
    pub language: String,
}

/// A single file entry for `ls` results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
}

/// A symbol entry for `ls --tree` results, with an indent level for nesting.
#[derive(Debug, Clone, Serialize)]
pub struct LsSymbolEntry {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    /// Nesting depth (0 = top-level). Skipped in JSON output.
    #[serde(skip)]
    pub indent: usize,
    /// Parent scope name (e.g. class name for a method). Skipped when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// A dependency edge for `deps` / `rdeps` results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepOutput {
    pub file: String,
    pub depends_on: String,
}

/// A single member of a cluster, shown as a representative symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterMemberOutput {
    pub file: String,
    pub line: usize,
    pub symbol_name: String,
    pub symbol_kind: String,
    pub distance_to_centroid: f32,
}

/// A cluster of semantically similar symbols.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterOutput {
    pub cluster_id: usize,
    pub total_members: usize,
    pub representatives: Vec<ClusterMemberOutput>,
}

/// A semantic search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticOutput {
    pub file: String,
    pub line: usize,
    pub symbol_name: String,
    pub symbol_kind: String,
    pub similarity_score: f32,
    #[serde(skip)]
    pub symbol_id: i64,
}

/// A symbol reference in impact analysis output (the changed or impacted symbol).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactSymbolOutput {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
}

/// A single impacted symbol entry with similarity score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactEntryOutput {
    pub file: String,
    pub line: usize,
    pub symbol_name: String,
    pub symbol_kind: String,
    pub similarity_score: f32,
}

/// Full impact output for JSON: groups impacted symbols under their changed symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactOutput {
    pub changed_symbol: ImpactSymbolOutput,
    pub impacted: Vec<ImpactEntryOutput>,
}

/// A symbol with its full source body, returned by `wonk show`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowOutput {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    pub source: String,
    pub language: String,
}

impl From<&ShowResult> for ShowOutput {
    fn from(sr: &ShowResult) -> Self {
        Self {
            name: sr.name.clone(),
            kind: sr.kind.to_string(),
            file: sr.file.clone(),
            line: sr.line,
            end_line: sr.end_line,
            source: sr.source.clone(),
            language: sr.language.clone(),
        }
    }
}

/// A caller of a symbol, for `wonk callers` output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallerOutput {
    pub caller_name: String,
    pub caller_kind: String,
    pub file: String,
    pub line: usize,
    pub signature: String,
    pub depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_file: Option<String>,
    pub confidence: f64,
}

/// A symbol count entry for summary output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolCountEntry {
    pub kind: String,
    pub count: usize,
}

/// A language entry for summary output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageEntry {
    pub language: String,
    pub count: usize,
}

/// Aggregated metrics for summary output, with optional fields for detail-level control.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryMetricsOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_counts: Option<Vec<SymbolCountEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub languages: Option<Vec<LanguageEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dependency_count: Option<usize>,
}

/// Structural summary output for a file or directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummaryOutput {
    pub path: String,
    #[serde(rename = "type")]
    pub path_type: String,
    pub detail_level: String,
    pub metrics: SummaryMetricsOutput,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<SummaryOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl SummaryOutput {
    /// Build a `SummaryOutput` from a `SummaryResult`, applying detail-level filtering.
    pub fn from_result(sr: &crate::types::SummaryResult) -> Self {
        use crate::types::DetailLevel;

        let m = &sr.metrics;

        // Build shared mappings once, referenced per detail level.
        let symbol_counts_out: Vec<SymbolCountEntry> = m
            .symbol_counts
            .iter()
            .map(|(k, c)| SymbolCountEntry {
                kind: k.clone(),
                count: *c,
            })
            .collect();
        let languages_out: Vec<LanguageEntry> = m
            .language_breakdown
            .iter()
            .map(|(l, c)| LanguageEntry {
                language: l.clone(),
                count: *c,
            })
            .collect();

        let metrics = match sr.detail_level {
            DetailLevel::Rich => SummaryMetricsOutput {
                file_count: Some(m.file_count),
                line_count: Some(m.line_count),
                symbol_counts: Some(symbol_counts_out),
                languages: Some(languages_out),
                dependency_count: Some(m.dependency_count),
            },
            DetailLevel::Light => SummaryMetricsOutput {
                file_count: Some(m.file_count),
                line_count: None,
                symbol_counts: Some(symbol_counts_out),
                languages: Some(languages_out),
                dependency_count: None,
            },
            DetailLevel::Symbols => SummaryMetricsOutput {
                file_count: None,
                line_count: None,
                symbol_counts: Some(symbol_counts_out),
                languages: None,
                dependency_count: None,
            },
        };

        let children = sr.children.iter().map(SummaryOutput::from_result).collect();

        Self {
            path: sr.path.clone(),
            path_type: sr.path_type.to_string(),
            detail_level: sr.detail_level.to_string(),
            metrics,
            children,
            description: sr.description.clone(),
        }
    }
}

/// A single hop in a call path, for `wonk callpath` output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallPathHopOutput {
    pub symbol_name: String,
    pub symbol_kind: String,
    pub file: String,
    pub line: usize,
}

/// A callee of a symbol, for `wonk callees` output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalleeOutput {
    pub callee_name: String,
    pub file: String,
    pub line: usize,
    pub context: String,
    pub depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    pub confidence: f64,
}

/// A single step in an execution flow, for `wonk flows` output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowStepOutput {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub depth: usize,
}

/// A traced execution flow, for `wonk flows <entry>` output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlowOutput {
    pub entry_point: FlowStepOutput,
    pub steps: Vec<FlowStepOutput>,
    pub step_count: usize,
}

impl From<&crate::types::FlowStep> for FlowStepOutput {
    fn from(step: &crate::types::FlowStep) -> Self {
        Self {
            name: step.name.clone(),
            kind: step.kind.to_string(),
            file: step.file.clone(),
            line: step.line,
            depth: step.depth,
        }
    }
}

impl From<&crate::types::ExecutionFlow> for FlowOutput {
    fn from(flow: &crate::types::ExecutionFlow) -> Self {
        Self {
            entry_point: FlowStepOutput::from(&flow.entry_point),
            steps: flow.steps.iter().map(FlowStepOutput::from).collect(),
            step_count: flow.step_count,
        }
    }
}

/// Truncation metadata emitted as a final JSON line when `--budget` truncates
/// output. In grep mode the summary goes to stderr instead.
#[derive(Debug, Clone, Serialize)]
pub struct TruncationMeta {
    pub truncated_count: usize,
    pub budget_tokens: usize,
    pub used_tokens: usize,
}

/// Indicates whether a format call actually wrote data or was skipped due to
/// budget exhaustion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetStatus {
    /// The result was formatted and written to the output.
    Written,
    /// The result was skipped because the token budget was exhausted.
    Skipped,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

impl SearchOutput {
    /// Build a `SearchOutput` from the internal `search::SearchResult`.
    pub fn from_search_result(file: &Path, line: u64, col: u64, content: &str) -> Self {
        Self {
            file: file.to_string_lossy().into_owned(),
            line,
            col,
            content: content.to_string(),
            annotation: None,
            source: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Formatter
// ---------------------------------------------------------------------------

/// A compiled highlight pattern for match highlighting in search results.
pub struct HighlightPattern {
    re: Regex,
}

/// Output formatter that can render results in grep-compatible text,
/// JSON Lines, or TOON format.
pub struct Formatter<W: Write> {
    writer: W,
    format: OutputFormat,
    color: bool,
    highlight: Option<HighlightPattern>,
    budget: Option<TokenBudget>,
}

impl<W: Write> Formatter<W> {
    /// Create a new formatter.
    ///
    /// * `writer` - The destination for output (e.g. `std::io::stdout()`).
    /// * `format` - Output format (grep, json, or toon).
    /// * `color`  - When `true`, emit ANSI color codes in grep-style output.
    pub fn new(writer: W, format: OutputFormat, color: bool) -> Self {
        Self {
            writer,
            format,
            color,
            highlight: None,
            budget: None,
        }
    }

    /// Set a highlight pattern for match highlighting in search results.
    ///
    /// When color is enabled and a highlight pattern is set, matching portions
    /// of content will be wrapped in ANSI bold+red codes.
    pub fn set_highlight(&mut self, pattern: &str, is_regex: bool, ignore_case: bool) {
        let pattern_str = if is_regex {
            pattern.to_string()
        } else {
            regex::escape(pattern)
        };
        let pattern_str = if ignore_case {
            format!("(?i){pattern_str}")
        } else {
            pattern_str
        };
        if let Ok(re) = Regex::new(&pattern_str) {
            self.highlight = Some(HighlightPattern { re });
        }
    }

    /// Set a token budget. When set, format methods will check whether each
    /// result fits within the remaining budget before writing it.
    pub fn set_budget(&mut self, limit: usize) {
        self.budget = Some(TokenBudget::new(limit));
    }

    /// Borrow the underlying writer for direct output (e.g. table headers).
    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// Return the number of tokens consumed so far (0 if no budget is set).
    pub fn budget_used(&self) -> usize {
        self.budget.as_ref().map_or(0, |b| b.used())
    }

    /// Check the budget for a given byte buffer. Returns `Written` if the
    /// data fits (or no budget is set), `Skipped` if the budget is exhausted.
    ///
    /// Uses byte length directly for token estimation, avoiding the overhead
    /// of converting bytes to a string via `String::from_utf8_lossy`.
    fn check_budget_bytes(&mut self, data: &[u8]) -> BudgetStatus {
        match self.budget.as_mut() {
            None => BudgetStatus::Written,
            Some(budget) => {
                if budget.try_consume_bytes(data.len()) {
                    BudgetStatus::Written
                } else {
                    BudgetStatus::Skipped
                }
            }
        }
    }

    /// Returns `true` if a token budget is currently active.
    fn has_budget(&self) -> bool {
        self.budget.is_some()
    }

    /// Render a formatting closure to a temporary buffer, check the budget,
    /// and write to the real writer only if the budget allows.
    ///
    /// The closure receives a `Formatter` backed by a temporary `Vec<u8>` with
    /// the same `json`/`color`/`highlight` settings as `self`.
    ///
    /// **Note:** This is only called when a budget is active. When no budget is
    /// set, callers should use the fast path that writes directly to `self`.
    fn budgeted_write<F>(&mut self, render: F) -> std::io::Result<BudgetStatus>
    where
        F: FnOnce(&mut Formatter<&mut Vec<u8>>) -> std::io::Result<()>,
    {
        let mut buf = Vec::new();
        {
            let mut tmp = Formatter {
                writer: &mut buf,
                format: self.format,
                color: self.color,
                highlight: None,
                budget: None,
            };
            // Transfer highlight pattern temporarily.
            std::mem::swap(&mut tmp.highlight, &mut self.highlight);
            let result = render(&mut tmp);
            std::mem::swap(&mut tmp.highlight, &mut self.highlight);
            result?;
        }

        let status = self.check_budget_bytes(&buf);
        if status == BudgetStatus::Written {
            self.writer.write_all(&buf)?;
        }
        Ok(status)
    }

    /// Serialize a value to the active structured format (JSON or TOON).
    ///
    /// Only called when `self.format` is `Json` or `Toon`.
    fn serialize_structured<T: Serialize>(
        format: OutputFormat,
        value: &T,
    ) -> std::io::Result<String> {
        match format {
            OutputFormat::Json => serde_json::to_string(value).map_err(std::io::Error::other),
            OutputFormat::Toon => {
                serde_toon2::to_string(value).map_err(|e| std::io::Error::other(e.to_string()))
            }
            OutputFormat::Grep => unreachable!("serialize_structured called in grep mode"),
        }
    }

    // -- Color helper methods -----------------------------------------------

    /// Write a file path, colorized if color is enabled.
    fn write_file(&mut self, path: &str) -> std::io::Result<()> {
        if self.color {
            write!(self.writer, "{}{}{}", color::FILE, path, color::RESET)
        } else {
            write!(self.writer, "{}", path)
        }
    }

    /// Write a line number, colorized if color is enabled.
    fn write_line_no(&mut self, line: impl Display) -> std::io::Result<()> {
        if self.color {
            write!(self.writer, "{}{}{}", color::LINE_NO, line, color::RESET)
        } else {
            write!(self.writer, "{}", line)
        }
    }

    /// Write a separator (`:`) colorized if color is enabled.
    fn write_sep(&mut self) -> std::io::Result<()> {
        if self.color {
            write!(self.writer, "{}:{}", color::SEP, color::RESET)
        } else {
            write!(self.writer, ":")
        }
    }

    /// Write content with match highlighting if a highlight pattern is set.
    fn write_content(&mut self, content: &str) -> std::io::Result<()> {
        if self.color
            && let Some(ref hl) = self.highlight
        {
            return write_highlighted(&mut self.writer, content, &hl.re);
        }
        write!(self.writer, "{}", content)
    }

    /// Format a single text-search result.
    pub fn format_search_result(&mut self, result: &SearchOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            // Fast path: write directly, no temp buffer or clone needed.
            Self::render_search_result(self, result)?;
            return Ok(BudgetStatus::Written);
        }
        let result = result.clone();
        self.budgeted_write(move |fmt| Self::render_search_result(fmt, &result))
    }

    /// Shared render logic for a search result.
    fn render_search_result<W2: Write>(
        fmt: &mut Formatter<W2>,
        result: &SearchOutput,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, result)?;
            writeln!(fmt.writer, "{line}")
        } else {
            fmt.write_file(&result.file)?;
            fmt.write_sep()?;
            fmt.write_line_no(result.line)?;
            fmt.write_sep()?;
            fmt.write_content(&result.content)?;
            if let Some(ref ann) = result.annotation {
                write!(fmt.writer, "  {ann}")?;
            }
            writeln!(fmt.writer)
        }
    }

    /// Format a single symbol definition result.
    pub fn format_symbol(&mut self, sym: &SymbolOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_symbol(self, sym)?;
            return Ok(BudgetStatus::Written);
        }
        let sym = sym.clone();
        self.budgeted_write(move |fmt| Self::render_symbol(fmt, &sym))
    }

    /// Shared render logic for a symbol result.
    fn render_symbol<W2: Write>(
        fmt: &mut Formatter<W2>,
        sym: &SymbolOutput,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, sym)?;
            writeln!(fmt.writer, "{line}")
        } else {
            fmt.write_file(&sym.file)?;
            fmt.write_sep()?;
            fmt.write_line_no(sym.line)?;
            fmt.write_sep()?;
            writeln!(fmt.writer, "  {}", sym.signature)
        }
    }

    /// Format a single reference result.
    pub fn format_reference(&mut self, reference: &RefOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_reference(self, reference)?;
            return Ok(BudgetStatus::Written);
        }
        let reference = reference.clone();
        self.budgeted_write(move |fmt| Self::render_reference(fmt, &reference))
    }

    /// Shared render logic for a reference result.
    fn render_reference<W2: Write>(
        fmt: &mut Formatter<W2>,
        reference: &RefOutput,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, reference)?;
            writeln!(fmt.writer, "{line}")
        } else {
            fmt.write_file(&reference.file)?;
            fmt.write_sep()?;
            fmt.write_line_no(reference.line)?;
            fmt.write_sep()?;
            writeln!(fmt.writer, "{}", reference.context)
        }
    }

    /// Format a single signature result.
    pub fn format_signature(&mut self, sig: &SignatureOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_signature(self, sig)?;
            return Ok(BudgetStatus::Written);
        }
        let sig = sig.clone();
        self.budgeted_write(move |fmt| Self::render_signature(fmt, &sig))
    }

    /// Shared render logic for a signature result.
    fn render_signature<W2: Write>(
        fmt: &mut Formatter<W2>,
        sig: &SignatureOutput,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, sig)?;
            writeln!(fmt.writer, "{line}")
        } else {
            fmt.write_file(&sig.file)?;
            fmt.write_sep()?;
            fmt.write_line_no(sig.line)?;
            fmt.write_sep()?;
            writeln!(fmt.writer, "  {}", sig.signature)
        }
    }

    /// Format a single ls-symbol entry (used by `wonk ls --tree`).
    ///
    /// Grep format: `file:line:  [indent]kind name`
    /// JSON: all fields except `indent`.
    pub fn format_ls_symbol(&mut self, entry: &LsSymbolEntry) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_ls_symbol(self, entry)?;
            return Ok(BudgetStatus::Written);
        }
        let entry = entry.clone();
        self.budgeted_write(move |fmt| Self::render_ls_symbol(fmt, &entry))
    }

    /// Shared render logic for an ls-symbol entry.
    fn render_ls_symbol<W2: Write>(
        fmt: &mut Formatter<W2>,
        entry: &LsSymbolEntry,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, entry)?;
            writeln!(fmt.writer, "{line}")
        } else {
            let padding = "  ".repeat(entry.indent + 1);
            fmt.write_file(&entry.file)?;
            fmt.write_sep()?;
            fmt.write_line_no(entry.line)?;
            fmt.write_sep()?;
            writeln!(fmt.writer, "{}{} {}", padding, entry.kind, entry.name)
        }
    }

    /// Format a single file-list entry.
    pub fn format_file_list(&mut self, entry: &FileEntry) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_file_list(self, entry)?;
            return Ok(BudgetStatus::Written);
        }
        let entry = entry.clone();
        self.budgeted_write(move |fmt| Self::render_file_list(fmt, &entry))
    }

    /// Shared render logic for a file-list entry.
    fn render_file_list<W2: Write>(
        fmt: &mut Formatter<W2>,
        entry: &FileEntry,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, entry)?;
            writeln!(fmt.writer, "{line}")
        } else {
            fmt.write_file(&entry.path)?;
            writeln!(fmt.writer)
        }
    }

    /// Format a single dependency edge.
    pub fn format_dep(&mut self, dep: &DepOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_dep(self, dep)?;
            return Ok(BudgetStatus::Written);
        }
        let dep = dep.clone();
        self.budgeted_write(move |fmt| Self::render_dep(fmt, &dep))
    }

    /// Shared render logic for a dependency edge.
    fn render_dep<W2: Write>(fmt: &mut Formatter<W2>, dep: &DepOutput) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, dep)?;
            writeln!(fmt.writer, "{line}")
        } else {
            fmt.write_file(&dep.file)?;
            write!(fmt.writer, " -> ")?;
            fmt.write_file(&dep.depends_on)?;
            writeln!(fmt.writer)
        }
    }

    /// Format a single semantic search result.
    pub fn format_semantic_result(
        &mut self,
        result: &SemanticOutput,
    ) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_semantic_result(self, result)?;
            return Ok(BudgetStatus::Written);
        }
        let result = result.clone();
        self.budgeted_write(move |fmt| Self::render_semantic_result(fmt, &result))
    }

    /// Shared render logic for a semantic search result.
    fn render_semantic_result<W2: Write>(
        fmt: &mut Formatter<W2>,
        result: &SemanticOutput,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, result)?;
            writeln!(fmt.writer, "{line}")
        } else {
            fmt.write_file(&result.file)?;
            fmt.write_sep()?;
            fmt.write_line_no(result.line)?;
            fmt.write_sep()?;
            writeln!(
                fmt.writer,
                "  {} ({}) [{:.4}]",
                result.symbol_name, result.symbol_kind, result.similarity_score
            )
        }
    }

    /// Format a truncation metadata object (structured mode only).
    ///
    /// Emits a final line with truncation info when `--budget` truncates
    /// output. In grep mode, callers should use [`print_budget_summary`] instead.
    pub fn format_truncation_meta(&mut self, meta: &TruncationMeta) -> std::io::Result<()> {
        let line = Self::serialize_structured(self.format, meta)?;
        writeln!(self.writer, "{line}")
    }

    /// Format a single cluster member (representative symbol).
    pub fn format_cluster_member(
        &mut self,
        member: &ClusterMemberOutput,
    ) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_cluster_member(self, member)?;
            return Ok(BudgetStatus::Written);
        }
        let member = member.clone();
        self.budgeted_write(move |fmt| Self::render_cluster_member(fmt, &member))
    }

    /// Shared render logic for a cluster member.
    fn render_cluster_member<W2: Write>(
        fmt: &mut Formatter<W2>,
        member: &ClusterMemberOutput,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, member)?;
            writeln!(fmt.writer, "{line}")
        } else {
            fmt.write_file(&member.file)?;
            fmt.write_sep()?;
            fmt.write_line_no(member.line)?;
            fmt.write_sep()?;
            writeln!(
                fmt.writer,
                "  {} ({}) [{:.4}]",
                member.symbol_name, member.symbol_kind, member.distance_to_centroid
            )
        }
    }

    /// Format a full cluster (structured modes only: JSON/TOON).
    ///
    /// In grep mode, callers should use [`print_cluster_header`] for the
    /// header and [`format_cluster_member`] for each representative.
    pub fn format_cluster(&mut self, cluster: &ClusterOutput) -> std::io::Result<BudgetStatus> {
        if !self.format.is_structured() {
            return Ok(BudgetStatus::Written);
        }
        if !self.has_budget() {
            let line = Self::serialize_structured(self.format, cluster)?;
            writeln!(self.writer, "{line}")?;
            return Ok(BudgetStatus::Written);
        }
        let cluster = cluster.clone();
        self.budgeted_write(move |fmt| {
            let line = Self::serialize_structured(fmt.format, &cluster)?;
            writeln!(fmt.writer, "{line}")
        })
    }

    /// Format a single impact entry line (impacted symbol with similarity score).
    pub fn format_impact_entry(
        &mut self,
        entry: &ImpactEntryOutput,
    ) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_impact_entry(self, entry)?;
            return Ok(BudgetStatus::Written);
        }
        let entry = entry.clone();
        self.budgeted_write(move |fmt| Self::render_impact_entry(fmt, &entry))
    }

    /// Format a full impact group (changed symbol + impacted entries) for structured output.
    pub fn format_impact(&mut self, out: &ImpactOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            let line = Self::serialize_structured(self.format, out)?;
            writeln!(self.writer, "{line}")?;
            return Ok(BudgetStatus::Written);
        }
        let out = out.clone();
        self.budgeted_write(move |fmt| {
            let line = Self::serialize_structured(fmt.format, &out)?;
            writeln!(fmt.writer, "{line}")
        })
    }

    /// Shared render logic for an impact entry.
    fn render_impact_entry<W2: Write>(
        fmt: &mut Formatter<W2>,
        entry: &ImpactEntryOutput,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, entry)?;
            writeln!(fmt.writer, "{line}")
        } else {
            write!(fmt.writer, "  -> ")?;
            fmt.write_file(&entry.file)?;
            fmt.write_sep()?;
            fmt.write_line_no(entry.line)?;
            fmt.write_sep()?;
            writeln!(
                fmt.writer,
                "  {} ({}) [{:.4}]",
                entry.symbol_name, entry.symbol_kind, entry.similarity_score
            )
        }
    }

    /// Format a single `wonk show` result.
    pub fn format_show(&mut self, out: &ShowOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_show(self, out)?;
            return Ok(BudgetStatus::Written);
        }
        let out = out.clone();
        self.budgeted_write(move |fmt| Self::render_show(fmt, &out))
    }

    /// Shared render logic for a show result.
    fn render_show<W2: Write>(fmt: &mut Formatter<W2>, out: &ShowOutput) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, out)?;
            writeln!(fmt.writer, "{line}")
        } else {
            // Grep mode: number each source line starting from `out.line`.
            for (i, content) in out.source.lines().enumerate() {
                let line_no = out.line + i;
                fmt.write_line_no(format_args!("{line_no:>4}"))?;
                writeln!(fmt.writer, "| {content}")?;
            }
            Ok(())
        }
    }

    /// Format a single caller result.
    pub fn format_caller(&mut self, out: &CallerOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_caller(self, out)?;
            return Ok(BudgetStatus::Written);
        }
        let out = out.clone();
        self.budgeted_write(move |fmt| Self::render_caller(fmt, &out))
    }

    /// Shared render logic for a caller result.
    fn render_caller<W2: Write>(
        fmt: &mut Formatter<W2>,
        out: &CallerOutput,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, out)?;
            writeln!(fmt.writer, "{line}")
        } else {
            fmt.write_file(&out.file)?;
            fmt.write_sep()?;
            fmt.write_line_no(out.line)?;
            fmt.write_sep()?;
            writeln!(fmt.writer, "  {}", out.signature)
        }
    }

    /// Format a single callee result.
    pub fn format_callee(&mut self, out: &CalleeOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_callee(self, out)?;
            return Ok(BudgetStatus::Written);
        }
        let out = out.clone();
        self.budgeted_write(move |fmt| Self::render_callee(fmt, &out))
    }

    /// Shared render logic for a callee result.
    fn render_callee<W2: Write>(
        fmt: &mut Formatter<W2>,
        out: &CalleeOutput,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, out)?;
            writeln!(fmt.writer, "{line}")
        } else {
            fmt.write_file(&out.file)?;
            fmt.write_sep()?;
            fmt.write_line_no(out.line)?;
            fmt.write_sep()?;
            writeln!(fmt.writer, "{}", out.context)
        }
    }

    /// Format a call path (sequence of hops from source to target).
    pub fn format_callpath(&mut self, hops: &[CallPathHopOutput]) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_callpath(self, hops)?;
            return Ok(BudgetStatus::Written);
        }
        let hops = hops.to_vec();
        self.budgeted_write(move |fmt| Self::render_callpath(fmt, &hops))
    }

    /// Format a summary result.
    pub fn format_summary(&mut self, out: &SummaryOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_summary(self, out, 0)?;
            return Ok(BudgetStatus::Written);
        }
        let out = out.clone();
        self.budgeted_write(move |fmt| Self::render_summary(fmt, &out, 0))
    }

    /// Shared render logic for a summary result (recursive with indent level).
    fn render_summary<W2: Write>(
        fmt: &mut Formatter<W2>,
        out: &SummaryOutput,
        indent: usize,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, out)?;
            writeln!(fmt.writer, "{line}")
        } else {
            let prefix = "  ".repeat(indent);

            // Path header.
            writeln!(
                fmt.writer,
                "{prefix}Summary: {} ({})",
                out.path, out.path_type
            )?;

            let m = &out.metrics;

            if let Some(fc) = m.file_count {
                writeln!(fmt.writer, "{prefix}  Files: {fc}")?;
            }
            if let Some(lc) = m.line_count {
                writeln!(fmt.writer, "{prefix}  Lines: {lc}")?;
            }
            if let Some(ref syms) = m.symbol_counts
                && !syms.is_empty()
            {
                let parts: Vec<String> = syms
                    .iter()
                    .map(|s| format!("{}: {}", s.kind, s.count))
                    .collect();
                writeln!(fmt.writer, "{prefix}  Symbols: {}", parts.join(", "))?;
            }
            if let Some(ref langs) = m.languages
                && !langs.is_empty()
            {
                let parts: Vec<String> = langs
                    .iter()
                    .map(|l| format!("{}: {}", l.language, l.count))
                    .collect();
                writeln!(fmt.writer, "{prefix}  Languages: {}", parts.join(", "))?;
            }
            if let Some(dc) = m.dependency_count {
                writeln!(fmt.writer, "{prefix}  Dependencies: {dc}")?;
            }
            if let Some(ref desc) = out.description {
                writeln!(fmt.writer, "{prefix}  Description: {desc}")?;
            }

            // Render children recursively.
            for child in &out.children {
                Self::render_summary(fmt, child, indent + 1)?;
            }

            Ok(())
        }
    }

    /// Format a single flow entry point (list mode).
    pub fn format_flow_entry(&mut self, out: &FlowStepOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_flow_entry(self, out)?;
            return Ok(BudgetStatus::Written);
        }
        let out = out.clone();
        self.budgeted_write(move |fmt| Self::render_flow_entry(fmt, &out))
    }

    /// Shared render logic for a flow entry point.
    fn render_flow_entry<W2: Write>(
        fmt: &mut Formatter<W2>,
        out: &FlowStepOutput,
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, out)?;
            writeln!(fmt.writer, "{line}")
        } else {
            fmt.write_file(&out.file)?;
            fmt.write_sep()?;
            fmt.write_line_no(out.line)?;
            fmt.write_sep()?;
            writeln!(fmt.writer, "{} ({})", out.name, out.kind)
        }
    }

    /// Format a traced execution flow.
    pub fn format_flow(&mut self, out: &FlowOutput) -> std::io::Result<BudgetStatus> {
        if !self.has_budget() {
            Self::render_flow(self, out)?;
            return Ok(BudgetStatus::Written);
        }
        let out = out.clone();
        self.budgeted_write(move |fmt| Self::render_flow(fmt, &out))
    }

    /// Shared render logic for a traced flow.
    fn render_flow<W2: Write>(fmt: &mut Formatter<W2>, out: &FlowOutput) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            let line = Self::serialize_structured(fmt.format, out)?;
            writeln!(fmt.writer, "{line}")
        } else {
            // Chain line: entry -> step1 -> step2 ...
            let chain: Vec<&str> = out.steps.iter().map(|s| s.name.as_str()).collect();
            writeln!(fmt.writer, "{}", chain.join(" -> "))?;
            // Per-step details.
            for step in &out.steps {
                writeln!(
                    fmt.writer,
                    "  {} ({})\t{}:{}",
                    step.name, step.kind, step.file, step.line
                )?;
            }
            Ok(())
        }
    }

    /// Shared render logic for a call path.
    fn render_callpath<W2: Write>(
        fmt: &mut Formatter<W2>,
        hops: &[CallPathHopOutput],
    ) -> std::io::Result<()> {
        if fmt.format.is_structured() {
            // Emit each hop as a separate JSON/TOON line.
            for hop in hops {
                let line = Self::serialize_structured(fmt.format, hop)?;
                writeln!(fmt.writer, "{line}")?;
            }
            Ok(())
        } else {
            // Grep format: chain line then per-hop file:line details.
            let chain: Vec<&str> = hops.iter().map(|h| h.symbol_name.as_str()).collect();
            writeln!(fmt.writer, "{}", chain.join(" -> "))?;
            for hop in hops {
                writeln!(
                    fmt.writer,
                    "  {} ({})\t{}:{}",
                    hop.symbol_name, hop.symbol_kind, hop.file, hop.line
                )?;
            }
            Ok(())
        }
    }
}

/// Print a show header to stderr (grep mode): "file:start-end".
pub fn print_show_header(file: &str, start_line: usize, end_line: Option<usize>, suppress: bool) {
    if !suppress {
        if let Some(end) = end_line {
            eprintln!("{file}:{start_line}-{end}");
        } else {
            eprintln!("{file}:{start_line}");
        }
    }
}

/// Print an impact header to stderr (grep mode): "Changed: name (kind) in file:line".
pub fn print_impact_header(name: &str, kind: &str, file: &str, line: usize, suppress: bool) {
    if !suppress {
        eprintln!("Changed: {name} ({kind}) in {file}:{line}");
    }
}

/// Print a cluster header to stderr (grep mode).
pub fn print_cluster_header(cluster_id: usize, total_members: usize, suppress: bool) {
    if !suppress {
        eprintln!("Cluster {} ({} symbols):", cluster_id + 1, total_members);
    }
}

// ---------------------------------------------------------------------------
// Highlight helper (free function to avoid borrow conflicts)
// ---------------------------------------------------------------------------

/// Write content with regex matches highlighted in bold+underline+red ANSI codes.
/// Bold and underline provide non-color indicators for color-blind accessibility.
fn write_highlighted<W: Write>(writer: &mut W, content: &str, re: &Regex) -> std::io::Result<()> {
    let mut last_end = 0;
    for mat in re.find_iter(content) {
        write!(writer, "{}", &content[last_end..mat.start()])?;
        write!(
            writer,
            "{}{}{}",
            color::MATCH,
            &content[mat.start()..mat.end()],
            color::RESET
        )?;
        last_end = mat.end();
    }
    write!(writer, "{}", &content[last_end..])
}

// ---------------------------------------------------------------------------
// Stderr helpers
// ---------------------------------------------------------------------------

/// Print a hint message to stderr (suppressed when `suppress` is true).
pub fn print_hint(msg: &str, suppress: bool) {
    if !suppress {
        eprintln!("hint: {msg}");
    }
}

/// Print a budget truncation summary to stderr (grep mode).
///
/// Format: `-- {truncated} more results truncated (budget: {budget} tokens) --`
pub fn print_budget_summary(truncated: usize, budget: usize) {
    eprintln!("-- {truncated} more results truncated (budget: {budget} tokens) --");
}

/// Format the search mode indicator message.
///
/// Returns `Some("(smart: N symbols matched)")` when ranking is active, or
/// `Some("(text search)")` for plain grep mode. Returns `None` when suppressed.
pub fn format_mode_indicator(symbol_count: u64, suppress: bool) -> Option<String> {
    if suppress {
        return None;
    }
    if symbol_count > 0 {
        Some(format!("(smart: {} symbols matched)", symbol_count))
    } else {
        Some("(text search)".to_string())
    }
}

/// Print the search mode indicator to stderr.
pub fn print_mode_indicator(symbol_count: u64, suppress: bool) {
    if let Some(msg) = format_mode_indicator(symbol_count, suppress) {
        eprintln!("{msg}");
    }
}

/// Print a category header to stderr.
///
/// Headers go to stderr so they don't break grep-compatible stdout parsing.
pub fn print_category_header(header: &str) {
    eprintln!("{header}");
}

/// Print an error message to stderr.
pub fn print_error(msg: &str) {
    eprintln!("error: {msg}");
}

/// Format a [`WonkError`] to stderr with structured `error:` / `hint:` lines.
///
/// * Always prints `error: <message>` to stderr.
/// * When `suppress` is `false` and the error carries a contextual hint, also
///   prints `hint: <suggestion>` to stderr.
/// * Returns the appropriate process exit code.
pub fn format_error(err: &crate::errors::WonkError, suppress: bool) -> i32 {
    print_error(&format!("{err}"));
    if let Some(hint) = err.hint() {
        print_hint(hint, suppress);
    }
    err.exit_code()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: renders output into a String (no color).
    fn render<F, T: std::fmt::Debug>(format: OutputFormat, f: F) -> String
    where
        F: FnOnce(&mut Formatter<&mut Vec<u8>>) -> std::io::Result<T>,
    {
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, format, false);
            f(&mut fmt).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    /// Helper: renders output into a String with color enabled.
    fn render_color<F, T: std::fmt::Debug>(f: F) -> String
    where
        F: FnOnce(&mut Formatter<&mut Vec<u8>>) -> std::io::Result<T>,
    {
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, true);
            f(&mut fmt).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    // -- SearchOutput --------------------------------------------------------

    #[test]
    fn search_result_grep_format() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_search_result(&result));
        assert_eq!(out, "src/main.rs:42:fn main() {}\n");
    }

    #[test]
    fn search_result_json_format() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_search_result(&result));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["file"], "src/main.rs");
        assert_eq!(v["line"], 42);
        assert_eq!(v["col"], 1);
        assert_eq!(v["content"], "fn main() {}");
    }

    // -- SymbolOutput --------------------------------------------------------

    #[test]
    fn symbol_grep_format() {
        let sym = SymbolOutput {
            name: "main".into(),
            kind: "function".into(),
            file: "src/main.rs".into(),
            line: 10,
            col: 0,
            end_line: Some(20),
            scope: None,
            signature: "fn main()".into(),
            language: "Rust".into(),
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_symbol(&sym));
        assert_eq!(out, "src/main.rs:10:  fn main()\n");
    }

    #[test]
    fn symbol_json_format() {
        let sym = SymbolOutput {
            name: "main".into(),
            kind: "function".into(),
            file: "src/main.rs".into(),
            line: 10,
            col: 0,
            end_line: Some(20),
            scope: Some("MyModule".into()),
            signature: "fn main()".into(),
            language: "Rust".into(),
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_symbol(&sym));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["name"], "main");
        assert_eq!(v["kind"], "function");
        assert_eq!(v["file"], "src/main.rs");
        assert_eq!(v["line"], 10);
        assert_eq!(v["end_line"], 20);
        assert_eq!(v["scope"], "MyModule");
    }

    #[test]
    fn symbol_json_skips_none_optional_fields() {
        let sym = SymbolOutput {
            name: "Foo".into(),
            kind: "struct".into(),
            file: "lib.rs".into(),
            line: 5,
            col: 0,
            end_line: None,
            scope: None,
            signature: "struct Foo".into(),
            language: "Rust".into(),
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_symbol(&sym));
        // With skip_serializing_if = None, the JSON should not contain these keys.
        assert!(!out.contains("end_line"));
        assert!(!out.contains("scope"));
    }

    // -- RefOutput -----------------------------------------------------------

    #[test]
    fn reference_grep_format() {
        let reference = RefOutput {
            name: "foo".into(),
            kind: "call".into(),
            file: "src/lib.rs".into(),
            line: 99,
            col: 4,
            context: "    foo(42);".into(),
            caller_name: None,
            confidence: 0.5,
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_reference(&reference));
        assert_eq!(out, "src/lib.rs:99:    foo(42);\n");
    }

    #[test]
    fn reference_json_format() {
        let reference = RefOutput {
            name: "foo".into(),
            kind: "call".into(),
            file: "src/lib.rs".into(),
            line: 99,
            col: 4,
            context: "    foo(42);".into(),
            caller_name: None,
            confidence: 0.85,
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_reference(&reference));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["name"], "foo");
        assert_eq!(v["kind"], "call");
        assert_eq!(v["context"], "    foo(42);");
    }

    // -- SignatureOutput -----------------------------------------------------

    #[test]
    fn signature_grep_format() {
        let sig = SignatureOutput {
            name: "process".into(),
            file: "src/engine.rs".into(),
            line: 15,
            signature: "fn process(input: &str) -> Result<()>".into(),
            language: "Rust".into(),
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_signature(&sig));
        assert_eq!(
            out,
            "src/engine.rs:15:  fn process(input: &str) -> Result<()>\n"
        );
    }

    #[test]
    fn signature_json_format() {
        let sig = SignatureOutput {
            name: "process".into(),
            file: "src/engine.rs".into(),
            line: 15,
            signature: "fn process(input: &str) -> Result<()>".into(),
            language: "Rust".into(),
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_signature(&sig));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["name"], "process");
        assert_eq!(v["signature"], "fn process(input: &str) -> Result<()>");
    }

    // -- FileEntry -----------------------------------------------------------

    #[test]
    fn file_list_grep_format() {
        let entry = FileEntry {
            path: "src/output.rs".into(),
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_file_list(&entry));
        assert_eq!(out, "src/output.rs\n");
    }

    #[test]
    fn file_list_json_format() {
        let entry = FileEntry {
            path: "src/output.rs".into(),
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_file_list(&entry));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["path"], "src/output.rs");
    }

    // -- DepOutput -----------------------------------------------------------

    #[test]
    fn dep_grep_format() {
        let dep = DepOutput {
            file: "src/main.rs".into(),
            depends_on: "src/lib.rs".into(),
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_dep(&dep));
        assert_eq!(out, "src/main.rs -> src/lib.rs\n");
    }

    #[test]
    fn dep_json_format() {
        let dep = DepOutput {
            file: "src/main.rs".into(),
            depends_on: "src/lib.rs".into(),
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_dep(&dep));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["file"], "src/main.rs");
        assert_eq!(v["depends_on"], "src/lib.rs");
    }

    // -- Multiple results produce valid NDJSON / multi-line grep output ------

    #[test]
    fn multiple_search_results_ndjson() {
        let results = vec![
            SearchOutput {
                file: "a.rs".into(),
                line: 1,
                col: 1,
                content: "first".into(),
                annotation: None,
                source: None,
            },
            SearchOutput {
                file: "b.rs".into(),
                line: 2,
                col: 1,
                content: "second".into(),
                annotation: None,
                source: None,
            },
        ];
        let out = render(OutputFormat::Json, |fmt| {
            for r in &results {
                fmt.format_search_result(r)?;
            }
            Ok(())
        });
        let lines: Vec<&str> = out.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        // Each line must be valid JSON.
        for line in &lines {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn multiple_search_results_grep() {
        let results = vec![
            SearchOutput {
                file: "a.rs".into(),
                line: 1,
                col: 1,
                content: "first".into(),
                annotation: None,
                source: None,
            },
            SearchOutput {
                file: "b.rs".into(),
                line: 2,
                col: 1,
                content: "second".into(),
                annotation: None,
                source: None,
            },
        ];
        let out = render(OutputFormat::Grep, |fmt| {
            for r in &results {
                fmt.format_search_result(r)?;
            }
            Ok(())
        });
        let lines: Vec<&str> = out.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "a.rs:1:first");
        assert_eq!(lines[1], "b.rs:2:second");
    }

    // -- SearchOutput::from_search_result helper ----------------------------

    #[test]
    fn from_search_result_helper() {
        let path = std::path::PathBuf::from("src/foo.rs");
        let out = SearchOutput::from_search_result(&path, 10, 3, "let x = 1;");
        assert_eq!(out.file, "src/foo.rs");
        assert_eq!(out.line, 10);
        assert_eq!(out.col, 3);
        assert_eq!(out.content, "let x = 1;");
    }

    // -- Annotation display -------------------------------------------------

    #[test]
    fn search_result_with_annotation_grep_format() {
        let result = SearchOutput {
            file: "src/lib.rs".into(),
            line: 10,
            col: 1,
            content: "pub fn foo() {}".into(),
            annotation: Some("(+3 other locations)".into()),
            source: None,
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_search_result(&result));
        assert_eq!(out, "src/lib.rs:10:pub fn foo() {}  (+3 other locations)\n");
    }

    #[test]
    fn search_result_without_annotation_no_trailing_space() {
        let result = SearchOutput {
            file: "src/lib.rs".into(),
            line: 10,
            col: 1,
            content: "pub fn foo() {}".into(),
            annotation: None,
            source: None,
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_search_result(&result));
        assert_eq!(out, "src/lib.rs:10:pub fn foo() {}\n");
    }

    #[test]
    fn search_result_annotation_in_json() {
        let result = SearchOutput {
            file: "src/lib.rs".into(),
            line: 10,
            col: 1,
            content: "pub fn foo() {}".into(),
            annotation: Some("(+2 other locations)".into()),
            source: None,
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_search_result(&result));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["annotation"], "(+2 other locations)");
    }

    #[test]
    fn search_result_json_skips_annotation_when_none() {
        let result = SearchOutput {
            file: "src/lib.rs".into(),
            line: 10,
            col: 1,
            content: "pub fn foo() {}".into(),
            annotation: None,
            source: None,
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_search_result(&result));
        assert!(!out.contains("annotation"));
    }

    // -- Source field ---------------------------------------------------------

    #[test]
    fn search_result_json_includes_source_when_set() {
        let result = SearchOutput {
            file: "src/lib.rs".into(),
            line: 10,
            col: 1,
            content: "pub fn foo() {}".into(),
            annotation: None,
            source: Some("structural".into()),
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_search_result(&result));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["source"], "structural");
    }

    #[test]
    fn search_result_json_skips_source_when_none() {
        let result = SearchOutput {
            file: "src/lib.rs".into(),
            line: 10,
            col: 1,
            content: "pub fn foo() {}".into(),
            annotation: None,
            source: None,
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_search_result(&result));
        assert!(!out.contains("source"));
    }

    #[test]
    fn from_search_result_sets_source_none() {
        let path = std::path::PathBuf::from("src/foo.rs");
        let out = SearchOutput::from_search_result(&path, 10, 3, "let x = 1;");
        assert!(out.source.is_none());
    }

    // -- Content with special characters ------------------------------------

    #[test]
    fn search_result_with_colon_in_content() {
        let result = SearchOutput {
            file: "cfg.toml".into(),
            line: 5,
            col: 1,
            content: "key: value".into(),
            annotation: None,
            source: None,
        };
        // Grep format: file:line:content (colons in content are fine)
        let out = render(OutputFormat::Grep, |fmt| fmt.format_search_result(&result));
        assert_eq!(out, "cfg.toml:5:key: value\n");
    }

    #[test]
    fn json_escapes_special_characters() {
        let result = SearchOutput {
            file: "test.rs".into(),
            line: 1,
            col: 1,
            content: "he said \"hello\"".into(),
            annotation: None,
            source: None,
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_search_result(&result));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["content"], "he said \"hello\"");
    }

    // -- format_error tests -------------------------------------------------

    #[test]
    fn format_error_returns_exit_code_1_for_general_error() {
        use crate::errors::{DbError, EXIT_ERROR, WonkError};
        let err = WonkError::Db(DbError::NoIndex);
        let code = super::format_error(&err, false);
        assert_eq!(code, EXIT_ERROR);
    }

    #[test]
    fn format_error_returns_exit_code_2_for_usage_error() {
        use crate::errors::{EXIT_USAGE, WonkError};
        let err = WonkError::Usage("bad arg".into());
        let code = super::format_error(&err, false);
        assert_eq!(code, EXIT_USAGE);
    }

    #[test]
    fn format_error_suppresses_hint_in_json_mode() {
        use crate::errors::{DbError, WonkError};
        // We cannot easily capture stderr in a unit test, but we can verify
        // the function runs without panic and returns the right code.
        let err = WonkError::Db(DbError::NoIndex);
        let code = super::format_error(&err, true);
        assert_eq!(code, 1);
    }

    // -- LsSymbolEntry -------------------------------------------------------

    #[test]
    fn ls_symbol_grep_format_flat() {
        let entry = LsSymbolEntry {
            name: "main".into(),
            kind: "function".into(),
            file: "src/main.rs".into(),
            line: 1,
            indent: 0,
            scope: None,
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_ls_symbol(&entry));
        assert_eq!(out, "src/main.rs:1:  function main\n");
    }

    #[test]
    fn ls_symbol_grep_format_indented() {
        let entry = LsSymbolEntry {
            name: "process".into(),
            kind: "method".into(),
            file: "src/lib.rs".into(),
            line: 15,
            indent: 1,
            scope: Some("Worker".into()),
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_ls_symbol(&entry));
        assert_eq!(out, "src/lib.rs:15:    method process\n");
    }

    #[test]
    fn ls_symbol_grep_format_deeply_nested() {
        let entry = LsSymbolEntry {
            name: "inner".into(),
            kind: "function".into(),
            file: "src/lib.rs".into(),
            line: 30,
            indent: 2,
            scope: Some("Outer".into()),
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_ls_symbol(&entry));
        assert_eq!(out, "src/lib.rs:30:      function inner\n");
    }

    #[test]
    fn ls_symbol_json_format_includes_all_fields() {
        let entry = LsSymbolEntry {
            name: "process".into(),
            kind: "method".into(),
            file: "src/lib.rs".into(),
            line: 15,
            indent: 1,
            scope: Some("Worker".into()),
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_ls_symbol(&entry));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["name"], "process");
        assert_eq!(v["kind"], "method");
        assert_eq!(v["file"], "src/lib.rs");
        assert_eq!(v["line"], 15);
        assert_eq!(v["scope"], "Worker");
    }

    #[test]
    fn ls_symbol_json_format_skips_indent() {
        let entry = LsSymbolEntry {
            name: "main".into(),
            kind: "function".into(),
            file: "src/main.rs".into(),
            line: 1,
            indent: 2,
            scope: None,
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_ls_symbol(&entry));
        // indent should NOT appear in JSON
        assert!(!out.contains("indent"));
        // scope should be omitted when None
        assert!(!out.contains("scope"));
    }

    // -- Color output tests -------------------------------------------------

    #[test]
    fn color_false_produces_identical_search_output() {
        // Verify that color=false matches the original non-colored output exactly.
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_search_result(&result));
        assert_eq!(out, "src/main.rs:42:fn main() {}\n");
    }

    #[test]
    fn color_wraps_file_path_in_magenta_bold() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let out = render_color(|fmt| fmt.format_search_result(&result));
        // File path should be wrapped in magenta+bold
        assert!(
            out.contains(&format!(
                "{}src/main.rs{}",
                crate::color::FILE,
                crate::color::RESET
            )),
            "expected magenta+bold file path, got: {out:?}"
        );
    }

    #[test]
    fn color_wraps_line_number_in_green() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let out = render_color(|fmt| fmt.format_search_result(&result));
        // Line number should be wrapped in green
        assert!(
            out.contains(&format!(
                "{}42{}",
                crate::color::LINE_NO,
                crate::color::RESET
            )),
            "expected green line number, got: {out:?}"
        );
    }

    #[test]
    fn color_wraps_separator_in_cyan() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let out = render_color(|fmt| fmt.format_search_result(&result));
        // Separator should be wrapped in cyan
        assert!(
            out.contains(&format!("{}:{}", crate::color::SEP, crate::color::RESET)),
            "expected cyan separator, got: {out:?}"
        );
    }

    #[test]
    fn json_output_has_no_ansi_codes_even_with_color() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Json, true);
            fmt.format_search_result(&result).unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains('\x1b'),
            "JSON output should never contain ANSI escape codes, got: {out:?}"
        );
    }

    #[test]
    fn match_highlighting_wraps_literal_matches() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, true);
            fmt.set_highlight("main", false, false);
            fmt.format_search_result(&result).unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        let expected_match = format!("{}main{}", crate::color::MATCH, crate::color::RESET);
        assert!(
            out.contains(&expected_match),
            "expected highlighted match, got: {out:?}"
        );
    }

    #[test]
    fn match_highlighting_works_for_regex_patterns() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, true);
            fmt.set_highlight("ma.n", true, false);
            fmt.format_search_result(&result).unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        let expected_match = format!("{}main{}", crate::color::MATCH, crate::color::RESET);
        assert!(
            out.contains(&expected_match),
            "expected highlighted regex match, got: {out:?}"
        );
    }

    #[test]
    fn no_highlighting_when_pattern_does_not_match() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, true);
            fmt.set_highlight("zzzzz", false, false);
            fmt.format_search_result(&result).unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        // Content should appear without MATCH codes (but file/line still get color)
        assert!(
            !out.contains(crate::color::MATCH),
            "should not have match highlighting when pattern doesn't match, got: {out:?}"
        );
    }

    #[test]
    fn color_symbol_format() {
        let sym = SymbolOutput {
            name: "main".into(),
            kind: "function".into(),
            file: "src/main.rs".into(),
            line: 10,
            col: 0,
            end_line: Some(20),
            scope: None,
            signature: "fn main()".into(),
            language: "Rust".into(),
        };
        let out = render_color(|fmt| fmt.format_symbol(&sym));
        assert!(out.contains(&format!(
            "{}src/main.rs{}",
            crate::color::FILE,
            crate::color::RESET
        )));
        assert!(out.contains(&format!(
            "{}10{}",
            crate::color::LINE_NO,
            crate::color::RESET
        )));
    }

    #[test]
    fn color_file_list_format() {
        let entry = FileEntry {
            path: "src/output.rs".into(),
        };
        let out = render_color(|fmt| fmt.format_file_list(&entry));
        assert!(out.contains(&format!(
            "{}src/output.rs{}",
            crate::color::FILE,
            crate::color::RESET
        )));
    }

    #[test]
    fn color_dep_format() {
        let dep = DepOutput {
            file: "src/main.rs".into(),
            depends_on: "src/lib.rs".into(),
        };
        let out = render_color(|fmt| fmt.format_dep(&dep));
        assert!(out.contains(&format!(
            "{}src/main.rs{}",
            crate::color::FILE,
            crate::color::RESET
        )));
        assert!(out.contains(&format!(
            "{}src/lib.rs{}",
            crate::color::FILE,
            crate::color::RESET
        )));
    }

    // -- Budget output helpers -----------------------------------------------

    #[test]
    fn truncation_meta_json_format() {
        let meta = TruncationMeta {
            truncated_count: 42,
            budget_tokens: 500,
            used_tokens: 498,
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_truncation_meta(&meta));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["truncated_count"], 42);
        assert_eq!(v["budget_tokens"], 500);
        assert_eq!(v["used_tokens"], 498);
    }

    #[test]
    fn truncation_meta_serializes_all_fields() {
        let meta = TruncationMeta {
            truncated_count: 10,
            budget_tokens: 100,
            used_tokens: 95,
        };
        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"truncated_count\":10"));
        assert!(json.contains("\"budget_tokens\":100"));
        assert!(json.contains("\"used_tokens\":95"));
    }

    #[test]
    fn budget_truncates_search_results_grep_mode() {
        // Create results that together exceed the budget.
        let results: Vec<SearchOutput> = (0..10)
            .map(|i| SearchOutput {
                file: "src/main.rs".into(),
                line: i + 1,
                col: 1,
                content: "fn some_function_here() {}".into(),
                annotation: None,
                source: None,
            })
            .collect();

        let mut buf = Vec::new();
        let mut emitted = 0usize;
        let mut truncated = 0usize;
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, false);
            // Set a small budget: each line is ~40 chars -> ~10 tokens.
            // Budget of 25 tokens should allow ~2-3 results.
            fmt.set_budget(25);
            for r in &results {
                match fmt.format_search_result(r) {
                    Ok(BudgetStatus::Written) => emitted += 1,
                    Ok(BudgetStatus::Skipped) => truncated += 1,
                    Err(e) => panic!("unexpected error: {e}"),
                }
            }
        }
        let out = String::from_utf8(buf).unwrap();
        // Should have emitted some but not all.
        assert!(emitted > 0, "should emit at least one result");
        assert!(emitted < 10, "should not emit all results");
        assert_eq!(emitted + truncated, 10);
        // Output should contain only the emitted lines.
        let lines: Vec<&str> = out.trim().split('\n').collect();
        assert_eq!(lines.len(), emitted);
    }

    #[test]
    fn budget_truncates_search_results_json_mode() {
        let results: Vec<SearchOutput> = (0..10)
            .map(|i| SearchOutput {
                file: "src/main.rs".into(),
                line: i + 1,
                col: 1,
                content: "fn some_function_here() {}".into(),
                annotation: None,
                source: None,
            })
            .collect();

        let mut buf = Vec::new();
        let mut emitted = 0usize;
        let mut truncated = 0usize;
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Json, false);
            fmt.set_budget(25);
            for r in &results {
                match fmt.format_search_result(r) {
                    Ok(BudgetStatus::Written) => emitted += 1,
                    Ok(BudgetStatus::Skipped) => truncated += 1,
                    Err(e) => panic!("unexpected error: {e}"),
                }
            }
            // Emit truncation meta.
            if truncated > 0 {
                let meta = TruncationMeta {
                    truncated_count: truncated,
                    budget_tokens: 25,
                    used_tokens: fmt.budget_used(),
                };
                fmt.format_truncation_meta(&meta).unwrap();
            }
        }
        let out = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = out.trim().split('\n').collect();
        // Last line should be the truncation meta.
        assert!(emitted > 0);
        assert!(truncated > 0);
        let last_line = lines.last().unwrap();
        let meta: serde_json::Value = serde_json::from_str(last_line).unwrap();
        assert_eq!(meta["truncated_count"], truncated);
    }

    #[test]
    fn no_budget_means_all_results_written() {
        let results: Vec<SearchOutput> = (0..5)
            .map(|i| SearchOutput {
                file: "src/main.rs".into(),
                line: i + 1,
                col: 1,
                content: "fn main() {}".into(),
                annotation: None,
                source: None,
            })
            .collect();

        let mut buf = Vec::new();
        let mut emitted = 0usize;
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, false);
            // No set_budget call - no budget.
            for r in &results {
                match fmt.format_search_result(r) {
                    Ok(BudgetStatus::Written) => emitted += 1,
                    Ok(BudgetStatus::Skipped) => panic!("should not skip without budget"),
                    Err(e) => panic!("unexpected error: {e}"),
                }
            }
        }
        assert_eq!(emitted, 5);
    }

    #[test]
    fn budget_tracks_used_tokens() {
        let mut buf = Vec::new();
        let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, false);
        fmt.set_budget(1000);
        assert_eq!(fmt.budget_used(), 0);

        let r = SearchOutput {
            file: "src/main.rs".into(),
            line: 1,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        fmt.format_search_result(&r).unwrap();
        assert!(fmt.budget_used() > 0);
    }

    #[test]
    fn budget_applies_to_symbol_output() {
        let syms: Vec<SymbolOutput> = (0..10)
            .map(|i| SymbolOutput {
                name: "some_really_long_function_name".into(),
                kind: "function".into(),
                file: "src/very/deep/nested/module.rs".into(),
                line: i + 1,
                col: 0,
                end_line: None,
                scope: None,
                signature:
                    "fn some_really_long_function_name(arg1: Type1, arg2: Type2) -> ReturnType"
                        .into(),
                language: "Rust".into(),
            })
            .collect();

        let mut buf = Vec::new();
        let mut emitted = 0usize;
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, false);
            fmt.set_budget(30);
            for s in &syms {
                match fmt.format_symbol(s) {
                    Ok(BudgetStatus::Written) => emitted += 1,
                    Ok(BudgetStatus::Skipped) => {}
                    Err(e) => panic!("unexpected error: {e}"),
                }
            }
        }
        assert!(emitted > 0);
        assert!(emitted < 10);
    }

    #[test]
    fn match_highlighting_case_insensitive() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 1,
            col: 1,
            content: "Hello WORLD hello".into(),
            annotation: None,
            source: None,
        };
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, true);
            fmt.set_highlight("hello", false, true);
            fmt.format_search_result(&result).unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        // Both "Hello" and "hello" should be highlighted
        let hl_hello = format!("{}Hello{}", crate::color::MATCH, crate::color::RESET);
        let hl_hello2 = format!("{}hello{}", crate::color::MATCH, crate::color::RESET);
        assert!(
            out.contains(&hl_hello) && out.contains(&hl_hello2),
            "expected both case variants highlighted, got: {out:?}"
        );
    }

    // -- Mode indicator -------------------------------------------------------

    #[test]
    fn mode_indicator_smart_with_symbols() {
        assert_eq!(
            format_mode_indicator(5, false),
            Some("(smart: 5 symbols matched)".to_string()),
        );
    }

    #[test]
    fn mode_indicator_text_search() {
        assert_eq!(
            format_mode_indicator(0, false),
            Some("(text search)".to_string()),
        );
    }

    #[test]
    fn mode_indicator_suppressed() {
        assert_eq!(format_mode_indicator(5, true), None);
        assert_eq!(format_mode_indicator(0, true), None);
    }

    // -- TOON output tests ---------------------------------------------------

    #[test]
    fn search_result_toon_format() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let out = render(OutputFormat::Toon, |fmt| fmt.format_search_result(&result));
        assert!(!out.is_empty());
        // Should be parseable by serde_toon2.
        let parsed: SearchOutput = serde_toon2::from_str(out.trim()).unwrap();
        assert_eq!(parsed.file, "src/main.rs");
        assert_eq!(parsed.line, 42);
    }

    #[test]
    fn symbol_toon_format() {
        let sym = SymbolOutput {
            name: "main".into(),
            kind: "function".into(),
            file: "src/main.rs".into(),
            line: 10,
            col: 0,
            end_line: None,
            scope: None,
            signature: "fn main()".into(),
            language: "Rust".into(),
        };
        let out = render(OutputFormat::Toon, |fmt| fmt.format_symbol(&sym));
        let parsed: SymbolOutput = serde_toon2::from_str(out.trim()).unwrap();
        assert_eq!(parsed.name, "main");
        assert_eq!(parsed.kind, "function");
    }

    #[test]
    fn reference_toon_format() {
        let reference = RefOutput {
            name: "foo".into(),
            kind: "call".into(),
            file: "src/lib.rs".into(),
            line: 99,
            col: 4,
            context: "    foo(42);".into(),
            caller_name: None,
            confidence: 0.5,
        };
        let out = render(OutputFormat::Toon, |fmt| fmt.format_reference(&reference));
        let parsed: RefOutput = serde_toon2::from_str(out.trim()).unwrap();
        assert_eq!(parsed.name, "foo");
    }

    #[test]
    fn signature_toon_format() {
        let sig = SignatureOutput {
            name: "process".into(),
            file: "src/engine.rs".into(),
            line: 15,
            signature: "fn process(input: &str) -> Result<()>".into(),
            language: "Rust".into(),
        };
        let out = render(OutputFormat::Toon, |fmt| fmt.format_signature(&sig));
        let parsed: SignatureOutput = serde_toon2::from_str(out.trim()).unwrap();
        assert_eq!(parsed.name, "process");
    }

    #[test]
    fn file_list_toon_format() {
        let entry = FileEntry {
            path: "src/output.rs".into(),
        };
        let out = render(OutputFormat::Toon, |fmt| fmt.format_file_list(&entry));
        let parsed: FileEntry = serde_toon2::from_str(out.trim()).unwrap();
        assert_eq!(parsed.path, "src/output.rs");
    }

    #[test]
    fn dep_toon_format() {
        let dep = DepOutput {
            file: "src/main.rs".into(),
            depends_on: "src/lib.rs".into(),
        };
        let out = render(OutputFormat::Toon, |fmt| fmt.format_dep(&dep));
        let parsed: DepOutput = serde_toon2::from_str(out.trim()).unwrap();
        assert_eq!(parsed.file, "src/main.rs");
        assert_eq!(parsed.depends_on, "src/lib.rs");
    }

    #[test]
    fn toon_output_has_no_ansi_codes() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
            annotation: None,
            source: None,
        };
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Toon, true);
            fmt.format_search_result(&result).unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        assert!(
            !out.contains('\x1b'),
            "TOON output should never contain ANSI escape codes, got: {out:?}"
        );
    }

    // -- SemanticOutput -------------------------------------------------------

    #[test]
    fn semantic_result_grep_format() {
        let result = SemanticOutput {
            file: "src/auth.rs".into(),
            line: 42,
            symbol_name: "authenticate".into(),
            symbol_kind: "function".into(),
            similarity_score: 0.8765,
            symbol_id: 1,
        };
        let out = render(OutputFormat::Grep, |fmt| {
            fmt.format_semantic_result(&result)
        });
        assert_eq!(out, "src/auth.rs:42:  authenticate (function) [0.8765]\n");
    }

    #[test]
    fn semantic_result_json_format() {
        let result = SemanticOutput {
            file: "src/auth.rs".into(),
            line: 42,
            symbol_name: "authenticate".into(),
            symbol_kind: "function".into(),
            similarity_score: 0.8765,
            symbol_id: 1,
        };
        let out = render(OutputFormat::Json, |fmt| {
            fmt.format_semantic_result(&result)
        });
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["file"], "src/auth.rs");
        assert_eq!(v["line"], 42);
        assert_eq!(v["symbol_name"], "authenticate");
        assert_eq!(v["symbol_kind"], "function");
        assert!((v["similarity_score"].as_f64().unwrap() - 0.8765).abs() < 0.001);
        assert!(
            v.get("symbol_id").is_none(),
            "symbol_id should not appear in JSON output"
        );
    }

    #[test]
    fn semantic_result_budget_truncation() {
        let results: Vec<SemanticOutput> = (0..10)
            .map(|i| SemanticOutput {
                file: "src/some_module.rs".into(),
                line: i + 1,
                symbol_name: "some_long_function_name".into(),
                symbol_kind: "function".into(),
                similarity_score: 0.9 - (i as f32) * 0.05,
                symbol_id: i as i64,
            })
            .collect();

        let mut buf = Vec::new();
        let mut emitted = 0usize;
        let mut truncated = 0usize;
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, false);
            fmt.set_budget(25);
            for r in &results {
                match fmt.format_semantic_result(r) {
                    Ok(BudgetStatus::Written) => emitted += 1,
                    Ok(BudgetStatus::Skipped) => truncated += 1,
                    Err(e) => panic!("unexpected error: {e}"),
                }
            }
        }
        assert!(emitted > 0, "should emit at least one result");
        assert!(emitted < 10, "should not emit all results");
        assert_eq!(emitted + truncated, 10);
    }

    // -- ClusterOutput -------------------------------------------------------

    #[test]
    fn cluster_member_grep_format() {
        let member = ClusterMemberOutput {
            file: "src/auth/middleware.ts".into(),
            line: 15,
            symbol_name: "verifyToken".into(),
            symbol_kind: "function".into(),
            distance_to_centroid: 0.12,
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_cluster_member(&member));
        assert_eq!(
            out,
            "src/auth/middleware.ts:15:  verifyToken (function) [0.1200]\n"
        );
    }

    #[test]
    fn cluster_member_json_format() {
        let member = ClusterMemberOutput {
            file: "src/auth/middleware.ts".into(),
            line: 15,
            symbol_name: "verifyToken".into(),
            symbol_kind: "function".into(),
            distance_to_centroid: 0.12,
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_cluster_member(&member));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["file"], "src/auth/middleware.ts");
        assert_eq!(v["line"], 15);
        assert_eq!(v["symbol_name"], "verifyToken");
        assert_eq!(v["symbol_kind"], "function");
    }

    #[test]
    fn cluster_output_json_format() {
        let cluster = ClusterOutput {
            cluster_id: 0,
            total_members: 15,
            representatives: vec![ClusterMemberOutput {
                file: "src/auth/middleware.ts".into(),
                line: 15,
                symbol_name: "verifyToken".into(),
                symbol_kind: "function".into(),
                distance_to_centroid: 0.12,
            }],
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_cluster(&cluster));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["cluster_id"], 0);
        assert_eq!(v["total_members"], 15);
        assert!(v["representatives"].is_array());
        assert_eq!(v["representatives"][0]["symbol_name"], "verifyToken");
    }

    #[test]
    fn cluster_member_budget_truncation() {
        let members: Vec<ClusterMemberOutput> = (0..10)
            .map(|i| ClusterMemberOutput {
                file: "src/some_module.rs".into(),
                line: i + 1,
                symbol_name: "some_long_function_name".into(),
                symbol_kind: "function".into(),
                distance_to_centroid: 0.1 + (i as f32) * 0.05,
            })
            .collect();

        let mut buf = Vec::new();
        let mut emitted = 0usize;
        let mut truncated = 0usize;
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, false);
            fmt.set_budget(25);
            for m in &members {
                match fmt.format_cluster_member(m) {
                    Ok(BudgetStatus::Written) => emitted += 1,
                    Ok(BudgetStatus::Skipped) => truncated += 1,
                    Err(e) => panic!("unexpected error: {e}"),
                }
            }
        }
        assert!(emitted > 0, "should emit at least one result");
        assert!(emitted < 10, "should not emit all results");
        assert_eq!(emitted + truncated, 10);
    }

    // -- ImpactOutput -------------------------------------------------------

    #[test]
    fn impact_entry_grep_format() {
        let entry = ImpactEntryOutput {
            file: "src/auth/session.ts".into(),
            line: 8,
            symbol_name: "validateSession".into(),
            symbol_kind: "function".into(),
            similarity_score: 0.89,
        };
        let out = render(OutputFormat::Grep, |fmt| fmt.format_impact_entry(&entry));
        assert!(out.contains("src/auth/session.ts"));
        assert!(out.contains(":8:"));
        assert!(out.contains("validateSession"));
        assert!(out.contains("(function)"));
        assert!(out.contains("[0.8900]"));
    }

    #[test]
    fn impact_entry_json_format() {
        let entry = ImpactEntryOutput {
            file: "src/auth/session.ts".into(),
            line: 8,
            symbol_name: "validateSession".into(),
            symbol_kind: "function".into(),
            similarity_score: 0.89,
        };
        let out = render(OutputFormat::Json, |fmt| fmt.format_impact_entry(&entry));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["file"], "src/auth/session.ts");
        assert_eq!(v["line"], 8);
        assert_eq!(v["symbol_name"], "validateSession");
        assert_eq!(v["symbol_kind"], "function");
        assert!((v["similarity_score"].as_f64().unwrap() - 0.89).abs() < 0.01);
    }

    #[test]
    fn impact_full_json_output() {
        let output = ImpactOutput {
            changed_symbol: ImpactSymbolOutput {
                name: "verifyToken".into(),
                kind: "function".into(),
                file: "src/auth/middleware.ts".into(),
                line: 15,
            },
            impacted: vec![ImpactEntryOutput {
                file: "src/auth/session.ts".into(),
                line: 8,
                symbol_name: "validateSession".into(),
                symbol_kind: "function".into(),
                similarity_score: 0.89,
            }],
        };
        let json = serde_json::to_string(&output).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["changed_symbol"]["name"], "verifyToken");
        assert_eq!(v["impacted"][0]["symbol_name"], "validateSession");
    }

    // -- ShowOutput ----------------------------------------------------------

    #[test]
    fn show_grep_format_numbered_lines() {
        let out = ShowOutput {
            name: "processPayment".into(),
            kind: "function".into(),
            file: "src/billing.ts".into(),
            line: 10,
            end_line: Some(12),
            source: "function processPayment() {\n  return true;\n}".into(),
            language: "TypeScript".into(),
        };
        let rendered = render(OutputFormat::Grep, |fmt| fmt.format_show(&out));
        assert!(rendered.contains("  10| function processPayment()"));
        assert!(rendered.contains("  11|   return true;"));
        assert!(rendered.contains("  12| }"));
    }

    #[test]
    fn show_grep_colorized_line_numbers() {
        let out = ShowOutput {
            name: "foo".into(),
            kind: "function".into(),
            file: "src/lib.rs".into(),
            line: 5,
            end_line: Some(5),
            source: "fn foo() {}".into(),
            language: "Rust".into(),
        };
        let rendered = render_color(|fmt| fmt.format_show(&out));
        assert!(
            rendered.contains(crate::color::LINE_NO),
            "expected colorized line number in show output, got: {rendered:?}"
        );
    }

    #[test]
    fn show_json_format() {
        let out = ShowOutput {
            name: "foo".into(),
            kind: "function".into(),
            file: "src/lib.rs".into(),
            line: 5,
            end_line: Some(8),
            source: "fn foo() {\n  42\n}".into(),
            language: "Rust".into(),
        };
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_show(&out));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert_eq!(v["name"], "foo");
        assert_eq!(v["kind"], "function");
        assert_eq!(v["file"], "src/lib.rs");
        assert_eq!(v["line"], 5);
        assert_eq!(v["end_line"], 8);
        assert!(v["source"].as_str().unwrap().contains("fn foo()"));
        assert_eq!(v["language"], "Rust");
    }

    #[test]
    fn show_json_skips_end_line_when_none() {
        let out = ShowOutput {
            name: "MAX".into(),
            kind: "constant".into(),
            file: "src/config.rs".into(),
            line: 3,
            end_line: None,
            source: "const MAX: usize = 1024;".into(),
            language: "Rust".into(),
        };
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_show(&out));
        assert!(!rendered.contains("end_line"));
    }

    #[test]
    fn show_grep_single_line() {
        let out = ShowOutput {
            name: "MAX".into(),
            kind: "constant".into(),
            file: "src/config.rs".into(),
            line: 3,
            end_line: None,
            source: "const MAX: usize = 1024;".into(),
            language: "Rust".into(),
        };
        let rendered = render(OutputFormat::Grep, |fmt| fmt.format_show(&out));
        assert_eq!(rendered, "   3| const MAX: usize = 1024;\n");
    }

    // -- CallerOutput --------------------------------------------------------

    #[test]
    fn caller_grep_format() {
        let out = CallerOutput {
            caller_name: "dispatch".into(),
            caller_kind: "function".into(),
            file: "src/router.rs".into(),
            line: 50,
            signature: "fn dispatch()".into(),
            depth: 1,
            target_file: None,
            confidence: 0.5,
        };
        let rendered = render(OutputFormat::Grep, |fmt| fmt.format_caller(&out));
        assert!(rendered.contains("src/router.rs"));
        assert!(rendered.contains("50"));
        assert!(rendered.contains("fn dispatch()"));
    }

    #[test]
    fn caller_json_format() {
        let out = CallerOutput {
            caller_name: "dispatch".into(),
            caller_kind: "function".into(),
            file: "src/router.rs".into(),
            line: 50,
            signature: "fn dispatch()".into(),
            depth: 1,
            target_file: Some("src/db.rs".into()),
            confidence: 0.5,
        };
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_caller(&out));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert_eq!(v["caller_name"], "dispatch");
        assert_eq!(v["file"], "src/router.rs");
        assert_eq!(v["line"], 50);
        assert_eq!(v["depth"], 1);
        assert_eq!(v["target_file"], "src/db.rs");
    }

    #[test]
    fn caller_json_omits_null_target_file() {
        let out = CallerOutput {
            caller_name: "foo".into(),
            caller_kind: "function".into(),
            file: "a.rs".into(),
            line: 1,
            signature: "fn foo()".into(),
            depth: 1,
            target_file: None,
            confidence: 0.5,
        };
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_caller(&out));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert!(v.get("target_file").is_none());
    }

    #[test]
    fn caller_json_includes_confidence() {
        let out = CallerOutput {
            caller_name: "dispatch".into(),
            caller_kind: "function".into(),
            file: "src/router.rs".into(),
            line: 50,
            signature: "fn dispatch()".into(),
            depth: 1,
            target_file: None,
            confidence: 0.85,
        };
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_caller(&out));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert_eq!(v["confidence"], 0.85);
    }

    // -- CalleeOutput --------------------------------------------------------

    #[test]
    fn callee_grep_format() {
        let out = CalleeOutput {
            callee_name: "open_db".into(),
            file: "src/db.rs".into(),
            line: 10,
            context: "    let conn = open_db(&path);".into(),
            depth: 1,
            source_file: None,
            confidence: 0.5,
        };
        let rendered = render(OutputFormat::Grep, |fmt| fmt.format_callee(&out));
        assert!(rendered.contains("src/db.rs"));
        assert!(rendered.contains("10"));
        assert!(rendered.contains("open_db"));
    }

    #[test]
    fn callee_json_format() {
        let out = CalleeOutput {
            callee_name: "open_db".into(),
            file: "src/db.rs".into(),
            line: 10,
            context: "    let conn = open_db(&path);".into(),
            depth: 1,
            source_file: Some("src/router.rs".into()),
            confidence: 0.5,
        };
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_callee(&out));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert_eq!(v["callee_name"], "open_db");
        assert_eq!(v["file"], "src/db.rs");
        assert_eq!(v["line"], 10);
        assert_eq!(v["depth"], 1);
        assert_eq!(v["source_file"], "src/router.rs");
    }

    #[test]
    fn callee_json_omits_null_source_file() {
        let out = CalleeOutput {
            callee_name: "bar".into(),
            file: "b.rs".into(),
            line: 5,
            context: "bar()".into(),
            depth: 1,
            source_file: None,
            confidence: 0.5,
        };
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_callee(&out));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert!(v.get("source_file").is_none());
    }

    #[test]
    fn callee_json_includes_confidence() {
        let out = CalleeOutput {
            callee_name: "open_db".into(),
            file: "src/db.rs".into(),
            line: 10,
            context: "let conn = open_db(&path);".into(),
            depth: 1,
            source_file: None,
            confidence: 0.95,
        };
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_callee(&out));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert_eq!(v["confidence"], 0.95);
    }

    // -- CallPathHopOutput ---------------------------------------------------

    #[test]
    fn callpath_grep_format() {
        let hops = vec![
            CallPathHopOutput {
                symbol_name: "main".into(),
                symbol_kind: "function".into(),
                file: "src/main.rs".into(),
                line: 1,
            },
            CallPathHopOutput {
                symbol_name: "dispatch".into(),
                symbol_kind: "function".into(),
                file: "src/router.rs".into(),
                line: 50,
            },
            CallPathHopOutput {
                symbol_name: "open_db".into(),
                symbol_kind: "function".into(),
                file: "src/db.rs".into(),
                line: 10,
            },
        ];
        let rendered = render(OutputFormat::Grep, |fmt| fmt.format_callpath(&hops));
        // First line should be the chain.
        assert!(rendered.contains("main -> dispatch -> open_db"));
        // Should include file:line details for each hop.
        assert!(rendered.contains("src/main.rs:1"));
        assert!(rendered.contains("src/router.rs:50"));
        assert!(rendered.contains("src/db.rs:10"));
    }

    #[test]
    fn callpath_json_format() {
        let hops = vec![
            CallPathHopOutput {
                symbol_name: "a".into(),
                symbol_kind: "function".into(),
                file: "a.rs".into(),
                line: 1,
            },
            CallPathHopOutput {
                symbol_name: "b".into(),
                symbol_kind: "function".into(),
                file: "b.rs".into(),
                line: 5,
            },
        ];
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_callpath(&hops));
        // Each line should be a valid JSON object.
        let lines: Vec<&str> = rendered.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        let v0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v0["symbol_name"], "a");
        assert_eq!(v0["file"], "a.rs");
        assert_eq!(v0["line"], 1);
        let v1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v1["symbol_name"], "b");
        assert_eq!(v1["file"], "b.rs");
        assert_eq!(v1["line"], 5);
    }

    // -- SummaryOutput --------------------------------------------------------

    fn make_summary_output() -> SummaryOutput {
        SummaryOutput {
            path: "src/".into(),
            path_type: "directory".into(),
            detail_level: "rich".into(),
            metrics: SummaryMetricsOutput {
                file_count: Some(10),
                line_count: Some(500),
                symbol_counts: Some(vec![
                    SymbolCountEntry {
                        kind: "function".into(),
                        count: 20,
                    },
                    SymbolCountEntry {
                        kind: "class".into(),
                        count: 5,
                    },
                ]),
                languages: Some(vec![LanguageEntry {
                    language: "Rust".into(),
                    count: 10,
                }]),
                dependency_count: Some(15),
            },
            children: vec![],
            description: None,
        }
    }

    #[test]
    fn summary_grep_format_shows_metrics() {
        let out = make_summary_output();
        let rendered = render(OutputFormat::Grep, |fmt| fmt.format_summary(&out));
        assert!(rendered.contains("Summary: src/ (directory)"));
        assert!(rendered.contains("Files: 10"));
        assert!(rendered.contains("Lines: 500"));
        assert!(rendered.contains("function: 20"));
        assert!(rendered.contains("class: 5"));
        assert!(rendered.contains("Rust: 10"));
        assert!(rendered.contains("Dependencies: 15"));
    }

    #[test]
    fn summary_json_format() {
        let out = make_summary_output();
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_summary(&out));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert_eq!(v["path"], "src/");
        assert_eq!(v["type"], "directory");
        assert_eq!(v["detail_level"], "rich");
        assert_eq!(v["metrics"]["file_count"], 10);
        assert_eq!(v["metrics"]["line_count"], 500);
        assert_eq!(v["metrics"]["dependency_count"], 15);
        assert!(v["metrics"]["symbol_counts"].is_array());
        assert!(v["metrics"]["languages"].is_array());
    }

    #[test]
    fn summary_json_omits_empty_children() {
        let out = make_summary_output();
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_summary(&out));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert!(v.get("children").is_none());
    }

    #[test]
    fn summary_json_omits_null_description() {
        let out = make_summary_output();
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_summary(&out));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert!(v.get("description").is_none());
    }

    #[test]
    fn summary_output_from_result_rich() {
        use crate::types::{DetailLevel, SummaryMetrics, SummaryPathType, SummaryResult};

        let sr = SummaryResult {
            path: "src/".into(),
            path_type: SummaryPathType::Directory,
            detail_level: DetailLevel::Rich,
            metrics: SummaryMetrics {
                file_count: 5,
                line_count: 200,
                symbol_counts: vec![("function".into(), 10)],
                language_breakdown: vec![("Rust".into(), 5)],
                dependency_count: 3,
            },
            children: vec![],
            description: None,
        };

        let out = SummaryOutput::from_result(&sr);
        assert_eq!(out.path, "src/");
        assert_eq!(out.metrics.file_count, Some(5));
        assert_eq!(out.metrics.line_count, Some(200));
        assert_eq!(out.metrics.dependency_count, Some(3));
        assert!(out.metrics.symbol_counts.is_some());
        assert!(out.metrics.languages.is_some());
    }

    #[test]
    fn summary_output_from_result_light() {
        use crate::types::{DetailLevel, SummaryMetrics, SummaryPathType, SummaryResult};

        let sr = SummaryResult {
            path: "src/".into(),
            path_type: SummaryPathType::Directory,
            detail_level: DetailLevel::Light,
            metrics: SummaryMetrics {
                file_count: 5,
                line_count: 200,
                symbol_counts: vec![("function".into(), 10)],
                language_breakdown: vec![("Rust".into(), 5)],
                dependency_count: 3,
            },
            children: vec![],
            description: None,
        };

        let out = SummaryOutput::from_result(&sr);
        assert_eq!(out.metrics.file_count, Some(5));
        assert!(out.metrics.line_count.is_none());
        assert!(out.metrics.dependency_count.is_none());
        assert!(out.metrics.symbol_counts.is_some());
        assert!(out.metrics.languages.is_some());
    }

    #[test]
    fn summary_output_from_result_symbols() {
        use crate::types::{DetailLevel, SummaryMetrics, SummaryPathType, SummaryResult};

        let sr = SummaryResult {
            path: "src/".into(),
            path_type: SummaryPathType::Directory,
            detail_level: DetailLevel::Symbols,
            metrics: SummaryMetrics {
                file_count: 5,
                line_count: 200,
                symbol_counts: vec![("function".into(), 10)],
                language_breakdown: vec![("Rust".into(), 5)],
                dependency_count: 3,
            },
            children: vec![],
            description: None,
        };

        let out = SummaryOutput::from_result(&sr);
        assert!(out.metrics.file_count.is_none());
        assert!(out.metrics.line_count.is_none());
        assert!(out.metrics.dependency_count.is_none());
        assert!(out.metrics.symbol_counts.is_some());
        assert!(out.metrics.languages.is_none());
    }

    #[test]
    fn summary_grep_with_children() {
        let child = SummaryOutput {
            path: "src/lib.rs".into(),
            path_type: "file".into(),
            detail_level: "rich".into(),
            metrics: SummaryMetricsOutput {
                file_count: Some(1),
                line_count: Some(100),
                symbol_counts: Some(vec![]),
                languages: Some(vec![]),
                dependency_count: Some(0),
            },
            children: vec![],
            description: None,
        };
        let mut out = make_summary_output();
        out.children = vec![child];
        let rendered = render(OutputFormat::Grep, |fmt| fmt.format_summary(&out));
        // Child should be indented.
        assert!(rendered.contains("  Summary: src/lib.rs (file)"));
    }

    // -- FlowStepOutput / FlowOutput -----------------------------------------

    fn make_flow_step(name: &str, depth: usize) -> FlowStepOutput {
        FlowStepOutput {
            name: name.into(),
            kind: "function".into(),
            file: format!("src/{name}.rs"),
            line: depth * 10 + 1,
            depth,
        }
    }

    #[test]
    fn flow_entry_grep_format() {
        let step = make_flow_step("main", 0);
        let rendered = render(OutputFormat::Grep, |fmt| fmt.format_flow_entry(&step));
        assert!(rendered.contains("src/main.rs"));
        assert!(rendered.contains("main"));
    }

    #[test]
    fn flow_entry_json_format() {
        let step = make_flow_step("main", 0);
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_flow_entry(&step));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert_eq!(v["name"], "main");
        assert_eq!(v["kind"], "function");
        assert_eq!(v["file"], "src/main.rs");
        assert_eq!(v["depth"], 0);
    }

    #[test]
    fn flow_grep_format() {
        let entry = make_flow_step("main", 0);
        let flow = FlowOutput {
            entry_point: entry.clone(),
            steps: vec![entry, make_flow_step("dispatch", 1)],
            step_count: 2,
        };
        let rendered = render(OutputFormat::Grep, |fmt| fmt.format_flow(&flow));
        // Should show entry point and steps.
        assert!(rendered.contains("main"));
        assert!(rendered.contains("dispatch"));
    }

    #[test]
    fn flow_json_format() {
        let entry = make_flow_step("main", 0);
        let flow = FlowOutput {
            entry_point: entry.clone(),
            steps: vec![entry, make_flow_step("dispatch", 1)],
            step_count: 2,
        };
        let rendered = render(OutputFormat::Json, |fmt| fmt.format_flow(&flow));
        let v: serde_json::Value = serde_json::from_str(rendered.trim()).unwrap();
        assert_eq!(v["entry_point"]["name"], "main");
        assert_eq!(v["step_count"], 2);
        assert_eq!(v["steps"].as_array().unwrap().len(), 2);
    }
}
