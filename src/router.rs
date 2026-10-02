//! Query routing layer.
//!
//! [`dispatch`] handles CLI command dispatch.  [`QueryRouter`] provides the
//! core query interface: it tries the SQLite index first and, when the index
//! is unavailable or returns no results, falls back to grep-based heuristic
//! search patterns that cover all 11 supported languages.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use rusqlite::Connection;

use crate::cli::{Cli, Command, ContextArgs, DaemonCommand, McpCommand, ReposCommand};
use crate::db;
use crate::errors::DbError;
#[cfg(test)]
use crate::errors::SearchError;
use crate::output::{
    self, AffectedFlowOutput, BlastOutput, BudgetStatus, CallPathHopOutput, CalleeOutput,
    CallerOutput, ChangedSymbolOutput, ChangesOutput, FlowOutput, FlowStepOutput, Formatter,
    OutputFormat, RefOutput, SearchOutput, SemanticOutput, ShowOutput, SignatureOutput,
    SummaryOutput, SymbolOutput,
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
    let quiet = cli.quiet;
    let stdout = io::stdout().lock();

    // Load config early so we can resolve format and color.
    let repo_root_for_config = std::env::current_dir()
        .ok()
        .and_then(|cwd| db::find_repo_root(&cwd).ok());
    let config = crate::config::Config::load(repo_root_for_config.as_deref())?;

    // Resolve format: CLI flag > config default_format > grep.
    let format = cli.format.unwrap_or_else(|| {
        config
            .output
            .default_format
            .parse()
            .unwrap_or(OutputFormat::Grep)
    });
    let suppress = format.is_structured() || quiet;

    // Resolve color: disabled for structured formats.
    let color = if format.is_structured() {
        false
    } else {
        crate::color::resolve_color(&config.output.color)
    };

    // When stdout is piped (not a terminal), group output by file (one line
    // per file) so `| grep "path/"` filters correctly and `| head -N` limits
    // by file count.  Auto-budget is applied in cli::parse().
    let is_piped = !std::io::IsTerminal::is_terminal(&stdout);
    // Set when a text-mode search already terminated its output with the
    // slate line, so the piped-mode final newline below is not doubled.
    let mut text_slate_written = false;
    let budget_limit = cli.budget;
    let page = cli.page;
    let include_tests = cli.include_tests;

    let mut fmt = Formatter::new(stdout, format, color);
    fmt.set_single_line(is_piped);
    if let Some(limit) = budget_limit {
        if let Some(p) = page {
            fmt.set_budget_with_page(limit, p);
        } else {
            fmt.set_budget(limit);
        }
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
        // Signal daemon to build embeddings in background.
        if let Ok(index_path) = db::index_path_for(&repo_root, false)
            && let Ok(conn) = db::open(&index_path)
        {
            crate::daemon::request_embedding_build(&conn).ok();
        }
        // Spawn daemon after auto-init (best-effort).
        spawn_daemon_background(&repo_root);
    }

    match cli.command {
        Command::Search(args) => {
            // Auto-detect regex metacharacters and enable regex mode.
            let auto_regex = !args.regex && search::looks_like_regex(&args.pattern);
            let mut regex = if auto_regex {
                output::print_hint("pattern looks like regex; auto-enabled --regex", suppress);
                true
            } else {
                args.regex
            };

            // Set up match highlighting for search results.
            fmt.set_highlight(&args.pattern, regex, args.ignore_case);

            // Merge --file into paths list.
            let mut paths = args.paths;
            if let Some(f) = args.file {
                paths.insert(0, f);
            }

            let mut results = search::text_search(&args.pattern, regex, args.ignore_case, &paths);

            // When auto-regex detected the pattern but it fails to compile as
            // regex (e.g. unmatched parens), fall back to literal search.
            if auto_regex && results.is_err() {
                output::print_hint(
                    "regex compilation failed; falling back to literal search",
                    suppress,
                );
                regex = false;
                fmt.set_highlight(&args.pattern, regex, args.ignore_case);
                results = search::text_search(&args.pattern, regex, args.ignore_case, &paths);
            }

            let mut results = results?;

            // Exclude test/doc/example files unless --include-tests.
            // TASK-094 keep: the include_tests user opt-out — an exclusion, never a ranking demotion (the graded path signal only orders).
            if !include_tests {
                results.retain(|r| !crate::ranker::is_test_file(&r.file));
            }

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

            let mode = detect_search_mode(
                args.raw,
                args.smart || args.why || args.query_class.is_some(),
                symbol_count,
            );

            // Print mode indicator (skip for raw — user explicitly chose it).
            if !args.raw {
                output::print_mode_indicator(symbol_count, suppress);
            }

            let blend_semantic = args.semantic;

            let mut truncated = 0usize;

            if blend_semantic {
                // RRF fusion mode: fetch semantic results, fuse with structural,
                // output interleaved by descending RRF score.
                use crate::ranker;

                let rrf_k = config.search.rrf_k;
                let semantic_results = fetch_semantic_results(
                    &args.pattern,
                    conn.as_ref(),
                    config.embedding.provider,
                    suppress,
                )?;

                // Re-rank the grep candidate set by BM25 (TASK-079) before it
                // enters fusion; `None` means no usable term statistics, in
                // which case the list is passed through in V4 order.
                let ranked = conn.as_ref().and_then(|c| {
                    crate::bm25::rerank_lexical(
                        c,
                        &results,
                        &args.pattern,
                        crate::bm25::Bm25Params::from(&config.search),
                    )
                });
                if conn.is_some() && ranked.is_none() && !results.is_empty() {
                    output::print_hint(
                        "bm25 ranking skipped: index predates term statistics \
                         (run `wonk init` to re-index)",
                        suppress,
                    );
                }
                let lexical: &[search::SearchResult] = ranked.as_deref().unwrap_or(&results);

                let fused = ranker::fuse_rrf(lexical, &semantic_results, rrf_k);

                for fr in &fused {
                    let out = SearchOutput {
                        file: fr.file.clone(),
                        line: fr.line,
                        col: fr.col,
                        content: fr.content.clone(),
                        annotation: fr.annotation.clone(),
                        source: Some(fr.source.to_string()),
                        why: None,
                        query_class: None,
                        slate: None,
                        identity: None,
                    };
                    if fmt.format_search_result(&out)? == BudgetStatus::Skipped {
                        truncated += 1;
                    }
                }
            } else {
                match mode {
                    SearchMode::Smart(_) => {
                        // Ranked mode: classify, then either the legacy
                        // lexicographic sort or the signal pipeline, then the
                        // shared dedup/group with headers. REQ-017: the
                        // pipeline is config-gated and off by default;
                        // --why opts in for this invocation.
                        use crate::ranker;

                        // Defense in depth: config load already rejected
                        // unknown signal names.
                        let mut settings = crate::rerank::RankSettings::from_config(
                            &config.rank,
                            &config.search,
                            config.embedding.provider,
                            args.query_class,
                            config.topology.enabled,
                            config.duplicate.threshold,
                        )?;
                        // --why opts into the pipeline for this invocation;
                        // [feedback] enabled does too (the same
                        // implication, config-consented — a legacy-path
                        // slate carries no contributions to learn from).
                        settings.use_pipeline |= args.why || config.feedback.enabled;
                        // Feedback capture (TASK-105): widen the prepared
                        // slices and thread the optional working-context
                        // hint (--context) into the slate's features; the
                        // hint never affects ranking. Capture stays on
                        // under --no-feedback — recording is not influence.
                        settings.feedback_capture = config.feedback.enabled;
                        settings.feedback_author_features = config.feedback.author_features;
                        settings.working_context = args.context.clone();
                        // Learned weights (TASK-102): the gated overlay
                        // joins the settings — ONE read, best-effort like
                        // the slate recording (a missing table is silent;
                        // other errors warn and disable the overlay).
                        // --no-feedback (TASK-103, PRD-FB-REQ-017) skips
                        // the load outright; the flag ALSO strips any
                        // attached table inside the ranking seam —
                        // belt-and-suspenders (AR-039).
                        settings.feedback_free = args.no_feedback;
                        if config.feedback.enabled
                            && !args.no_feedback
                            && let Some(index_conn) = conn.as_ref()
                        {
                            settings.learned = crate::learning::load_learned(
                                index_conn,
                                &config.feedback,
                                &config.rank.weights,
                                system_secs(),
                            )
                            .unwrap_or_else(|e| {
                                eprintln!("wonk: learned-weight load failed: {e:#}");
                                None
                            });
                        }
                        let ranked = crate::rerank::rank_and_explain_classed(
                            &results,
                            conn.as_ref(),
                            &args.pattern,
                            &settings,
                        );
                        // Best-effort REQ-003 memo: the novelty pass
                        // compared these pairs anyway; persisting them
                        // never fails the search.
                        crate::shingles::record_pairs_best_effort(
                            conn.as_ref(),
                            &ranked.near_duplicates,
                        );
                        // Feedback slate capture (TASK-101): the same
                        // best-effort contract — gated by [feedback]
                        // enabled, a failure degrades with a warning.
                        let stored_slate = record_slate_best_effort(
                            conn.as_ref(),
                            &args.pattern,
                            &ranked,
                            &config.feedback,
                        );
                        let identity_of = stored_slate
                            .as_ref()
                            .map(|s| {
                                s.members
                                    .iter()
                                    .map(|m| ((m.file.clone(), m.line), m.identity.clone()))
                                    .collect::<std::collections::HashMap<_, _>>()
                            })
                            .unwrap_or_default();
                        // One class line per query, before any why lines
                        // (DR-038): a misclassification is diagnosable from
                        // the breakdown it produced. The `learned:` line
                        // follows (TASK-102): the gated weights in effect
                        // for THIS query's class, named numbers with their
                        // counts.
                        if args.why
                            && let Some(class) = ranked.query_class
                        {
                            output::print_query_class_line(class);
                            if let Some(table) = settings.learned.as_ref() {
                                let resolved = table.resolve(class);
                                if !resolved.evidence.is_empty() {
                                    output::print_learned_line(&resolved.evidence);
                                }
                            }
                        }

                        for (category, items) in &ranked.groups {
                            if !suppress {
                                output::print_category_header(ranker::category_header(*category));
                            }
                            for item in items {
                                let mut out = SearchOutput::from_search_result(
                                    &item.classified.result.file,
                                    item.classified.result.line,
                                    item.classified.result.col,
                                    &item.classified.result.content,
                                );
                                out.annotation = item.classified.annotation.clone();
                                out.query_class =
                                    ranked.query_class.map(|c| c.as_str().to_string());
                                if let Some(slate) = stored_slate.as_ref() {
                                    out.slate = Some(slate.token.clone());
                                    out.identity = identity_of
                                        .get(&(
                                            item.classified
                                                .result
                                                .file
                                                .to_string_lossy()
                                                .into_owned(),
                                            item.classified.result.line,
                                        ))
                                        .cloned();
                                }
                                if args.why {
                                    out.why = Some(crate::output::WhyOutput::from_contributions(
                                        item.score,
                                        &item.contributions,
                                    ));
                                }
                                let status = fmt.format_search_result(&out)?;
                                if status == BudgetStatus::Skipped {
                                    truncated += 1;
                                } else if args.why {
                                    let why = out.why.as_ref().expect("set above");
                                    output::print_why_line(&out.file, out.line, why);
                                }
                            }
                        }
                        // Text mode: one trailing machine-cuttable line
                        // referencing the slate (JSON rows carry the
                        // reference in their fields instead). In
                        // single-line mode the last collapsed row omits
                        // its newline, so complete it first.
                        if let Some(slate) = stored_slate.as_ref()
                            && !format.is_structured()
                        {
                            if fmt.is_single_line() {
                                writeln!(fmt.writer_mut())?;
                                text_slate_written = true;
                            }
                            writeln!(fmt.writer_mut(), "slate: {}", slate.token)?;
                        }
                    }
                    SearchMode::Plain => {
                        // Plain text mode: output directly without ranking/dedup.
                        for r in &results {
                            let out = SearchOutput::from_search_result(
                                &r.file, r.line, r.col, &r.content,
                            );
                            if fmt.format_search_result(&out)? == BudgetStatus::Skipped {
                                truncated += 1;
                            }
                        }
                    }
                }
            }

            emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
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

            // Support qualified paths: `Client.get` → name="get", scope="Client".
            let split = split_qualified_name(&args.name);
            let kind_str = args.kind.as_deref();
            let file_str = args.file.as_deref().or(split.file_hint.as_deref());
            let mut results =
                if let (Some(conn), Some(scope)) = (router.conn(), split.scope_hint.as_deref()) {
                    query_symbols_db_with_filters(
                        conn,
                        split.name,
                        kind_str,
                        file_str,
                        Some(scope),
                        args.exact,
                    )?
                } else {
                    router.query_symbols_with_file(split.name, kind_str, file_str, args.exact)?
                };

            // TASK-094 keep: the include_tests user opt-out — an exclusion, never a ranking demotion (the graded path signal only orders).
            if !include_tests {
                results.retain(|r| !crate::ranker::is_test_file(Path::new(&r.file)));
            }

            if results.is_empty() {
                output::print_hint(
                    "no symbols found; try a broader query or omit --exact",
                    suppress,
                );
            }

            // Apply --limit after deduplication/sorting.
            if let Some(limit) = args.limit {
                results.truncate(limit);
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
            emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
        }
        Command::Ref(args) => {
            let router = QueryRouter::new(None, false);

            if !router.has_index() {
                output::print_hint(
                    "no index found; falling back to grep (run `wonk init` for faster results)",
                    suppress,
                );
            }

            // Merge --file into paths list.
            let mut paths = args.paths;
            if let Some(f) = args.file {
                paths.insert(0, f);
            }

            let mut results = router.query_references(&args.name, &paths)?;

            // Also query subclasses/implementors from type_edges.
            let mut subclass_results = router
                .conn()
                .and_then(|conn| query_subclasses_db(conn, &args.name).ok())
                .unwrap_or_default();

            // TASK-094 keep: the include_tests user opt-out — an exclusion, never a ranking demotion (the graded path signal only orders).
            if !include_tests {
                results.retain(|r| !crate::ranker::is_test_file(Path::new(&r.file)));
                subclass_results.retain(|r| !crate::ranker::is_test_file(Path::new(&r.file)));
            }

            if results.is_empty() && subclass_results.is_empty() {
                output::print_hint("no references found", suppress);
            }

            // Files-only mode: return just unique file paths.
            if args.output == "files" {
                let mut files: Vec<String> = results.iter().map(|r| r.file.clone()).collect();
                files.extend(subclass_results.iter().map(|s| s.file.clone()));
                files.sort();
                files.dedup();
                for f in &files {
                    writeln!(fmt.writer_mut(), "{f}")?;
                }
            } else {
                let mut truncated = 0usize;

                // Show subclasses first if present.
                if !subclass_results.is_empty() && !suppress {
                    output::print_category_header("-- subclasses --");
                }
                for sym in &subclass_results {
                    let out = RefOutput {
                        name: sym.name.clone(),
                        kind: "subclass".to_string(),
                        file: sym.file.clone(),
                        line: sym.line,
                        col: sym.col,
                        context: sym.signature.clone(),
                        caller_name: None,
                        confidence: 1.0,
                    };
                    if fmt.format_reference(&out)? == BudgetStatus::Skipped {
                        truncated += 1;
                    }
                }

                if !subclass_results.is_empty() && !results.is_empty() && !suppress {
                    output::print_category_header("-- references --");
                }
                for r in &results {
                    let out = RefOutput {
                        name: r.name.clone(),
                        kind: r.kind.to_string(),
                        file: r.file.clone(),
                        line: r.line,
                        col: r.col,
                        context: r.context.clone(),
                        caller_name: r.caller_name.clone(),
                        confidence: r.confidence,
                    };
                    if fmt.format_reference(&out)? == BudgetStatus::Skipped {
                        truncated += 1;
                    }
                }
                emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
            }
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
            emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
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
            emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
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
            emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
        }
        Command::Init(args) => {
            let repo_root = std::env::current_dir()?;
            let repo_root = db::find_repo_root(&repo_root)?;
            let progress_mode = progress::detect_mode(suppress);
            let provider = crate::embedding::create_provider(
                crate::embedding::resolve_provider_kind(args.provider, config.embedding.provider),
            )?;

            // Check if we can do an incremental update instead of a full rebuild.
            let index_path = db::index_path_for(&repo_root, args.local)?;
            let needs_full_rebuild = !index_path.exists()
                || db::read_meta(&index_path)
                    .ok()
                    .and_then(|m| m.wonk_version)
                    .as_deref()
                    != Some(env!("CARGO_PKG_VERSION"));

            if needs_full_rebuild {
                let progress = Progress::new("Indexing", "Indexed", progress_mode);
                let stats = pipeline::build_index_with_progress(&repo_root, args.local, &progress)?;
                progress.finish(&stats);

                // Full embedding build.
                let index_path = db::index_path_for(&repo_root, args.local)?;
                let conn = db::open(&index_path)?;
                let emb_stats = pipeline::build_embeddings(
                    &conn,
                    &repo_root,
                    provider.as_ref(),
                    progress_mode,
                )?;
                if !suppress && !emb_stats.skipped && emb_stats.embedded_count > 0 {
                    eprintln!(
                        "Embedded {} symbols in {:.1}s",
                        emb_stats.embedded_count,
                        emb_stats.elapsed.as_secs_f64(),
                    );
                }
            } else {
                // Incremental structural update.
                let stats = pipeline::incremental_update(&repo_root, args.local)?;
                if !suppress {
                    eprintln!(
                        "Updated index ({} files, {} symbols) in {:.1}s",
                        stats.file_count,
                        stats.symbol_count,
                        stats.elapsed.as_secs_f64(),
                    );
                }

                // Incremental embedding update.
                let index_path = db::index_path_for(&repo_root, args.local)?;
                let conn = db::open(&index_path)?;
                match pipeline::build_missing_embeddings(
                    &conn,
                    &repo_root,
                    provider.as_ref(),
                    progress_mode,
                ) {
                    Ok(emb_stats) => {
                        if !suppress && emb_stats.embedded_count > 0 {
                            eprintln!(
                                "Embedded {} symbols in {:.1}s",
                                emb_stats.embedded_count,
                                emb_stats.elapsed.as_secs_f64(),
                            );
                        }
                    }
                    Err(_) => {
                        // Ollama unavailable — skip silently for incremental update.
                    }
                }
            }
        }
        Command::Update(args) => {
            let repo_root = std::env::current_dir()?;
            let repo_root = db::find_repo_root(&repo_root)?;
            let progress_mode = progress::detect_mode(suppress);
            let provider = crate::embedding::create_provider(
                crate::embedding::resolve_provider_kind(args.provider, config.embedding.provider),
            )?;

            // Decide whether we need a full rebuild or can do incremental.
            let index_path = db::index_path_for(&repo_root, false)?;
            let needs_full_rebuild = args.force
                || !index_path.exists()
                || db::read_meta(&index_path)
                    .ok()
                    .and_then(|m| m.wonk_version)
                    .as_deref()
                    != Some(env!("CARGO_PKG_VERSION"));

            if needs_full_rebuild {
                let progress = Progress::new("Re-indexing", "Re-indexed", progress_mode);
                let stats = pipeline::rebuild_index_with_progress(&repo_root, false, &progress)?;
                progress.finish(&stats);

                if !args.skip_embed {
                    // Full embedding rebuild.
                    let index_path = db::index_path_for(&repo_root, false)?;
                    let conn = db::open(&index_path)?;
                    let emb_stats = pipeline::build_embeddings(
                        &conn,
                        &repo_root,
                        provider.as_ref(),
                        progress_mode,
                    )?;
                    if !suppress && !emb_stats.skipped && emb_stats.embedded_count > 0 {
                        eprintln!(
                            "Embedded {} symbols in {:.1}s",
                            emb_stats.embedded_count,
                            emb_stats.elapsed.as_secs_f64(),
                        );
                    }
                }
            } else {
                // Incremental structural update.
                let stats = pipeline::incremental_update(&repo_root, false)?;
                if !suppress {
                    eprintln!(
                        "Updated index ({} files, {} symbols) in {:.1}s",
                        stats.file_count,
                        stats.symbol_count,
                        stats.elapsed.as_secs_f64(),
                    );
                }

                if !args.skip_embed {
                    // Incremental embedding update (graceful skip if Ollama unavailable).
                    let index_path = db::index_path_for(&repo_root, false)?;
                    let conn = db::open(&index_path)?;
                    match pipeline::build_missing_embeddings(
                        &conn,
                        &repo_root,
                        provider.as_ref(),
                        progress_mode,
                    ) {
                        Ok(emb_stats) => {
                            if !suppress && emb_stats.embedded_count > 0 {
                                eprintln!(
                                    "Embedded {} symbols in {:.1}s",
                                    emb_stats.embedded_count,
                                    emb_stats.elapsed.as_secs_f64(),
                                );
                            }
                        }
                        Err(_) => {
                            // Ollama unavailable — skip silently for incremental update.
                        }
                    }
                }
            }
        }
        Command::Ask(args) => {
            // Discover repo root (needed for embedding build).
            let repo_root = std::env::current_dir()
                .ok()
                .and_then(|cwd| db::find_repo_root(&cwd).ok());

            let conn = repo_root
                .as_ref()
                .and_then(|root| db::find_existing_index(root))
                .and_then(|path| db::open(&path).ok());

            let conn = match conn {
                Some(c) => c,
                None => {
                    output::print_hint(
                        "no index found; run `wonk init` to build the index",
                        suppress,
                    );
                    return Ok(());
                }
            };

            // Resolve the query provider against the stored vector spaces: an
            // unreachable configured Ollama degrades to the bundled provider
            // with a warning (PRD-EMB-REQ-009), while a stored space that
            // disagrees with the resolved provider blocks with a re-embed
            // command (PRD-EMB-REQ-005).
            let plan = crate::embedding::plan_query_provider(&conn, config.embedding.provider)?;
            if let Some(warning) = plan.fallback_warning {
                output::print_warning(warning);
            }
            let mut provider = plan.provider;

            // Validate --from/--to files exist in the index before computing
            // reachability (fail fast with a clear error).
            if let Some(ref f) = args.from
                && !db::file_exists_in_index(&conn, f)?
            {
                output::print_error(&format!(
                    "file not found in index: {f}\nRun `wonk init` to rebuild the index, \
                     or check the path is repo-relative."
                ));
                return Ok(());
            }
            if let Some(ref t) = args.to
                && !db::file_exists_in_index(&conn, t)?
            {
                output::print_error(&format!(
                    "file not found in index: {t}\nRun `wonk init` to rebuild the index, \
                     or check the path is repo-relative."
                ));
                return Ok(());
            }

            // Compute dependency-scoped file set for --from/--to filtering.
            let reachable_files = crate::semantic::compute_reachable_files(
                &conn,
                args.from.as_deref(),
                args.to.as_deref(),
            )?;

            // Check embedding completeness and build if needed.
            let (symbol_count, embedding_count) =
                crate::embedding::embedding_completeness(&conn, provider.as_ref())?;

            if symbol_count > 0 && embedding_count < symbol_count {
                let progress_mode = progress::detect_mode(suppress);
                let repo = match repo_root.as_deref() {
                    Some(r) => r,
                    None => {
                        output::print_error("no repository root found");
                        return Ok(());
                    }
                };
                match pipeline::build_missing_embeddings(
                    &conn,
                    repo,
                    provider.as_ref(),
                    progress_mode,
                ) {
                    Ok(stats) => {
                        if !suppress && stats.embedded_count > 0 {
                            eprintln!(
                                "Embedded {} symbols in {:.1}s",
                                stats.embedded_count,
                                stats.elapsed.as_secs_f64(),
                            );
                        }
                    }
                    Err(e) => {
                        // A configured Ollama that dies mid-build degrades to
                        // the bundled provider instead of failing the query —
                        // but only on an actual disconnect. The pipeline types
                        // its unreachability bails (the pre-flight health
                        // check and the mid-batch interruption) as
                        // EmbeddingError::OllamaUnreachable; any other build
                        // failure (model not found, storage, chunking) stays
                        // visible instead of masquerading as an unreachable
                        // provider.
                        let disconnected = e.chain().any(|cause| {
                            matches!(
                                <dyn std::error::Error>::downcast_ref::<
                                    crate::errors::EmbeddingError,
                                >(cause),
                                Some(crate::errors::EmbeddingError::OllamaUnreachable)
                            )
                        });
                        if disconnected {
                            let fallback = crate::embedding::fallback_after_disconnect(
                                &conn,
                                config.embedding.provider,
                            )?;
                            output::print_warning(
                                fallback
                                    .fallback_warning
                                    .unwrap_or(crate::embedding::BUNDLED_FALLBACK_WARNING),
                            );
                            provider = fallback.provider;
                        } else {
                            output::print_error(&format!("embedding build failed: {e:#}"));
                            return Ok(());
                        }
                    }
                }
            }

            // Load embeddings — scoped to reachable files when --from/--to
            // is specified, otherwise load all.
            let embeddings = match &reachable_files {
                Some(files) => {
                    crate::embedding::load_embeddings_for_files(&conn, files, provider.as_ref())?
                }
                None => crate::embedding::load_all_embeddings(&conn, provider.as_ref())?,
            };
            if embeddings.is_empty() {
                output::print_hint(
                    "no embeddings available; run `wonk init` to build embeddings",
                    suppress,
                );
                return Ok(());
            }

            let mut query_vec = match provider.embed_single(&args.query) {
                Ok(v) => v,
                Err(crate::errors::EmbeddingError::OllamaUnreachable) => {
                    // Ollama died between the health check and the query
                    // embed: re-plan and degrade if the stored space allows.
                    let fallback = crate::embedding::fallback_after_disconnect(
                        &conn,
                        config.embedding.provider,
                    )?;
                    output::print_warning(
                        fallback
                            .fallback_warning
                            .unwrap_or(crate::embedding::BUNDLED_FALLBACK_WARNING),
                    );
                    fallback.provider.embed_single(&args.query)?
                }
                Err(e) => return Err(e.into()),
            };
            crate::embedding::normalize(&mut query_vec);

            let scored = crate::semantic::semantic_search(&query_vec, &embeddings, 50);
            let results = crate::semantic::resolve_results(&conn, &scored)?;

            if results.is_empty() {
                output::print_hint(
                    "no results found; try a different query or run `wonk init` to rebuild embeddings",
                    suppress,
                );
            }

            let mut truncated = 0usize;
            for sr in &results {
                let out = SemanticOutput {
                    file: sr.file.clone(),
                    line: sr.line,
                    symbol_name: sr.symbol_name.clone(),
                    symbol_kind: sr.symbol_kind.to_string(),
                    similarity_score: sr.similarity_score,
                    symbol_id: sr.symbol_id,
                };
                if fmt.format_semantic_result(&out)? == BudgetStatus::Skipped {
                    truncated += 1;
                }
            }

            if !results.is_empty() {
                output::print_hint(
                    &format!(
                        "{} results (top score: {:.4})",
                        results.len(),
                        results[0].similarity_score,
                    ),
                    suppress,
                );
            }

            emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
        }
        Command::Status => {
            let repo_root = std::env::current_dir()
                .ok()
                .and_then(|cwd| db::find_repo_root(&cwd).ok());
            let index_path = repo_root
                .as_ref()
                .and_then(|root| db::find_existing_index(root));
            let conn = index_path.as_ref().and_then(|path| db::open(path).ok());

            let workspace = match (repo_root.as_ref(), index_path.as_ref()) {
                (Some(root), Some(index)) => {
                    let declared = crate::config::Config::load(Some(root))
                        .map(|c| c.contracts.workspace)
                        .unwrap_or_default();
                    crate::contracts::default_repos_dir().map(|repos| {
                        crate::contracts::workspace_status(&repos, root, index, &declared)
                    })
                }
                _ => None,
            };

            let info = query_status_info(
                conn.as_ref(),
                config.embedding.provider,
                workspace,
                &config.topology,
                &config.feedback,
                &config.rank.weights,
            );

            if format.is_structured() {
                let json =
                    serde_json::to_string_pretty(&serde_json::to_value(&info).unwrap_or_default())
                        .unwrap_or_default();
                writeln!(fmt.writer_mut(), "{json}")?;
            } else {
                eprintln!("{}", format_status_info(&info));
            }
        }
        Command::Daemon(args) => match args.command {
            DaemonCommand::Start => {
                let repo_root = db::find_repo_root(&std::env::current_dir()?)?;
                crate::daemon::spawn_daemon(&repo_root, false)?;
            }
            DaemonCommand::Stop(stop_args) => {
                if stop_args.all {
                    let repo_root = std::env::current_dir()
                        .ok()
                        .and_then(|cwd| db::find_repo_root(&cwd).ok());
                    let results = crate::daemon::stop_all_daemons(repo_root.as_deref());
                    for (repo_path, result) in &results {
                        match result {
                            Ok(()) => {
                                output::print_hint(
                                    &format!("stopped daemon for {repo_path}"),
                                    suppress,
                                );
                            }
                            Err(e) => {
                                output::print_error(&format!(
                                    "failed to stop daemon for {repo_path}: {e}"
                                ));
                            }
                        }
                    }
                    if results.is_empty() {
                        output::print_hint("no running daemons found", suppress);
                    }
                } else {
                    let repo_root = db::find_repo_root(&std::env::current_dir()?)?;
                    crate::daemon::stop_daemon(&repo_root, false)?;
                    output::print_hint("daemon stopped", suppress);
                }
            }
            DaemonCommand::Status => {
                let repo_root = std::env::current_dir()
                    .ok()
                    .and_then(|cwd| db::find_repo_root(&cwd).ok());

                // Check if daemon process is alive.
                let daemon_pid = repo_root
                    .as_ref()
                    .and_then(|r| crate::daemon::daemon_status(r, false).ok())
                    .flatten();

                // Read detailed status from DB.
                let conn = repo_root
                    .as_ref()
                    .and_then(|root| db::find_existing_index(root))
                    .and_then(|path| db::open(&path).ok());
                let info = conn
                    .as_ref()
                    .and_then(|c| crate::daemon::read_all_status(c).ok())
                    .unwrap_or_default();

                if format.is_structured() {
                    let mut status = serde_json::Map::new();
                    status.insert(
                        "running".to_string(),
                        serde_json::Value::Bool(daemon_pid.is_some()),
                    );
                    if let Some(pid) = daemon_pid {
                        status.insert("pid".to_string(), serde_json::Value::Number(pid.into()));
                    }
                    if let Some(ref state) = info.state {
                        status.insert(
                            "state".to_string(),
                            serde_json::Value::String(state.clone()),
                        );
                    }
                    if let Some(ref uptime_start) = info.uptime_start {
                        let uptime = uptime_start.parse::<i64>().ok();
                        status.insert(
                            "uptime".to_string(),
                            serde_json::Value::String(crate::daemon::format_uptime(uptime)),
                        );
                    }
                    if let Some(ref last_activity) = info.last_activity {
                        status.insert(
                            "last_activity".to_string(),
                            serde_json::Value::String(last_activity.clone()),
                        );
                    }
                    if let Some(ref last_error) = info.last_error {
                        status.insert(
                            "last_error".to_string(),
                            serde_json::Value::String(last_error.clone()),
                        );
                    }
                    if let Some(ref ebr) = info.embedding_build_requested {
                        status.insert(
                            "embedding_build_requested".to_string(),
                            serde_json::Value::Bool(ebr == "1"),
                        );
                    }
                    let json = serde_json::to_string_pretty(&status)?;
                    writeln!(fmt.writer_mut(), "{json}")?;
                } else if let Some(pid) = daemon_pid {
                    let uptime = info
                        .uptime_start
                        .as_ref()
                        .and_then(|s| s.parse::<i64>().ok());
                    eprintln!(
                        "Daemon: running (PID {pid}, uptime {})",
                        crate::daemon::format_uptime(uptime)
                    );
                    if let Some(ref last_activity) = info.last_activity {
                        let display = last_activity
                            .parse::<i64>()
                            .ok()
                            .map(|e| format!("{} ago", crate::daemon::format_uptime(Some(e))))
                            .unwrap_or_else(|| format!("epoch {last_activity}"));
                        eprintln!("Last activity: {display}");
                    }
                    if let Some(ref last_error) = info.last_error {
                        eprintln!("Last error: {last_error}");
                    }
                    if info.embedding_build_requested.as_deref() == Some("1") {
                        eprintln!("Embedding build: requested (pending)");
                    }
                } else {
                    eprintln!("Daemon: not running");
                }
            }
            DaemonCommand::List => {
                let repo_root = std::env::current_dir()
                    .ok()
                    .and_then(|cwd| db::find_repo_root(&cwd).ok());
                let daemons = crate::daemon::discover_all_daemons(repo_root.as_deref());
                if daemons.is_empty() {
                    output::print_hint("no running daemons found", suppress);
                } else {
                    dispatch_daemon_list(&mut fmt, &daemons, format)?;
                }
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
        Command::Mcp(args) => match args.command {
            McpCommand::Serve => crate::mcp::serve()?,
        },
        Command::Cluster(args) => {
            let conn = std::env::current_dir()
                .ok()
                .and_then(|cwd| db::find_repo_root(&cwd).ok())
                .and_then(|root| db::find_existing_index(&root))
                .and_then(|path| db::open(&path).ok());

            let conn = match conn {
                Some(c) => c,
                None => {
                    output::print_hint(
                        "no index found; run `wonk init` to build the index",
                        suppress,
                    );
                    return Ok(());
                }
            };

            let plan = crate::embedding::plan_query_provider(&conn, config.embedding.provider)?;
            if let Some(warning) = plan.fallback_warning {
                output::print_warning(warning);
            }
            let provider = plan.provider;

            // Normalize path: strip leading "./", normalize "." to empty.
            let prefix = args.path.strip_prefix("./").unwrap_or(&args.path);
            let prefix = if prefix == "." { "" } else { prefix };

            let embeddings = crate::embedding::load_embeddings_for_path_prefix(
                &conn,
                prefix,
                provider.as_ref(),
            )?;

            if embeddings.is_empty() {
                output::print_hint(
                    "no embeddings found for this path; run `wonk init` to build embeddings",
                    suppress,
                );
                return Ok(());
            }

            let mut clusters =
                crate::cluster::cluster_embeddings(&embeddings, crate::cluster::ABSOLUTE_MAX_K);
            crate::cluster::resolve_cluster_members(&conn, &mut clusters)?;

            let to_member_output = |m: &crate::types::ClusterMember| output::ClusterMemberOutput {
                file: m.file.clone(),
                line: m.line,
                symbol_name: m.symbol_name.clone(),
                symbol_kind: m.symbol_kind.to_string(),
                distance_to_centroid: m.distance_to_centroid,
            };

            let mut truncated = 0usize;
            for cluster in &clusters {
                if format.is_structured() {
                    let out = output::ClusterOutput {
                        cluster_id: cluster.cluster_id,
                        total_members: cluster.members.len(),
                        representatives: cluster
                            .members
                            .iter()
                            .take(args.top)
                            .map(&to_member_output)
                            .collect(),
                    };
                    if fmt.format_cluster(&out)? == BudgetStatus::Skipped {
                        truncated += 1;
                    }
                } else {
                    // Header goes to stderr; member lines go to stdout via Formatter.
                    output::print_cluster_header(
                        cluster.cluster_id,
                        cluster.members.len(),
                        suppress,
                    );
                    for member in cluster.members.iter().take(args.top) {
                        let out = to_member_output(member);
                        if fmt.format_cluster_member(&out)? == BudgetStatus::Skipped {
                            truncated += 1;
                        }
                    }
                }
            }

            emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
        }
        Command::Impact(args) => {
            let repo_root = match std::env::current_dir()
                .ok()
                .and_then(|cwd| db::find_repo_root(&cwd).ok())
            {
                Some(r) => r,
                None => {
                    output::print_error("no repository root found");
                    return Ok(());
                }
            };

            let conn =
                match db::find_existing_index(&repo_root).and_then(|path| db::open(&path).ok()) {
                    Some(c) => c,
                    None => {
                        output::print_hint(
                            "no index found; run `wonk init` to build the index",
                            suppress,
                        );
                        return Ok(());
                    }
                };

            let plan = crate::embedding::plan_query_provider(&conn, config.embedding.provider)?;
            if let Some(warning) = plan.fallback_warning {
                output::print_warning(warning);
            }
            let provider = plan.provider;

            // Determine files to analyze.
            let files: Vec<String> = if let Some(ref since) = args.since {
                crate::impact::detect_changed_files_since(since, &repo_root)?
            } else {
                // Normalize the user-provided path to be repo-relative.
                let abs = if Path::new(&args.file).is_absolute() {
                    PathBuf::from(&args.file)
                } else {
                    std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."))
                        .join(&args.file)
                };
                let rel = match abs.strip_prefix(&repo_root) {
                    Ok(p) => p.to_string_lossy().into_owned(),
                    Err(_) => {
                        output::print_error(&format!(
                            "file is outside the repository root: {}",
                            args.file
                        ));
                        return Ok(());
                    }
                };
                vec![rel]
            };

            if files.is_empty() {
                output::print_hint("no changed files found", suppress);
                return Ok(());
            }

            // Load all embeddings once (shared across files for --since).
            let all_embeddings = crate::embedding::load_all_embeddings(&conn, provider.as_ref())?;
            if all_embeddings.is_empty() {
                output::print_error(
                    "no embeddings found in the index; run `wonk init` to build embeddings",
                );
                return Ok(());
            }

            // Aggregate results across all files.
            let mut all_results = Vec::new();
            for file in &files {
                match crate::impact::analyze_impact(
                    &conn,
                    file,
                    &repo_root,
                    provider.as_ref(),
                    &all_embeddings,
                ) {
                    Ok(results) => all_results.extend(results),
                    Err(e) => {
                        let msg = format!("{e:#}");
                        if msg.contains("unsupported language") {
                            // Skip files we can't parse (e.g. .md, .json).
                            continue;
                        }
                        return Err(e);
                    }
                }
            }

            // Re-sort after merging results across multiple files
            // (--since may produce results from several analyze_impact calls).
            all_results.sort_by(|a, b| {
                b.similarity_score
                    .partial_cmp(&a.similarity_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            if all_results.is_empty() {
                output::print_hint("no impact detected", suppress);
                return Ok(());
            }

            // Helper: build grouping key for a changed symbol.
            let changed_key = |r: &crate::types::ImpactResult| {
                format!(
                    "{}:{}:{}:{}",
                    r.changed_symbol.file,
                    r.changed_symbol.line,
                    r.changed_symbol.name,
                    r.changed_symbol.kind
                )
            };

            // Helper: convert SymbolRef to output struct.
            let to_symbol_output = |s: &crate::types::SymbolRef| output::ImpactSymbolOutput {
                name: s.name.clone(),
                kind: s.kind.to_string(),
                file: s.file.clone(),
                line: s.line,
            };

            let to_entry_output = |r: &crate::types::ImpactResult| output::ImpactEntryOutput {
                file: r.impacted_symbol.file.clone(),
                line: r.impacted_symbol.line,
                symbol_name: r.impacted_symbol.name.clone(),
                symbol_kind: r.impacted_symbol.kind.to_string(),
                similarity_score: r.similarity_score,
            };

            // Output results — route through Formatter for budget + TOON support.
            let mut truncated = 0usize;

            if format.is_structured() {
                // Structured mode: group by changed symbol, emit via Formatter.
                let mut groups: Vec<(
                    String,
                    output::ImpactSymbolOutput,
                    Vec<output::ImpactEntryOutput>,
                )> = Vec::new();

                for r in &all_results {
                    let key = changed_key(r);
                    if groups.last().is_some_and(|(k, _, _)| k == &key) {
                        groups.last_mut().unwrap().2.push(to_entry_output(r));
                    } else {
                        groups.push((
                            key,
                            to_symbol_output(&r.changed_symbol),
                            vec![to_entry_output(r)],
                        ));
                    }
                }

                for (_, changed, impacted) in groups {
                    let out = output::ImpactOutput {
                        changed_symbol: changed,
                        impacted,
                    };
                    if fmt.format_impact(&out)? == BudgetStatus::Skipped {
                        truncated += 1;
                    }
                }
            } else {
                // Grep mode: group by changed symbol, print header + entries.
                let mut current_key = String::new();

                for r in &all_results {
                    let key = changed_key(r);
                    if key != current_key {
                        output::print_impact_header(
                            &r.changed_symbol.name,
                            &r.changed_symbol.kind.to_string(),
                            &r.changed_symbol.file,
                            r.changed_symbol.line,
                            suppress,
                        );
                        current_key = key;
                    }
                    let entry = to_entry_output(r);
                    if fmt.format_impact_entry(&entry)? == BudgetStatus::Skipped {
                        truncated += 1;
                    }
                }
            }

            emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
        }
        Command::Show(args) => {
            let repo_root = match std::env::current_dir()
                .ok()
                .and_then(|cwd| db::find_repo_root(&cwd).ok())
            {
                Some(r) => r,
                None => {
                    output::print_error("no repository root found");
                    return Ok(());
                }
            };

            let conn =
                match db::find_existing_index(&repo_root).and_then(|path| db::open(&path).ok()) {
                    Some(c) => c,
                    None => {
                        output::print_error("no index found; run `wonk init` to build the index");
                        return Ok(());
                    }
                };

            // Merge --file and -- paths into a combined file filter list.
            let mut file_filters: Vec<String> = args.paths;
            if let Some(f) = args.file.clone() {
                file_filters.insert(0, f);
            }

            let mut all_results = Vec::new();

            if let Some(ref name) = args.name {
                // Auto-detect file paths passed as name: if the name contains
                // '/' or ends with a code file extension, treat it as --file.
                if file_filters.is_empty() && looks_like_file_path(name) {
                    // Auto-shallow: showing all symbols in a file can be very
                    // verbose, so default to shallow mode (signatures only)
                    // unless the user explicitly chose non-shallow.
                    let auto_shallow = !args.shallow;
                    if auto_shallow {
                        output::print_hint(
                            &format!(
                                "'{name}' looks like a file path; treating as --file {name} --shallow"
                            ),
                            suppress,
                        );
                    } else {
                        output::print_hint(
                            &format!("'{name}' looks like a file path; treating as --file {name}"),
                            suppress,
                        );
                    }
                    let options = crate::show::ShowOptions {
                        file: None,
                        kind: args.kind.clone(),
                        exact: false,
                        suppress,
                        shallow: true,
                        scope: None,
                        elide: args.elide.map(Into::into),
                        signatures_only: true, // auto-file-path: compact output
                    };
                    all_results.extend(crate::show::show_file(&conn, name, &repo_root, &options)?);
                } else {
                    // Support comma-separated names for batch lookup (e.g. "main,parse,validate").
                    let names: Vec<&str> = name
                        .split(',')
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .collect();

                    // When paths are specified, query per file filter and merge.
                    let file_list = if file_filters.is_empty() {
                        vec![None]
                    } else {
                        file_filters.iter().map(|f| Some(f.clone())).collect()
                    };

                    for n in &names {
                        // Support qualified paths: `Foo::bar` → name="bar", file hint="Foo".
                        let split = split_qualified_name(n);

                        for ff in &file_list {
                            let file = ff.clone().or(split.file_hint.clone());

                            let options = crate::show::ShowOptions {
                                file,
                                kind: args.kind.clone(),
                                exact: args.exact,
                                suppress,
                                shallow: args.shallow,
                                scope: split.scope_hint.clone(),
                                elide: args.elide.map(Into::into),
                                signatures_only: false,
                            };

                            all_results.extend(crate::show::show_symbol(
                                &conn, split.name, &repo_root, &options,
                            )?);
                        }
                    }
                } // close auto-detect else
            } else if !file_filters.is_empty() {
                // File-only mode: show all top-level symbols in file(s)/directory.
                for file_pattern in &file_filters {
                    let options = crate::show::ShowOptions {
                        file: None,
                        kind: args.kind.clone(),
                        exact: false,
                        suppress,
                        shallow: args.shallow,
                        scope: None,
                        elide: args.elide.map(Into::into),
                        signatures_only: false,
                    };
                    all_results.extend(crate::show::show_file(
                        &conn,
                        file_pattern,
                        &repo_root,
                        &options,
                    )?);
                }
            } else {
                output::print_error("show requires a symbol name or --file");
                return Ok(());
            }

            // TASK-094 keep: the include_tests user opt-out — an exclusion, never a ranking demotion (the graded path signal only orders).
            if !include_tests {
                all_results.retain(|r| !crate::ranker::is_test_file(Path::new(&r.file)));
            }

            if all_results.is_empty() {
                output::print_hint(
                    "no symbols found; try a broader query or omit --exact",
                    suppress,
                );
            } else if let Some(ref name) = args.name {
                // When no exact match exists in implementation files, hint
                // that the symbol may be dynamically assigned. Type
                // declarations (rerank::is_type_declaration — .d.ts, .h)
                // don't count as implementations.
                let has_impl_exact = all_results
                    .iter()
                    .any(|r| r.name == *name && !crate::rerank::is_type_declaration(&r.file));
                if !has_impl_exact && !args.exact {
                    output::print_hint(
                        &format!(
                            "no implementation of '{}' found — results are substring or type-only matches. \
                             If '{}' is dynamically assigned, try: wonk search \"{}\"",
                            name, name, name,
                        ),
                        suppress,
                    );
                }
            }

            let mut truncated = 0usize;
            let user_shallow = args.shallow;
            for sr in &all_results {
                // Auto-shallow: when a container's body exceeds MAX_SOURCE_LINES
                // and --shallow wasn't explicitly set, retry in shallow mode to
                // show signature + child signatures instead of a truncated body.
                if !user_shallow
                    && sr.kind.is_container()
                    && sr.source.lines().count() > ShowOutput::MAX_SOURCE_LINES
                {
                    let shallow_opts = crate::show::ShowOptions {
                        file: Some(sr.file.clone()),
                        kind: Some(sr.kind.to_string()),
                        exact: true,
                        suppress,
                        shallow: true,
                        scope: None,
                        // Shallow replaces the payload; elision never touches
                        // the shallow rendering (PRD-ELIDE-REQ-009).
                        elide: None,
                        signatures_only: false,
                    };
                    if let Ok(shallow_results) =
                        crate::show::show_symbol(&conn, &sr.name, &repo_root, &shallow_opts)
                        && let Some(shallow_sr) = shallow_results
                            .iter()
                            .find(|s| s.file == sr.file && s.line == sr.line)
                    {
                        let mut shallow_out = ShowOutput::from(shallow_sr);
                        shallow_out.auto_shallow = Some(true);
                        if !format.is_structured() {
                            output::print_show_header(&sr.file, sr.line, sr.end_line, suppress);
                        }
                        if fmt.format_show(&shallow_out)? == BudgetStatus::Skipped {
                            truncated += 1;
                        }
                        continue;
                    }
                }

                let mut out = ShowOutput::from(sr);

                // Per-result truncation when budget is active.
                if budget_limit.is_some()
                    && let Some(t) = out.truncated(ShowOutput::MAX_SOURCE_LINES)
                {
                    out = t;
                }

                if !format.is_structured() {
                    output::print_show_header(&sr.file, sr.line, sr.end_line, suppress);
                }

                if fmt.format_show(&out)? == BudgetStatus::Skipped {
                    // Adaptive truncation: try to fit within remaining budget.
                    if let Some(remaining_chars) = fmt.remaining_budget_chars() {
                        let source_lines: Vec<&str> = sr.source.lines().collect();
                        if !source_lines.is_empty() {
                            let avg_chars = sr.source.len() / source_lines.len();
                            if let Some(line_budget) = remaining_chars.checked_div(avg_chars) {
                                let max_lines = line_budget.clamp(
                                    ShowOutput::MIN_SOURCE_LINES,
                                    ShowOutput::MAX_SOURCE_LINES,
                                );
                                let fresh_out = ShowOutput::from(sr);
                                if let Some(t) = fresh_out.truncated(max_lines)
                                    && fmt.format_show(&t)? == BudgetStatus::Written
                                {
                                    continue;
                                }
                            }
                        }
                    }
                    truncated += 1;
                }
            }

            emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
        }
        Command::Callers(args) => {
            let (conn, depth) = match callgraph_setup(args.depth, suppress) {
                Some(pair) => pair,
                None => return Ok(()),
            };

            // Support qualified paths: `Client.get` → name="get", file hint from scope.
            let split = split_qualified_name(&args.name);
            let scope_file = resolve_file_for_scope(&conn, split.name, split.scope_hint.as_deref());
            let reference_file = args
                .reference_file
                .clone()
                .or(split.file_hint)
                .or(scope_file);

            let mut results = crate::callgraph::callers(
                &conn,
                split.name,
                depth,
                args.min_confidence,
                reference_file.as_deref(),
                args.callers_file.as_deref(),
            )?;

            // TASK-094 keep: the include_tests user opt-out — an exclusion, never a ranking demotion (the graded path signal only orders).
            if !include_tests {
                results.retain(|r| !crate::ranker::is_test_file(Path::new(&r.file)));
            }

            if results.is_empty() {
                output::print_hint("no callers found", suppress);
            }

            let mut truncated = 0usize;
            for cr in &results {
                let out = CallerOutput {
                    caller_name: cr.caller_name.clone(),
                    caller_kind: cr.caller_kind.to_string(),
                    file: cr.file.clone(),
                    line: cr.line,
                    signature: cr.signature.clone(),
                    depth: cr.depth,
                    target_file: cr.target_file.clone(),
                    confidence: cr.confidence,
                };

                if fmt.format_caller(&out)? == BudgetStatus::Skipped {
                    truncated += 1;
                }
            }

            emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
        }
        Command::Callees(args) => {
            let (conn, depth) = match callgraph_setup(args.depth, suppress) {
                Some(pair) => pair,
                None => return Ok(()),
            };

            // Support qualified paths: `Client.get` → name="get", file hint from scope.
            let split = split_qualified_name(&args.name);
            let scope_file = resolve_file_for_scope(&conn, split.name, split.scope_hint.as_deref());
            let reference_file = args
                .reference_file
                .clone()
                .or(split.file_hint)
                .or(scope_file);

            let mut results = crate::callgraph::callees(
                &conn,
                split.name,
                depth,
                args.min_confidence,
                reference_file.as_deref(),
                args.callees_file.as_deref(),
            )?;

            // TASK-094 keep: the include_tests user opt-out — an exclusion, never a ranking demotion (the graded path signal only orders).
            if !include_tests {
                results.retain(|r| !crate::ranker::is_test_file(Path::new(&r.file)));
            }

            if results.is_empty() {
                output::print_hint("no callees found", suppress);
            }

            let mut truncated = 0usize;
            for cr in &results {
                let out = CalleeOutput {
                    callee_name: cr.callee_name.clone(),
                    file: cr.file.clone(),
                    line: cr.line,
                    context: cr.context.clone(),
                    depth: cr.depth,
                    source_file: cr.source_file.clone(),
                    confidence: cr.confidence,
                };

                if fmt.format_callee(&out)? == BudgetStatus::Skipped {
                    truncated += 1;
                }
            }

            emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
        }
        Command::Callpath(args) => {
            let conn = match callgraph_conn(suppress) {
                Some(c) => c,
                None => return Ok(()),
            };

            let path = crate::callgraph::callpath(
                &conn,
                &args.from,
                &args.to,
                args.min_confidence,
                args.reference_file.as_deref(),
                args.destination_file.as_deref(),
            )?;

            match path {
                Some(hops) => {
                    let outputs: Vec<CallPathHopOutput> = hops
                        .iter()
                        .map(|h| CallPathHopOutput {
                            symbol_name: h.symbol_name.clone(),
                            symbol_kind: h.symbol_kind.to_string(),
                            file: h.file.clone(),
                            line: h.line,
                        })
                        .collect();

                    fmt.format_callpath(&outputs)?;
                }
                None => {
                    output::print_hint("no path found", suppress);
                }
            }
        }
        Command::Summary(args) => {
            let repo_root = match std::env::current_dir()
                .ok()
                .and_then(|cwd| db::find_repo_root(&cwd).ok())
            {
                Some(r) => r,
                None => {
                    output::print_error("no repository root found");
                    return Ok(());
                }
            };

            let conn =
                match db::find_existing_index(&repo_root).and_then(|path| db::open(&path).ok()) {
                    Some(c) => c,
                    None => {
                        output::print_error("no index found; run `wonk init` to build the index");
                        return Ok(());
                    }
                };

            let detail = match args.detail.parse::<crate::types::DetailLevel>() {
                Ok(d) => d,
                Err(e) => {
                    output::print_error(&e);
                    return Ok(());
                }
            };

            let depth = if args.recursive {
                None // unlimited
            } else {
                Some(args.depth)
            };

            crate::db::ensure_summaries_table(&conn)?;

            let options = crate::summary::SummaryOptions {
                detail,
                depth,
                suppress,
                elide: args.elide.map(Into::into),
            };

            let result = crate::summary::summarize_path(&conn, &args.path, &options)?;

            let out = SummaryOutput::from_result(&result);
            fmt.format_summary(&out)?;
        }
        Command::Flows(args) => {
            let conn = match callgraph_conn(suppress) {
                Some(c) => c,
                None => return Ok(()),
            };

            let (depth, clamped) = crate::flows::clamp_depth(args.depth);
            if clamped {
                output::print_hint(
                    &format!(
                        "depth {} exceeds cap; using max depth {}",
                        args.depth,
                        crate::flows::MAX_DEPTH,
                    ),
                    suppress,
                );
            }

            let options = crate::flows::FlowOptions {
                depth,
                branching: args.branching,
                min_confidence: args.min_confidence,
                from_file: args.from.clone(),
            };

            if let Some(entry_name) = &args.entry {
                // Trace mode: trace a specific entry point.
                match crate::flows::trace_flow(&conn, entry_name, &options)? {
                    Some(ref flow) => {
                        let out = FlowOutput::from(flow);
                        fmt.format_flow(&out)?;
                    }
                    None => {
                        output::print_hint(
                            "no flow found (entry point not found or flow too short)",
                            suppress,
                        );
                    }
                }
            } else {
                // List mode: detect all entry points.
                let entries = crate::flows::detect_entry_points(&conn, &options)?;

                // Auto-trace: when file filter is set and exactly one entry point
                // is found, automatically trace it instead of listing.
                if entries.len() == 1 && args.from.is_some() {
                    match crate::flows::trace_flow(&conn, &entries[0].name, &options)? {
                        Some(ref flow) => {
                            let out = FlowOutput::from(flow);
                            fmt.format_flow(&out)?;
                        }
                        None => {
                            // Fall back to listing the entry point.
                            let out = FlowStepOutput::from(&entries[0]);
                            fmt.format_flow_entry(&out)?;
                        }
                    }
                } else if entries.is_empty() {
                    output::print_hint("no entry points detected", suppress);
                } else {
                    let mut truncated = 0usize;
                    for entry in &entries {
                        let out = FlowStepOutput::from(entry);
                        if fmt.format_flow_entry(&out)? == BudgetStatus::Skipped {
                            truncated += 1;
                        }
                    }

                    emit_budget_summary_with_page(&mut fmt, truncated, budget_limit, format, page)?;
                }
            }
        }
        Command::Blast(args) => {
            let conn = match callgraph_conn(suppress) {
                Some(c) => c,
                None => return Ok(()),
            };

            let (depth, clamped) = crate::blast::clamp_depth(args.depth);
            if clamped {
                output::print_hint(
                    &format!(
                        "depth {} exceeds cap; using max depth {}",
                        args.depth,
                        crate::blast::MAX_DEPTH,
                    ),
                    suppress,
                );
            }

            let direction = match &args.direction {
                Some(d) => d
                    .parse::<crate::types::BlastDirection>()
                    .map_err(crate::errors::WonkError::Usage)?,
                None => crate::types::BlastDirection::Upstream,
            };

            let options = crate::blast::BlastOptions {
                depth,
                direction,
                include_tests: include_tests || args.include_tests,
                min_confidence: args.min_confidence,
                use_reach: config.reach.enabled,
            };

            let mut result = crate::blast::analyze_blast(&conn, &args.symbol, &options)?;

            // Cross-repo tier (TASK-084): when the target owns provider
            // contracts, sibling consumers of those contracts append below
            // the depth tiers. Registry problems never fail blast — the
            // depth-tier result stands with a hint.
            if let Err(e) = append_cross_repo_blast_tier(&conn, &args.symbol, &mut result) {
                output::print_hint(&format!("cross-repo impact not resolved: {e}"), suppress);
            }

            if result.total_affected == 0 {
                output::print_hint("no affected symbols found", suppress);
            }

            let out = BlastOutput::from(&result);
            fmt.format_blast(&out)?;
        }
        Command::Changes(args) => {
            dispatch_changes(args, &mut fmt, suppress)?;
        }
        Command::Context(args) => {
            dispatch_context(args, &mut fmt, suppress, include_tests)?;
        }
        Command::Contracts(args) => {
            dispatch_contracts(args, &mut fmt, suppress)?;
        }
        Command::Duplicates(args) => {
            dispatch_duplicates(args, &mut fmt, suppress)?;
        }
        Command::Feedback(args) => {
            dispatch_feedback(args, &mut fmt, suppress, format)?;
        }
        Command::Review(args) => {
            dispatch_review(args, &mut fmt, suppress)?;
        }
    }

    // In single-line (piped) grep mode, emit a final newline so the output is
    // a complete line for the shell to capture (single-line emit omits the
    // trailing newline). Structured formats are exempt: their rows are each
    // newline-terminated already, and appending another would leave a blank
    // line that breaks strict NDJSON consumers.
    if is_piped && !format.is_structured() && !text_slate_written {
        writeln!(fmt.writer_mut())?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// `wonk changes` dispatch (TASK-072)
// ---------------------------------------------------------------------------

/// Parse a scope string plus optional base ref into a [`ChangeScope`]
/// (REQ-008 verbatim parsing; compare requires and validates the base ref).
fn parse_change_scope(scope: &str, base: Option<&str>) -> Result<crate::types::ChangeScope> {
    use crate::types::ChangeScope;

    if scope == "compare" {
        let base =
            base.ok_or_else(|| anyhow::anyhow!("--base is required when --scope=compare"))?;
        crate::impact::validate_git_ref(base)?;
        return Ok(ChangeScope::Compare(base.to_string()));
    }
    scope
        .parse::<ChangeScope>()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn dispatch_changes<W: io::Write>(
    args: crate::cli::ChangesArgs,
    fmt: &mut Formatter<W>,
    suppress: bool,
) -> Result<()> {
    // 1. Resolve repo root and open connection.
    let repo_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| db::find_repo_root(&cwd).ok())
        .ok_or_else(|| anyhow::anyhow!("no repository root found"))?;

    let conn = db::find_existing_index(&repo_root)
        .and_then(|path| db::open(&path).ok())
        .ok_or_else(|| anyhow::anyhow!("no index found; run `wonk init` first"))?;

    let reach_enabled = crate::config::Config::load(Some(&repo_root))
        .map(|c| c.reach.enabled)
        .unwrap_or(true);

    // 2. Parse scope string to ChangeScope enum.
    let scope = parse_change_scope(&args.scope, args.base.as_deref())?;

    // 3. Detect changes.
    let analysis = crate::impact::detect_changes(&conn, &scope, &repo_root)?;

    if analysis.changed_symbols.is_empty() {
        output::print_hint("no changed symbols detected", suppress);
    }

    // 4. Build output with optional blast/flow chaining.
    let changes_out = build_changes_output(
        &conn,
        &analysis,
        &scope,
        &ChangesChainOptions {
            blast: args.blast,
            flows: args.flows,
            min_confidence: args.min_confidence,
            reach_enabled,
        },
        |msg| output::print_hint(msg, suppress),
    )?;

    fmt.format_changes(&changes_out)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// `wonk review` dispatch (TASK-085)
// ---------------------------------------------------------------------------

fn dispatch_review<W: io::Write>(
    args: crate::cli::ReviewArgs,
    fmt: &mut Formatter<W>,
    suppress: bool,
) -> Result<()> {
    // 0. `wonk review suppress ...` manages durable suppressions; it shares
    // the review's no-auto-init guard, never a review of the diff.
    if let Some(cmd) = args.suppress {
        return dispatch_review_suppress(cmd, fmt, suppress);
    }

    // 1. Resolve repo root. No auto-init here (deliberately): the index must
    // reflect the diff's base state, and indexing the current tree would
    // empty the diff and fake an APPROVE.
    let repo_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| db::find_repo_root(&cwd).ok())
        .ok_or_else(|| anyhow::anyhow!("no repository root found"))?;

    let conn = db::find_existing_index(&repo_root)
        .and_then(|path| db::open(&path).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no index found; run `wonk init` first so the index reflects the base \
                 state of the diff (indexing the current tree mid-diff would hide it)"
            )
        })?;

    // 2. --since <ref> is sugar for --scope=compare --base=<ref>.
    let (scope_str, base) = match &args.since {
        Some(since) => ("compare".to_string(), Some(since.clone())),
        None => (args.scope.clone(), args.base.clone()),
    };
    let scope = parse_change_scope(&scope_str, base.as_deref())?;

    // 3. Load [review] rule switches, the [reach] kill switch, and the
    //    REQ-015 filter knobs from the CLI (default off = today's report).
    let config = crate::config::Config::load(Some(&repo_root))?;
    let options = crate::review::ReviewOptions {
        breaking_change: config.review.breaking_change,
        coverage_gap: config.review.coverage_gap,
        cross_repo: config.review.cross_repo,
        reach_enabled: config.reach.enabled,
        min_confidence: args.min_confidence,
        min_severity: args.min_severity,
        kinds: args.kind,
        max_findings: args.max_findings,
        elide: args.elide.map(Into::into),
        ..crate::review::ReviewOptions::default()
    };

    // Cross-repo inputs resolved once here — run_review never touches
    // $HOME itself, and a disabled rule C passes no inputs at all.
    let cross_repo = config
        .review
        .cross_repo
        .then(|| crate::review::CrossRepoInputs::discover(&repo_root))
        .flatten();

    // 4. Run the review. The verdict is data: exit code stays 0 — a
    // non-zero exit would force piping consumers to treat REVIEW (the
    // normal outcome of a productive review) as a command failure.
    let result =
        crate::review::run_review(&conn, &scope, &repo_root, &options, cross_repo.as_ref())?;
    for warning in &result.warnings {
        output::print_hint(warning, suppress);
    }
    if let Some(summary) = result.drops.summary_line() {
        output::print_hint(&summary, suppress);
    }
    if result.findings.is_empty() {
        output::print_hint("no findings for this scope", suppress);
    }

    fmt.format_review(&output::ReviewOutput::from(&result))?;
    Ok(())
}

/// `wonk review suppress list|add|remove` (PRD-REV-REQ-014). Same
/// no-auto-init guard as the review itself: suppressions live in the
/// index DB, and auto-indexing mid-diff would fake the next review.
fn dispatch_review_suppress<W: io::Write>(
    cmd: crate::cli::ReviewSuppressCommand,
    fmt: &mut Formatter<W>,
    suppress: bool,
) -> Result<()> {
    let crate::cli::ReviewSuppressCommand::Suppress { action } = cmd;

    let repo_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| db::find_repo_root(&cwd).ok())
        .ok_or_else(|| anyhow::anyhow!("no repository root found"))?;

    let conn = db::find_existing_index(&repo_root)
        .and_then(|path| db::open(&path).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no index found; run `wonk init` first so suppressions land in the \
                 repository's index"
            )
        })?;
    crate::db::ensure_review_suppressions_table(&conn)?;

    use crate::cli::ReviewSuppressAction;
    match action {
        ReviewSuppressAction::List { rule } => {
            let rows = crate::review::list_suppressions(&conn, rule.as_deref())?;
            if rows.is_empty() {
                if rule.is_some() {
                    output::print_hint("no suppressions for this rule", suppress);
                } else {
                    output::print_hint("no suppressions", suppress);
                }
                return Ok(());
            }
            for row in &rows {
                fmt.format_suppression(&output::SuppressionOutput::from(row))?;
            }
        }
        ReviewSuppressAction::Add {
            identity,
            rule,
            file,
            note,
        } => {
            crate::review::add_suppression(
                &conn,
                &identity,
                rule.as_deref().unwrap_or(""),
                file.as_deref().unwrap_or(""),
                note.as_deref(),
            )?;
            output::print_hint(&format!("suppressed {identity}"), suppress);
        }
        ReviewSuppressAction::Remove(args) => {
            let removed =
                crate::review::remove_suppressions(&conn, &args.identities, args.rule.as_deref())?;
            output::print_hint(&format!("removed {} suppression(s)", removed), suppress);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `wonk context` dispatch (TASK-073)
// ---------------------------------------------------------------------------

fn dispatch_context<W: io::Write>(
    args: ContextArgs,
    fmt: &mut Formatter<W>,
    suppress: bool,
    include_tests: bool,
) -> Result<()> {
    let conn = match callgraph_conn(suppress) {
        Some(c) => c,
        None => return Ok(()),
    };

    // Support qualified paths: `Client.get` → name="get", scope="Client".
    let split = split_qualified_name(&args.name);
    let file = args.file.or(split.file_hint);

    let options = crate::context::ContextOptions {
        file,
        kind: args.kind,
        min_confidence: args.min_confidence,
        scope: split.scope_hint,
        elide: args.elide.map(Into::into),
    };

    let mut contexts = crate::context::symbol_context(&conn, split.name, &options)?;

    // TASK-094 keep: the include_tests user opt-out — an exclusion, never a ranking demotion (the graded path signal only orders).
    if !include_tests {
        contexts.retain(|c| !crate::ranker::is_test_file(Path::new(&c.file)));
    }

    if contexts.is_empty() {
        output::print_hint("no matching symbols found", suppress);
        return Ok(());
    }

    let outputs: Vec<output::SymbolContextOutput> = contexts
        .iter()
        .map(output::SymbolContextOutput::from)
        .collect();
    fmt.format_context(&outputs)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// `wonk contracts` dispatch (TASK-083, widened by TASK-084)
// ---------------------------------------------------------------------------

/// Filters for [`build_contracts_payload`] (`wonk contracts` CLI and the
/// `wonk_contracts` MCP tool share them verbatim).
#[derive(Debug, Clone, Default)]
pub(crate) struct ContractsQueryFilters {
    /// Restrict to one contract kind.
    pub kind: Option<crate::types::ContractKind>,
    /// Restrict to one role.
    pub role: Option<crate::types::ContractRole>,
    /// `--links`: list cross-repo pairs instead of stored rows.
    pub links: bool,
    /// `--orphans`: widen to consumers with no provider in the workspace.
    pub orphans: bool,
    /// `--unused-providers`: list unconsumed provider rows.
    pub unused_providers: bool,
}

/// Everything `wonk contracts` (and its MCP twin) can display, built in
/// one pass: the workspace context, the mode-selected rows, the per-row
/// consumer statuses, and the resolved cross-repo links.
#[derive(Debug, Clone)]
pub(crate) struct ContractsPayload {
    /// Declared/effective/stored workspaces and co-members.
    pub workspace: crate::contracts::WorkspaceStatus,
    /// Rows selected for the active display mode.
    pub rows: Vec<crate::contracts::ContractRow>,
    /// Consumer statuses for every own row, keyed `(canonical_id, file, line)`.
    pub status:
        std::collections::HashMap<(String, String, usize), crate::contracts::ConsumerStatus>,
    /// Resolved cross-repo pairs involving this repo.
    pub links: Vec<crate::contracts::CrossRepoLink>,
    /// Own provider rows with no consumer in repo or members.
    pub unused_providers: Vec<crate::contracts::ContractRow>,
}

impl ContractsPayload {
    /// Grep/NDJSON token for a row: `orphan`/`unscoped` on unmatched
    /// consumers only — linked consumers and providers stay 083-shaped.
    pub(crate) fn status_token(&self, row: &crate::contracts::ContractRow) -> Option<&'static str> {
        if row.role != crate::types::ContractRole::Consumer {
            return None;
        }
        match self
            .status
            .get(&(row.canonical_id.clone(), row.file.clone(), r_line(row)))
        {
            Some(crate::contracts::ConsumerStatus::Linked) | None => None,
            Some(st) => Some(st.as_str()),
        }
    }
}

/// Row line as the status-map key's third element.
fn r_line(row: &crate::contracts::ContractRow) -> usize {
    row.line
}

/// Build the contracts payload: load repo-local workspace config, run the
/// within-repo listing, then ALWAYS resolve the workspace (REQ-012 — no
/// members is a no-op map pass, never an error).
pub(crate) fn build_contracts_payload(
    conn: &Connection,
    repo_root: &std::path::Path,
    repos_dir: &std::path::Path,
    filters: &ContractsQueryFilters,
) -> Result<ContractsPayload> {
    let declared = crate::config::Config::load(Some(repo_root))?
        .contracts
        .workspace;
    let own_index = db::find_existing_index(repo_root)
        .ok_or_else(|| anyhow::anyhow!("no index found; run `wonk init` first"))?;
    let workspace = crate::contracts::workspace_status(repos_dir, repo_root, &own_index, &declared);

    // Within-repo pre-filter (TASK-083 behavior); `orphans` stays a
    // within-repo notion here — the workspace widening is layered on top.
    let query = crate::contracts::ContractQuery {
        kind: filters.kind,
        role: filters.role,
        orphans: filters.orphans,
    };
    let rows = crate::contracts::list_contracts(conn, &query)?;

    // Resolution over ALL rows: statuses and links must see the whole repo
    // even when the display mode filters the listing.
    let all_rows =
        crate::contracts::list_contracts(conn, &crate::contracts::ContractQuery::default())?;
    let resolution =
        crate::contracts::resolve_workspace(repo_root, &all_rows, &declared, repos_dir)?;

    let rows = if filters.orphans {
        rows.into_iter()
            .filter(|r| {
                r.role == crate::types::ContractRole::Consumer
                    && matches!(
                        resolution
                            .status
                            .get(&(r.canonical_id.clone(), r.file.clone(), r_line(r))),
                        Some(crate::contracts::ConsumerStatus::Orphan)
                            | Some(crate::contracts::ConsumerStatus::Unscoped)
                    )
            })
            .collect()
    } else {
        rows
    };

    let mut links = resolution.links.clone();
    if let Some(kind) = filters.kind {
        links.retain(|l| l.provider.kind == kind);
    }

    Ok(ContractsPayload {
        workspace,
        rows,
        status: resolution.status,
        links,
        unused_providers: resolution.unused_providers,
    })
}

/// Append the CrossRepo tier to a blast result when the target owns
/// provider contracts with sibling consumers (PRD-CTR-REQ-010).
fn append_cross_repo_blast_tier(
    conn: &Connection,
    symbol: &str,
    result: &mut crate::types::BlastAnalysis,
) -> Result<()> {
    let provider_ids = crate::blast::provider_contract_ids(conn, symbol)?;
    if provider_ids.is_empty() {
        return Ok(());
    }
    let repo_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| db::find_repo_root(&cwd).ok())
        .ok_or_else(|| anyhow::anyhow!("no repository root found"))?;
    let Some(repos_dir) = crate::contracts::default_repos_dir() else {
        return Ok(());
    };
    let declared = crate::config::Config::load(Some(&repo_root))
        .map(|c| c.contracts.workspace)
        .unwrap_or_default();
    let consumers = crate::blast::resolve_cross_repo_consumers(
        &repo_root,
        conn,
        &declared,
        &repos_dir,
        &provider_ids,
    )?;
    crate::blast::append_cross_repo_tier(result, consumers);
    Ok(())
}

/// Effective-workspace context hints, on stderr so stdout stays parseable.
fn print_workspace_hints(workspace: &crate::contracts::WorkspaceStatus, suppress: bool) {
    let comembers = if workspace.comembers.is_empty() {
        String::new()
    } else {
        format!(" (co-members: {})", workspace.comembers.join(", "))
    };
    if !workspace.declared.is_empty() {
        output::print_hint(
            &format!("workspace: {}{}", workspace.effective.join(", "), comembers),
            suppress,
        );
    } else {
        // AR-026: the exact line to add, so "undeclared" is actionable.
        output::print_hint(
            &format!(
                "workspace: {} (undeclared — add 'workspace = \"{}\"' under [contracts] in .wonk/config.toml to link sibling repos)",
                workspace.effective.join(", "),
                workspace.effective.first().cloned().unwrap_or_default()
            ),
            suppress,
        );
    }
    if workspace.stored_diverges {
        output::print_hint(
            "run wonk update to publish the declared workspace",
            suppress,
        );
    }
}

fn dispatch_contracts<W: io::Write>(
    args: crate::cli::ContractsArgs,
    fmt: &mut Formatter<W>,
    suppress: bool,
) -> Result<()> {
    // 1. Resolve repo root and open connection (callgraph_conn-style errors).
    let repo_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| db::find_repo_root(&cwd).ok())
        .ok_or_else(|| anyhow::anyhow!("no repository root found"))?;

    let conn = db::find_existing_index(&repo_root)
        .and_then(|path| db::open(&path).ok())
        .ok_or_else(|| anyhow::anyhow!("no index found; run `wonk init` first"))?;

    // 2. Parse raw filter strings into enums (BlastDirection precedent).
    let kind = match &args.kind {
        Some(k) => Some(
            k.parse::<crate::types::ContractKind>()
                .map_err(crate::errors::WonkError::Usage)?,
        ),
        None => None,
    };
    let role = match &args.role {
        Some(r) => Some(
            r.parse::<crate::types::ContractRole>()
                .map_err(crate::errors::WonkError::Usage)?,
        ),
        None => None,
    };

    // 3. Build the payload (listing + live workspace resolution).
    let repos_dir = crate::contracts::default_repos_dir().ok_or_else(|| {
        anyhow::anyhow!("no home directory; cannot resolve cross-repo workspaces")
    })?;
    let filters = ContractsQueryFilters {
        kind,
        role,
        links: args.links,
        orphans: args.orphans,
        unused_providers: args.unused_providers,
    };
    let payload = build_contracts_payload(&conn, &repo_root, &repos_dir, &filters)?;
    print_workspace_hints(&payload.workspace, suppress);

    // 4. Mode dispatch: --links rows, then flag-filtered rows, then the
    //    default 083 row set with status tokens on unmatched consumers.
    if filters.links {
        if payload.links.is_empty() {
            output::print_hint(
                "no cross-repo links; declare matching [contracts] workspace values and index the sibling repos",
                suppress,
            );
            return Ok(());
        }
        for link in &payload.links {
            fmt.format_contract_link(&output::LinkOutput::from(link))?;
        }
        return Ok(());
    }

    let rows = if filters.unused_providers {
        payload.unused_providers.clone()
    } else {
        payload.rows.clone()
    };

    if rows.is_empty() {
        if filters.orphans {
            output::print_hint("no orphan consumers in this workspace", suppress);
        } else if filters.unused_providers {
            output::print_hint("no unused providers in this workspace", suppress);
        } else {
            output::print_hint(
                "no contracts found; if this index predates contract storage, run `wonk update` to re-index",
                suppress,
            );
        }
        return Ok(());
    }

    for row in &rows {
        let mut out = output::ContractOutput::from(row);
        out.status = payload.status_token(row).map(str::to_string);
        fmt.format_contract(&out)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Feedback slate capture (TASK-101)
// ---------------------------------------------------------------------------

/// Record the feedback slate for a ranked search, best-effort (TASK-101,
/// DR-042): gated on `[feedback] enabled`, so a default-config search
/// writes nothing; a missing connection (grep fallback) is a no-op and a
/// write failure degrades with a stderr warning, never failing the
/// search — the `record_pairs_best_effort` contract at the same call
/// site. Returns the stored slate so rows can carry its token and
/// identities.
pub(crate) fn record_slate_best_effort(
    conn: Option<&Connection>,
    query: &str,
    ranked: &crate::rerank::RankedSearch,
    feedback: &crate::config::FeedbackConfig,
) -> Option<crate::feedback::StoredSlate> {
    // Disabled capture records nothing, and a search whose signal
    // pipeline did not run (`query_class` None, e.g. the legacy path)
    // would carry no contributions to learn from.
    if !feedback.enabled || ranked.query_class.is_none() {
        return None;
    }
    let conn = conn?;
    match crate::feedback::build_and_store_slate(conn, query, ranked, feedback) {
        Ok(stored) => Some(stored),
        Err(e) => {
            eprintln!("warn: could not record feedback slate: {e:#}");
            None
        }
    }
}

/// Handle `wonk duplicates` dispatch (TASK-100, PRD-DUP-REQ-006): resolve
/// the repo's index, pick the threshold (CLI override > `[duplicate]`
/// threshold > 0.85), and print the sweep's groups.
fn dispatch_duplicates<W: io::Write>(
    args: crate::cli::DuplicatesArgs,
    fmt: &mut Formatter<W>,
    suppress: bool,
) -> Result<()> {
    // 1. Resolve repo root and open connection (the contracts error path).
    let repo_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| db::find_repo_root(&cwd).ok())
        .ok_or_else(|| anyhow::anyhow!("no repository root found"))?;

    let conn = db::find_existing_index(&repo_root)
        .and_then(|path| db::open(&path).ok())
        .ok_or_else(|| anyhow::anyhow!("no index found; run `wonk init` first"))?;

    // 2. Threshold: CLI override > loaded [duplicate] threshold > default.
    let threshold = args.threshold.unwrap_or_else(|| {
        crate::config::Config::load(Some(&repo_root))
            .map(|config| config.duplicate.threshold)
            .unwrap_or(0.85)
    });
    if !threshold.is_finite() || threshold <= 0.0 || threshold > 1.0 {
        anyhow::bail!("--threshold must be finite and in (0, 1] (got {threshold})");
    }

    run_duplicates(&conn, threshold, fmt, suppress)
}

/// Handle `wonk feedback` dispatch (TASK-101, PRD-FB-REQ-003): resolve the
/// repo's index, enforce the `[feedback]` gate, and record.
fn dispatch_feedback<W: io::Write>(
    args: crate::cli::FeedbackArgs,
    fmt: &mut Formatter<W>,
    suppress: bool,
    format: OutputFormat,
) -> Result<()> {
    let repo_root = std::env::current_dir()
        .ok()
        .and_then(|cwd| db::find_repo_root(&cwd).ok())
        .ok_or_else(|| anyhow::anyhow!("no repository root found"))?;

    let config = crate::config::Config::load(Some(&repo_root))?;

    let conn = db::find_existing_index(&repo_root)
        .and_then(|path| db::open(&path).ok())
        .ok_or_else(|| anyhow::anyhow!("no index found; run `wonk init` first"))?;

    // Pre-TASK-101 indexes migrate instead of erroring (the
    // `ensure_summaries_table` precedent).
    db::ensure_feedback_tables(&conn)?;

    // The `[feedback] enabled` gate covers RECORDING only (TASK-103):
    // inspecting and wiping leftover state after opting out is exactly
    // when --weights/--list/--export/--reset-*/--clear-* matter, and
    // none of them writes to the recording path.
    let recording = !args.weights
        && !args.list
        && !args.export
        && !args.clear_events
        && !args.reset_weights
        && args.clear_result.is_none()
        && args.reset_weight.is_none();
    if recording && !config.feedback.enabled {
        anyhow::bail!(
            "feedback capture is disabled; set [feedback] enabled = true in .wonk/config.toml"
        );
    }

    run_feedback(&conn, &args, &config, fmt, suppress, format)
}

/// Record feedback and print the summary. Split from
/// [`dispatch_feedback`] so tests drive it with a seeded connection
/// instead of the process working directory.
fn run_feedback<W: io::Write>(
    conn: &Connection,
    args: &crate::cli::FeedbackArgs,
    config: &crate::config::Config,
    fmt: &mut Formatter<W>,
    _suppress: bool,
    format: OutputFormat,
) -> Result<()> {
    if args.weights {
        return run_feedback_weights(conn, config, fmt, format);
    }
    if args.list {
        return run_feedback_list(conn, fmt, format);
    }
    if args.export {
        return run_feedback_export(conn, fmt);
    }
    if args.clear_events {
        return run_feedback_clear_events(conn, fmt, format);
    }
    if let Some(identity) = args.clear_result.as_deref() {
        return run_feedback_clear_result(conn, identity, fmt, format);
    }
    if args.reset_weights {
        return run_feedback_reset_weights(conn, fmt, format);
    }
    if let Some(feature) = args.reset_weight.as_deref() {
        return run_feedback_reset_weight(conn, feature, fmt, format);
    }
    let summary = crate::feedback::record_feedback(
        conn,
        args.slate.as_deref().unwrap_or_default(),
        &args.useful,
        args.session.as_deref().unwrap_or_default(),
    )?;
    // Learning (TASK-102) runs synchronously in the dispatch, best-effort:
    // a failure warns, the events stay recorded, and the watermark stays
    // put so the next feedback call replays them.
    learn_pending_best_effort(conn, config);
    if format.is_structured() {
        let json = serde_json::to_string(&summary)?;
        writeln!(fmt.writer_mut(), "{json}")?;
        return Ok(());
    }
    let class = summary.query_class.as_deref().unwrap_or("unknown");
    writeln!(
        fmt.writer_mut(),
        "recorded {} event(s) against slate {} (query {:?}, class {})",
        summary.recorded,
        args.slate.as_deref().unwrap_or_default(),
        summary.query,
        class
    )?;
    for event in &summary.events {
        let symbol = event.symbol.as_deref().unwrap_or("-");
        writeln!(
            fmt.writer_mut(),
            "rank {}  {}:{}  {}  [useful]",
            event.rank,
            event.file,
            event.line,
            symbol
        )?;
        if !event.live {
            writeln!(
                fmt.writer_mut(),
                "note: {} no longer resolves in the index; the entry will not apply",
                event.identity
            )?;
        }
    }
    Ok(())
}

/// Wall-clock seconds since the epoch — the `created_at` precedent for
/// learning's injected clock.
fn system_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// Learn from pending events, best-effort (both dispatch surfaces):
/// a failure warns and leaves the recorded events for the next call.
pub(crate) fn learn_pending_best_effort(conn: &Connection, config: &crate::config::Config) {
    if let Err(e) =
        crate::learning::learn_pending(conn, &config.feedback, &config.rank.weights, system_secs())
    {
        eprintln!("wonk: feedback learning deferred: {e:#}");
    }
}

/// `wonk feedback --weights` (TASK-102, PRD-FB-REQ-029/012): every
/// learned row — gated and inert — with its default and supporting
/// counts, decay visible in the effective value (PRD-FB-REQ-011).
fn run_feedback_weights<W: io::Write>(
    conn: &Connection,
    config: &crate::config::Config,
    fmt: &mut Formatter<W>,
    format: OutputFormat,
) -> Result<()> {
    let rows =
        crate::learning::list_learned(conn, &config.feedback, &config.rank.weights, system_secs())?;
    if format.is_structured() {
        let outputs: Vec<output::LearnedWeightOutput> = rows
            .iter()
            .map(|row| output::LearnedWeightOutput {
                feature: row.feature.clone(),
                query_class: (!row.query_class.is_empty()).then(|| row.query_class.clone()),
                effective: row.effective,
                default: row.default,
                observations: row.observations,
                sessions: row.sessions,
                gated: row.gated,
            })
            .collect();
        let json = serde_json::to_string(&outputs)?;
        writeln!(fmt.writer_mut(), "{json}")?;
        return Ok(());
    }
    for row in &rows {
        let scope = if row.query_class.is_empty() {
            "overall"
        } else {
            row.query_class.as_str()
        };
        let mut line = format!(
            "{} [{}] {:.3} (default {:.3}) {} obs, {} sessions",
            row.feature, scope, row.effective, row.default, row.observations, row.sessions
        );
        if !row.gated {
            line.push_str(" [below gate]");
        }
        writeln!(fmt.writer_mut(), "{line}")?;
    }
    Ok(())
}

/// `wonk feedback --list` (TASK-103, PRD-FB-REQ-019): every recorded
/// event with its session, class, and read-time liveness. JSON emits
/// the [`crate::feedback::EventListing`] objects.
fn run_feedback_list<W: io::Write>(
    conn: &Connection,
    fmt: &mut Formatter<W>,
    format: OutputFormat,
) -> Result<()> {
    let events = crate::feedback::list_events(conn)?;
    if format.is_structured() {
        let json = serde_json::to_string(&events)?;
        writeln!(fmt.writer_mut(), "{json}")?;
        return Ok(());
    }
    for event in &events {
        let session = event.session.as_deref().unwrap_or("-");
        let class = event.query_class.as_deref().unwrap_or("-");
        let symbol = event.symbol.as_deref().unwrap_or("-");
        let mut line = format!(
            "#{} rank {}  {}:{}  {}  session {}  class {}",
            event.id, event.rank, event.file, event.line, symbol, session, class
        );
        if !event.live {
            line.push_str("  [retired]");
        }
        writeln!(fmt.writer_mut(), "{line}")?;
    }
    Ok(())
}

/// `wonk feedback --export` (TASK-103): the complete event store —
/// features payloads included — as one JSON array on stdout, verbatim
/// round-trippable (`> events.json` to save).
fn run_feedback_export<W: io::Write>(conn: &Connection, fmt: &mut Formatter<W>) -> Result<()> {
    let events = crate::feedback::load_events(conn)?;
    let json = serde_json::to_string(&events)?;
    writeln!(fmt.writer_mut(), "{json}")?;
    Ok(())
}

/// `wonk feedback --clear-events` (TASK-103, PRD-FB-REQ-013/019): wipe
/// the whole event store; learned weights are untouched.
fn run_feedback_clear_events<W: io::Write>(
    conn: &Connection,
    fmt: &mut Formatter<W>,
    format: OutputFormat,
) -> Result<()> {
    let cleared = crate::feedback::clear_events(conn)?;
    if format.is_structured() {
        let json = serde_json::json!({"cleared": cleared});
        writeln!(fmt.writer_mut(), "{json}")?;
        return Ok(());
    }
    writeln!(
        fmt.writer_mut(),
        "cleared {cleared} feedback event(s); learned weights untouched"
    )?;
    Ok(())
}

/// `wonk feedback --clear-result <IDENTITY>` (TASK-103): wipe one
/// result's events; learned weights are untouched.
fn run_feedback_clear_result<W: io::Write>(
    conn: &Connection,
    identity: &str,
    fmt: &mut Formatter<W>,
    format: OutputFormat,
) -> Result<()> {
    let cleared = crate::feedback::clear_result_events(conn, identity)?;
    if format.is_structured() {
        let json = serde_json::json!({"cleared": cleared, "identity": identity});
        writeln!(fmt.writer_mut(), "{json}")?;
        return Ok(());
    }
    writeln!(
        fmt.writer_mut(),
        "cleared {cleared} feedback event(s) for {identity}; learned weights untouched"
    )?;
    Ok(())
}

/// `wonk feedback --reset-weights` (TASK-103, PRD-FB-REQ-013): every
/// learned weight back to its configured default, all scopes; TASK-104's
/// per-result preferences clear with them (learned state resets
/// together). The event history is untouched.
fn run_feedback_reset_weights<W: io::Write>(
    conn: &Connection,
    fmt: &mut Formatter<W>,
    format: OutputFormat,
) -> Result<()> {
    let (weights, preferences) = crate::learning::reset_learned_weights(conn)?;
    if format.is_structured() {
        let json = serde_json::json!({"reset": weights, "preferences": preferences});
        writeln!(fmt.writer_mut(), "{json}")?;
        return Ok(());
    }
    writeln!(
        fmt.writer_mut(),
        "reset {weights} learned weight row(s) to defaults; \
         {preferences} result preference(s) cleared; event history untouched"
    )?;
    Ok(())
}

/// `wonk feedback --reset-weight <FEATURE>` (TASK-103): one feature,
/// all of its scopes, back to defaults; the event history is untouched.
fn run_feedback_reset_weight<W: io::Write>(
    conn: &Connection,
    feature: &str,
    fmt: &mut Formatter<W>,
    format: OutputFormat,
) -> Result<()> {
    let reset = crate::learning::reset_learned_feature(conn, feature)?;
    if format.is_structured() {
        let json = serde_json::json!({"reset": reset, "feature": feature});
        writeln!(fmt.writer_mut(), "{json}")?;
        return Ok(());
    }
    writeln!(
        fmt.writer_mut(),
        "reset {reset} learned weight row(s) for {feature} to defaults; event history untouched"
    )?;
    Ok(())
}

/// Sweep and print the near-duplicate groups. Split from
/// [`dispatch_duplicates`] so tests drive it with a seeded connection
/// instead of the process working directory.
///
/// The duplicate tables are ensured here (the `ensure_summaries_table`
/// precedent) so a pre-TASK-100 index migrates instead of erroring: the
/// sweep then sees an empty signature table and reports no groups.
///
/// Text output only in this task — the grep-shaped lines are
/// machine-cuttable; JSON output is a follow-up.
fn run_duplicates<W: io::Write>(
    conn: &Connection,
    threshold: f32,
    fmt: &mut Formatter<W>,
    suppress: bool,
) -> Result<()> {
    crate::db::ensure_duplicate_tables(conn)?;
    let report = crate::shingles::sweep_near_duplicates(conn, threshold)?;
    if report.groups.is_empty() {
        output::print_hint(
            &format!("no near-duplicate groups above threshold {threshold:.2}"),
            suppress,
        );
        return Ok(());
    }
    if report.truncated_buckets > 0 {
        output::print_hint(
            &format!(
                "{} oversized buckets truncated to {} members; more duplicates may exist",
                report.truncated_buckets,
                crate::shingles::MAX_BUCKET_MEMBERS
            ),
            suppress,
        );
    }
    if report.truncated_groups > 0 {
        output::print_hint(
            &format!(
                "{} duplicate groups capped at {} recorded pairs (strongest first); \
                 run `wonk init` to refresh signatures",
                report.truncated_groups,
                crate::shingles::MAX_PAIRS_PER_GROUP
            ),
            suppress,
        );
    }
    for (n, group) in report.groups.iter().enumerate() {
        writeln!(
            fmt.writer_mut(),
            "dup-group {} size={} mean-sim={:.2}",
            n + 1,
            group.members.len(),
            group.mean_similarity
        )?;
        for member in &group.members {
            writeln!(
                fmt.writer_mut(),
                "  {}:{} {} {}",
                member.file,
                member.line,
                member.kind,
                member.name
            )?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared changes output builder (used by CLI dispatch and MCP tool)
// ---------------------------------------------------------------------------

/// Options for blast/flow chaining in `build_changes_output`.
pub(crate) struct ChangesChainOptions {
    pub blast: bool,
    pub flows: bool,
    pub min_confidence: Option<f64>,
    pub reach_enabled: bool,
}

/// Build a [`ChangesOutput`] from a [`ChangeAnalysis`], optionally chaining
/// blast radius and flow detection. `warn_fn` is called for non-fatal blast
/// errors (CLI prints to stderr; MCP can ignore).
pub(crate) fn build_changes_output(
    conn: &rusqlite::Connection,
    analysis: &crate::types::ChangeAnalysis,
    scope: &crate::types::ChangeScope,
    opts: &ChangesChainOptions,
    mut warn_fn: impl FnMut(&str),
) -> Result<ChangesOutput> {
    use crate::types::{BlastRiskLevel, ChangeType};

    // Build ChangedSymbolOutput vec, optionally with blast radius.
    let mut combined_risk: Option<BlastRiskLevel> = None;
    let mut changed_outputs: Vec<ChangedSymbolOutput> = Vec::new();

    for cs in &analysis.changed_symbols {
        let blast_out = if opts.blast && cs.change_type != ChangeType::Removed {
            let blast_opts = crate::blast::BlastOptions {
                depth: crate::blast::DEFAULT_DEPTH,
                direction: crate::types::BlastDirection::Upstream,
                include_tests: false,
                min_confidence: opts.min_confidence,
                use_reach: opts.reach_enabled,
            };
            match crate::blast::analyze_blast(conn, &cs.name, &blast_opts) {
                Ok(ref result) => {
                    combined_risk = Some(
                        combined_risk
                            .map_or(result.risk_level, |c| std::cmp::max(c, result.risk_level)),
                    );
                    Some(BlastOutput::from(result))
                }
                Err(e) => {
                    warn_fn(&format!("blast analysis failed for {}: {e}", cs.name));
                    None
                }
            }
        } else {
            None
        };

        changed_outputs.push(ChangedSymbolOutput {
            name: cs.name.clone(),
            kind: cs.kind.to_string(),
            file: cs.file.clone(),
            line: cs.line,
            change_type: cs.change_type.to_string(),
            blast_radius: blast_out,
        });
    }

    // Detect affected execution flows.
    let affected_flows = if opts.flows && !analysis.changed_symbols.is_empty() {
        let flow_opts = crate::flows::FlowOptions {
            depth: crate::flows::DEFAULT_DEPTH,
            branching: crate::flows::DEFAULT_BRANCHING,
            min_confidence: opts.min_confidence,
            from_file: None,
        };

        let changed_names: std::collections::HashSet<&str> = analysis
            .changed_symbols
            .iter()
            .map(|cs| cs.name.as_str())
            .collect();

        let entries = crate::flows::detect_entry_points(conn, &flow_opts)?;
        let mut matched_flows: Vec<AffectedFlowOutput> = Vec::new();

        for entry in &entries {
            if let Some(flow) = crate::flows::trace_flow(conn, &entry.name, &flow_opts)? {
                // Check entry point AND steps for changed symbol matches.
                let matched: Vec<String> = std::iter::once(&flow.entry_point)
                    .chain(flow.steps.iter())
                    .filter(|step| changed_names.contains(step.name.as_str()))
                    .map(|step| step.name.clone())
                    .collect();

                if !matched.is_empty() {
                    let steps: Vec<output::FlowStepOutput> = flow
                        .steps
                        .iter()
                        .map(output::FlowStepOutput::from)
                        .collect();
                    matched_flows.push(AffectedFlowOutput {
                        entry_point: output::FlowStepOutput::from(&flow.entry_point),
                        step_count: steps.len(),
                        steps,
                        matched_symbols: matched,
                    });
                }
            }
        }

        if matched_flows.is_empty() {
            None
        } else {
            Some(matched_flows)
        }
    } else {
        None
    };

    Ok(ChangesOutput {
        scope: scope.to_string(),
        changed_symbols: changed_outputs,
        combined_risk_level: combined_risk.map(|r| r.to_string()),
        affected_flows,
    })
}

/// Open a call graph connection: resolve repo root, open index, check
/// caller_id data. Returns `None` when an early-exit error/hint was emitted.
fn callgraph_conn(suppress: bool) -> Option<Connection> {
    let repo_root = match std::env::current_dir()
        .ok()
        .and_then(|cwd| db::find_repo_root(&cwd).ok())
    {
        Some(r) => r,
        None => {
            output::print_error("no repository root found");
            return None;
        }
    };

    let conn = match db::find_existing_index(&repo_root).and_then(|path| db::open(&path).ok()) {
        Some(c) => c,
        None => {
            output::print_error("no index found; run `wonk init` to build the index");
            return None;
        }
    };

    if !crate::callgraph::has_caller_id_data(&conn) {
        output::print_hint(
            "index lacks call graph data; run `wonk update` to re-index",
            suppress,
        );
        return None;
    }

    Some(conn)
}

/// Shared setup for `Command::Callers` and `Command::Callees`: open connection
/// and clamp depth. Returns `None` when an early-exit error/hint was emitted.
fn callgraph_setup(requested_depth: usize, suppress: bool) -> Option<(Connection, usize)> {
    let conn = callgraph_conn(suppress)?;

    let (depth, clamped) = crate::callgraph::clamp_depth(requested_depth);
    if clamped {
        output::print_hint(
            &format!(
                "depth {} exceeds cap; using max depth {}",
                requested_depth,
                crate::callgraph::MAX_DEPTH_CAP
            ),
            suppress,
        );
    }

    Some((conn, depth))
}

/// Returns `true` if the string looks like a file path rather than a symbol name.
/// Detects paths containing `/` or ending with common code file extensions.
pub fn looks_like_file_path(s: &str) -> bool {
    if s.contains('/') {
        return true;
    }
    let lower = s.to_lowercase();
    [
        ".rs", ".py", ".js", ".ts", ".tsx", ".jsx", ".go", ".java", ".c", ".h", ".cpp", ".cc",
        ".hpp", ".rb", ".php", ".cs",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

/// Resolve a file path for a scoped symbol (e.g. `Client.get` → find the file
/// containing `get` with `scope = 'Client'`). Returns the file as a string
/// hint suitable for `reference_file` filtering.
fn resolve_file_for_scope(conn: &Connection, name: &str, scope: Option<&str>) -> Option<String> {
    let scope = scope?;
    let sql = "SELECT file FROM symbols WHERE name = ?1 AND scope = ?2 LIMIT 1";
    conn.query_row(sql, rusqlite::params![name, scope], |row| {
        row.get::<_, String>(0)
    })
    .ok()
}

/// Parsed result from a qualified name like `Foo::bar`, `Client.get`, or
/// `module::Class.method`.
pub struct QualifiedSplit<'a> {
    /// The bare symbol name (last segment).
    pub name: &'a str,
    /// File path hint from `::` segments (e.g. `tokio/runtime`).
    pub file_hint: Option<String>,
    /// Scope hint from `.` segments (e.g. `Client` for `Client.get`).
    pub scope_hint: Option<String>,
}

/// Split a qualified name into bare name, optional file hint, and optional
/// scope hint.
///
/// `::` separators produce file_hint (Rust module paths).
/// `.` separators produce scope_hint (class/scope, Python/JS-style).
/// Mixed paths like `module::Class.method` produce both.
///
/// Examples:
///   `tokio::runtime::Handle` → name `Handle`, file_hint `tokio/runtime`
///   `Client.get`             → name `get`, scope_hint `Client`
///   `foo.bar.baz`            → name `baz`, scope_hint `bar`
///   `module::Class.method`   → name `method`, file_hint `module`, scope_hint `Class`
pub fn split_qualified_name(name: &str) -> QualifiedSplit<'_> {
    // Try `::` first (Rust-style qualified paths).
    if let Some(pos) = name.rfind("::") {
        let prefix = &name[..pos];
        let bare = &name[pos + 2..];
        if bare.is_empty() {
            return QualifiedSplit {
                name,
                file_hint: None,
                scope_hint: None,
            };
        }
        let hint = camel_to_snake_hint(prefix);
        // Check if the bare part contains a dot (mixed: `module::Class.method`).
        if let Some(dot_pos) = bare.rfind('.') {
            let scope = &bare[..dot_pos];
            let method = &bare[dot_pos + 1..];
            if method.is_empty() {
                return QualifiedSplit {
                    name: bare,
                    file_hint: Some(hint),
                    scope_hint: None,
                };
            }
            return QualifiedSplit {
                name: method,
                file_hint: Some(hint),
                scope_hint: Some(scope.to_string()),
            };
        }
        return QualifiedSplit {
            name: bare,
            file_hint: Some(hint),
            scope_hint: None,
        };
    }
    // Try `.` next (Python/JS-style qualified paths like `Client.get`).
    if let Some(pos) = name.rfind('.') {
        let prefix = &name[..pos];
        let bare = &name[pos + 1..];
        if bare.is_empty() {
            return QualifiedSplit {
                name,
                file_hint: None,
                scope_hint: None,
            };
        }
        // The immediate parent is the scope hint. If there are multiple dots,
        // the immediate parent (last segment of prefix) is scope_hint.
        let scope_hint = if let Some(last_dot) = prefix.rfind('.') {
            prefix[last_dot + 1..].to_string()
        } else {
            prefix.to_string()
        };
        // For single-segment dot paths like `Client.get`, check if the prefix
        // looks like a class name (starts with uppercase) → scope_hint only.
        // For multi-segment like `foo.bar.baz`, the prefix before scope is file_hint.
        let file_hint = if prefix.contains('.') {
            // Multi-segment: everything before the last dot could be file hint.
            prefix
                .rfind('.')
                .map(|last_dot| prefix[..last_dot].replace('.', "/"))
        } else if prefix.chars().next().is_some_and(|c| c.is_uppercase()) {
            // Single uppercase prefix like `Client` → scope only, no file hint.
            None
        } else {
            // Single lowercase prefix like `foo` → ambiguous, treat as file hint
            // for backward compat (but also set scope).
            Some(prefix.to_string())
        };
        return QualifiedSplit {
            name: bare,
            file_hint,
            scope_hint: Some(scope_hint),
        };
    }
    QualifiedSplit {
        name,
        file_hint: None,
        scope_hint: None,
    }
}

/// Convert a qualified path prefix into a file hint.
///
/// If the prefix looks like a CamelCase type name (starts uppercase, no `/`),
/// convert it to snake_case so `ScheduledIo` → `scheduled_io`.
/// Otherwise replace `::` with `/` as before.
fn camel_to_snake_hint(prefix: &str) -> String {
    let segments: Vec<&str> = prefix.split("::").collect();
    segments
        .iter()
        .map(|seg| {
            if seg.chars().next().is_some_and(|c| c.is_uppercase()) && !seg.contains('/') {
                camel_to_snake(seg)
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Convert a CamelCase string to snake_case.
fn camel_to_snake(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                // Don't insert underscore between consecutive uppercase letters
                // unless the next char is lowercase (e.g. "IO" stays "io", "IOHandler" → "io_handler").
                let prev_upper = s.chars().nth(i - 1).is_some_and(|c| c.is_uppercase());
                let next_lower = s.chars().nth(i + 1).is_some_and(|c| c.is_lowercase());
                if !prev_upper || next_lower {
                    result.push('_');
                }
            }
            result.push(ch.to_lowercase().next().unwrap());
        } else {
            result.push(ch);
        }
    }
    result
}

fn is_query_command(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::Search(_)
            | Command::Sym(_)
            | Command::Ref(_)
            | Command::Sig(_)
            | Command::Deps(_)
            | Command::Rdeps(_)
            | Command::Ask(_)
            | Command::Cluster(_)
            | Command::Impact(_)
            | Command::Callers(_)
            | Command::Callees(_)
            | Command::Callpath(_)
            | Command::Summary(_)
            | Command::Flows(_)
            | Command::Blast(_)
            | Command::Changes(_)
            | Command::Context(_)
            | Command::Contracts(_)
            | Command::Duplicates(_)
    )
}

// ---------------------------------------------------------------------------
// Semantic blending helpers
// ---------------------------------------------------------------------------

/// Fetch semantic search results without formatting them.
///
/// Returns the resolved semantic results, or an empty Vec on graceful
/// degradation (no DB, no embeddings, provider died mid-query).
fn fetch_semantic_results(
    pattern: &str,
    conn: Option<&Connection>,
    configured: crate::embedding::EmbeddingProviderKind,
    suppress: bool,
) -> Result<Vec<crate::types::SemanticResult>> {
    let conn = match conn {
        Some(c) => c,
        None => {
            output::print_hint("semantic blending skipped: no index available", suppress);
            return Ok(Vec::new());
        }
    };

    // Resolve the query provider against the stored spaces: unreachable
    // configured Ollama degrades to bundled with a warning; a mismatched
    // stored space blocks with a re-embed command.
    let plan = crate::embedding::plan_query_provider(conn, configured)?;
    if let Some(warning) = plan.fallback_warning {
        output::print_warning(warning);
    }
    let provider = plan.provider;

    let all_embeddings = match crate::embedding::load_all_embeddings(conn, provider.as_ref()) {
        Ok(e) if !e.is_empty() => e,
        Ok(_) => {
            output::print_hint(
                "semantic blending skipped: no embeddings available (run `wonk init`)",
                suppress,
            );
            return Ok(Vec::new());
        }
        Err(error @ crate::errors::EmbeddingError::VectorSpaceMismatch { .. }) => {
            return Err(error.into());
        }
        Err(_) => {
            output::print_hint(
                "semantic blending skipped: failed to load embeddings",
                suppress,
            );
            return Ok(Vec::new());
        }
    };

    let mut query_vec = match provider.embed_single(pattern) {
        Ok(v) => v,
        Err(crate::errors::EmbeddingError::OllamaUnreachable) => {
            // Mid-query disconnect: degrade when the stored space allows it,
            // otherwise surface the re-embed instruction.
            let fallback = crate::embedding::fallback_after_disconnect(conn, configured)?;
            output::print_warning(
                fallback
                    .fallback_warning
                    .unwrap_or(crate::embedding::BUNDLED_FALLBACK_WARNING),
            );
            fallback.provider.embed_single(pattern)?
        }
        Err(e) => return Err(e.into()),
    };
    crate::embedding::normalize(&mut query_vec);

    let scored = crate::semantic::semantic_search(&query_vec, &all_embeddings, 20);
    let resolved = crate::semantic::resolve_results(conn, &scored)?;

    Ok(resolved)
}

/// Emit a budget summary if any results were truncated.
///
/// In grep mode, prints the summary to stderr. In structured mode (JSON/TOON),
/// emits a truncation metadata line to the formatter.
fn emit_budget_summary_with_page<W: io::Write>(
    fmt: &mut Formatter<W>,
    truncated: usize,
    budget_limit: Option<usize>,
    format: OutputFormat,
    page: Option<usize>,
) -> Result<()> {
    if truncated == 0 && page.is_none() {
        return Ok(());
    }
    if let Some(limit) = budget_limit {
        let has_more = truncated > 0;
        if format.is_structured() {
            let meta = output::TruncationMeta {
                truncated_count: truncated,
                budget_tokens: limit,
                used_tokens: fmt.budget_used(),
                page,
                has_more,
            };
            fmt.format_truncation_meta(&meta)?;
        } else if has_more {
            if let Some(p) = page {
                output::print_budget_summary_with_page(truncated, limit, p);
            } else {
                output::print_budget_summary(truncated, limit);
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Structured status information for `wonk status`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StatusInfo {
    pub indexed: bool,
    pub file_count: i64,
    pub symbol_count: i64,
    pub reference_count: i64,
    pub embedding_count: usize,
    pub stale_embedding_count: usize,
    /// The provider a semantic query would use (`bundled` or `ollama`).
    pub active_provider: String,
    /// Dominant stored vector space; `None` when no embeddings exist.
    pub stored_vector_provider: Option<String>,
    pub stored_vector_dim: Option<usize>,
    /// Ollama reachability, probed only when Ollama is relevant (configured
    /// provider or stored ollama rows). `None` means "not probed".
    pub ollama_reachable: Option<bool>,
    /// Effective workspace ids (declared, or the repo's own name) —
    /// TASK-084, REQ-021. Empty when no repo/index context exists.
    pub workspaces: Vec<String>,
    /// Whether `[contracts] workspace` is declared in repo-local config.
    pub workspace_declared: bool,
    /// Names of other indexed repos sharing a workspace (AR-027).
    pub workspace_comembers: Vec<String>,
    /// Graph-topology scoring state (TASK-098): the staleness marker is
    /// visible here because a stale score is served, never awaited on
    /// (PRD-TOPO-REQ-007).
    pub topology: TopologyStatus,
    /// The feedback loop's state (TASK-103): event count, distinct
    /// sessions, and the current learned-weight deviation — legible
    /// even with the feature off, because inert-but-legible is the
    /// inspection contract.
    pub feedback: FeedbackStatus,
}

/// The feedback-loop state `wonk status` reports (TASK-103).
#[derive(Debug, Clone, serde::Serialize, PartialEq)]
pub struct FeedbackStatus {
    /// Whether `[feedback] enabled` is on.
    pub enabled: bool,
    /// Recorded feedback events, whole store.
    pub events: i64,
    /// Distinct sessions among them (NULL counts as one).
    pub sessions: i64,
    /// The largest `|effective − default|` over gated learned rows —
    /// the same value `wonk feedback --weights` displays per row.
    /// `learn_max_deviation` is its ceiling.
    pub deviation: f32,
    /// Stored per-result preferences (TASK-104), whole store — gated and
    /// inert alike, like the event count.
    pub preferences: i64,
}

/// Compute the feedback state (TASK-103) with an injected clock — the
/// repo's testability pattern. Stored state is consulted even when
/// `[feedback] enabled = false`: turning the feature off must not hide
/// the history it left behind.
pub(crate) fn feedback_status(
    conn: Option<&Connection>,
    feedback: &crate::config::FeedbackConfig,
    weights: &std::collections::HashMap<String, f32>,
    now: i64,
) -> FeedbackStatus {
    let (events, sessions, deviation, preferences) = match conn {
        Some(conn) => {
            let stats = crate::feedback::event_store_stats(conn).unwrap_or((0, 0));
            let deviation =
                crate::learning::current_deviation(conn, feedback, weights, now).unwrap_or(0.0);
            let preferences = crate::learning::preference_count(conn);
            (stats.0, stats.1, deviation, preferences)
        }
        None => (0, 0, 0.0, 0),
    };
    FeedbackStatus {
        enabled: feedback.enabled,
        events,
        sessions,
        deviation,
        preferences,
    }
}

/// The topology pass's state as `wonk status` reports it (TASK-098).
#[derive(Debug, Clone, serde::Serialize)]
pub struct TopologyStatus {
    /// Symbols carrying hub/authority scores.
    pub scored: i64,
    /// Distinct connectivity communities among the scored symbols
    /// (TASK-099); NULL communities — a TASK-098-scored index — count
    /// toward none, so a pre-upgrade index reports 0.
    pub communities: i64,
    /// Epoch seconds of the last recompute; `None` when never run.
    pub last_computed: Option<i64>,
    /// Whether the stored scores are older than `[topology] stale_after`.
    pub stale: bool,
    /// Whether the `[topology]` pass is enabled.
    pub enabled: bool,
}

/// Format status info as a human-readable string for stderr output.
pub fn format_status_info(info: &StatusInfo) -> String {
    if !info.indexed {
        return "No index found. Run `wonk init` to build one.".to_string();
    }

    let mut lines = Vec::new();
    lines.push(format!(
        "Index: {} files, {} symbols, {} references",
        info.file_count, info.symbol_count, info.reference_count
    ));

    if !info.workspaces.is_empty() {
        let mut ws = format!("Workspaces: {}", info.workspaces.join(", "));
        if !info.workspace_declared {
            ws.push_str(" (undeclared)");
        }
        if !info.workspace_comembers.is_empty() {
            ws.push_str(&format!(
                " (co-members: {})",
                info.workspace_comembers.join(", ")
            ));
        }
        lines.push(ws);
    }

    if info.embedding_count > 0 {
        let mut emb_line = format!("Embeddings: {} embeddings", info.embedding_count);
        if info.stale_embedding_count > 0 {
            emb_line.push_str(&format!(" ({} stale)", info.stale_embedding_count));
        }
        lines.push(emb_line);
    } else {
        lines.push("Embeddings: none".to_string());
    }

    lines.push(topology_status_line(&info.topology));

    lines.push(feedback_status_line(&info.feedback));

    lines.push(format!("Provider: {}", info.active_provider));

    match (&info.stored_vector_provider, info.stored_vector_dim) {
        (Some(provider), Some(dim)) => {
            lines.push(format!("Stored vectors: {provider}, {dim}-dim"));
        }
        _ => lines.push("Stored vectors: none".to_string()),
    }

    if let Some(reachable) = info.ollama_reachable {
        if reachable {
            lines.push("Ollama: reachable".to_string());
        } else if info.active_provider == "ollama" {
            lines.push(
                "Ollama: unreachable — semantic queries fall back to the bundled provider"
                    .to_string(),
            );
        } else {
            lines.push("Ollama: unreachable".to_string());
        }
    }

    lines.join("\n")
}

/// The `Topology:` line of `wonk status` (TASK-098): one of `disabled`,
/// `none` (never scored), `N symbols scored`, or — when the scores are
/// past `[topology] stale_after` — the same count with the staleness
/// marker and its age, so the user knows to expect drift, not silence
/// (PRD-TOPO-REQ-007).
fn topology_status_line(status: &TopologyStatus) -> String {
    if !status.enabled {
        return "Topology: disabled".to_string();
    }
    if status.scored == 0 {
        return "Topology: none".to_string();
    }
    let mut line = format!(
        "Topology: {} symbols scored ({} communities)",
        status.scored, status.communities
    );
    if status.stale {
        let age = status
            .last_computed
            .map(|stamp| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
                    .saturating_sub(stamp)
            })
            .unwrap_or(0);
        line.push_str(&format!(" (stale, computed {age}s ago)"));
    }
    line
}

/// The `Feedback:` line of `wonk status` (TASK-103): the loop's state at
/// a glance. Enabled always shows the full counts; disabled shows them
/// only when leftover state exists — the opt-out hides influence, not
/// history.
fn feedback_status_line(status: &FeedbackStatus) -> String {
    let state = if status.enabled {
        "enabled"
    } else {
        "disabled"
    };
    let has_state = status.events > 0 || status.deviation != 0.0 || status.preferences > 0;
    if !status.enabled && !has_state {
        return "Feedback: disabled".to_string();
    }
    format!(
        "Feedback: {state}, {} events, {} sessions, weight deviation {:.3}, \
         {} result preferences",
        status.events, status.sessions, status.deviation, status.preferences
    )
}

/// Query status from the database and the embedding-provider state.
///
/// Ollama is probed (quick 500 ms check) only when it is relevant — the
/// configured provider is Ollama or stored vectors include ollama rows — so
/// bundled-only users pay no network round trip.
pub fn query_status_info(
    conn: Option<&Connection>,
    configured: crate::embedding::EmbeddingProviderKind,
    workspace: Option<crate::contracts::WorkspaceStatus>,
    topology_config: &crate::config::TopologyConfig,
    feedback_config: &crate::config::FeedbackConfig,
    rank_weights: &std::collections::HashMap<String, f32>,
) -> StatusInfo {
    let (workspaces, workspace_declared, workspace_comembers) = match &workspace {
        Some(ws) => (
            ws.effective.clone(),
            !ws.declared.is_empty(),
            ws.comembers.clone(),
        ),
        None => (Vec::new(), false, Vec::new()),
    };
    let active_provider = match configured {
        crate::embedding::EmbeddingProviderKind::Bundled => "bundled",
        crate::embedding::EmbeddingProviderKind::Ollama => "ollama",
    };

    let Some(conn) = conn else {
        return StatusInfo {
            indexed: false,
            file_count: 0,
            symbol_count: 0,
            reference_count: 0,
            embedding_count: 0,
            stale_embedding_count: 0,
            active_provider: active_provider.to_string(),
            stored_vector_provider: None,
            stored_vector_dim: None,
            ollama_reachable: (configured == crate::embedding::EmbeddingProviderKind::Ollama)
                .then(|| crate::embedding::OllamaProvider::new().is_healthy_quick()),
            workspaces,
            workspace_declared,
            workspace_comembers,
            topology: TopologyStatus {
                scored: 0,
                communities: 0,
                last_computed: None,
                stale: false,
                enabled: topology_config.enabled,
            },
            feedback: feedback_status(None, feedback_config, rank_weights, system_secs()),
        };
    };

    let file_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
        .unwrap_or(0);
    let symbol_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))
        .unwrap_or(0);
    let reference_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM \"references\"", [], |row| row.get(0))
        .unwrap_or(0);
    let (embedding_count, stale_embedding_count) =
        crate::embedding::embedding_stats(conn).unwrap_or((0, 0));

    let stored = crate::embedding::stored_vector_spaces(conn).unwrap_or_default();
    let dominant = stored.first();
    let stored_ollama = stored
        .iter()
        .any(|s| s.provider == "ollama" && s.dim == crate::embedding::OLLAMA_DIM);
    let probe_ollama =
        configured == crate::embedding::EmbeddingProviderKind::Ollama || stored_ollama;

    let (topology_scored, topology_communities) = conn
        .query_row(
            "SELECT COUNT(*), COUNT(DISTINCT community) FROM symbol_topology",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .unwrap_or((0, 0));
    let topology = TopologyStatus {
        scored: topology_scored,
        communities: topology_communities,
        last_computed: crate::topology::last_computed(conn),
        stale: crate::topology::is_stale(conn, topology_config.stale_after),
        enabled: topology_config.enabled,
    };

    StatusInfo {
        indexed: true,
        file_count,
        symbol_count,
        reference_count,
        embedding_count,
        stale_embedding_count,
        active_provider: active_provider.to_string(),
        stored_vector_provider: dominant.map(|s| s.provider.clone()),
        stored_vector_dim: dominant.map(|s| s.dim),
        ollama_reachable: probe_ollama
            .then(|| crate::embedding::OllamaProvider::new().is_healthy_quick()),
        workspaces,
        workspace_declared,
        workspace_comembers,
        topology,
        feedback: feedback_status(Some(conn), feedback_config, rank_weights, system_secs()),
    }
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

/// Handle `wonk daemon list` dispatch.
///
/// Prints a table of running daemons (grep mode) or JSON array (structured).
fn dispatch_daemon_list<W: io::Write>(
    fmt: &mut Formatter<W>,
    daemons: &[crate::daemon::DaemonEntry],
    format: OutputFormat,
) -> Result<()> {
    if format.is_structured() {
        // JSON / TOON: emit as a JSON array.
        let json = serde_json::to_string(&daemons)?;
        writeln!(fmt.writer_mut(), "{json}")?;
    } else {
        // Grep mode: table format.
        let header = format!(
            "{:<10} {:<40} {:<12} {}",
            "PID", "REPO PATH", "UPTIME", "STATUS"
        );
        writeln!(fmt.writer_mut(), "{header}")?;
        for entry in daemons {
            let status = if entry.alive { "running" } else { "dead" };
            let line = format!(
                "{:<10} {:<40} {:<12} {}",
                entry.pid, entry.repo_path, entry.uptime, status
            );
            writeln!(fmt.writer_mut(), "{line}")?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Heuristic grep patterns
// ---------------------------------------------------------------------------

/// Build a regex pattern to find symbol definitions via grep.
///
/// Covers all 11 supported languages:
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
///   C#:         `class`, `struct`, `interface`, `enum`, `delegate`
pub fn symbol_grep_pattern(name: &str) -> String {
    // Use word boundary around the name to reduce false positives.
    format!(
        r"(fn|pub\s+fn|pub\(crate\)\s+fn|def|function|func|class|struct|enum|trait|interface|module|type|const|let|var|val|delegate)\s+{}\b",
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
        "type_alias" => "type|delegate",
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
/// Covers all 11 supported languages:
///   Rust:       `use ... name`
///   Python:     `import name`, `from ... import name`
///   Ruby:       `require ... name`
///   JavaScript: `import ... name`, `require(... name ...)`
///   TypeScript: `import ... name`
///   Go:         `import ... name`
///   Java:       `import ... name`
///   C/C++:      `#include ... name`
///   PHP:        `use ... name`, `require ... name`, `include ... name`
///   C#:         `using ... name`
pub fn import_grep_pattern(name: &str) -> String {
    format!(
        r"(import|from|require|use|using|include)\s+.*{}",
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

    /// Returns a reference to the underlying database connection, if available.
    pub fn conn(&self) -> Option<&Connection> {
        self.conn.as_ref()
    }

    /// Returns a reference to the repository root path.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Re-open the database connection for the current repo root.
    /// Used after rebuilding the index to pick up the new data.
    pub fn refresh_connection(&mut self) {
        self.conn = db::index_path_for(&self.repo_root, false)
            .ok()
            .filter(|p| p.exists())
            .and_then(|p| db::open_existing(&p).ok());
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
        self.query_symbols_with_file(name, kind, None, exact)
    }

    pub fn query_symbols_with_file(
        &self,
        name: &str,
        kind: Option<&str>,
        file: Option<&str>,
        exact: bool,
    ) -> Result<Vec<Symbol>, DbError> {
        // Try SQLite first.
        if let Some(conn) = &self.conn {
            let results = query_symbols_db_with_file(conn, name, kind, file, exact)?;
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
                    doc_comment: None,
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
            let mut results = query_references_db(conn, name)?;
            if !results.is_empty() {
                if !paths.is_empty() {
                    results.retain(|r| paths.iter().any(|p| r.file.starts_with(p)));
                }
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
                    caller_name: None,
                    confidence: 0.5,
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
                    doc_comment: None,
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
                    doc_comment: None,
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
pub fn query_symbols_db(
    conn: &Connection,
    name: &str,
    kind: Option<&str>,
    exact: bool,
) -> Result<Vec<Symbol>, DbError> {
    query_symbols_db_with_file(conn, name, kind, None, exact)
}

pub fn query_symbols_db_with_file(
    conn: &Connection,
    name: &str,
    kind: Option<&str>,
    file: Option<&str>,
    exact: bool,
) -> Result<Vec<Symbol>, DbError> {
    query_symbols_db_with_filters(conn, name, kind, file, None, exact)
}

pub fn query_symbols_db_with_filters(
    conn: &Connection,
    name: &str,
    kind: Option<&str>,
    file: Option<&str>,
    scope: Option<&str>,
    exact: bool,
) -> Result<Vec<Symbol>, DbError> {
    let mut sql = String::from(
        "SELECT name, kind, file, line, col, end_line, scope, signature, language FROM symbols WHERE ",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if exact {
        sql.push_str("name = ?");
        params.push(Box::new(name.to_string()));
    } else {
        sql.push_str("name LIKE ?");
        params.push(Box::new(format!("%{}%", name)));
    }

    if let Some(k) = kind {
        sql.push_str(" AND kind = ?");
        params.push(Box::new(k.to_string()));
    }

    if let Some(f) = file {
        sql.push_str(" AND file LIKE ?");
        params.push(Box::new(format!("%{}%", f)));
    }

    if let Some(s) = scope {
        sql.push_str(" AND scope = ?");
        params.push(Box::new(s.to_string()));
    }

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(param_refs), row_to_symbol)?;

    let mut results: Vec<Symbol> = Vec::new();
    for row in rows {
        results.push(row?);
    }

    // TASK-094: the ONE graded path-character demotion (absorbing the old
    // local test-path heuristic): ordinary source files sort before
    // barrels/module entries, type declarations, shims, examples, and
    // tests. Stable, so equal ladder values keep the row order; generated
    // files demote only with an index-verified hand-written peer.
    let files: Vec<String> = results.iter().map(|s| s.file.clone()).collect();
    let values = crate::rerank::path_character_values(&files, Some(conn));
    results.sort_by(|a, b| {
        let a_value = values.get(&a.file).copied().unwrap_or(1.0);
        let b_value = values.get(&b.file).copied().unwrap_or(1.0);
        b_value
            .partial_cmp(&a_value)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(results)
}

/// Query references from the SQLite index.
pub fn query_references_db(conn: &Connection, name: &str) -> Result<Vec<Reference>, DbError> {
    let sql = "SELECT r.name, r.file, r.line, r.col, r.context, s.name, r.confidence \
               FROM \"references\" r \
               LEFT JOIN symbols s ON r.caller_id = s.id \
               WHERE r.name = ?1";
    let mut stmt = conn.prepare_cached(sql)?;

    let rows = stmt.query_map(rusqlite::params![name], |row| {
        let line: i64 = row.get(2)?;
        let col: i64 = row.get(3)?;
        Ok(Reference {
            name: row.get(0)?,
            kind: ReferenceKind::Call,
            file: row.get(1)?,
            line: line as usize,
            col: col as usize,
            context: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            caller_name: row.get(5)?,
            confidence: row.get::<_, Option<f64>>(6)?.unwrap_or(0.5),
        })
    })?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Query subclasses/implementors of a symbol via the type_edges table.
pub fn query_subclasses_db(conn: &Connection, name: &str) -> Result<Vec<Symbol>, DbError> {
    let sql = "SELECT s.name, s.kind, s.file, s.line, s.col, s.end_line, s.scope, s.signature, s.language \
               FROM type_edges te \
               JOIN symbols parent ON te.parent_id = parent.id \
               JOIN symbols s ON te.child_id = s.id \
               WHERE parent.name LIKE ?1 \
               ORDER BY s.file, s.line";
    let name_param = format!("%{}%", name);
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params![name_param], row_to_symbol)?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Query function/method signatures from the SQLite index.
pub fn query_signatures_db(conn: &Connection, name: &str) -> Result<Vec<Symbol>, DbError> {
    let sql = "SELECT name, kind, file, line, col, end_line, scope, signature, language \
               FROM symbols WHERE name LIKE ?1 AND kind IN ('function', 'method')";
    let name_param = format!("%{}%", name);
    let mut stmt = conn.prepare_cached(sql)?;

    let rows = stmt.query_map(rusqlite::params![name_param], row_to_symbol)?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row?);
    }
    Ok(results)
}

/// Query all symbols in a specific file from the SQLite index.
pub fn query_symbols_in_file_db(conn: &Connection, path: &str) -> Result<Vec<Symbol>, DbError> {
    let sql = "SELECT name, kind, file, line, col, end_line, scope, signature, language \
               FROM symbols WHERE file = ?1 ORDER BY line";
    let mut stmt = conn.prepare_cached(sql)?;

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
pub fn query_deps_db(conn: &Connection, file: &str) -> Result<Vec<String>, DbError> {
    let sql = "SELECT DISTINCT import_path FROM file_imports WHERE source_file = ?1";
    let mut stmt = conn.prepare_cached(sql)?;

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
pub fn query_rdeps_db(conn: &Connection, file: &str) -> Result<Vec<String>, DbError> {
    let stem = Path::new(file)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| file.to_string());

    let sql = "SELECT DISTINCT source_file FROM file_imports \
               WHERE import_path LIKE ?1 AND source_file != ?2";
    let stem_param = format!("%{}", stem);
    let mut stmt = conn.prepare_cached(sql)?;

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
        doc_comment: None,
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{DepsArgs, InitArgs, SearchArgs, SymArgs, UpdateArgs};
    use std::fs;
    use tempfile::TempDir;

    // -- query_symbols_db graded path ordering (TASK-094) ---------------------

    #[test]
    fn query_symbols_db_orders_by_graded_path_character() {
        // Direct symbol rows in insertion order: the graded ladder orders
        // ordinary > type declaration > test. Names merely RESEMBLING test
        // paths under the old local heuristic (contest.rs, mock dirs,
        // benches) are NOT demoted — the ladder's buckets are the spec'd
        // set, nothing more.
        let dir = TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        for file in [
            "tests/real.rs",
            "src/mock_data.rs",
            "src/alpha.d.ts",
            "src/core.rs",
        ] {
            conn.execute(
                "INSERT INTO symbols (name, kind, file, line, col, language) \
                 VALUES ('alpha', 'function', ?1, 1, 0, 'ts')",
                rusqlite::params![file],
            )
            .unwrap();
        }

        let results =
            query_symbols_db_with_filters(&conn, "alpha", None, None, None, true).unwrap();
        let files: Vec<&str> = results.iter().map(|s| s.file.as_str()).collect();
        assert_eq!(
            files,
            vec![
                "src/mock_data.rs",
                "src/core.rs",
                "src/alpha.d.ts",
                "tests/real.rs"
            ],
            "ordinary (insertion order) > .d.ts > test"
        );
    }

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
    fn test_router_query_references_db_path_filtering() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        // Insert references in different directories.
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["Widget", "src/ui/button.rs", 10, 4, "use Widget;"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                "Widget",
                "src/core/layout.rs",
                20,
                4,
                "let w = Widget::new();"
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["Widget", "tests/widget_test.rs", 5, 4, "Widget::default()"],
        )
        .unwrap();

        let router = QueryRouter::with_conn(conn, dir.path().to_path_buf());

        // No path filter — returns all.
        let all = router.query_references("Widget", &[]).unwrap();
        assert_eq!(all.len(), 3);

        // Filter to src/ui/ — returns only button.rs.
        let filtered = router
            .query_references("Widget", &["src/ui/".to_string()])
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].file, "src/ui/button.rs");

        // Filter to src/ — returns both src/ files.
        let src_only = router
            .query_references("Widget", &["src/".to_string()])
            .unwrap();
        assert_eq!(src_only.len(), 2);
        assert!(src_only.iter().all(|r| r.file.starts_with("src/")));

        // Multiple path prefixes.
        let multi = router
            .query_references("Widget", &["src/ui/".to_string(), "tests/".to_string()])
            .unwrap();
        assert_eq!(multi.len(), 2);
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
            let mut fmt = output::Formatter::new(&mut buf, OutputFormat::Grep, false);
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
            let mut fmt = output::Formatter::new(&mut buf, OutputFormat::Json, false);
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

    #[test]
    fn test_wonk_error_from_embedding_error() {
        use crate::errors::{EmbeddingError, WonkError};
        let emb_err = EmbeddingError::NoEmbeddings;
        let wonk_err: WonkError = emb_err.into();
        assert!(matches!(
            wonk_err,
            WonkError::Embedding(EmbeddingError::NoEmbeddings)
        ));
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
            let mut fmt = output::Formatter::new(&mut buf, OutputFormat::Grep, false);
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
            let mut fmt = output::Formatter::new(&mut buf, OutputFormat::Json, false);
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
            let mut fmt = output::Formatter::new(&mut buf, OutputFormat::Grep, false);
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
        format: OutputFormat,
    ) -> String {
        let results = router.query_symbols(name, kind, exact).unwrap();
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, format, false);
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
        let output = run_sym_query(&router, "processPayment", None, false, OutputFormat::Grep);
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
        let output = run_sym_query(&router, "processPayment", None, false, OutputFormat::Json);
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
        let output = run_sym_query(&router, "process", None, false, OutputFormat::Grep);
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
        let output = run_sym_query(&router, "processPayment", None, true, OutputFormat::Grep);
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
        let output = run_sym_query(
            &router,
            "Payment",
            Some("function"),
            true,
            OutputFormat::Grep,
        );
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
        let output = run_sym_query(&router, "processPayment", None, false, OutputFormat::Grep);
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
        let output = run_sym_query(&router, "processPayment", None, true, OutputFormat::Json);
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
            caller_name: None,
            confidence: 0.85,
        };

        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Grep, false);
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
            caller_name: None,
            confidence: 0.85,
        };

        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, OutputFormat::Json, false);
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
            semantic: false,
            why: false,
            query_class: None,
            context: None,
            no_feedback: false,
            file: None,
            paths: vec![],
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_sym() {
        let cmd = Command::Sym(SymArgs {
            name: "foo".into(),
            kind: None,
            file: None,
            exact: false,
            limit: None,
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
        let cmd = Command::Init(InitArgs {
            local: false,
            provider: None,
        });
        assert!(!is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_not_update() {
        assert!(!is_query_command(&Command::Update(UpdateArgs {
            force: false,
            skip_embed: false,
            provider: None,
        })));
    }

    #[test]
    fn test_is_query_command_not_status() {
        assert!(!is_query_command(&Command::Status));
    }

    #[test]
    fn test_is_query_command_contracts() {
        use crate::cli::ContractsArgs;
        assert!(is_query_command(&Command::Contracts(ContractsArgs {
            kind: None,
            role: None,
            orphans: false,
            links: false,
            unused_providers: false,
        })));
    }

    #[test]
    fn test_is_query_command_ask() {
        use crate::cli::AskArgs;
        let cmd = Command::Ask(AskArgs {
            query: "test query".into(),
            from: None,
            to: None,
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_cluster() {
        use crate::cli::ClusterArgs;
        let cmd = Command::Cluster(ClusterArgs {
            path: "src/auth/".into(),
            top: 5,
        });
        assert!(is_query_command(&cmd));
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

    // -- StatusInfo tests ---------------------------------------------------

    #[test]
    fn test_status_info_format_workspaces_line() {
        let info = StatusInfo {
            indexed: true,
            file_count: 10,
            symbol_count: 50,
            reference_count: 200,
            embedding_count: 0,
            stale_embedding_count: 0,
            active_provider: "bundled".to_string(),
            stored_vector_provider: None,
            stored_vector_dim: None,
            ollama_reachable: None,
            workspaces: vec!["payments".to_string(), "platform".to_string()],
            workspace_declared: true,
            workspace_comembers: vec!["repoB".to_string(), "repoC".to_string()],
            topology: TopologyStatus {
                scored: 0,
                communities: 0,
                last_computed: None,
                stale: false,
                enabled: true,
            },
            feedback: FeedbackStatus {
                enabled: false,
                events: 0,
                sessions: 0,
                deviation: 0.0,
                preferences: 0,
            },
        };
        let output = format_status_info(&info);
        assert!(
            output.contains("Workspaces: payments, platform (co-members: repoB, repoC)"),
            "got: {output}"
        );
    }

    #[test]
    fn test_status_info_format_undeclared_workspace_singleton_note() {
        let info = StatusInfo {
            indexed: true,
            file_count: 10,
            symbol_count: 50,
            reference_count: 200,
            embedding_count: 0,
            stale_embedding_count: 0,
            active_provider: "bundled".to_string(),
            stored_vector_provider: None,
            stored_vector_dim: None,
            ollama_reachable: None,
            workspaces: vec!["lone-api".to_string()],
            workspace_declared: false,
            workspace_comembers: Vec::new(),
            topology: TopologyStatus {
                scored: 0,
                communities: 0,
                last_computed: None,
                stale: false,
                enabled: true,
            },
            feedback: FeedbackStatus {
                enabled: false,
                events: 0,
                sessions: 0,
                deviation: 0.0,
                preferences: 0,
            },
        };
        let output = format_status_info(&info);
        assert!(
            output.contains("Workspaces: lone-api (undeclared)"),
            "got: {output}"
        );
    }

    #[test]
    fn test_status_info_format_with_embeddings() {
        let info = StatusInfo {
            indexed: true,
            file_count: 100,
            symbol_count: 500,
            reference_count: 2000,
            embedding_count: 300,
            stale_embedding_count: 10,
            active_provider: "ollama".to_string(),
            stored_vector_provider: Some("ollama".to_string()),
            stored_vector_dim: Some(768),
            ollama_reachable: Some(true),
            workspaces: Vec::new(),
            workspace_declared: false,
            workspace_comembers: Vec::new(),
            topology: TopologyStatus {
                scored: 0,
                communities: 0,
                last_computed: None,
                stale: false,
                enabled: true,
            },
            feedback: FeedbackStatus {
                enabled: false,
                events: 0,
                sessions: 0,
                deviation: 0.0,
                preferences: 0,
            },
        };
        let output = format_status_info(&info);
        assert!(output.contains("100 files"));
        assert!(output.contains("500 symbols"));
        assert!(output.contains("2000 references"));
        assert!(output.contains("300 embeddings"));
        assert!(output.contains("10 stale"));
        assert!(output.contains("Provider: ollama"));
        assert!(output.contains("Stored vectors: ollama, 768-dim"));
        assert!(output.contains("Ollama: reachable"));
    }

    #[test]
    fn test_status_info_format_no_index() {
        let info = StatusInfo {
            indexed: false,
            file_count: 0,
            symbol_count: 0,
            reference_count: 0,
            embedding_count: 0,
            stale_embedding_count: 0,
            active_provider: "bundled".to_string(),
            stored_vector_provider: None,
            stored_vector_dim: None,
            ollama_reachable: None,
            workspaces: Vec::new(),
            workspace_declared: false,
            workspace_comembers: Vec::new(),
            topology: TopologyStatus {
                scored: 0,
                communities: 0,
                last_computed: None,
                stale: false,
                enabled: true,
            },
            feedback: FeedbackStatus {
                enabled: false,
                events: 0,
                sessions: 0,
                deviation: 0.0,
                preferences: 0,
            },
        };
        let output = format_status_info(&info);
        assert!(output.contains("No index"));
    }

    #[test]
    fn test_status_info_format_ollama_unreachable() {
        let info = StatusInfo {
            indexed: true,
            file_count: 50,
            symbol_count: 200,
            reference_count: 800,
            embedding_count: 0,
            stale_embedding_count: 0,
            active_provider: "ollama".to_string(),
            stored_vector_provider: Some("bundled".to_string()),
            stored_vector_dim: Some(256),
            ollama_reachable: Some(false),
            workspaces: Vec::new(),
            workspace_declared: false,
            workspace_comembers: Vec::new(),
            topology: TopologyStatus {
                scored: 0,
                communities: 0,
                last_computed: None,
                stale: false,
                enabled: true,
            },
            feedback: FeedbackStatus {
                enabled: false,
                events: 0,
                sessions: 0,
                deviation: 0.0,
                preferences: 0,
            },
        };
        let output = format_status_info(&info);
        assert!(
            output.contains(
                "Ollama: unreachable — semantic queries fall back to the bundled provider"
            ),
            "got: {output}"
        );
    }

    #[test]
    fn test_status_info_format_bundled_only_skips_ollama_probe_line() {
        let info = StatusInfo {
            indexed: true,
            file_count: 42,
            symbol_count: 300,
            reference_count: 1200,
            embedding_count: 280,
            stale_embedding_count: 3,
            active_provider: "bundled".to_string(),
            stored_vector_provider: Some("bundled".to_string()),
            stored_vector_dim: Some(256),
            ollama_reachable: None,
            workspaces: Vec::new(),
            workspace_declared: false,
            workspace_comembers: Vec::new(),
            topology: TopologyStatus {
                scored: 0,
                communities: 0,
                last_computed: None,
                stale: false,
                enabled: true,
            },
            feedback: FeedbackStatus {
                enabled: false,
                events: 0,
                sessions: 0,
                deviation: 0.0,
                preferences: 0,
            },
        };
        let output = format_status_info(&info);
        assert!(output.contains("Provider: bundled"));
        assert!(output.contains("Stored vectors: bundled, 256-dim"));
        assert!(!output.contains("Ollama:"), "got: {output}");
    }

    #[test]
    fn test_status_info_format_no_stored_vectors() {
        let info = StatusInfo {
            indexed: true,
            file_count: 1,
            symbol_count: 1,
            reference_count: 0,
            embedding_count: 0,
            stale_embedding_count: 0,
            active_provider: "bundled".to_string(),
            stored_vector_provider: None,
            stored_vector_dim: None,
            ollama_reachable: None,
            workspaces: Vec::new(),
            workspace_declared: false,
            workspace_comembers: Vec::new(),
            topology: TopologyStatus {
                scored: 0,
                communities: 0,
                last_computed: None,
                stale: false,
                enabled: true,
            },
            feedback: FeedbackStatus {
                enabled: false,
                events: 0,
                sessions: 0,
                deviation: 0.0,
                preferences: 0,
            },
        };
        let output = format_status_info(&info);
        assert!(output.contains("Stored vectors: none"), "got: {output}");
    }

    #[test]
    fn test_status_info_serializes_provider_fields() {
        let info = StatusInfo {
            indexed: true,
            file_count: 1,
            symbol_count: 1,
            reference_count: 0,
            embedding_count: 2,
            stale_embedding_count: 0,
            active_provider: "bundled".to_string(),
            stored_vector_provider: Some("bundled".to_string()),
            stored_vector_dim: Some(256),
            ollama_reachable: None,
            workspaces: Vec::new(),
            workspace_declared: false,
            workspace_comembers: Vec::new(),
            topology: TopologyStatus {
                scored: 0,
                communities: 0,
                last_computed: None,
                stale: false,
                enabled: true,
            },
            feedback: FeedbackStatus {
                enabled: false,
                events: 0,
                sessions: 0,
                deviation: 0.0,
                preferences: 0,
            },
        };
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value["active_provider"], "bundled");
        assert_eq!(value["stored_vector_provider"], "bundled");
        assert_eq!(value["stored_vector_dim"], 256);
        assert_eq!(value["ollama_reachable"], serde_json::Value::Null);
    }

    // -- topology status line (TASK-098) --------------------------------------

    fn topology_status_info(
        scored: i64,
        communities: i64,
        last_computed: Option<i64>,
        stale: bool,
        enabled: bool,
    ) -> StatusInfo {
        StatusInfo {
            indexed: true,
            file_count: 1,
            symbol_count: 1,
            reference_count: 0,
            embedding_count: 0,
            stale_embedding_count: 0,
            active_provider: "bundled".to_string(),
            stored_vector_provider: None,
            stored_vector_dim: None,
            ollama_reachable: None,
            workspaces: Vec::new(),
            workspace_declared: false,
            workspace_comembers: Vec::new(),
            topology: TopologyStatus {
                scored,
                communities,
                last_computed,
                stale,
                enabled,
            },
            feedback: FeedbackStatus {
                enabled: false,
                events: 0,
                sessions: 0,
                deviation: 0.0,
                preferences: 0,
            },
        }
    }

    #[test]
    fn test_status_topology_line_disabled_none_and_fresh() {
        // The kill switch says so.
        let out = format_status_info(&topology_status_info(0, 0, None, false, false));
        assert!(out.contains("Topology: disabled"), "got: {out}");

        // Never scored: absent data reads as none.
        let out = format_status_info(&topology_status_info(0, 0, None, false, true));
        assert!(out.contains("Topology: none"), "got: {out}");

        // Scored and fresh: the count and the community count.
        let out = format_status_info(&topology_status_info(4321, 4, Some(1_000_000), false, true));
        assert!(
            out.contains("Topology: 4321 symbols scored (4 communities)"),
            "got: {out}"
        );
        assert!(!out.contains("stale"), "fresh is unmarked: {out}");
    }

    #[test]
    fn test_status_topology_line_stale_names_the_age() {
        let out = format_status_info(&topology_status_info(12, 3, Some(1_000_000), true, true));
        assert!(
            out.contains("Topology: 12 symbols scored (3 communities) (stale, computed "),
            "got: {out}"
        );
        assert!(out.contains("s ago)"), "the age renders in seconds: {out}");
    }

    // -- feedback status (TASK-103) --------------------------------------------

    #[test]
    fn test_status_feedback_line_pins_all_three_shapes() {
        let with = |feedback: FeedbackStatus| StatusInfo {
            feedback,
            ..topology_status_info(0, 0, None, false, true)
        };
        // Enabled: the full state, always.
        let out = format_status_info(&with(FeedbackStatus {
            enabled: true,
            events: 40,
            sessions: 40,
            deviation: 0.05,
            preferences: 3,
        }));
        assert!(
            out.contains(
                "Feedback: enabled, 40 events, 40 sessions, weight deviation 0.050, \
                 3 result preferences"
            ),
            "got: {out}"
        );
        // Disabled with leftover state: the counts stay legible.
        let out = format_status_info(&with(FeedbackStatus {
            enabled: false,
            events: 40,
            sessions: 40,
            deviation: 0.05,
            preferences: 3,
        }));
        assert!(
            out.contains(
                "Feedback: disabled, 40 events, 40 sessions, weight deviation 0.050, \
                 3 result preferences"
            ),
            "got: {out}"
        );
        // Disabled with nothing recorded: bare.
        let out = format_status_info(&with(FeedbackStatus {
            enabled: false,
            events: 0,
            sessions: 0,
            deviation: 0.0,
            preferences: 0,
        }));
        assert!(out.contains("Feedback: disabled"), "got: {out}");
        assert!(
            !out.contains("weight deviation"),
            "no counts to show: {out}"
        );
        // Preferences alone are leftover state: legible with the feature off.
        let out = format_status_info(&with(FeedbackStatus {
            enabled: false,
            events: 0,
            sessions: 0,
            deviation: 0.0,
            preferences: 2,
        }));
        assert!(
            out.contains(
                "Feedback: disabled, 0 events, 0 sessions, weight deviation 0.000, \
                 2 result preferences"
            ),
            "a leftover preference surfaces the line: {out}"
        );
    }

    #[test]
    fn test_status_info_serializes_feedback_fields() {
        let info = StatusInfo {
            feedback: FeedbackStatus {
                enabled: true,
                events: 7,
                sessions: 3,
                deviation: 0.25,
                preferences: 2,
            },
            ..topology_status_info(0, 0, None, false, true)
        };
        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(value["feedback"]["enabled"], true);
        assert_eq!(value["feedback"]["events"], 7);
        assert_eq!(value["feedback"]["sessions"], 3);
        assert_eq!(value["feedback"]["deviation"], 0.25);
        assert_eq!(value["feedback"]["preferences"], 2);
    }

    #[test]
    fn feedback_status_counts_events_sessions_and_deviation() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        crate::db::ensure_feedback_tables(&conn).unwrap();
        let features = r#"{"schema":1,"slate":"t","members":[]}"#;
        for (identity, session) in [
            ("id1", "a"),
            ("id1", "a"),
            ("id2", "b"),
            ("id2", "b"),
            ("id3", "c"),
        ] {
            conn.execute(
                "INSERT INTO feedback_events \
                 (result_identity, query_class, chosen_rank, features, useful, session, created_at) \
                 VALUES (?1, NULL, 2, ?2, 1, ?3, 1000)",
                rusqlite::params![identity, features, session],
            )
            .unwrap();
        }
        // A gated row at updated_at = now (decay exactly 1.0): stored
        // 0.55 against default 0.6 → deviation 0.05.
        conn.execute(
            "INSERT INTO learned_weights \
             (feature, query_class, weight, observations, sessions, updated_at) \
             VALUES ('path_character', '', 0.55, 40, 9, 1000)",
            [],
        )
        .unwrap();
        for identity in ["id1", "id2"] {
            conn.execute(
                "INSERT INTO result_preferences \
                 (result_identity, strength, observations, sessions, updated_at) \
                 VALUES (?1, 0.3, 4, 4, 1000)",
                [identity],
            )
            .unwrap();
        }
        let weights = std::collections::HashMap::from([("path_character".to_string(), 0.6)]);

        let status = feedback_status(
            Some(&conn),
            &crate::config::FeedbackConfig {
                enabled: true,
                ..crate::config::FeedbackConfig::default()
            },
            &weights,
            1000,
        );
        assert_eq!(status.events, 5);
        assert_eq!(status.sessions, 3, "distinct sessions");
        assert_eq!(status.preferences, 2, "stored result preferences");
        assert!((status.deviation - 0.05).abs() < 1e-6, "{status:?}");

        // No connection at all: zeros, enabled still reported.
        let status = feedback_status(
            None,
            &crate::config::FeedbackConfig::default(),
            &weights,
            1000,
        );
        assert_eq!(
            (
                status.events,
                status.sessions,
                status.deviation,
                status.preferences
            ),
            (0, 0, 0.0, 0)
        );
        assert!(!status.enabled);
    }

    #[test]
    fn test_query_status_info_populates_topology_status() {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) \
             VALUES ('core', 'class', 'src/a.rs', 1, 1, 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbol_topology (symbol_id, hub, authority) VALUES (1, 0.5, 0.5)",
            [],
        )
        .unwrap();
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
            - 10;
        conn.execute(
            "INSERT INTO topology_meta (key, value) VALUES ('last_computed', ?1)",
            [stamp.to_string()],
        )
        .unwrap();

        // Fresh against a generous threshold.
        let info = query_status_info(
            Some(&conn),
            crate::embedding::EmbeddingProviderKind::Bundled,
            None,
            &crate::config::TopologyConfig::default(),
            &crate::config::FeedbackConfig::default(),
            &std::collections::HashMap::new(),
        );
        assert_eq!(info.topology.scored, 1);
        assert_eq!(
            info.topology.communities, 0,
            "a TASK-098 row with NULL community counts toward no community"
        );
        assert_eq!(info.topology.last_computed, Some(stamp));
        assert!(!info.topology.stale);
        assert!(info.topology.enabled);

        // Distinct communities: two scored symbols in two communities.
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) \
             VALUES ('leaf', 'function', 'src/b.rs', 1, 1, 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbol_topology (symbol_id, hub, authority, community) \
             VALUES (2, 0.1, 0.1, 7)",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE symbol_topology SET community = 7 WHERE symbol_id = 1",
            [],
        )
        .unwrap();
        let info = query_status_info(
            Some(&conn),
            crate::embedding::EmbeddingProviderKind::Bundled,
            None,
            &crate::config::TopologyConfig::default(),
            &crate::config::FeedbackConfig::default(),
            &std::collections::HashMap::new(),
        );
        assert_eq!(info.topology.scored, 2);
        assert_eq!(info.topology.communities, 1, "both rows share community 7");

        // Aged past the threshold: the marker flips.
        let aged = crate::config::TopologyConfig {
            stale_after: 1,
            ..Default::default()
        };
        let info = query_status_info(
            Some(&conn),
            crate::embedding::EmbeddingProviderKind::Bundled,
            None,
            &aged,
            &crate::config::FeedbackConfig::default(),
            &std::collections::HashMap::new(),
        );
        assert!(info.topology.stale, "age >> 1s must read as stale");

        // The kill switch reflects through.
        let off = crate::config::TopologyConfig {
            enabled: false,
            ..Default::default()
        };
        let info = query_status_info(
            Some(&conn),
            crate::embedding::EmbeddingProviderKind::Bundled,
            None,
            &off,
            &crate::config::FeedbackConfig::default(),
            &std::collections::HashMap::new(),
        );
        assert!(!info.topology.enabled);
    }

    // -- Semantic fetch + RRF helpers -----------------------------------------

    #[test]
    fn test_fetch_semantic_no_conn_returns_empty() {
        let result = fetch_semantic_results(
            "test",
            None,
            crate::embedding::EmbeddingProviderKind::Ollama,
            true,
        );
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_fetch_semantic_no_embeddings_returns_empty() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        let result = fetch_semantic_results(
            "test",
            Some(&conn),
            crate::embedding::EmbeddingProviderKind::Ollama,
            true,
        );
        assert!(result.unwrap().is_empty());
    }

    #[test]
    fn test_fetch_semantic_with_embeddings_does_not_panic() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = db::open(&db_path).unwrap();

        // Insert a symbol and a fake bundled-space embedding so the test is
        // deterministic offline (no Ollama health probe on the active path).
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES ('test_fn', 'function', 'src/test.rs', 1, 0, 'Rust', 'fn test_fn()')",
            [],
        )
        .unwrap();
        let symbol_id = conn.last_insert_rowid();
        let fake_vec: Vec<f32> = vec![0.1; 256];
        let bytes: &[u8] = bytemuck::cast_slice(&fake_vec);
        conn.execute(
            "INSERT INTO embeddings \
             (symbol_id, file, chunk_text, vector, stale, created_at, provider, dim) \
             VALUES (?1, 'src/test.rs', 'test chunk', ?2, 0, strftime('%s','now'), 'bundled', 256)",
            rusqlite::params![symbol_id, bytes],
        )
        .unwrap();

        let result = fetch_semantic_results(
            "test_query",
            Some(&conn),
            crate::embedding::EmbeddingProviderKind::Bundled,
            true,
        );
        assert!(result.is_ok(), "fetch_semantic_results should not error");
    }

    // -- Impact query command test -------------------------------------------

    #[test]
    fn test_is_query_command_impact() {
        use crate::cli::ImpactArgs;
        let cmd = Command::Impact(ImpactArgs {
            file: "src/main.rs".into(),
            since: None,
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_impact_with_since() {
        use crate::cli::ImpactArgs;
        let cmd = Command::Impact(ImpactArgs {
            file: "src/main.rs".into(),
            since: Some("HEAD~3".into()),
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_callers() {
        use crate::cli::CallersArgs;
        let cmd = Command::Callers(CallersArgs {
            name: "dispatch".into(),
            reference_file: None,
            callers_file: None,
            depth: 1,
            min_confidence: None,
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_callees() {
        use crate::cli::CalleesArgs;
        let cmd = Command::Callees(CalleesArgs {
            name: "main".into(),
            reference_file: None,
            callees_file: None,
            depth: 1,
            min_confidence: None,
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_callpath() {
        use crate::cli::CallpathArgs;
        let cmd = Command::Callpath(CallpathArgs {
            from: "main".into(),
            to: "dispatch".into(),
            reference_file: None,
            destination_file: None,
            min_confidence: None,
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_flows() {
        use crate::cli::FlowsArgs;
        let cmd = Command::Flows(FlowsArgs {
            entry: None,
            from: None,
            depth: 10,
            branching: 4,
            min_confidence: None,
        });
        assert!(is_query_command(&cmd));
    }

    #[test]
    fn test_is_query_command_blast() {
        use crate::cli::BlastArgs;
        let cmd = Command::Blast(BlastArgs {
            symbol: "processPayment".into(),
            direction: None,
            depth: 3,
            include_tests: false,
            min_confidence: None,
        });
        assert!(is_query_command(&cmd));
    }

    // -- Changes tests (TASK-072) --------------------------------------------

    #[test]
    fn test_is_query_command_changes() {
        use crate::cli::ChangesArgs;
        let cmd = Command::Changes(ChangesArgs {
            scope: "unstaged".into(),
            base: None,
            blast: false,
            flows: false,
            min_confidence: None,
        });
        assert!(is_query_command(&cmd));
    }

    // -- Context tests (TASK-073) --------------------------------------------

    #[test]
    fn test_is_query_command_context() {
        use crate::cli::ContextArgs;
        let cmd = Command::Context(ContextArgs {
            name: "processPayment".into(),
            file: None,
            kind: None,
            min_confidence: None,
            elide: None,
        });
        assert!(is_query_command(&cmd));
    }

    // -- Review tests (TASK-085) ----------------------------------------------

    #[test]
    fn test_is_query_command_review_is_false_no_auto_init() {
        // Review must NOT trigger auto-init: indexing the current tree
        // mid-diff would empty the diff and fake an APPROVE. The no-index
        // path is an error telling the user to index the base state.
        use crate::cli::ReviewArgs;
        let cmd = Command::Review(ReviewArgs {
            scope: "unstaged".into(),
            base: None,
            since: None,
            min_confidence: None,
            min_severity: None,
            kind: Vec::new(),
            max_findings: None,
            suppress: None,
            elide: None,
        });
        assert!(!is_query_command(&cmd));
    }

    #[test]
    fn parse_change_scope_unstaged() {
        use crate::types::ChangeScope;
        assert_eq!(
            parse_change_scope("unstaged", None).unwrap(),
            ChangeScope::Unstaged
        );
    }

    #[test]
    fn parse_change_scope_compare_requires_base() {
        let err = parse_change_scope("compare", None).unwrap_err().to_string();
        assert!(err.contains("--base"), "error must name the flag: {err}");
    }

    #[test]
    fn parse_change_scope_compare_with_base() {
        use crate::types::ChangeScope;
        assert_eq!(
            parse_change_scope("compare", Some("main")).unwrap(),
            ChangeScope::Compare("main".into())
        );
    }

    #[test]
    fn parse_change_scope_rejects_injection_ref() {
        // validate_git_ref allows `-` (legal in ref names); shell/option
        // metacharacters like `;` are rejected.
        let err = parse_change_scope("compare", Some("main;rm -rf"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid git reference"), "got: {err}");
    }

    #[test]
    fn parse_change_scope_rejects_unknown_scope() {
        let err = parse_change_scope("bogus", None).unwrap_err().to_string();
        assert!(!err.is_empty());
    }

    // -- split_qualified_name tests -------------------------------------------

    #[test]
    fn test_split_qualified_rust_style() {
        let split = split_qualified_name("tokio::runtime::Handle");
        assert_eq!(split.name, "Handle");
        assert_eq!(split.file_hint.as_deref(), Some("tokio/runtime"));
        assert!(split.scope_hint.is_none());
    }

    #[test]
    fn test_split_qualified_rust_single_prefix() {
        let split = split_qualified_name("ScheduledIo::wake");
        assert_eq!(split.name, "wake");
        assert_eq!(split.file_hint.as_deref(), Some("scheduled_io"));
        assert!(split.scope_hint.is_none());
    }

    #[test]
    fn test_split_qualified_dot_style() {
        let split = split_qualified_name("Client.get");
        assert_eq!(split.name, "get");
        assert!(
            split.file_hint.is_none(),
            "uppercase prefix → scope only, no file hint"
        );
        assert_eq!(split.scope_hint.as_deref(), Some("Client"));
    }

    #[test]
    fn test_split_qualified_dot_nested() {
        let split = split_qualified_name("foo.bar.baz");
        assert_eq!(split.name, "baz");
        assert_eq!(split.file_hint.as_deref(), Some("foo"));
        assert_eq!(split.scope_hint.as_deref(), Some("bar"));
    }

    #[test]
    fn test_split_qualified_no_separator() {
        let split = split_qualified_name("dispatch");
        assert_eq!(split.name, "dispatch");
        assert!(split.file_hint.is_none());
        assert!(split.scope_hint.is_none());
    }

    #[test]
    fn test_split_qualified_trailing_colons() {
        let split = split_qualified_name("Foo::");
        assert_eq!(split.name, "Foo::");
        assert!(split.file_hint.is_none());
        assert!(split.scope_hint.is_none());
    }

    #[test]
    fn test_split_qualified_trailing_dot() {
        let split = split_qualified_name("Foo.");
        assert_eq!(split.name, "Foo.");
        assert!(split.file_hint.is_none());
        assert!(split.scope_hint.is_none());
    }

    #[test]
    fn test_split_qualified_colons_preferred_over_dots() {
        // `::` takes priority even when dots are present.
        let split = split_qualified_name("std.io::Write");
        assert_eq!(split.name, "Write");
        assert_eq!(split.file_hint.as_deref(), Some("std.io"));
        assert!(split.scope_hint.is_none());
    }

    #[test]
    fn test_split_qualified_mixed_colons_and_dot() {
        // `module::Class.method` → name=method, file_hint=module, scope_hint=Class
        let split = split_qualified_name("module::Class.method");
        assert_eq!(split.name, "method");
        assert_eq!(split.file_hint.as_deref(), Some("module"));
        assert_eq!(split.scope_hint.as_deref(), Some("Class"));
    }

    // -- `wonk duplicates` (TASK-100) -----------------------------------------

    const DUP_HANDLER: &str = "pub fn handle_user_created(event: &CreateEvent, store: &mut Store) -> Result<(), Error> {\n    let user = event.payload_user();\n    if user.email.is_empty() {\n        return Err(Error::Validation(\"email required\"));\n    }\n    let existing = store.find_by_email(&user.email)?;\n    if existing.is_some() {\n        return Err(Error::Conflict(\"email already registered\"));\n    }\n    let record = store.insert(&user)?;\n    metrics::count(\"user_created\", 1);\n    notifier::welcome(&record.email)?;\n    audit::log(\"user_created\", record.id);\n    Ok(())\n}";

    fn duplicates_conn() -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        (dir, conn)
    }

    fn seed_dup_symbol(conn: &Connection, name: &str, file: &str, body: &str) -> i64 {
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) \
             VALUES (?1, 'function', ?2, 1, 0, 'rust')",
            rusqlite::params![name, file],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO symbol_shingles (symbol_id, signature) VALUES (?1, ?2)",
            rusqlite::params![
                id,
                crate::shingles::encode_sketch(&crate::shingles::body_signature(body))
            ],
        )
        .unwrap();
        id
    }

    fn run_dups(conn: &Connection, threshold: f32) -> String {
        let mut buf = Vec::new();
        let mut fmt = output::Formatter::new(&mut buf, OutputFormat::Grep, false);
        run_duplicates(conn, threshold, &mut fmt, true).unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn duplicates_dispatch_prints_groups() {
        let (_dir, conn) = duplicates_conn();
        seed_dup_symbol(&conn, "handler_a", "src/a.rs", DUP_HANDLER);
        seed_dup_symbol(&conn, "handler_b", "src/b.rs", DUP_HANDLER);
        seed_dup_symbol(
            &conn,
            "sort_records",
            "src/c.rs",
            "fn sort_records(items: &mut [Record]) {\n    items.sort_by_key(|r| r.priority);\n}\n",
        );

        let text = run_dups(&conn, 0.85);
        assert!(
            text.contains("dup-group 1 size=2 mean-sim=1.00"),
            "one pair group header: {text}"
        );
        assert!(text.contains("  src/a.rs:1 function handler_a"), "{text}");
        assert!(text.contains("  src/b.rs:1 function handler_b"), "{text}");
        assert!(!text.contains("src/c.rs"), "singleton dropped: {text}");
    }

    #[test]
    fn duplicates_empty_index_hints() {
        let (_dir, conn) = duplicates_conn();
        let text = run_dups(&conn, 0.85);
        assert!(text.trim().is_empty(), "no groups, no stdout noise: {text}");
    }

    #[test]
    fn duplicates_on_pre_task100_index_migrates_and_degrades() {
        let (_dir, conn) = duplicates_conn();
        // Strip the TASK-100 tables: the shape of a pre-TASK-100 index.
        conn.execute("DROP TABLE symbol_shingles", []).unwrap();
        conn.execute("DROP TABLE near_duplicates", []).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) \
             VALUES ('handler', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        )
        .unwrap();

        // `wonk duplicates` must degrade to an empty report over the
        // migrated (ensure_*) schema — never a raw SQL error.
        let text = run_dups(&conn, 0.85);
        assert!(text.trim().is_empty(), "old index: empty, graceful: {text}");

        // The migration ran: both duplicate tables exist afterwards.
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                 AND name IN ('symbol_shingles', 'near_duplicates')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2, "ensure_duplicate_tables migrated the old index");
    }

    #[test]
    fn search_records_near_duplicates() {
        let (_dir, conn) = duplicates_conn();
        let a = seed_dup_symbol(&conn, "handler_a", "a.rs", DUP_HANDLER);
        let b = seed_dup_symbol(&conn, "handler_b", "b.rs", DUP_HANDLER);
        let pairs = vec![crate::shingles::NearDuplicatePair {
            symbol_id_a: a,
            symbol_id_b: b,
            similarity: 1.0,
        }];
        crate::shingles::record_pairs_best_effort(Some(&conn), &pairs);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM near_duplicates", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // No connection (grep fallback search) and empty pairs degrade
        // silently.
        crate::shingles::record_pairs_best_effort(None, &pairs);
        crate::shingles::record_pairs_best_effort(Some(&conn), &[]);
    }

    // -- feedback slate capture on the search path (TASK-101) -------------------

    /// A one-group ranked search over an indexed fixture file.
    fn feedback_ranked(conn: &Connection) -> crate::rerank::RankedSearch {
        crate::rerank::rank_and_explain_classed(
            &[crate::search::SearchResult {
                file: std::path::PathBuf::from("a.rs"),
                line: 1,
                col: 1,
                content: "pub fn handle_user_created(".to_string(),
            }],
            Some(conn),
            "handle_user_created",
            &crate::rerank::RankSettings {
                use_pipeline: true,
                ..Default::default()
            },
        )
    }

    fn feedback_config(enabled: bool) -> crate::config::FeedbackConfig {
        crate::config::FeedbackConfig {
            enabled,
            ..crate::config::FeedbackConfig::default()
        }
    }

    fn slate_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM feedback_slates", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn record_slate_best_effort_records_when_enabled() {
        let (dir, conn) = duplicates_conn();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, DUP_HANDLER).unwrap();
        crate::pipeline::build_index(dir.path(), true).unwrap();
        // Reindexing opened its own connection; use a fresh one.
        drop(conn);
        let index = crate::db::find_existing_index(dir.path()).unwrap();
        let conn = crate::db::open(&index).unwrap();

        let ranked = feedback_ranked(&conn);
        assert!(ranked.query_class.is_some(), "pipeline ran");

        let stored = record_slate_best_effort(
            Some(&conn),
            "handle_user_created",
            &ranked,
            &feedback_config(true),
        );
        assert!(stored.is_some(), "enabled search records a slate");
        let stored = stored.unwrap();
        assert_eq!(stored.token.len(), 16);
        assert!(!stored.members.is_empty());
        assert_eq!(slate_count(&conn), 1);
    }

    #[test]
    fn record_slate_best_effort_noop_when_disabled_or_connless() {
        let (dir, conn) = duplicates_conn();
        let ranked = feedback_ranked(&conn);

        // Disabled (the default): nothing written, nothing returned.
        assert!(
            record_slate_best_effort(Some(&conn), "q", &ranked, &feedback_config(false)).is_none()
        );
        assert_eq!(slate_count(&conn), 0, "default config writes nothing");

        // Enabled but no connection (grep fallback search): silent no-op.
        assert!(record_slate_best_effort(None, "q", &ranked, &feedback_config(true)).is_none());
        assert_eq!(slate_count(&conn), 0);
        drop(dir);
    }

    // -- `wonk feedback` (TASK-101) ---------------------------------------------

    /// A real tempdir repo indexed with the fixture, and a stored slate
    /// from the enabled capture path.
    fn feedback_repo_with_slate() -> (TempDir, Connection, String) {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("a.rs"), DUP_HANDLER).unwrap();
        crate::pipeline::build_index(dir.path(), true).unwrap();
        let index = crate::db::find_existing_index(dir.path()).unwrap();
        let conn = crate::db::open(&index).unwrap();
        let ranked = feedback_ranked(&conn);
        let token = record_slate_best_effort(
            Some(&conn),
            "handle_user_created",
            &ranked,
            &feedback_config(true),
        )
        .unwrap()
        .token;
        (dir, conn, token)
    }

    fn feedback_args(slate: &str, useful: &[&str]) -> crate::cli::FeedbackArgs {
        crate::cli::FeedbackArgs {
            slate: Some(slate.to_string()),
            session: Some("conv-1".to_string()),
            useful: useful.iter().map(|u| u.to_string()).collect(),
            weights: false,
            list: false,
            export: false,
            clear_events: false,
            clear_result: None,
            reset_weights: false,
            reset_weight: None,
        }
    }

    fn run_fb(conn: &Connection, args: &crate::cli::FeedbackArgs) -> String {
        let mut buf = Vec::new();
        let mut fmt = output::Formatter::new(&mut buf, OutputFormat::Grep, false);
        run_feedback(
            conn,
            args,
            &learning_config(),
            &mut fmt,
            true,
            OutputFormat::Grep,
        )
        .unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn feedback_event_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM feedback_events", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn run_feedback_records_and_prints_summary() {
        let (dir, conn, token) = feedback_repo_with_slate();
        let out = run_fb(&conn, &feedback_args(&token, &["1"]));
        assert_eq!(feedback_event_count(&conn), 1);
        assert!(
            out.contains(&format!("recorded 1 event(s) against slate {token}")),
            "summary line: {out}"
        );
        assert!(
            out.contains("(query \"handle_user_created\", class symbol)"),
            "summary carries the query and class: {out}"
        );
        assert!(
            out.contains("rank 1  a.rs:1  handle_user_created"),
            "per-event line: {out}"
        );
        drop(dir);
    }

    #[test]
    fn run_feedback_notes_dead_identity() {
        let (dir, conn, token) = feedback_repo_with_slate();
        // Retire the recorded identity: rename the symbol and re-index.
        let retired = DUP_HANDLER.replace("handle_user_created", "handle_user_renamed");
        std::fs::write(dir.path().join("a.rs"), retired).unwrap();
        crate::pipeline::build_index(dir.path(), true).unwrap();

        let out = run_fb(&conn, &feedback_args(&token, &["1"]));
        assert_eq!(feedback_event_count(&conn), 1, "history is honest");
        assert!(
            out.contains("no longer resolves in the index"),
            "dead-identity note: {out}"
        );
        drop(dir);
    }

    #[test]
    fn run_feedback_errors_on_unknown_slate() {
        let (dir, conn, _token) = feedback_repo_with_slate();
        let mut buf = Vec::new();
        let mut fmt = output::Formatter::new(&mut buf, OutputFormat::Grep, false);
        let err = run_feedback(
            &conn,
            &feedback_args("deadbeefdeadbeef", &["1"]),
            &learning_config(),
            &mut fmt,
            true,
            OutputFormat::Grep,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("slate not found"), "{err}");
        assert_eq!(feedback_event_count(&conn), 0);
        drop(dir);
    }

    // -- TASK-102: learning dispatch + --weights --------------------------------

    /// A two-result slate — useful at rank 2 with a real alternative —
    /// over a genuinely indexed twin-symbol repo.
    fn learning_repo_with_slate() -> (TempDir, Connection, String) {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("a.rs"), DUP_HANDLER).unwrap();
        std::fs::write(
            dir.path().join("b.rs"),
            DUP_HANDLER.replace("handle_user_created", "handle_user_backup"),
        )
        .unwrap();
        crate::pipeline::build_index(dir.path(), true).unwrap();
        let index = crate::db::find_existing_index(dir.path()).unwrap();
        let conn = crate::db::open(&index).unwrap();
        let ranked = crate::rerank::rank_and_explain_classed(
            &[
                crate::search::SearchResult {
                    file: std::path::PathBuf::from("a.rs"),
                    line: 1,
                    col: 1,
                    content: "pub fn handle_user_created(".to_string(),
                },
                crate::search::SearchResult {
                    file: std::path::PathBuf::from("b.rs"),
                    line: 1,
                    col: 1,
                    content: "pub fn handle_user_backup(".to_string(),
                },
            ],
            Some(&conn),
            "handle_user_created",
            &crate::rerank::RankSettings {
                use_pipeline: true,
                ..Default::default()
            },
        );
        let token = record_slate_best_effort(
            Some(&conn),
            "handle_user_created",
            &ranked,
            &feedback_config(true),
        )
        .unwrap()
        .token;
        (dir, conn, token)
    }

    fn learning_config() -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.feedback.enabled = true;
        config
    }

    fn session_args(slate: &str, session: &str, useful: &str) -> crate::cli::FeedbackArgs {
        crate::cli::FeedbackArgs {
            slate: Some(slate.to_string()),
            session: Some(session.to_string()),
            useful: vec![useful.to_string()],
            weights: false,
            list: false,
            export: false,
            clear_events: false,
            clear_result: None,
            reset_weights: false,
            reset_weight: None,
        }
    }

    fn learned_weight_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM learned_weights", [], |row| row.get(0))
            .unwrap()
    }

    fn learned_watermark(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT value FROM learned_meta WHERE key = 'event_watermark'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
        .parse()
        .unwrap()
    }

    #[test]
    fn run_feedback_learns_synchronously_after_recording() {
        let (dir, conn, token) = learning_repo_with_slate();
        for session in ["s1", "s2", "s3", "s4"] {
            run_fb(&conn, &session_args(&token, session, "2"));
        }
        assert!(
            learned_weight_count(&conn) > 0,
            "the dispatch learned: run_feedback must call learn_pending"
        );
        assert_eq!(learned_watermark(&conn), 4, "one id per event");
        drop(dir);
    }

    #[test]
    fn feedback_weights_lists_every_row_with_its_counts() {
        let (dir, conn, token) = learning_repo_with_slate();
        for session in ["s1", "s2", "s3", "s4"] {
            run_fb(&conn, &session_args(&token, session, "2"));
        }
        let args = crate::cli::FeedbackArgs {
            weights: true,
            ..session_args(&token, "", "")
        };
        let mut buf = Vec::new();
        let mut fmt = output::Formatter::new(&mut buf, OutputFormat::Grep, false);
        run_feedback(
            &conn,
            &args,
            &learning_config(),
            &mut fmt,
            true,
            OutputFormat::Grep,
        )
        .unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(
            out.contains("kind [overall] 1.000 (default 1.000) 4 obs, 4 sessions"),
            "{out}"
        );
        assert!(
            out.contains("[below gate]"),
            "inert rows stay legible with their counts: {out}"
        );
        drop(dir);
    }

    #[test]
    fn feedback_weights_json_emits_row_objects() {
        let (dir, conn, token) = learning_repo_with_slate();
        for session in ["s1", "s2"] {
            run_fb(&conn, &session_args(&token, session, "2"));
        }
        let args = crate::cli::FeedbackArgs {
            weights: true,
            ..session_args(&token, "", "")
        };
        let mut buf = Vec::new();
        let mut fmt = output::Formatter::new(&mut buf, OutputFormat::Json, false);
        run_feedback(
            &conn,
            &args,
            &learning_config(),
            &mut fmt,
            true,
            OutputFormat::Json,
        )
        .unwrap();
        let rows: serde_json::Value =
            serde_json::from_str(&String::from_utf8(buf).unwrap()).unwrap();
        let rows = rows.as_array().unwrap();
        assert!(!rows.is_empty());
        let kind = rows
            .iter()
            .find(|row| row["feature"] == "kind" && row["query_class"].is_null())
            .unwrap();
        assert_eq!(kind["observations"], 2);
        assert_eq!(kind["sessions"], 2);
        assert_eq!(kind["gated"], false);
        assert_eq!(kind["default"], 1.0);
        drop(dir);
    }

    // -- TASK-103: feedback management modes ------------------------------------

    /// A management-mode args base (everything off) — each test flips
    /// exactly one mode on.
    fn mode_args() -> crate::cli::FeedbackArgs {
        crate::cli::FeedbackArgs {
            slate: None,
            session: None,
            useful: vec![],
            weights: false,
            list: false,
            export: false,
            clear_events: false,
            clear_result: None,
            reset_weights: false,
            reset_weight: None,
        }
    }

    fn run_fb_json(conn: &Connection, args: &crate::cli::FeedbackArgs) -> String {
        let mut buf = Vec::new();
        let mut fmt = output::Formatter::new(&mut buf, OutputFormat::Json, false);
        run_feedback(
            conn,
            args,
            &learning_config(),
            &mut fmt,
            true,
            OutputFormat::Json,
        )
        .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn run_feedback_list_prints_one_line_per_event() {
        let (dir, conn, token) = learning_repo_with_slate();
        run_fb(&conn, &session_args(&token, "sess-1", "2"));
        run_fb(&conn, &session_args(&token, "sess-2", "1"));

        let mut args = mode_args();
        args.list = true;
        let out = run_fb(&conn, &args);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 2, "one line per event: {out}");
        assert!(out.contains("rank 2"), "the chosen rank: {out}");
        assert!(out.contains("a.rs:1"), "the file and line: {out}");
        assert!(out.contains("sess-1"), "the session: {out}");
        assert!(out.contains("class symbol"), "the query class: {out}");

        // JSON: the EventListing objects, one array.
        let json = run_fb_json(&conn, &args);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(parsed[0]["session"], "sess-1");
        assert_eq!(parsed[0]["rank"], 2);
        drop(dir);
    }

    #[test]
    fn run_feedback_export_round_trips_the_store() {
        let (dir, conn, token) = learning_repo_with_slate();
        run_fb(&conn, &session_args(&token, "sess-1", "2"));
        run_fb(&conn, &session_args(&token, "sess-2", "1"));

        let mut args = mode_args();
        args.export = true;
        let out = run_fb(&conn, &args);
        let exported: Vec<crate::feedback::FeedbackEvent> = serde_json::from_str(&out).unwrap();
        let stored = crate::feedback::load_events(&conn).unwrap();
        assert_eq!(exported, stored, "the export is the store, verbatim");
        drop(dir);
    }

    #[test]
    fn run_feedback_clear_events_keeps_learned_weights() {
        let (dir, conn, token) = learning_repo_with_slate();
        for session in ["s1", "s2", "s3", "s4"] {
            run_fb(&conn, &session_args(&token, session, "2"));
        }
        assert!(learned_weight_count(&conn) > 0, "weights learned");

        let mut args = mode_args();
        args.clear_events = true;
        let out = run_fb(&conn, &args);
        assert!(
            out.contains("cleared 4 feedback event(s); learned weights untouched"),
            "the confirmation names the independence: {out}"
        );
        assert_eq!(feedback_event_count(&conn), 0, "events wiped");
        assert!(
            learned_weight_count(&conn) > 0,
            "learned weights survive the history wipe"
        );

        let json = run_fb_json(&conn, &args);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cleared"], 0, "idempotent, structured");
        drop(dir);
    }

    #[test]
    fn run_feedback_reset_weights_keeps_events() {
        let (dir, conn, token) = learning_repo_with_slate();
        for session in ["s1", "s2", "s3", "s4"] {
            run_fb(&conn, &session_args(&token, session, "2"));
        }
        assert!(learned_weight_count(&conn) > 0, "weights learned");
        let events_before = feedback_event_count(&conn);
        assert!(events_before > 0);

        let mut args = mode_args();
        args.reset_weights = true;
        let out = run_fb(&conn, &args);
        assert!(
            out.contains("reset") && out.contains("learned weight row(s) to defaults"),
            "the confirmation: {out}"
        );
        assert!(
            out.contains("result preference(s) cleared"),
            "preferences ride the reset confirmation: {out}"
        );
        assert!(
            out.contains("event history untouched"),
            "the independence is in the message: {out}"
        );
        assert_eq!(learned_weight_count(&conn), 0, "weights reset");
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM learned_weight_sessions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(sessions, 0, "session bookkeeping reset with them");
        let preferences: i64 = conn
            .query_row("SELECT COUNT(*) FROM result_preferences", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(preferences, 0, "result preferences reset with them");
        assert_eq!(
            feedback_event_count(&conn),
            events_before,
            "the recorded history stands"
        );
        drop(dir);
    }

    #[test]
    fn run_feedback_clear_result_wipes_one_identity() {
        let (dir, conn, token) = learning_repo_with_slate();
        run_fb(&conn, &session_args(&token, "sess-1", "2"));
        run_fb(&conn, &session_args(&token, "sess-2", "1"));
        run_fb(&conn, &session_args(&token, "sess-3", "2"));
        let identity: String = conn
            .query_row(
                "SELECT result_identity FROM feedback_events                  WHERE chosen_rank = 1 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();

        let mut args = mode_args();
        args.clear_result = Some(identity.clone());
        let out = run_fb(&conn, &args);
        assert!(
            out.contains(&format!("cleared 1 feedback event(s) for {identity}")),
            "per-result confirmation: {out}"
        );
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM feedback_events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 2, "the other result's events stand");
        let none_left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM feedback_events WHERE result_identity = ?1",
                [&identity],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(none_left, 0);
        drop(dir);
    }

    #[test]
    fn run_feedback_reset_weight_scopes_to_one_feature() {
        let (dir, conn, token) = learning_repo_with_slate();
        for session in ["s1", "s2", "s3", "s4"] {
            run_fb(&conn, &session_args(&token, session, "2"));
        }
        assert!(learned_weight_count(&conn) > 1, "several features learned");
        let kind_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM learned_weights WHERE feature = 'kind'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(kind_rows > 0, "kind learned in some scope");

        let mut args = mode_args();
        args.reset_weight = Some("kind".to_string());
        let out = run_fb(&conn, &args);
        assert!(
            out.contains(&format!("reset {kind_rows} learned weight row(s) for kind")),
            "the per-feature confirmation: {out}"
        );
        let features: Vec<String> = conn
            .prepare("SELECT DISTINCT feature FROM learned_weights")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert!(!features.contains(&"kind".to_string()), "kind reset");
        assert!(!features.is_empty(), "sibling features stand");
        drop(dir);
    }
}
