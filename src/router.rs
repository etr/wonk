//! Query routing layer.
//!
//! [`dispatch`] handles CLI command dispatch.  [`QueryRouter`] provides the
//! core query interface: it tries the SQLite index first and, when the index
//! is unavailable or returns no results, falls back to grep-based heuristic
//! search patterns that cover all 10 supported languages.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::Connection;

use crate::cli::{Cli, Command, DaemonCommand, LsArgs, ReposCommand};
use crate::db;
use crate::errors::DbError;
#[cfg(test)]
use crate::errors::SearchError;
use crate::output::{
    self, BudgetStatus, Formatter, LsSymbolEntry, RefOutput, SearchOutput, SignatureOutput,
    SymbolOutput,
};
use crate::pipeline;
use crate::progress::{self, Progress};
use crate::search;
use crate::types::{Reference, ReferenceKind, Symbol, SymbolKind};

// ---------------------------------------------------------------------------
// Search mode detection
// ---------------------------------------------------------------------------

/// Search mode for `wonk search`, determined by flags and symbol detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchMode {
    /// Unranked grep output (`--raw` or no symbol match).
    Plain,
    /// Ranked output with structural metadata (symbol_count).
    Smart(u64),
}

/// Determine search mode based on flags and symbol count.
pub fn detect_search_mode(raw: bool, smart: bool, symbol_count: u64) -> SearchMode {
    if raw {
        SearchMode::Plain
    } else if smart || symbol_count > 0 {
        SearchMode::Smart(symbol_count)
    } else {
        SearchMode::Plain
    }
}

// ---------------------------------------------------------------------------
// CLI dispatch (kept from original router)
// ---------------------------------------------------------------------------

pub fn dispatch(cli: Cli) -> Result<()> {
    let json = cli.json;
    let quiet = cli.quiet;
    let suppress = json || quiet;
    let stdout = io::stdout().lock();

    // Resolve color: load config and check env/TTY.
    let color = if json {
        false
    } else {
        let repo_root_for_config = std::env::current_dir()
            .ok()
            .and_then(|cwd| db::find_repo_root(&cwd).ok());
        let config =
            crate::config::Config::load(repo_root_for_config.as_deref()).unwrap_or_default();
        crate::color::resolve_color(&config.output.color)
    };

    let mut fmt = Formatter::new(stdout, json, color);
    let budget_limit = cli.budget;
    if let Some(limit) = budget_limit {
        fmt.set_budget(limit);
    }

    // Auto-init: if this is a query command and no index exists, build one.
    if is_query_command(&cli.command)
        && let Ok(cwd) = std::env::current_dir()
        && let Ok(repo_root) = db::find_repo_root(&cwd)
        && db::find_existing_index(&repo_root).is_none()
    {
        let progress = Progress::new("Indexing", "Indexed", progress::detect_mode(suppress));
        let stats = pipeline::build_index_with_progress(&repo_root, false, &progress)?;
        progress.finish(&stats);
        // Spawn daemon after auto-init (best-effort).
        spawn_daemon_background(&repo_root);
    }

    match cli.command {
        Command::Search(args) => {
            // Set up match highlighting for search results.
            fmt.set_highlight(&args.pattern, args.regex, args.ignore_case);

            let results =
                search::text_search(&args.pattern, args.regex, args.ignore_case, &args.paths)?;

            if results.is_empty() {
                output::print_hint(
                    "no results found; try a broader pattern or different paths",
                    suppress,
                );
            }

            // Open DB connection once (shared between detection and ranking).
            // Skip DB work entirely in raw mode — user explicitly chose unranked.
            let conn = if args.raw {
                None
            } else {
                std::env::current_dir()
                    .ok()
                    .and_then(|cwd| db::find_repo_root(&cwd).ok())
                    .and_then(|root| db::find_existing_index(&root))
                    .and_then(|path| db::open(&path).ok())
            };

            // Count symbol matches for mode detection and indicator display.
            let symbol_count = conn
                .as_ref()
                .map(|c| db::count_matching_symbols(c, &args.pattern))
                .unwrap_or(0);

            let mode = detect_search_mode(args.raw, args.smart, symbol_count);

            // Print mode indicator (skip for raw — user explicitly chose it).
            if !args.raw {
                output::print_mode_indicator(symbol_count, suppress);
            }

            let mut truncated = 0usize;
            match mode {
                SearchMode::Smart(_) => {
                    // Ranked mode: classify, sort, dedup, and group with headers.
                    use crate::ranker;

                    let groups = ranker::rank_and_dedup(&results, conn.as_ref(), &args.pattern);

                    for (category, items) in &groups {
                        if !suppress {
                            output::print_category_header(ranker::category_header(*category));
                        }
                        for item in items {
                            let mut out = SearchOutput::from_search_result(
                                &item.result.file,
                                item.result.line,
                                item.result.col,
                                &item.result.content,
                            );
                            out.annotation = item.annotation.clone();
                            if fmt.format_search_result(&out)? == BudgetStatus::Skipped {
                                truncated += 1;
                            }
                        }
                    }
                }
                SearchMode::Plain => {
                    // Plain text mode: output directly without ranking/dedup.
                    for r in &results {
                        let out =
                            SearchOutput::from_search_result(&r.file, r.line, r.col, &r.content);
                        if fmt.format_search_result(&out)? == BudgetStatus::Skipped {
                            truncated += 1;
                        }
                    }
                }
            }
            emit_budget_summary(&mut fmt, truncated, budget_limit, json)?;
        }
        Command::Sym(args) => {
            let repo_root =
                db::find_repo_root(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
                    .ok();
            let router = QueryRouter::new(repo_root, false);

            if !router.has_index() {
                output::print_hint(
                    "no index found; falling back to grep (run `wonk init` for faster results)",
                    suppress,
                );
            }

            let kind_str = args.kind.as_deref();
            let results = router.query_symbols(&args.name, kind_str, args.exact)?;

            if results.is_empty() {
                output::print_hint(
                    "no symbols found; try a broader query or omit --exact",
                    suppress,
                );
            }

            let mut truncated = 0usize;
            for sym in &results {
                let out = SymbolOutput {
                    name: sym.name.clone(),
                    kind: sym.kind.to_string(),
                    file: sym.file.clone(),
                    line: sym.line,
                    col: sym.col,
                    end_line: sym.end_line,
                    scope: sym.scope.clone(),
                    signature: sym.signature.clone(),
                    language: sym.language.clone(),
                };
                if fmt.format_symbol(&out)? == BudgetStatus::Skipped {
                    truncated += 1;
                }
            }
            emit_budget_summary(&mut fmt, truncated, budget_limit, json)?;
        }
        Command::Ref(args) => {
            let router = QueryRouter::new(None, false);

            if !router.has_index() {
                output::print_hint(
                    "no index found; falling back to grep (run `wonk init` for faster results)",
                    suppress,
                );
            }

            let results = router.query_references(&args.name, &args.paths)?;

            if results.is_empty() {
                output::print_hint("no references found", suppress);
            }

            let mut truncated = 0usize;
            for r in &results {
                let out = RefOutput {
                    name: r.name.clone(),
                    kind: r.kind.to_string(),
                    file: r.file.clone(),
                    line: r.line,
                    col: r.col,
                    context: r.context.clone(),
                };
                if fmt.format_reference(&out)? == BudgetStatus::Skipped {
                    truncated += 1;
                }
            }
            emit_budget_summary(&mut fmt, truncated, budget_limit, json)?;
        }
        Command::Sig(args) => {
            let router = QueryRouter::new(None, false);

            if !router.has_index() {
                output::print_hint(
                    "no index found; falling back to grep (run `wonk init` for faster results)",
                    suppress,
                );
            }

            let results = router.query_signatures(&args.name)?;

            if results.is_empty() {
                output::print_hint("no signatures found", suppress);
            }

            let mut truncated = 0usize;
            for sym in &results {
                let out = SignatureOutput {
                    name: sym.name.clone(),
                    file: sym.file.clone(),
                    line: sym.line,
                    signature: sym.signature.clone(),
                    language: sym.language.clone(),
                };
                if fmt.format_signature(&out)? == BudgetStatus::Skipped {
                    truncated += 1;
                }
            }
            emit_budget_summary(&mut fmt, truncated, budget_limit, json)?;
        }
        Command::Ls(args) => {
            let truncated = dispatch_ls(args, suppress, &mut fmt)?;
            emit_budget_summary(&mut fmt, truncated, budget_limit, json)?;
        }
        Command::Deps(args) => {
            let repo_root =
                db::find_repo_root(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
                    .ok();
            let router = QueryRouter::new(repo_root, false);

            if !router.has_index() {
                output::print_hint(
                    "no index found; falling back to grep (run `wonk init` for faster results)",
                    suppress,
                );
            }

            let results = router.query_deps(&args.file)?;

            if results.is_empty() {
                output::print_hint("no dependencies found", suppress);
            }

            let mut truncated = 0usize;
            for dep in &results {
                let out = output::DepOutput {
                    file: args.file.clone(),
                    depends_on: dep.clone(),
                };
                if fmt.format_dep(&out)? == BudgetStatus::Skipped {
                    truncated += 1;
                }
            }
            emit_budget_summary(&mut fmt, truncated, budget_limit, json)?;
        }
        Command::Rdeps(args) => {
            let repo_root =
                db::find_repo_root(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
                    .ok();
            let router = QueryRouter::new(repo_root, false);

            if !router.has_index() {
                output::print_hint(
                    "no index found; falling back to grep (run `wonk init` for faster results)",
                    suppress,
                );
            }

            let results = router.query_rdeps(&args.file)?;

            if results.is_empty() {
                output::print_hint("no reverse dependencies found", suppress);
            }

            let mut truncated = 0usize;
            for source in &results {
                let out = output::DepOutput {
                    file: source.clone(),
                    depends_on: args.file.clone(),
                };
                if fmt.format_dep(&out)? == BudgetStatus::Skipped {
                    truncated += 1;
                }
            }
            emit_budget_summary(&mut fmt, truncated, budget_limit, json)?;
        }
        Command::Init(args) => {
            let repo_root = std::env::current_dir()?;
            let repo_root = db::find_repo_root(&repo_root)?;
            let progress = Progress::new("Indexing", "Indexed", progress::detect_mode(suppress));
            let stats = pipeline::build_index_with_progress(&repo_root, args.local, &progress)?;
            progress.finish(&stats);
        }
        Command::Update => {
            let repo_root = std::env::current_dir()?;
            let repo_root = db::find_repo_root(&repo_root)?;
            let progress =
                Progress::new("Re-indexing", "Re-indexed", progress::detect_mode(suppress));
            let stats = pipeline::rebuild_index_with_progress(&repo_root, false, &progress)?;
            progress.finish(&stats);
        }
        Command::Status => {
            output::print_hint("status: not yet implemented", suppress);
        }
        Command::Daemon(args) => match args.command {
            DaemonCommand::Start => {
                output::print_hint("daemon start: not yet implemented", suppress);
            }
            DaemonCommand::Stop => {
                output::print_hint("daemon stop: not yet implemented", suppress);
            }
            DaemonCommand::Status => {
                output::print_hint("daemon status: not yet implemented", suppress);
            }
        },
        Command::Repos(args) => match args.command {
            ReposCommand::List => {
                output::print_hint("repos list: not yet implemented", suppress);
            }
            ReposCommand::Clean => {
                output::print_hint("repos clean: not yet implemented", suppress);
            }
        },
    }
    Ok(())
}

/// Returns `true` for commands that query the index and should trigger
/// auto-initialization when no index exists.
fn is_query_command(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Search(_)
            | Command::Sym(_)
            | Command::Ref(_)
            | Command::Sig(_)
            | Command::Ls(_)
            | Command::Deps(_)
            | Command::Rdeps(_)
    )
}

/// Emit a budget summary if any results were truncated.
///
/// In grep mode, prints the summary to stderr. In JSON mode, emits a
/// `TruncationMeta` JSON line to the formatter.
fn emit_budget_summary<W: io::Write>(
    fmt: &mut Formatter<W>,
    truncated: usize,
    budget_limit: Option<usize>,
    json: bool,
) -> Result<()> {
    if truncated == 0 {
        return Ok(());
    }
    if let Some(limit) = budget_limit {
        if json {
            let meta = output::TruncationMeta {
                truncated_count: truncated,
                budget_tokens: limit,
                used_tokens: fmt.budget_used(),
            };
            fmt.format_truncation_meta(&meta)?;
        } else {
            output::print_budget_summary(truncated, limit);
        }
    }
    Ok(())
}

/// Spawn the daemon as a background subprocess (best-effort).
///
/// Uses `std::process::Command` to launch `wonk daemon start` as a detached
/// child process.  Errors are silently ignored since the daemon is optional.
fn spawn_daemon_background(repo_root: &Path) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .args(["daemon", "start"])
            .current_dir(repo_root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

/// Handle `wonk ls <path>` dispatch.
///
/// Lists symbols in a single file or recursively for a directory.
/// When `--tree` is set, groups symbols by scope hierarchy (e.g. methods
/// under their parent class).
/// Returns the number of results truncated by budget (0 if no budget active).
fn dispatch_ls<W: io::Write>(
    args: LsArgs,
    suppress: bool,
    fmt: &mut Formatter<W>,
) -> Result<usize> {
    let path = PathBuf::from(&args.path);
    let repo_root =
        db::find_repo_root(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))).ok();
    let router = QueryRouter::new(repo_root, false);

    if !router.has_index() {
        output::print_hint(
            "no index found; falling back to grep (run `wonk init` for faster results)",
            suppress,
        );
    }

    // Collect file paths: single file or recursive directory walk.
    let files: Vec<String> = if path.is_dir() {
        let walker = crate::walker::Walker::new(&path);
        walker
            .collect_paths()
            .into_iter()
            .filter(|p| p.is_file())
            .map(|p| p.to_string_lossy().into_owned())
            .collect()
    } else {
        vec![args.path.clone()]
    };

    let mut all_symbols = Vec::new();
    for file in &files {
        let symbols = router.query_symbols_in_file(file, args.tree)?;
        all_symbols.extend(symbols);
    }

    if all_symbols.is_empty() {
        output::print_hint("no symbols found", suppress);
    }

    let mut truncated = 0usize;
    if args.tree {
        let entries = build_tree_entries(&all_symbols);
        for entry in &entries {
            if fmt.format_ls_symbol(entry)? == BudgetStatus::Skipped {
                truncated += 1;
            }
        }
    } else {
        for sym in &all_symbols {
            let out = SymbolOutput {
                name: sym.name.clone(),
                kind: sym.kind.to_string(),
                file: sym.file.clone(),
                line: sym.line,
                col: sym.col,
                end_line: sym.end_line,
                scope: sym.scope.clone(),
                signature: sym.signature.clone(),
                language: sym.language.clone(),
            };
            if fmt.format_symbol(&out)? == BudgetStatus::Skipped {
                truncated += 1;
            }
        }
    }

    Ok(truncated)
}

// ---------------------------------------------------------------------------
// Heuristic grep patterns
// ---------------------------------------------------------------------------

/// Build a regex pattern to find symbol definitions via grep.
///
/// Covers all 10 supported languages:
///   Rust:       `fn`, `pub fn`, `pub(crate) fn`, `struct`, `enum`, `trait`
///   Python:     `def`, `class`
///   Ruby:       `def`, `class`, `module`
///   JavaScript: `function`, `class`
///   TypeScript: `function`, `class`, `interface`, `enum`
///   Go:         `func`, `type ... struct`, `type ... interface`
///   Java:       `class`, `interface`, `enum`
///   C:          function-like patterns (captured by generic regex)
///   C++:        `class`, `struct`, `enum`, function-like patterns
///   PHP:        `function`, `class`, `interface`, `trait`
pub fn symbol_grep_pattern(name: &str) -> String {
    // Use word boundary around the name to reduce false positives.
    format!(
        r"(fn|pub\s+fn|pub\(crate\)\s+fn|def|function|func|class|struct|enum|trait|interface|module|type|const|let|var|val)\s+{}\b",
        regex_escape(name)
    )
}

/// Build a regex pattern to find symbol definitions filtered by kind.
pub fn symbol_kind_grep_pattern(name: &str, kind: &str) -> String {
    let keywords = match kind {
        "function" | "method" => "fn|pub\\s+fn|pub\\(crate\\)\\s+fn|def|function|func",
        "class" => "class",
        "struct" => "struct",
        "interface" => "interface",
        "enum" => "enum",
        "trait" => "trait",
        "type_alias" => "type",
        "constant" => "const",
        "variable" => "let|var|val",
        "module" => "module|mod",
        _ => return symbol_grep_pattern(name),
    };
    format!(r"({})\s+{}\b", keywords, regex_escape(name))
}

/// Build a regex pattern to find references (usages) of a name via grep.
///
/// This is a broad pattern that looks for the name as a word boundary match,
/// which captures calls, type annotations, and other usages.
pub fn reference_grep_pattern(name: &str) -> String {
    format!(r"\b{}\b", regex_escape(name))
}

/// Build a regex pattern to find import/use statements mentioning a name.
///
/// Covers all 10 supported languages:
///   Rust:       `use ... name`
///   Python:     `import name`, `from ... import name`
///   Ruby:       `require ... name`
///   JavaScript: `import ... name`, `require(... name ...)`
///   TypeScript: `import ... name`
///   Go:         `import ... name`
///   Java:       `import ... name`
///   C/C++:      `#include ... name`
///   PHP:        `use ... name`, `require ... name`, `include ... name`
pub fn import_grep_pattern(name: &str) -> String {
    format!(
        r"(import|from|require|use|include)\s+.*{}",
        regex_escape(name)
    )
}

/// Build a regex pattern to find signature lines (function/method declarations).
pub fn signature_grep_pattern(name: &str) -> String {
    format!(
        r"(fn|pub\s+fn|pub\(crate\)\s+fn|def|function|func)\s+{}\s*\(",
        regex_escape(name)
    )
}

/// Escape special regex characters in a literal name.
fn regex_escape(s: &str) -> String {
    let mut escaped = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

// ---------------------------------------------------------------------------
// SymbolKind parsing helpers
// ---------------------------------------------------------------------------

/// Parse a `SymbolKind` from the string stored in the database.
fn parse_symbol_kind(s: &str) -> SymbolKind {
    match s {
        "function" => SymbolKind::Function,
        "method" => SymbolKind::Method,
        "class" => SymbolKind::Class,
        "struct" => SymbolKind::Struct,
        "interface" => SymbolKind::Interface,
        "enum" => SymbolKind::Enum,
        "trait" => SymbolKind::Trait,
        "type_alias" => SymbolKind::TypeAlias,
        "constant" => SymbolKind::Constant,
        "variable" => SymbolKind::Variable,
        "module" => SymbolKind::Module,
        _ => SymbolKind::Function, // fallback
    }
}

// ---------------------------------------------------------------------------
// QueryRouter
// ---------------------------------------------------------------------------

/// Routes queries to the SQLite index when available, falling back to
/// grep-based heuristic search when the index is missing or returns no
/// results.
pub struct QueryRouter {
    /// Open database connection, or `None` if no index was found.
    conn: Option<Connection>,
    /// Repository root directory (used as the base for grep searches).
    repo_root: PathBuf,
}

impl QueryRouter {
    /// Create a new `QueryRouter`.
    ///
    /// * `repo_root` - If `Some`, use this as the repo root.  If `None`,
    ///   attempt to discover it from the current directory.
    /// * `local` - When `true`, look for a local `.wonk/index.db` inside the
    ///   repo; otherwise use the central `~/.wonk/repos/<hash>/index.db`.
    ///
    /// If no index database is found, the router is still usable -- all
    /// queries will go through the grep fallback.
    pub fn new(repo_root: Option<PathBuf>, local: bool) -> Self {
        let root = repo_root
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|cwd| db::find_repo_root(&cwd).ok())
            })
            .unwrap_or_else(|| PathBuf::from("."));

        let conn = db::index_path_for(&root, local)
            .ok()
            .filter(|p| p.exists())
            .and_then(|p| db::open_existing(&p).ok());

        Self {
            conn,
            repo_root: root,
        }
    }

    /// Create a `QueryRouter` with an explicit connection (useful for testing).
    #[cfg(test)]
    pub fn with_conn(conn: Connection, repo_root: PathBuf) -> Self {
        Self {
            conn: Some(conn),
            repo_root,
        }
    }

    /// Create a `QueryRouter` with no database (grep-only mode, useful for testing).
    #[cfg(test)]
    pub fn grep_only(repo_root: PathBuf) -> Self {
        Self {
            conn: None,
            repo_root,
        }
    }

    /// Returns `true` if the router has an open index database.
    pub fn has_index(&self) -> bool {
        self.conn.is_some()
    }

    // -- Symbol queries -----------------------------------------------------

    /// Look up symbols by name.
    ///
    /// * `name` - The symbol name to search for.
    /// * `kind` - Optional filter by symbol kind (e.g. "function", "class").
    /// * `exact` - When `true`, match the name exactly; otherwise substring match.
    ///
    /// Tries the SQLite index first; falls back to grep on `NoIndex` or empty
    /// results.
    pub fn query_symbols(
        &self,
        name: &str,
        kind: Option<&str>,
        exact: bool,
    ) -> Result<Vec<Symbol>, DbError> {
        // Try SQLite first.
        if let Some(conn) = &self.conn {
            let results = query_symbols_db(conn, name, kind, exact)?;
            if !results.is_empty() {
                return Ok(results);
            }
        }

        // Fallback to grep.
        Ok(self.query_symbols_grep(name, kind))
    }

    /// Grep-based symbol search fallback.
    fn query_symbols_grep(&self, name: &str, kind: Option<&str>) -> Vec<Symbol> {
        let pattern = match kind {
            Some(k) => symbol_kind_grep_pattern(name, k),
            None => symbol_grep_pattern(name),
        };

        let root_str = self.repo_root.to_string_lossy().into_owned();
        let results = search::text_search(&pattern, true, false, &[root_str]);

        match results {
            Ok(hits) => hits
                .into_iter()
                .map(|r| Symbol {
                    name: name.to_string(),
                    kind: kind.map(parse_symbol_kind).unwrap_or(SymbolKind::Function),
                    file: r.file.to_string_lossy().into_owned(),
                    line: r.line as usize,
                    col: r.col as usize,
                    end_line: None,
                    scope: None,
                    signature: r.content.clone(),
                    language: String::new(),
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    // -- Reference queries --------------------------------------------------

    /// Find references to a symbol name.
    ///
    /// * `name` - The name to search for references to.
    /// * `paths` - Optional path restrictions for the search.
    ///
    /// Tries the SQLite index first; falls back to grep.
    pub fn query_references(
        &self,
        name: &str,
        paths: &[String],
    ) -> Result<Vec<Reference>, DbError> {
        // Try SQLite first.
        if let Some(conn) = &self.conn {
            let results = query_references_db(conn, name)?;
            if !results.is_empty() {
                return Ok(results);
            }
        }

        // Fallback to grep.
        Ok(self.query_references_grep(name, paths))
    }

    /// Grep-based reference search fallback.
    fn query_references_grep(&self, name: &str, paths: &[String]) -> Vec<Reference> {
        let pattern = reference_grep_pattern(name);
        let search_paths = if paths.is_empty() {
            vec![self.repo_root.to_string_lossy().into_owned()]
        } else {
            paths.to_vec()
        };

        let results = search::text_search(&pattern, true, false, &search_paths);

        match results {
            Ok(hits) => hits
                .into_iter()
                .map(|r| Reference {
                    name: name.to_string(),
                    kind: ReferenceKind::Call,
                    file: r.file.to_string_lossy().into_owned(),
                    line: r.line as usize,
                    col: r.col as usize,
                    context: r.content.clone(),
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    // -- Signature queries --------------------------------------------------

    /// Look up function/method signatures by name.
    ///
    /// Tries the SQLite index first; falls back to grep.
    pub fn query_signatures(&self, name: &str) -> Result<Vec<Symbol>, DbError> {
        // Try SQLite first (signatures are symbols with kind=function/method).
        if let Some(conn) = &self.conn {
            let results = query_signatures_db(conn, name)?;
            if !results.is_empty() {
                return Ok(results);
            }
        }

        // Fallback to grep.
        Ok(self.query_signatures_grep(name))
    }

    /// Grep-based signature search fallback.
    fn query_signatures_grep(&self, name: &str) -> Vec<Symbol> {
        let pattern = signature_grep_pattern(name);
        let root_str = self.repo_root.to_string_lossy().into_owned();
        let results = search::text_search(&pattern, true, false, &[root_str]);

        match results {
            Ok(hits) => hits
                .into_iter()
                .map(|r| Symbol {
                    name: name.to_string(),
                    kind: SymbolKind::Function,
                    file: r.file.to_string_lossy().into_owned(),
                    line: r.line as usize,
                    col: r.col as usize,
                    end_line: None,
                    scope: None,
                    signature: r.content.clone(),
                    language: String::new(),
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    // -- File symbol listing ------------------------------------------------

    /// List all symbols in a given file.
    ///
    /// * `path` - File path to list symbols for.
    /// * `tree` - When `true`, attempt to use tree-sitter parsing as fallback
    ///   instead of grep (currently not implemented; placeholder for future).
    ///
    /// Tries the SQLite index first; falls back to grep for function/class
    /// definitions.
    pub fn query_symbols_in_file(&self, path: &str, _tree: bool) -> Result<Vec<Symbol>, DbError> {
        // Try SQLite first.
        if let Some(conn) = &self.conn {
            let results = query_symbols_in_file_db(conn, path)?;
            if !results.is_empty() {
                return Ok(results);
            }
        }

        // Fallback: grep for common definition patterns in the specific file.
        Ok(self.query_symbols_in_file_grep(path))
    }

    /// Grep-based file symbol listing fallback.
    fn query_symbols_in_file_grep(&self, path: &str) -> Vec<Symbol> {
        let pattern = r"(fn|pub\s+fn|pub\(crate\)\s+fn|def|function|func|class|struct|enum|trait|interface|module)\s+\w+".to_string();
        let results = search::text_search(&pattern, true, false, &[path.to_string()]);

        match results {
            Ok(hits) => hits
                .into_iter()
                .map(|r| Symbol {
                    name: extract_symbol_name(&r.content),
                    kind: SymbolKind::Function,
                    file: r.file.to_string_lossy().into_owned(),
                    line: r.line as usize,
                    col: r.col as usize,
                    end_line: None,
                    scope: None,
                    signature: r.content.clone(),
                    language: String::new(),
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    // -- Dependency queries -------------------------------------------------

    /// Find dependencies of a file (files it imports/uses).
    ///
    /// Tries the SQLite index first; falls back to grep for import statements.
    pub fn query_deps(&self, file: &str) -> Result<Vec<String>, DbError> {
        // Try SQLite first.
        if let Some(conn) = &self.conn {
            let results = query_deps_db(conn, file)?;
            if !results.is_empty() {
                return Ok(results);
            }
        }

        // Fallback to grep for import patterns.
        Ok(self.query_deps_grep(file))
    }

    /// Grep-based dependency search fallback.
    fn query_deps_grep(&self, file: &str) -> Vec<String> {
        let pattern = r"(import|from|require|use|include)\s+".to_string();
        let results = search::text_search(&pattern, true, false, &[file.to_string()]);

        match results {
            Ok(hits) => hits
                .into_iter()
                .map(|r| r.content.trim().to_string())
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    // -- Reverse dependency queries -----------------------------------------

    /// Find reverse dependencies of a file (files that import/use it).
    ///
    /// Tries the SQLite index first; falls back to grep for import statements
    /// mentioning the file's name.
    pub fn query_rdeps(&self, file: &str) -> Result<Vec<String>, DbError> {
        // Try SQLite first.
        if let Some(conn) = &self.conn {
            let results = query_rdeps_db(conn, file)?;
            if !results.is_empty() {
                return Ok(results);
            }
        }

        // Fallback to grep: search for imports mentioning this file's stem.
        Ok(self.query_rdeps_grep(file))
    }

    /// Grep-based reverse dependency search fallback.
    fn query_rdeps_grep(&self, file: &str) -> Vec<String> {
        // Extract the file stem (e.g. "foo" from "src/foo.rs").
        let stem = Path::new(file)
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.to_string());

        let pattern = import_grep_pattern(&stem);
        let root_str = self.repo_root.to_string_lossy().into_owned();
        let results = search::text_search(&pattern, true, false, &[root_str]);

        match results {
            Ok(hits) => {
                let mut files: Vec<String> = hits
                    .into_iter()
                    .map(|r| r.file.to_string_lossy().into_owned())
                    .collect();
                files.sort();
                files.dedup();
                files
            }
            Err(_) => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite query functions
// ---------------------------------------------------------------------------

/// Query symbols from the SQLite index.
fn query_symbols_db(
    conn: &Connection,
    name: &str,
    kind: Option<&str>,
    exact: bool,
) -> Result<Vec<Symbol>, DbError> {
    let mut sql = String::from(
        "SELECT name, kind, file, line, col, end_line, scope, signature, language FROM symbols",
    );
    let mut conditions = Vec::new();

    if exact {
        conditions.push("name = ?1".to_string());
    } else {
        conditions.push("name LIKE ?1".to_string());
    }

    if kind.is_some() {
        conditions.push("kind = ?2".to_string());
    }

    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }

    let name_param = if exact {
        name.to_string()
    } else {
        format!("%{}%", name)
    };

    let mut stmt = conn.prepare(&sql)?;

    let rows = if let Some(k) = kind {
        stmt.query_map(rusqlite::params![name_param, k], row_to_symbol)?
    } else {
        stmt.query_map(rusqlite::params![name_param], row_to_symbol)?
    };

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Query references from the SQLite index.
fn query_references_db(conn: &Connection, name: &str) -> Result<Vec<Reference>, DbError> {
    let sql = "SELECT name, file, line, col, context FROM \"references\" WHERE name LIKE ?1";
    let name_param = format!("%{}%", name);
    let mut stmt = conn.prepare(sql)?;

    let rows = stmt.query_map(rusqlite::params![name_param], |row| {
        let line: i64 = row.get(2)?;
        let col: i64 = row.get(3)?;
        Ok(Reference {
            name: row.get(0)?,
            kind: ReferenceKind::Call,
            file: row.get(1)?,
            line: line as usize,
            col: col as usize,
            context: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Query function/method signatures from the SQLite index.
fn query_signatures_db(conn: &Connection, name: &str) -> Result<Vec<Symbol>, DbError> {
    let sql = "SELECT name, kind, file, line, col, end_line, scope, signature, language \
               FROM symbols WHERE name LIKE ?1 AND kind IN ('function', 'method')";
    let name_param = format!("%{}%", name);
    let mut stmt = conn.prepare(sql)?;

    let rows = stmt.query_map(rusqlite::params![name_param], row_to_symbol)?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Query all symbols in a specific file from the SQLite index.
fn query_symbols_in_file_db(conn: &Connection, path: &str) -> Result<Vec<Symbol>, DbError> {
    let sql = "SELECT name, kind, file, line, col, end_line, scope, signature, language \
               FROM symbols WHERE file = ?1 ORDER BY line";
    let mut stmt = conn.prepare(sql)?;

    let rows = stmt.query_map(rusqlite::params![path], row_to_symbol)?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Query file dependencies from the `file_imports` table.
///
/// Returns the list of import paths for the given source file.
fn query_deps_db(conn: &Connection, file: &str) -> Result<Vec<String>, DbError> {
    let sql = "SELECT DISTINCT import_path FROM file_imports WHERE source_file = ?1";
    let mut stmt = conn.prepare(sql)?;

    let rows = stmt.query_map(rusqlite::params![file], |row| row.get::<_, String>(0))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Query reverse dependencies from the `file_imports` table.
///
/// Finds all files whose import paths contain the target file's stem
/// (e.g. searching for "utils.ts" matches imports like "./utils",
/// "../utils", "utils" etc.).
fn query_rdeps_db(conn: &Connection, file: &str) -> Result<Vec<String>, DbError> {
    let stem = Path::new(file)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.to_string());

    let sql = "SELECT DISTINCT source_file FROM file_imports \
               WHERE import_path LIKE ?1 AND source_file != ?2";
    let stem_param = format!("%{}", stem);
    let mut stmt = conn.prepare(sql)?;

    let rows = stmt.query_map(rusqlite::params![stem_param, file], |row| {
        row.get::<_, String>(0)
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    results.sort();
    results.dedup();
    Ok(results)
}

/// Convert a rusqlite row to a `Symbol`.
fn row_to_symbol(row: &rusqlite::Row) -> rusqlite::Result<Symbol> {
    let kind_str: String = row.get(1)?;
    let line: i64 = row.get(3)?;
    let col: i64 = row.get(4)?;
    let end_line: Option<i64> = row.get(5)?;
    Ok(Symbol {
        name: row.get(0)?,
        kind: parse_symbol_kind(&kind_str),
        file: row.get(2)?,
        line: line as usize,
        col: col as usize,
        end_line: end_line.map(|v| v as usize),
        scope: row.get(6)?,
        signature: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
        language: row.get(8)?,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract a symbol name from a matched line content.
///
/// Given a line like `fn my_func(...)`, tries to extract `my_func`.
fn extract_symbol_name(content: &str) -> String {
    // Split on whitespace, find the token after a keyword.
    let keywords = [
        "fn",
        "def",
        "function",
        "func",
        "class",
        "struct",
        "enum",
        "trait",
        "interface",
        "module",
    ];

    let tokens: Vec<&str> = content.split_whitespace().collect();
    for (i, tok) in tokens.iter().enumerate() {
        let clean = tok
            .trim_start_matches("pub(crate)")
            .trim_start_matches("pub")
            .trim();
        if keywords.contains(&clean)
            && let Some(next) = tokens.get(i + 1)
        {
            // Take only the identifier part: alphanumeric and underscores.
            let name: String = next
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return name;
            }
        }
    }

    // Last resort: extract the last word-like token.
    content
        .split_whitespace()
        .last()
        .unwrap_or("unknown")
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect()
}

// ---------------------------------------------------------------------------
// Tree-view helpers
// ---------------------------------------------------------------------------

/// Convert a flat list of symbols into `LsSymbolEntry` items with indent
/// levels derived from the `scope` field.
///
/// Symbols whose `scope` matches the `name` of an earlier symbol in the
/// list are indented one level deeper. Symbols with no scope (or a scope
/// that doesn't match any known parent) are placed at the top level.
pub fn build_tree_entries(symbols: &[Symbol]) -> Vec<LsSymbolEntry> {
    // Build a set of known top-level symbol names for scope matching.
    let parent_names: std::collections::HashSet<&str> = symbols
        .iter()
        .filter(|s| s.scope.is_none())
        .map(|s| s.name.as_str())
        .collect();

    symbols
        .iter()
        .map(|sym| {
            let indent = match &sym.scope {
                Some(scope) if parent_names.contains(scope.as_str()) => 1,
                _ => 0,
            };
            LsSymbolEntry {
                name: sym.name.clone(),
                kind: sym.kind.to_string(),
                file: sym.file.clone(),
                line: sym.line,
                indent,
                scope: sym.scope.clone(),
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{DepsArgs, InitArgs, SearchArgs, SymArgs};
    use std::fs;
    use tempfile::TempDir;

    // -- Pattern tests ------------------------------------------------------

    #[test]
    fn test_symbol_grep_pattern() {
        let pat = symbol_grep_pattern("my_func");
        assert!(pat.contains("fn"));
        assert!(pat.contains("def"));
        assert!(pat.contains("function"));
        assert!(pat.contains("func"));
        assert!(pat.contains("class"));
        assert!(pat.contains("struct"));
        assert!(pat.contains("enum"));
        assert!(pat.contains("trait"));
        assert!(pat.contains("interface"));
        assert!(pat.contains("my_func"));
    }

    #[test]
    fn test_symbol_kind_grep_pattern_function() {
        let pat = symbol_kind_grep_pattern("handler", "function");
        assert!(pat.contains("fn"));
        assert!(pat.contains("def"));
        assert!(pat.contains("function"));
        assert!(pat.contains("func"));
        assert!(pat.contains("handler"));
        // Should NOT contain class/struct etc.
        assert!(!pat.contains("class"));
    }

    #[test]
    fn test_symbol_kind_grep_pattern_class() {
        let pat = symbol_kind_grep_pattern("MyClass", "class");
        assert!(pat.contains("class"));
        assert!(pat.contains("MyClass"));
        // Should NOT contain function keywords.
        assert!(!pat.contains("def"));
    }

    #[test]
    fn test_import_grep_pattern() {
        let pat = import_grep_pattern("utils");
        assert!(pat.contains("import"));
        assert!(pat.contains("from"));
        assert!(pat.contains("require"));
        assert!(pat.contains("use"));
        assert!(pat.contains("include"));
        assert!(pat.contains("utils"));
    }

    #[test]
    fn test_reference_grep_pattern() {
        let pat = reference_grep_pattern("calculate");
        assert!(pat.contains("calculate"));
        assert!(pat.contains(r"\b"));
    }

    #[test]
    fn test_signature_grep_pattern() {
        let pat = signature_grep_pattern("process");
        assert!(pat.contains("fn"));
        assert!(pat.contains("def"));
        assert!(pat.contains("function"));
        assert!(pat.contains("func"));
        assert!(pat.contains("process"));
        assert!(pat.contains(r"\("));
    }

    #[test]
    fn test_regex_escape() {
        assert_eq!(regex_escape("hello"), "hello");
        assert_eq!(regex_escape("a.b"), r"a\.b");
        assert_eq!(regex_escape("fn()"), r"fn\(\)");
        assert_eq!(regex_escape("a+b*c"), r"a\+b\*c");
    }

    // -- extract_symbol_name tests ------------------------------------------

    #[test]
    fn test_extract_symbol_name_fn() {
        assert_eq!(extract_symbol_name("fn my_func() {"), "my_func");
    }

    #[test]
    fn test_extract_symbol_name_pub_fn() {
        assert_eq!(
            extract_symbol_name("pub fn handler(req: Request)"),
            "handler"
        );
    }

    #[test]
    fn test_extract_symbol_name_class() {
        assert_eq!(extract_symbol_name("class MyClass:"), "MyClass");
    }

    #[test]
    fn test_extract_symbol_name_def() {
        assert_eq!(extract_symbol_name("def calculate(x, y):"), "calculate");
    }

    // -- QueryRouter grep fallback tests ------------------------------------

    #[test]
    fn test_router_grep_only_mode() {
        let dir = TempDir::new().unwrap();
        let router = QueryRouter::grep_only(dir.path().to_path_buf());
        assert!(!router.has_index());
    }

    #[test]
    fn test_router_query_symbols_grep_fallback() {
        let dir = TempDir::new().unwrap();
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(
            src_dir.join("main.rs"),
            "fn main() {}\npub fn helper() {}\nlet x = 42;\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());
        let results = router.query_symbols("main", None, false).unwrap();
        assert!(!results.is_empty(), "grep fallback should find 'fn main'");
        assert!(results.iter().any(|s| s.name == "main"));
    }

    #[test]
    fn test_router_query_symbols_grep_kind_filter() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.py"),
            "def helper():\n    pass\n\nclass Helper:\n    pass\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());

        // Should find only the class when filtering by kind.
        let results = router
            .query_symbols("Helper", Some("class"), false)
            .unwrap();
        assert!(
            !results.is_empty(),
            "grep fallback should find 'class Helper'"
        );
    }

    #[test]
    fn test_router_query_references_grep_fallback() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "fn calc() {}\nfn main() { calc(); }\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());
        let results = router.query_references("calc", &[]).unwrap();
        assert!(
            !results.is_empty(),
            "grep fallback should find references to 'calc'"
        );
    }

    #[test]
    fn test_router_query_signatures_grep_fallback() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("code.rs"),
            "pub fn process(input: &str) -> Result<()> {\n    Ok(())\n}\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());
        let results = router.query_signatures("process").unwrap();
        assert!(
            !results.is_empty(),
            "grep fallback should find signature for 'process'"
        );
        assert!(results[0].signature.contains("process"));
    }

    #[test]
    fn test_router_query_deps_grep_fallback() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("main.py");
        fs::write(
            &file_path,
            "import os\nfrom sys import argv\nprint('hello')\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());
        let file_str = file_path.to_string_lossy().into_owned();
        let results = router.query_deps(&file_str).unwrap();
        assert!(
            !results.is_empty(),
            "grep fallback should find import statements"
        );
    }

    #[test]
    fn test_router_query_rdeps_grep_fallback() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.py"),
            "from utils import helper\nhelper()\n",
        )
        .unwrap();
        fs::write(dir.path().join("utils.py"), "def helper():\n    pass\n").unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());
        let results = router.query_rdeps("utils.py").unwrap();
        assert!(
            !results.is_empty(),
            "grep fallback should find files that import 'utils'"
        );
    }

    // -- SQLite query tests -------------------------------------------------

    #[test]
    fn test_router_query_symbols_from_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "my_func",
                "function",
                "src/main.rs",
                10,
                0,
                "rust",
                "fn my_func()"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        assert!(router.has_index());

        let results = router.query_symbols("my_func", None, true).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "my_func");
        assert_eq!(results[0].kind, SymbolKind::Function);
        assert_eq!(results[0].file, "src/main.rs");
        assert_eq!(results[0].line, 10);
    }

    #[test]
    fn test_router_query_symbols_from_db_substring() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "calculate_sum",
                "function",
                "lib.rs",
                5,
                0,
                "rust",
                "fn calculate_sum()"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "calculate_avg",
                "function",
                "lib.rs",
                15,
                0,
                "rust",
                "fn calculate_avg()"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());

        // Substring search should find both.
        let results = router.query_symbols("calculate", None, false).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_router_query_symbols_from_db_with_kind() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params!["Item", "struct", "types.rs", 1, 0, "rust", "struct Item"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params!["Item", "function", "factory.rs", 10, 0, "rust", "fn Item()"],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());

        // Filter by kind should only return the struct.
        let results = router.query_symbols("Item", Some("struct"), true).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].kind, SymbolKind::Struct);
    }

    #[test]
    fn test_router_query_references_from_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["my_func", "src/main.rs", 20, 4, "let x = my_func();"],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let results = router.query_references("my_func", &[]).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "my_func");
        assert_eq!(results[0].context, "let x = my_func();");
    }

    #[test]
    fn test_router_query_signatures_from_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "process",
                "function",
                "engine.rs",
                15,
                0,
                "rust",
                "fn process(input: &str) -> Result<()>"
            ],
        )
        .unwrap();
        // Also insert a struct with same name -- signatures should not include it.
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "process",
                "struct",
                "types.rs",
                1,
                0,
                "rust",
                "struct process"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let results = router.query_signatures("process").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].kind, SymbolKind::Function);
        assert!(results[0].signature.contains("fn process"));
    }

    #[test]
    fn test_router_query_symbols_in_file_from_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params!["main", "function", "src/main.rs", 1, 0, "rust", "fn main()"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "helper",
                "function",
                "src/main.rs",
                10,
                0,
                "rust",
                "fn helper()"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "other",
                "function",
                "src/other.rs",
                1,
                0,
                "rust",
                "fn other()"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let results = router.query_symbols_in_file("src/main.rs", false).unwrap();
        assert_eq!(results.len(), 2);
        // Should be ordered by line number.
        assert_eq!(results[0].name, "main");
        assert_eq!(results[1].name, "helper");
    }

    #[test]
    fn test_router_db_fallback_on_empty_results() {
        // When the DB has no matching results, the router should fall back to grep.
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        // DB is empty -- no symbols inserted.

        // Create a file that grep can find.
        fs::write(dir.path().join("code.rs"), "fn target_func() {}\n").unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        assert!(router.has_index());

        let results = router.query_symbols("target_func", None, true).unwrap();
        // DB returned nothing, so grep fallback should have found it.
        assert!(
            !results.is_empty(),
            "should fall back to grep when DB returns empty results"
        );
    }

    // -- Deps/Rdeps dispatch tests -------------------------------------------

    #[test]
    fn test_deps_dispatch_from_db() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();

        // Create a TypeScript file with imports.
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/main.ts"),
            "import { foo } from './utils';\nimport { bar } from './config';\nconsole.log(foo, bar);\n",
        )
        .unwrap();
        fs::write(root.join("src/utils.ts"), "export function foo() {}\n").unwrap();
        fs::write(root.join("src/config.ts"), "export const bar = 42;\n").unwrap();

        // Build index.
        pipeline::build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        let router = QueryRouter::with_conn(conn, root.to_path_buf());

        // Query deps for src/main.ts.
        let results = router.query_deps("src/main.ts").unwrap();
        assert!(
            results.len() >= 2,
            "should find at least 2 imports, got {}",
            results.len()
        );
        assert!(
            results.iter().any(|r| r.contains("utils")),
            "should include utils import"
        );
        assert!(
            results.iter().any(|r| r.contains("config")),
            "should include config import"
        );
    }

    #[test]
    fn test_rdeps_dispatch_from_db() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();

        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/main.ts"),
            "import { foo } from './utils';\nconsole.log(foo);\n",
        )
        .unwrap();
        fs::write(
            root.join("src/app.ts"),
            "import { helper } from './utils';\nhelper();\n",
        )
        .unwrap();
        fs::write(
            root.join("src/utils.ts"),
            "export function foo() {}\nexport function helper() {}\n",
        )
        .unwrap();

        // Build index.
        pipeline::build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        let router = QueryRouter::with_conn(conn, root.to_path_buf());

        // Query rdeps for src/utils.ts.
        let results = router.query_rdeps("src/utils.ts").unwrap();
        assert!(
            results.len() >= 2,
            "should find at least 2 reverse deps, got {}",
            results.len()
        );
    }

    #[test]
    fn test_deps_output_grep_format() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/main.ts", "./utils"],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let deps = router.query_deps("src/main.ts").unwrap();

        let mut buf = Vec::new();
        {
            let mut fmt = output::Formatter::new(&mut buf, false, false);
            for dep in &deps {
                let out = output::DepOutput {
                    file: "src/main.ts".to_string(),
                    depends_on: dep.clone(),
                };
                fmt.format_dep(&out).unwrap();
            }
        }
        let output_str = String::from_utf8(buf).unwrap();
        assert!(
            output_str.contains("src/main.ts -> ./utils"),
            "grep format: {output_str}"
        );
    }

    #[test]
    fn test_deps_output_json_format() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/main.ts", "./utils"],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let deps = router.query_deps("src/main.ts").unwrap();

        let mut buf = Vec::new();
        {
            let mut fmt = output::Formatter::new(&mut buf, true, false);
            for dep in &deps {
                let out = output::DepOutput {
                    file: "src/main.ts".to_string(),
                    depends_on: dep.clone(),
                };
                fmt.format_dep(&out).unwrap();
            }
        }
        let output_str = String::from_utf8(buf).unwrap();
        let v: serde_json::Value = serde_json::from_str(output_str.trim()).unwrap();
        assert_eq!(v["file"], "src/main.ts");
        assert_eq!(v["depends_on"], "./utils");
    }

    // -- Deps/Rdeps DB query tests (using file_imports table) ----------------

    #[test]
    fn test_router_query_deps_from_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        // Insert file_imports data.
        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/main.ts", "./utils"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/main.ts", "./config"],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let results = router.query_deps("src/main.ts").unwrap();
        assert_eq!(results.len(), 2);
        assert!(results.contains(&"./utils".to_string()));
        assert!(results.contains(&"./config".to_string()));
    }

    #[test]
    fn test_router_query_rdeps_from_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        // src/app.ts imports ./utils
        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/app.ts", "./utils"],
        )
        .unwrap();
        // src/main.ts imports ./utils
        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/main.ts", "./utils"],
        )
        .unwrap();
        // src/main.ts also imports ./config (not utils)
        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/main.ts", "./config"],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let results = router.query_rdeps("src/utils.ts").unwrap();
        // Both app.ts and main.ts import something matching "utils" stem.
        assert_eq!(results.len(), 2);
        assert!(results.contains(&"src/app.ts".to_string()));
        assert!(results.contains(&"src/main.ts".to_string()));
    }

    #[test]
    fn test_router_query_rdeps_excludes_self() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        // utils.ts imports ./helper (but has "utils" in its own imports table as source)
        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/utils.ts", "./helper"],
        )
        .unwrap();
        // app.ts imports ./utils
        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/app.ts", "./utils"],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let results = router.query_rdeps("src/utils.ts").unwrap();
        // Should not include utils.ts itself.
        assert_eq!(results.len(), 1);
        assert_eq!(results[0], "src/app.ts");
    }

    #[test]
    fn test_router_query_deps_empty_when_no_imports() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        // File has no imports in the DB.
        let results = router.query_deps("src/standalone.ts").unwrap();
        assert!(results.is_empty());
    }

    // -- Error type tests ---------------------------------------------------

    #[test]
    fn test_db_error_no_index_display() {
        let err = DbError::NoIndex;
        assert_eq!(format!("{err}"), "no index found for this repository");
    }

    #[test]
    fn test_search_error_display() {
        let err = SearchError::SearchFailed("bad pattern".to_string());
        assert_eq!(format!("{err}"), "search failed: bad pattern");
    }

    #[test]
    fn test_wonk_error_from_db_error() {
        use crate::errors::WonkError;
        let db_err = DbError::NoIndex;
        let wonk_err: WonkError = db_err.into();
        assert!(matches!(wonk_err, WonkError::Db(DbError::NoIndex)));
    }

    #[test]
    fn test_wonk_error_from_search_error() {
        use crate::errors::WonkError;
        let search_err = SearchError::SearchFailed("oops".to_string());
        let wonk_err: WonkError = search_err.into();
        assert!(matches!(wonk_err, WonkError::Search(_)));
    }

    #[test]
    fn test_wonk_error_from_io_error() {
        use crate::errors::WonkError;
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let wonk_err: WonkError = io_err.into();
        assert!(matches!(wonk_err, WonkError::Io(_)));
    }

    // -- Multi-language heuristic pattern coverage tests ---------------------

    #[test]
    fn test_symbol_pattern_matches_rust() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "pub fn handler() {}\npub(crate) fn internal() {}\nstruct Config {}\nenum State {}\ntrait Runnable {}\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());

        assert!(
            !router
                .query_symbols("handler", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("internal", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Config", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("State", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Runnable", None, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_symbol_pattern_matches_python() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.py"),
            "def process(data):\n    pass\n\nclass Worker:\n    pass\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());

        assert!(
            !router
                .query_symbols("process", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Worker", None, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_symbol_pattern_matches_javascript() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.js"),
            "function render() {}\nclass Component {}\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());

        assert!(
            !router
                .query_symbols("render", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Component", None, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_symbol_pattern_matches_go() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("main.go"),
            "func Handle(w http.ResponseWriter) {}\ntype Server struct {}\ntype Handler interface {}\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());

        assert!(
            !router
                .query_symbols("Handle", None, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_symbol_pattern_matches_typescript() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.ts"),
            "function execute() {}\ninterface Config {}\nenum Direction {}\nclass Service {}\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());

        assert!(
            !router
                .query_symbols("execute", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Config", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Direction", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Service", None, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_symbol_pattern_matches_ruby() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.rb"),
            "def process\nend\n\nclass Worker\nend\n\nmodule Utils\nend\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());

        assert!(
            !router
                .query_symbols("process", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Worker", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Utils", None, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_symbol_pattern_matches_php() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.php"),
            "function handle() {}\nclass Controller {}\ntrait Cacheable {}\ninterface Renderable {}\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());

        assert!(
            !router
                .query_symbols("handle", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Controller", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Cacheable", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Renderable", None, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_symbol_pattern_matches_java() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("App.java"),
            "class Application {}\ninterface Service {}\nenum Priority {}\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());

        assert!(
            !router
                .query_symbols("Application", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Service", None, false)
                .unwrap()
                .is_empty()
        );
        assert!(
            !router
                .query_symbols("Priority", None, false)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_import_pattern_matches_multiple_languages() {
        let dir = TempDir::new().unwrap();

        // Python imports
        fs::write(
            dir.path().join("py_app.py"),
            "import os\nfrom sys import argv\n",
        )
        .unwrap();

        // JavaScript requires
        fs::write(
            dir.path().join("js_app.js"),
            "const fs = require('fs');\nimport utils from './utils';\n",
        )
        .unwrap();

        // Rust use
        fs::write(
            dir.path().join("rs_app.rs"),
            "use std::io;\nuse crate::utils;\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());

        let results = router.query_rdeps("utils.py").unwrap();
        // Should find at least the JS and Rust files that reference "utils"
        assert!(
            !results.is_empty(),
            "import patterns should find files referencing 'utils'"
        );
    }

    // -- Sig dispatch integration tests -------------------------------------

    #[test]
    fn test_sig_dispatch_grep_format() {
        // Verify that signatures are formatted as file:line:  signature
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("code.rs"),
            "pub fn process(input: &str) -> Result<()> {\n    Ok(())\n}\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());
        let results = router.query_signatures("process").unwrap();
        assert!(!results.is_empty(), "should find signature for 'process'");

        // Format as grep-style text
        let mut buf = Vec::new();
        {
            let mut fmt = output::Formatter::new(&mut buf, false, false);
            for sym in &results {
                let out = SignatureOutput {
                    name: sym.name.clone(),
                    file: sym.file.clone(),
                    line: sym.line,
                    signature: sym.signature.clone(),
                    language: sym.language.clone(),
                };
                fmt.format_signature(&out).unwrap();
            }
        }
        let text = String::from_utf8(buf).unwrap();
        // Should be in file:line:  signature format
        assert!(
            text.contains("process"),
            "output should contain the function name"
        );
        assert!(
            text.contains(":"),
            "output should be in file:line:  sig format"
        );
    }

    #[test]
    fn test_sig_dispatch_json_format() {
        // Verify that signatures are formatted as JSON
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("code.rs"),
            "fn handler(req: Request) -> Response {\n    todo!()\n}\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());
        let results = router.query_signatures("handler").unwrap();
        assert!(!results.is_empty(), "should find signature for 'handler'");

        // Format as JSON
        let mut buf = Vec::new();
        {
            let mut fmt = output::Formatter::new(&mut buf, true, false);
            for sym in &results {
                let out = SignatureOutput {
                    name: sym.name.clone(),
                    file: sym.file.clone(),
                    line: sym.line,
                    signature: sym.signature.clone(),
                    language: sym.language.clone(),
                };
                fmt.format_signature(&out).unwrap();
            }
        }
        let text = String::from_utf8(buf).unwrap();
        let v: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(v["name"], "handler");
        assert!(v["signature"].as_str().unwrap().contains("handler"));
    }

    #[test]
    fn test_sig_dispatch_from_db() {
        // Verify sig command works when data is in the database
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "dispatch",
                "function",
                "src/router.rs",
                28,
                0,
                "rust",
                "pub fn dispatch(cli: Cli) -> Result<()>"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let results = router.query_signatures("dispatch").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "dispatch");
        assert_eq!(
            results[0].signature,
            "pub fn dispatch(cli: Cli) -> Result<()>"
        );

        // Format as grep text
        let mut buf = Vec::new();
        {
            let mut fmt = output::Formatter::new(&mut buf, false, false);
            let sym = &results[0];
            let out = SignatureOutput {
                name: sym.name.clone(),
                file: sym.file.clone(),
                line: sym.line,
                signature: sym.signature.clone(),
                language: sym.language.clone(),
            };
            fmt.format_signature(&out).unwrap();
        }
        let text = String::from_utf8(buf).unwrap();
        assert_eq!(
            text,
            "src/router.rs:28:  pub fn dispatch(cli: Cli) -> Result<()>\n"
        );
    }

    #[test]
    fn test_sig_dispatch_no_results() {
        // When no matching signatures exist, output should be empty
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("code.rs"),
            "struct Config {}\nlet x = 42;\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());
        let results = router.query_signatures("nonexistent_func").unwrap();
        assert!(
            results.is_empty(),
            "should return no results for non-existent function"
        );
    }

    // -- Sym dispatch integration tests -------------------------------------

    /// Helper: run sym query through QueryRouter and format results like dispatch does.
    fn run_sym_query(
        router: &QueryRouter,
        name: &str,
        kind: Option<&str>,
        exact: bool,
        json: bool,
    ) -> String {
        let results = router.query_symbols(name, kind, exact).unwrap();
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, json, false);
            for sym in &results {
                let out = SymbolOutput {
                    name: sym.name.clone(),
                    kind: sym.kind.to_string(),
                    file: sym.file.clone(),
                    line: sym.line,
                    col: sym.col,
                    end_line: sym.end_line,
                    scope: sym.scope.clone(),
                    signature: sym.signature.clone(),
                    language: sym.language.clone(),
                };
                fmt.format_symbol(&out).unwrap();
            }
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn test_sym_dispatch_grep_format() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "processPayment",
                "function",
                "src/billing.rs",
                42,
                0,
                "rust",
                "fn processPayment(amount: f64)"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let output = run_sym_query(&router, "processPayment", None, false, false);
        assert_eq!(
            output.trim(),
            "src/billing.rs:42:  fn processPayment(amount: f64)"
        );
    }

    #[test]
    fn test_sym_dispatch_json_format_all_fields() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, end_line, scope, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                "processPayment",
                "method",
                "src/billing.rs",
                42,
                4,
                55,
                "BillingService",
                "rust",
                "fn processPayment(&self, amount: f64)"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let output = run_sym_query(&router, "processPayment", None, false, true);
        let v: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(v["name"], "processPayment");
        assert_eq!(v["kind"], "method");
        assert_eq!(v["file"], "src/billing.rs");
        assert_eq!(v["line"], 42);
        assert_eq!(v["col"], 4);
        assert_eq!(v["end_line"], 55);
        assert_eq!(v["scope"], "BillingService");
        assert_eq!(v["signature"], "fn processPayment(&self, amount: f64)");
        assert_eq!(v["language"], "rust");
    }

    #[test]
    fn test_sym_dispatch_substring_match() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "processPayment",
                "function",
                "src/billing.rs",
                10,
                0,
                "rust",
                "fn processPayment()"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "processRefund",
                "function",
                "src/billing.rs",
                20,
                0,
                "rust",
                "fn processRefund()"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());

        // Substring match should find both.
        let output = run_sym_query(&router, "process", None, false, false);
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "substring 'process' should match both symbols"
        );
        assert!(output.contains("processPayment"));
        assert!(output.contains("processRefund"));
    }

    #[test]
    fn test_sym_dispatch_exact_match() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "processPayment",
                "function",
                "src/billing.rs",
                10,
                0,
                "rust",
                "fn processPayment()"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "processRefund",
                "function",
                "src/billing.rs",
                20,
                0,
                "rust",
                "fn processRefund()"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());

        // Exact match should find only processPayment.
        let output = run_sym_query(&router, "processPayment", None, true, false);
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 1, "--exact should return only exact matches");
        assert!(output.contains("processPayment"));
        assert!(!output.contains("processRefund"));
    }

    #[test]
    fn test_sym_dispatch_kind_filter() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "Payment",
                "function",
                "src/billing.rs",
                10,
                0,
                "rust",
                "fn Payment()"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "Payment",
                "struct",
                "src/types.rs",
                5,
                0,
                "rust",
                "struct Payment"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());

        // --kind function should only return the function.
        let output = run_sym_query(&router, "Payment", Some("function"), true, false);
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(
            lines.len(),
            1,
            "--kind function should filter to functions only"
        );
        assert!(output.contains("fn Payment()"));
        assert!(!output.contains("struct Payment"));
    }

    #[test]
    fn test_sym_dispatch_grep_fallback() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();
        // DB is empty -- no symbols inserted.

        // Create a file that grep can find.
        let src_dir = dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(
            src_dir.join("billing.rs"),
            "fn processPayment(amount: f64) -> bool {\n    true\n}\n",
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let output = run_sym_query(&router, "processPayment", None, false, false);
        assert!(
            !output.is_empty(),
            "should fall back to grep when DB returns empty"
        );
        assert!(output.contains("processPayment"));
    }

    #[test]
    fn test_sym_dispatch_json_optional_fields_omitted() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "processPayment",
                "function",
                "src/billing.rs",
                42,
                0,
                "rust",
                "fn processPayment()"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let output = run_sym_query(&router, "processPayment", None, true, true);
        // end_line and scope should be omitted when None.
        assert!(!output.contains("end_line"));
        assert!(!output.contains("scope"));
    }

    // -- Ref dispatch integration tests -------------------------------------

    #[test]
    fn test_ref_dispatch_grep_fallback_finds_references() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.rs"),
            "fn processPayment() {}\nfn main() { processPayment(); }\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());
        let results = router.query_references("processPayment", &[]).unwrap();
        assert!(
            !results.is_empty(),
            "grep fallback should find references to 'processPayment'"
        );
        assert!(results.iter().all(|r| r.name == "processPayment"));
        // Should find at least 2: the definition line and the call site
        assert!(
            results.len() >= 2,
            "expected at least 2 references, got {}",
            results.len()
        );
    }

    #[test]
    fn test_ref_dispatch_path_restriction() {
        let dir = TempDir::new().unwrap();
        let src_dir = dir.path().join("src");
        let tests_dir = dir.path().join("tests");
        fs::create_dir_all(&src_dir).unwrap();
        fs::create_dir_all(&tests_dir).unwrap();

        fs::write(
            src_dir.join("lib.rs"),
            "fn processPayment() {}\nfn handle() { processPayment(); }\n",
        )
        .unwrap();
        fs::write(
            tests_dir.join("test.rs"),
            "fn test_it() { processPayment(); }\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());

        // Restrict to src/ only
        let src_path = src_dir.to_string_lossy().into_owned();
        let results = router
            .query_references("processPayment", &[src_path])
            .unwrap();
        assert!(!results.is_empty(), "should find references in src/");
        // All results should be from src/ directory
        for r in &results {
            assert!(
                r.file.contains("src"),
                "result file '{}' should be in src/",
                r.file
            );
        }
    }

    #[test]
    fn test_ref_output_grep_format() {
        use crate::output::{Formatter, RefOutput};

        let reference = RefOutput {
            name: "processPayment".into(),
            kind: "call".into(),
            file: "src/billing.rs".into(),
            line: 42,
            col: 8,
            context: "    processPayment(order);".into(),
        };

        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, false, false);
            fmt.format_reference(&reference).unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        assert_eq!(out, "src/billing.rs:42:    processPayment(order);\n");
    }

    #[test]
    fn test_ref_output_json_format() {
        use crate::output::{Formatter, RefOutput};

        let reference = RefOutput {
            name: "processPayment".into(),
            kind: "call".into(),
            file: "src/billing.rs".into(),
            line: 42,
            col: 8,
            context: "    processPayment(order);".into(),
        };

        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, true, false);
            fmt.format_reference(&reference).unwrap();
        }
        let out = String::from_utf8(buf).unwrap();
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["name"], "processPayment");
        assert_eq!(v["kind"], "call");
        assert_eq!(v["file"], "src/billing.rs");
        assert_eq!(v["line"], 42);
        assert_eq!(v["col"], 8);
        assert_eq!(v["context"], "    processPayment(order);");
    }

    #[test]
    fn test_ref_db_fallback_to_grep_on_empty() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        // DB is empty -- no references inserted.

        // Create a file that grep can find.
        fs::write(
            dir.path().join("app.rs"),
            "fn processPayment() {}\nlet _ = processPayment();\n",
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        assert!(router.has_index());

        let results = router.query_references("processPayment", &[]).unwrap();
        assert!(
            !results.is_empty(),
            "should fall back to grep when DB returns empty ref results"
        );
    }

    #[test]
    fn test_ref_db_returns_results_without_fallback() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "processPayment",
                "src/billing.rs",
                42,
                8,
                "    processPayment(order);"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "processPayment",
                "src/main.rs",
                10,
                4,
                "    processPayment(item);"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let results = router.query_references("processPayment", &[]).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "processPayment");
        assert_eq!(results[0].file, "src/billing.rs");
        assert_eq!(results[0].line, 42);
        assert_eq!(results[0].context, "    processPayment(order);");
        assert_eq!(results[1].name, "processPayment");
        assert_eq!(results[1].file, "src/main.rs");
    }

    // -- build_tree_entries tests -------------------------------------------

    #[test]
    fn test_build_tree_entries_empty() {
        let symbols: Vec<Symbol> = vec![];
        let entries = build_tree_entries(&symbols);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_build_tree_entries_flat_no_scopes() {
        let symbols = vec![
            Symbol {
                name: "main".into(),
                kind: SymbolKind::Function,
                file: "src/main.rs".into(),
                line: 1,
                col: 0,
                end_line: Some(10),
                scope: None,
                signature: "fn main()".into(),
                language: "rust".into(),
            },
            Symbol {
                name: "helper".into(),
                kind: SymbolKind::Function,
                file: "src/main.rs".into(),
                line: 12,
                col: 0,
                end_line: Some(20),
                scope: None,
                signature: "fn helper()".into(),
                language: "rust".into(),
            },
        ];
        let entries = build_tree_entries(&symbols);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "main");
        assert_eq!(entries[0].indent, 0);
        assert!(entries[0].scope.is_none());
        assert_eq!(entries[1].name, "helper");
        assert_eq!(entries[1].indent, 0);
    }

    #[test]
    fn test_build_tree_entries_class_with_methods() {
        let symbols = vec![
            Symbol {
                name: "Worker".into(),
                kind: SymbolKind::Class,
                file: "src/lib.py".into(),
                line: 1,
                col: 0,
                end_line: Some(30),
                scope: None,
                signature: "class Worker:".into(),
                language: "python".into(),
            },
            Symbol {
                name: "process".into(),
                kind: SymbolKind::Method,
                file: "src/lib.py".into(),
                line: 5,
                col: 4,
                end_line: Some(15),
                scope: Some("Worker".into()),
                signature: "def process(self):".into(),
                language: "python".into(),
            },
            Symbol {
                name: "cleanup".into(),
                kind: SymbolKind::Method,
                file: "src/lib.py".into(),
                line: 17,
                col: 4,
                end_line: Some(25),
                scope: Some("Worker".into()),
                signature: "def cleanup(self):".into(),
                language: "python".into(),
            },
            Symbol {
                name: "standalone".into(),
                kind: SymbolKind::Function,
                file: "src/lib.py".into(),
                line: 32,
                col: 0,
                end_line: Some(40),
                scope: None,
                signature: "def standalone():".into(),
                language: "python".into(),
            },
        ];
        let entries = build_tree_entries(&symbols);
        assert_eq!(entries.len(), 4);

        // Worker is top-level
        assert_eq!(entries[0].name, "Worker");
        assert_eq!(entries[0].indent, 0);
        assert!(entries[0].scope.is_none());

        // process is nested under Worker
        assert_eq!(entries[1].name, "process");
        assert_eq!(entries[1].indent, 1);
        assert_eq!(entries[1].scope, Some("Worker".into()));

        // cleanup is nested under Worker
        assert_eq!(entries[2].name, "cleanup");
        assert_eq!(entries[2].indent, 1);
        assert_eq!(entries[2].scope, Some("Worker".into()));

        // standalone is top-level
        assert_eq!(entries[3].name, "standalone");
        assert_eq!(entries[3].indent, 0);
        assert!(entries[3].scope.is_none());
    }

    #[test]
    fn test_build_tree_entries_preserves_file_info() {
        let symbols = vec![
            Symbol {
                name: "MyStruct".into(),
                kind: SymbolKind::Struct,
                file: "src/types.rs".into(),
                line: 5,
                col: 0,
                end_line: Some(20),
                scope: None,
                signature: "struct MyStruct".into(),
                language: "rust".into(),
            },
            Symbol {
                name: "new".into(),
                kind: SymbolKind::Method,
                file: "src/types.rs".into(),
                line: 10,
                col: 4,
                end_line: Some(15),
                scope: Some("MyStruct".into()),
                signature: "fn new() -> Self".into(),
                language: "rust".into(),
            },
        ];
        let entries = build_tree_entries(&symbols);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].file, "src/types.rs");
        assert_eq!(entries[0].line, 5);
        assert_eq!(entries[0].kind, "struct");
        assert_eq!(entries[1].file, "src/types.rs");
        assert_eq!(entries[1].line, 10);
        assert_eq!(entries[1].kind, "method");
    }

    // -- Ls dispatch integration tests ----------------------------------------

    /// Helper: run ls query through QueryRouter and format results.
    fn run_ls_query(router: &QueryRouter, path: &str, tree: bool, json: bool) -> String {
        let results = router.query_symbols_in_file(path, tree).unwrap();
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, json, false);
            if tree {
                let entries = build_tree_entries(&results);
                for entry in &entries {
                    fmt.format_ls_symbol(entry).unwrap();
                }
            } else {
                for sym in &results {
                    let out = SymbolOutput {
                        name: sym.name.clone(),
                        kind: sym.kind.to_string(),
                        file: sym.file.clone(),
                        line: sym.line,
                        col: sym.col,
                        end_line: sym.end_line,
                        scope: sym.scope.clone(),
                        signature: sym.signature.clone(),
                        language: sym.language.clone(),
                    };
                    fmt.format_symbol(&out).unwrap();
                }
            }
        }
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn test_ls_flat_from_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params!["main", "function", "src/main.rs", 1, 0, "rust", "fn main()"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "helper",
                "function",
                "src/main.rs",
                10,
                0,
                "rust",
                "fn helper()"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let output = run_ls_query(&router, "src/main.rs", false, false);
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(output.contains("fn main()"));
        assert!(output.contains("fn helper()"));
    }

    #[test]
    fn test_ls_tree_from_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "Worker",
                "class",
                "src/lib.py",
                1,
                0,
                "python",
                "class Worker:"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, scope, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "process",
                "method",
                "src/lib.py",
                5,
                4,
                "Worker",
                "python",
                "def process(self):"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "standalone",
                "function",
                "src/lib.py",
                20,
                0,
                "python",
                "def standalone():"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let output = run_ls_query(&router, "src/lib.py", true, false);
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 3);

        // Worker at top level (2 spaces indent)
        assert!(lines[0].contains("  class Worker"));
        // process indented (4 spaces indent)
        assert!(lines[1].contains("    method process"));
        // standalone at top level
        assert!(lines[2].contains("  function standalone"));
    }

    #[test]
    fn test_ls_tree_json_from_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                "Worker",
                "class",
                "src/lib.py",
                1,
                0,
                "python",
                "class Worker:"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, scope, language, signature) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                "process",
                "method",
                "src/lib.py",
                5,
                4,
                "Worker",
                "python",
                "def process(self):"
            ],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());
        let output = run_ls_query(&router, "src/lib.py", true, true);
        let lines: Vec<&str> = output.trim().lines().collect();
        assert_eq!(lines.len(), 2);

        let v0: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v0["name"], "Worker");
        assert_eq!(v0["kind"], "class");
        // indent should NOT be in JSON
        assert!(v0.get("indent").is_none());

        let v1: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(v1["name"], "process");
        assert_eq!(v1["kind"], "method");
        assert_eq!(v1["scope"], "Worker");
    }

    #[test]
    fn test_ref_context_lines_included() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("code.rs"),
            "fn calc(x: i32) -> i32 { x + 1 }\nfn main() {\n    let y = calc(42);\n}\n",
        )
        .unwrap();

        let router = QueryRouter::grep_only(dir.path().to_path_buf());
        let results = router.query_references("calc", &[]).unwrap();
        assert!(!results.is_empty(), "should find references to 'calc'");
        // Every reference should have a non-empty context line
        for r in &results {
            assert!(
                !r.context.is_empty(),
                "context line should not be empty for reference at {}:{}",
                r.file,
                r.line
            );
            assert!(
                r.context.contains("calc"),
                "context '{}' should contain 'calc'",
                r.context
            );
        }
    }

    // -- is_query_command tests -----------------------------------------------

    #[test]
    fn test_is_query_command_search() {
        let cmd = Command::Search(SearchArgs {
            pattern: "test".into(),
            regex: false,
            ignore_case: false,
            raw: false,
            smart: false,
            paths: vec![],
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_sym() {
        let cmd = Command::Sym(SymArgs {
            name: "foo".into(),
            kind: None,
            exact: false,
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_ls() {
        let cmd = Command::Ls(LsArgs {
            path: ".".into(),
            tree: false,
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_deps() {
        let cmd = Command::Deps(DepsArgs {
            file: "src/main.rs".into(),
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_not_init() {
        let cmd = Command::Init(InitArgs { local: false });
        assert!(!is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_not_update() {
        assert!(!is_query_command(&Command::Update));
    }

    #[test]
    fn test_is_query_command_not_status() {
        assert!(!is_query_command(&Command::Status));
    }

    // -- SearchMode detection tests ------------------------------------------

    #[test]
    fn test_detect_search_mode_raw_always_plain() {
        assert_eq!(detect_search_mode(true, false, 5), SearchMode::Plain);
        assert_eq!(detect_search_mode(true, false, 0), SearchMode::Plain);
    }

    #[test]
    fn test_detect_search_mode_smart_always_ranked() {
        assert_eq!(detect_search_mode(false, true, 0), SearchMode::Smart(0));
        assert_eq!(detect_search_mode(false, true, 3), SearchMode::Smart(3));
    }

    #[test]
    fn test_detect_search_mode_auto_with_symbols() {
        assert_eq!(detect_search_mode(false, false, 5), SearchMode::Smart(5));
    }

    #[test]
    fn test_detect_search_mode_auto_no_symbols() {
        assert_eq!(detect_search_mode(false, false, 0), SearchMode::Plain);
    }
}
