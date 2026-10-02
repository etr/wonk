# Project Memory: wonk (csi)

## Build & Test
- **Language**: Rust (Cargo)
- **PATH needed**: `PATH="/usr/bin:/home/etr/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/bin:/sbin"` (cargo at /home/etr/.cargo/bin; `/usr/bin` MUST be included for `cc` linker; `source "$HOME/.cargo/env"` does NOT work)
- **Build**: `cargo build` / `cargo check`
- **Test**: `cargo test` (runs 1230+ tests, ~6s)
- **Test filter**: `cargo test module::tests::test_name` (only one pattern allowed per invocation)
- **Worktrees**: `.worktrees/` dir exists and is gitignored

## Architecture
- `src/cli.rs` - Clap CLI definitions (Command enum, Args structs)
- `src/router.rs` - CLI dispatch + QueryRouter (DB-first, grep-fallback pattern)
- `src/db.rs` - SQLite schema, connection mgmt, repo root discovery
- `src/pipeline.rs` - Index build/update pipeline (parallel with rayon)
- `src/indexer.rs` - Tree-sitter parsing, symbol/ref/import extraction for 10 languages
- `src/output.rs` - Formatter (grep-style text + JSON lines)
- `src/types.rs` - Shared types (Symbol, Reference, FileImports, SymbolKind, etc.)
- `src/search.rs` - grep-based text search
- `src/walker.rs` - File walking with ignore patterns
- `src/watcher.rs` - File watcher for incremental indexing
- `src/daemon.rs` - Background daemon
- `src/config.rs` - TOML configuration
- `src/errors.rs` - Error types (WonkError, DbError, SearchError)
- `src/ranker.rs` - Result classification, ranking, dedup, grouping (ResultCategory, ClassifiedResult)
- `src/budget.rs` - Token budget tracking (estimate_tokens, TokenBudget)
- `src/callgraph.rs` - Call graph traversal (callers/callees BFS with depth cap)

## Key Patterns
- DB schema in `SCHEMA_SQL` const in db.rs, applied via `apply_schema()`
- FileResult struct in pipeline.rs holds all parsed data for a file
- `parse_one_file()` extracts everything, `batch_insert()` stores it all
- `upsert_file_data()` for incremental updates, `delete_file_data()` for removals
- `drop_all_data()` for full rebuilds
- QueryRouter pattern: try DB first, fall back to grep on empty/no-index
- Tests use `tempfile::TempDir` for isolated DB/file fixtures
- `#[cfg(test)]` constructors: `QueryRouter::with_conn()`, `QueryRouter::grep_only()`

## Ranker Pipeline
- `classify_results()` -> `rank_results()` -> `dedup_reexports()` -> `group_by_category()`
- Orchestrated by `rank_and_dedup()` which takes SearchResult slice + optional DB conn
- ResultCategory has `tier()` method for sort ordering (Definition=0..Test=5)
- ClassifiedResult has `annotation` field for dedup count display
- SearchOutput has `annotation` field (Option<String>, skip_serializing_if None)
- Category headers go to stderr via `output::print_category_header()`
- `--raw` flag on SearchArgs bypasses ranking pipeline
- `--budget <n>` global flag limits output tokens via TokenBudget in Formatter

## Budget / Output Patterns
- Formatter.format_*() methods return `Result<BudgetStatus>` (Written or Skipped)
- `budgeted_write()` renders to temp buffer, checks budget, conditionally writes
- Highlight pattern transferred via `std::mem::swap` during budgeted_write
- Budget summary: stderr for grep mode (`print_budget_summary`), JSON line for JSON mode (`TruncationMeta`)
- `emit_budget_summary()` helper in router.rs handles both modes
- `dispatch_ls()` returns `usize` (truncated count) for budget summary emission

## DB Tables
- `symbols` - Symbol definitions (name, kind, file, line, col, etc.)
- `references` - Usage sites (name, file, line, col, context)
- `files` - File metadata (path, language, hash, last_indexed, etc.)
- `file_imports` - Import tracking for deps graph (source_file, import_path)
- `embeddings` - Vector embeddings (symbol_id, file, chunk_text, vector BLOB, stale flag)
- `daemon_status` - Daemon state
- `symbols_fts` - FTS5 virtual table synced via triggers

## Embedding Pipeline
- `chunk_all_symbols()` returns `Vec<(i64, String, String)>` (symbol_id, file_path, chunk_text)
- `build_embeddings()` in pipeline.rs: health check -> chunk -> batch embed (50/call) -> store
- `EmbeddingBuildStats` tracks embedded_count, total_symbols, skipped, elapsed
- `drop_all_data()` clears embeddings BEFORE symbols (FK cascade)
- `OllamaClient::is_healthy()` for reachability check
- Dead port pattern for testing: `OllamaClient::with_base_url("http://127.0.0.1:19999")`
- `StatusInfo` struct in router.rs for `wonk status` / MCP status
- `embedding_stats(conn)` returns `(total_count, stale_count)`

## Call Graph (TASK-061)
- `callgraph::callers(conn, name, max_depth)` - BFS callers via `references.caller_id JOIN symbols`
- `callgraph::callees(conn, name, max_depth)` - BFS callees via `references WHERE caller_id IN (SELECT id FROM symbols)`
- `callgraph::has_caller_id_data(conn)` - checks if old index lacks caller_id data
- `MAX_DEPTH_CAP = 10`, depth capped in router.rs with warning
- MCP tools: 19 total (18 existing + wonk_repos)
- Integration test in tests/mcp_integration.rs also asserts tool count

## Multi-repo MCP (TASK-074)
- `RepoEntry` + `RepoRegistry` in mcp.rs for multi-repo discovery
- `discover_repos(repos_dir)` scans `~/.wonk/repos/*/meta.json`
- `RepoRegistry::resolve(name)` matches by last path component, errors on ambiguity
- `RepoRegistry::get_or_open_connection()` lazy-opens SQLite connections
- `McpServer::resolve_repo(&args)` returns `(&Connection, PathBuf)` for either default or cross-repo
- `McpServer::has_repo_param(&args)` for tools that need grep fallback on default
- `wonk_repos` tool lists repos with stats (file_count, symbol_count, last_indexed)
- All 18 existing tools have optional `repo` param injected via `tool_definitions()`
- `query_symbols_db`, `query_references_db`, `query_signatures_db`, `query_symbols_in_file_db`, `query_deps_db`, `query_rdeps_db` made pub in router.rs for cross-repo queries
- `handle_tools_call` and all tool handlers now `&mut self` for lazy connection opening
- Borrow checker pattern: collect entry data into Vec of tuples first, then iterate with `&mut self`

## LLM / Semantic Summary (TASK-064)
- `src/llm.rs` - Content hash, prompt construction, Ollama generate client, cache layer
- `config::LlmConfig` - model (default "llama3.2:3b"), generate_url (default "http://localhost:11434/api/generate")
- `errors::LlmError` - OllamaUnreachable, ModelNotFound(String), OllamaError(String), InvalidResponse
- `db::ensure_summaries_table()` - Migration for `summaries` table (path PK, content_hash, description, created_at)
- `summary::SummaryOptions.semantic: Option<LlmConfig>` - None=structural only, Some=generate LLM desc
- Content hash: SHA-256 of sorted (symbol.id, file.hash) pairs
- Cache: `get_cached(conn, path, content_hash)` / `store_cache(conn, path, content_hash, desc)`
- Graceful degradation: OllamaUnreachable -> stderr hint + None description
- Description only at top level, not per-child in recursive traversal
- Dead port pattern for testing: `http://127.0.0.1:19999/api/generate`

## Query-time Provider Resolution (TASK-077)
- `plan_query_provider(conn, kind)` in embedding.rs resolves ask/search/cluster/impact providers: unreachable ollama -> bundled fallback with `BUNDLED_FALLBACK_WARNING`; mismatched stored space -> `EmbeddingError::VectorSpaceMismatch` (Display carries re-embed command)
- Pure core: `decide_query_provider(kind, healthy, &[StoredVectorSpace])` — exhaustive unit tests in embedding.rs tests
- `fallback_after_disconnect` re-plans when ollama dies mid-query (healthy=false)
- Offline seam for integration tests: env HTTP_PROXY=http://127.0.0.1:1 + NO_PROXY="" (ureq honors proxy env; kills localhost:11434 probes deterministically) — `offline_command()` in tests/ask_integration.rs
- `StatusInfo` now has active_provider / stored_vector_provider / stored_vector_dim; `ollama_reachable` is `Option<bool>` (None = not probed, bundled-only users skip the 500ms probe)
- `query_status_info(conn, configured_kind)` takes the configured kind; mcp.rs uses `embedding_provider_kind_for` helper
- OLLAMA_REQUIRED_MSG retired -> OLLAMA_UNREACHABLE_MSG (names bundled re-embed alternative)
- This machine (macOS arm64): cargo at /opt/homebrew/bin/cargo; PATH note in Build & Test above is for a different (Linux) host
- watcher::tests FSEvents delivery tests are flaky in this sandbox on macOS too (4 tests fail at baseline, unrelated to changes)

## Reach Index (TASK-080)
- `src/reach.rs`: name-collated bounded BFS build (`build_reach(tx, &ReachBuildOptions)`), `lookup_upstream(conn, name, depth)` -> `Option<ReachAnswer{affected, truncated}>`; `None` = fall back to BFS, `Some(empty)` = authoritative no-dependents
- Tables: `reach(source_id, target_id, min_depth, confidence)` PK pair + idx_reach_source_depth + idx_reach_target, `reach_truncated(source_id)`, `reach_meta(key,value)` built_depth/stale; `drop_all_data` clears all 3
- Shared predicate AR-021: `reach::edge_eligible(file, confidence, &EdgeFilter)` called by BOTH `analyze_blast` and `build_reach`; routing in analyze_blast requires use_reach && Upstream && !include_tests && min_conf<=0 && table fresh && depth <= built_depth
- `BlastOutput.truncated` has `#[serde(skip_serializing_if)]` so V4 JSON stays byte-identical when false; grep mode renders a lower-bound note
- Config: `[reach]` depth (3) / enabled (true); mcp/router/changes all thread `use_reach` from `Config::load(repo_root)`
- Bench: `cargo bench --bench reach` (61k-symbol synthetic repo, results in bench/reach-results.md; table p100 0.24ms vs BFS 53ms; ~72 B/symbol)
- Machine variance: bench session can run 1.2-1.5x slower than a prior recording (uncapped rebuild 451ms vs 540-697ms) — compare shape ratios, not absolute times

## Incremental reach repair (TASK-081)
- `upsert_file_data`/`delete_file_data` now call `reach::begin_file_edit(tx, rel_path)` BEFORE the delete block and `reach::finish_file_edit(tx, &scope)` after inserts, inside the file's own tx; on finish Err -> `mark_stale` in SAME tx + commit anyway (REQ-007)
- Affected sources = pre/post-edit canonical (MIN-eligible) ids of names in A_pre∪A_post (file symbol names + ref callee names w/ caller + type-edge parent names) ∪ predecessors from reverse `target_id` lookup (idx_reach_target), captured pre-delete in begin AND at finish (redundant by design — belt+braces; each alone covers the other's cases)
- FK cascade (foreign_keys=ON) removes rows referencing deleted symbol ids during the edit — repair's DELETE is idempotent; `rows_deleted` stat does NOT count cascaded rows
- Repair runs at table's own built_depth + DEFAULT cap via `SqlCandidates` (memoized prepared stmts) through the SAME `compute_source_rows` traversal `build_reach` uses over `GraphCandidates`; WI-1 parity test pins candidate-list equality
- Deviation from plan: repair does NOT no-op on an empty-but-built table (edge-adding edit on empty table must write rows; no-op would serve wrong Some(empty))
- Skip conditions: reach table missing / no built_depth / stale marker present
- Test oracle pattern: `assert_table_equivalent_to_bfs(&conn)` is `#[cfg(test)] pub(crate)` in reach.rs (shared by reach + pipeline test modules); `assert_incremental_equals_full_rebuild` rebuilds on the same conn (safe — leaves equivalent state)
- Gotcha: global test failpoints race under the parallel test runner — key `FAIL_NEXT_FINISH` to the probe file's rel_path (Mutex<Option<String>>) and use a uniquely-named probe file per test
- Gotcha: SQLite rowids shift on edit (delete+reinsert reuses max+1, alternating files hold the max) — never assert row-id-stable snapshots across edits; assert content via the rebuild-equivalence oracle
- Gotcha: `git checkout -- file` during a mutation teeth-check also wipes UNCOMMITTED new tests — commit the green suite BEFORE mutation-checking
- BlastDirection lives in types.rs and is NOT re-exported from blast.rs (private import there); bench/test code must `use wonk::types::BlastDirection`

## History Mining + Churn Signal (TASK-096)
- `src/history.rs`: one bounded `git log -n <window> --no-renames --format=%H%x09%ct --name-only` pass (cost ∝ window, never repo age); `run_git_output` extracted pub(crate) in impact.rs is the shared spawn helper
- 4 tables: `file_churn` aggregate + `mined_commits`/`commit_files` per-commit detail (TASK-097 co-change seam + exact-rescale mechanism) + `history_meta.mined_head`; drop_all_data clears all 4
- Age weight = linear 1−(head−ts)/span vs the MINED window's newest commit (not wall clock); recompute_file_churn always recomputes from DB detail ORDER BY commit_ts DESC, commit_id DESC — rescales old weights exactly on refresh (test asserts 1.5 → 0.5 after one new commit)
- refresh: has_git → rev-parse probe (Unchanged) → ranged log insert OR IGNORE → trim (explicit DELETEs, never relies on FK cascade) → recompute → set mined_head; rewrite/invalid head → ONE fallback full re-mine; every git error = eprintln + Failed with data retained
- Churn signal = 9th builtin ("churn"), log-damped ln(1+score)/ln(1+set_max) like centrality; weight DELIBERATELY absent from RankConfig defaults (0 = ranking unchanged until opted in); requires `with_file_churn` context slice, presence-probe + IN_CHUNK=900 batched load
- `[history] enabled=true window=500`; window=0 hard load error naming the key
- Git test fixtures: env dates need `@<unix> +0000` format (bare "100 +0000" is rejected by git); scope `git add src` in fixtures whose repo contains `.wonk/` (add . swallows the index db → phantom churn rows)
- Registry-count pins exist in THREE places when adding a signal: rerank.rs registry test, rerank.rs unknown-name error message, tests/rerank_path_signals.rs `default_weights_run_no_new_context_paths`
- Pipe-to-tail masks cargo exit codes — `cmd | grep -c error; echo $?` greps status, not cargo's; run clippy without piping or check its own exit before committing
- Daemon refreshes history best-effort per event batch (bare git commit emits no events — refreshes on next batch; wonk update authoritative)

## Co-change Coupling (TASK-097)
- `co_change(file_a, file_b, weight)` PK pair + `idx_co_change_a(file_a, weight DESC)`; weight = Σ age_weight over SHARED non-bulk commits (files.len() ≤ max_commit_files, strictly-more-than excluded), recomputed by `recompute_history_aggregates(conn, &MiningOptions{window, max_commit_files})` — churn + co_change from ONE detail load (replaced recompute_file_churn + HistoryOptions; call sites pipeline.rs ×2, daemon.rs ×1)
- `CO_CHANGE_TOP_K = 10` const; top-K per file_a independently both directions, (weight DESC, file_b ASC); 12 files × 2 commits → exactly 120 rows
- `[history] max_commit_files` default 50 (placeholder pending OQ-017), < 2 hard load error; bulk excluded from CO-CHANGE only (still churn + window)
- Signal = 10th builtin "co_change", SET-RELATIVE: w(f) = max coupling to another file IN THE RESPONSE SET (loader keeps only rows whose file_b is also a candidate; 99.0 coupling to non-candidate ignored), value = ln(1+w)/ln(1+set_max) like churn; default weight 0; CoChangeContext{best, max} + SharedContext::co_change_coupling/max_co_change
- Zero-path mirrors churn: no git/table/coupling/single-file set → 0.0 (single-file set inert because no row's file_b is in-set)
- e2e AC pattern lives in history.rs tests: grouped git fixture + `crate::rerank::rerank(classify_results(hits), …, WeightTable::from_pairs([("co_change",1.0)]))`
- Watcher FSEvents: 4 test_file_watcher_* failures are baseline in this sandbox (verified failing on clean main); `elide::tests::salience_under_20ms` is a parallel-load timing flake (passes in isolation)

## Hub/Authority Topology (TASK-098)
- `src/topology.rs`: `recompute(conn, &TopologyOptions{iterations})` = 3 deterministic queries (ids ORDER BY id = node order; name→MIN(id) non-module representative; caller_id refs, dedup HashSet → SORTED edges → CSR both directions) + HITS power method EXACTLY `iterations` iters, L1-normalized per iter, sum==0 → all-zero break; persist one tx (DELETE + id-ascending INSERTs + `topology_meta.last_computed` epoch). GOTCHA building CSR: the counting pass must key by the SAME endpoint the fill pass writes (flipped counts by dst) — a mismatch silently yields all-zero scores, no panic.
- Determinism is bitwise-asserted 3 ways (same conn twice, two conns, reversed insert order); simple PATH graphs converge at iteration 1 — evolving fixtures need asymmetric coupling (many callers → one node → one sink)
- Tables: `symbol_topology(symbol_id PK FK cascade, hub, authority, community NULL)` + `idx_topology_community` (TASK-099 owns column+index) + `topology_meta(key,value)`; `drop_all_data` deletes both BEFORE symbols
- `[topology] enabled=true iterations=20 interval=3600 stale_after=86400` (OQ-018 placeholders); iterations/interval/stale_after == 0 each a hard load error (interval message cites PRD-TOPO-REQ-006)
- Cadence: build_index step 5c + incremental_update recompute UNCONDITIONALLY when enabled (wonk update authoritative); daemon event loop uses `refresh_if_due(conn, opts, interval)` gated by last_computed age; grep gate pins zero topology in per-file fns (reindex_file/remove_file/index_new_file/process_events/delete_file_data/upsert_file_data)
- Signals 11th/12th: "hub"/"authority", per-(file,line) lookup via symbols JOIN (TopologyContext{scores,max_hub,max_authority}), `topology_value = (score/set_max).clamp(0,1)` NO log damper (HITS already L1-damped); default weights 0; registry-count pins now in rerank.rs registry test + unknown-name message + tests/rerank_path_signals.rs (12 signals)
- Kill switch: `RankSettings::from_config(…, topology_enabled: bool)` forces hub/authority weights to 0.0 → existing zero-weight skip gives bitwise-exact prior ranking; call sites router.rs/mcp.rs pass config.topology.enabled, tests/benches pass true (bench/rank_latency_bench.rs too — clippy --all-targets catches it)
- Staleness: `is_stale(conn, stale_after)`/`last_computed` are READ-ONLY helpers; query path never recomputes (pinned by stale_topology_never_blocks_a_query); `StatusInfo.topology: TopologyStatus{scored,last_computed,stale,enabled}` + `topology_status_line` after Embeddings ("Topology: disabled|none|N symbols scored[ (stale, computed Ns ago)]"); query_status_info grew a 4th param `&TopologyConfig` (mcp loads config for it)
- `wonk status` test fixtures: never use epoch-1000-style stamps with default stale_after 86400 — the fixture reads stale; stamp near now
