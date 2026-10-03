# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

```bash
cargo build                    # Build debug binary
cargo build --release          # Build release binary
cargo test                     # Run all tests
cargo test <test_name>         # Run a single test by name
cargo test --lib               # Run only unit tests (no integration tests)
cargo fmt --check              # Check formatting (CI enforced)
cargo fmt                      # Auto-format code
cargo clippy -- -D warnings    # Lint with warnings-as-errors (CI enforced)
```

CI enforces `RUSTFLAGS="-D warnings"` — all warnings are errors.

## Architecture

Wonk is a structure-aware code search CLI for LLM coding agents. It combines tree-sitter parsing, SQLite indexing, and ripgrep-based text search to return ranked, deduplicated results that minimize token consumption.

### Data Flow

```
CLI (clap) → Router → { SQLite index | grep search } → Ranker → Budget → Output
                                    ↑
                              Daemon (notify → pipeline → SQLite)
```

### Module Responsibilities

| Module | Role |
|--------|------|
| `cli.rs` | Clap-derived argument parsing, delegates to `router::dispatch()` |
| `router.rs` | Query dispatch — routes commands to index or grep fallback, auto-initializes index on first use |
| `indexer.rs` | Tree-sitter parsing — extracts symbols, references, and imports for 12 languages |
| `db.rs` | SQLite layer — schema (WAL mode), repo root detection, index path computation |
| `pipeline.rs` | Index build orchestration — parallel file walk + parse + batch insert; incremental re-indexing for daemon; provider-neutral embedding build pipeline (chunking → batch embed → vector storage) |
| `walker.rs` | File enumeration with gitignore/wonkignore support; worktree-aware boundary detection |
| `search.rs` | Text search wrapping the `grep` crate (ripgrep internals) |
| `semantic.rs` | Brute-force cosine similarity search — parallel dot product via rayon, top-N ranking, resolution of symbol IDs to SemanticResult structs; BFS dependency graph traversal for `--from`/`--to` reachability scoping |
| `cluster.rs` | K-Means clustering engine — auto-k selection via silhouette scoring, cluster member resolution from SQLite |
| `ranker.rs` | Classifies results (Definition > CallSite > Import > Other > Comment > Test), deduplicates re-exports; RRF fusion via `fuse_rrf()` for `--semantic` blending |
| `output.rs` | Dual format: grep-compatible (stdout+stderr) or NDJSON (stdout) |
| `embedding.rs` | Embedding provider abstraction, Ollama API client, symbol chunking engine, and vector storage — bundled offline default plus opt-in sync Ollama client, context-rich chunks, provider-isolated BLOB storage, and query-time provider fallback (unreachable Ollama → bundled, never across mismatched vector spaces) |
| `show.rs` | Source body retrieval — queries symbol index by name with optional file/kind/exact filters, reads source file lines line..end_line per match, falls back to signature when end_line is absent; shallow mode for containers shows signature + child signatures without bodies |
| `summary.rs` | Structural summary engine — queries SQLite to aggregate file count, line count, symbol counts by kind, language breakdown, and dependency count for a path; supports three detail levels (rich/light/symbols), recursive depth traversal, symbol listing with location metadata, and `--tree` scope-grouped display (absorbed former `wonk ls`) |
| `llm.rs` | LLM description generation and caching — content hash computation from (symbol.id, file.hash) pairs, prompt construction from structural metrics, Ollama `/api/generate` sync client, SQLite cache get/store for `wonk summary --semantic` |
| `impact.rs` | Symbol change detection — Tree-sitter re-parse vs. indexed symbols for Added/Modified/Removed; git CLI wrapper for `--since` file listing; scoped change detection via `ChangeScope` enum and `detect_changes()` with git diff hunk-to-symbol mapping |
| `daemon.rs` | Background file watcher — double-fork daemonization, PID file, SIGTERM handler, embedding worker thread, daemon status table |
| `watcher.rs` | Filesystem event classification and debouncing via `notify` |
| `config.rs` | Layered TOML config: built-in defaults → `~/.wonk/config.toml` → `<repo>/.wonk/config.toml` |
| `context.rs` | Unified symbol context — aggregates definition, callers, importers, type users, callees, imports, flow participation, and children from type_edges into a single `SymbolContext` response |
| `blast.rs` | Blast radius analysis — depth-annotated BFS from a target symbol, severity tiers (WILL BREAK / LIKELY AFFECTED / MAY NEED TESTING), risk level assessment, inheritance integration via type_edges, test file exclusion; qualifying upstream queries are answered from the precomputed reach table by default with BFS as the fallback beyond the built depth or when the table is stale/disabled |
| `callgraph.rs` | Call graph traversal — BFS callers/callees expansion and callpath shortest-path finder via caller_id JOIN, cycle detection via visited HashSet, depth cap enforcement |
| `flows.rs` | Entry point detection and flow tracing — SQL anti-join for functions/methods with no indexed callers, forward BFS callee expansion with depth/branching/min-confidence controls |
| `reach.rs` | Precomputed upstream reachability — bounded-depth reach table built inside the index transaction, name-keyed BFS mirroring blast's traversal, per-source fan-out cap with truncation markers, stale/absent-table BFS fallback, and the edge-eligibility predicate shared with blast (AR-021) |
| `history.rs` | Bounded git-history mining — one `git log` pass over the newest N commits into per-commit detail, aggregate churn + co-change tables recomputed from that detail, watermark-driven refresh (`wonk update` authoritative, daemon best-effort) |
| `topology.rs` | Symbol-graph topology — deterministic HITS hub/authority + label-propagation communities recomputed on update and at the daemon interval; read-only staleness helpers (the query path never recomputes) |
| `contracts.rs` | Service-contract detection across 12 languages (HTTP routes/clients, env, queues, websocket, jobs, gRPC, GraphQL, OpenAPI) + cross-repo workspace link resolution |
| `review.rs` | Diff-scoped review engine — rule families A (breaking change), B (coverage gap), C (cross-repo contract impact), D (suppressions), blast-radius-driven with confidence/severity filters |
| `feedback.rs` | Usage-feedback capture — slate recording of ranked-search result features, result identities (content-anchored, SHA-256), event store with prune/retention |
| `learning.rs` | Contrastive weight learning — bounded, decaying learned_weights over signal names and flattened descriptive features, session/observation gates, watermark-driven chunked replay, gated query-path overlay, per-result preferences |
| `mcp.rs` | MCP server — JSON-RPC 2.0 over stdio, exposes the query tools for AI coding assistants; multi-repo support via optional `repo` parameter and lazy-loaded connections |
| `budget.rs` | Token budget tracking (~4 chars/token heuristic) |

### Key Design Decisions

- **No async runtime** — sync Rust + rayon for CPU-bound tree-sitter parsing
- **No IPC** — CLI and daemon communicate only via shared SQLite (WAL mode + busy_timeout)
- **Single crate** — lib.rs exports all modules; main.rs is a thin entry point
- **Bundled everything** — SQLite, tree-sitter grammars, grep engine all compiled into a single static binary
- **Worktree isolation** — each git worktree gets a separate index at `~/.wonk/index/<hash>/`; walker skips nested `.git` boundaries

### Supported Languages (tree-sitter)

TypeScript, TSX, JavaScript, Python, Rust, Go, Java, C, C++, Ruby, PHP, C#

## Specifications

Detailed product requirements and architecture docs live in `specs/`:
- `specs/product_specs.md` — PRD with EARS requirements
- `specs/architecture.md` — architectural decisions and component design
- `specs/tasks.md` — development task tracking
