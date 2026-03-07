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
