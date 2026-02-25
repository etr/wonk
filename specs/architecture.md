# System Architecture

**Version:** 0.3
**Last updated:** 2026-02-24
**Status:** Draft
**Owner:** TBD

---

## 1) Executive Summary

Wonk is a single-binary Rust CLI tool that provides structure-aware code search optimized for LLM coding agents. Its core value is **reducing token burn**: where raw grep returns hundreds of noisy, unranked lines that consume an agent's context window, Wonk uses structural understanding to filter, rank, and deduplicate results — delivering higher signal in fewer tokens.

It combines a Tree-sitter-based structural indexer with the `grep` crate (ripgrep internals) for text search, backed by SQLite for persistent storage. A Smart Search layer sits between the query router and the output, using index metadata to rank results (definitions before usages, deduplication of re-exports, deprioritization of tests and comments) and optionally enforcing a token budget.

**V2 adds semantic search via embeddings.** When structural and text search can only find code matching syntactically, semantic search bridges the vocabulary gap — `wonk ask "authentication"` finds `verifyToken`, `checkCredentials`, and `validateSession` even though the word "authentication" never appears. Embeddings are generated via Ollama (`nomic-embed-text`), stored as BLOBs in the existing SQLite database, and searched via brute-force cosine similarity. Building on embeddings, V2 also adds semantic dependency analysis (scope-limited semantic search via the dep graph), clustering (discover conceptual groupings in a directory), and change impact analysis (find semantically related code affected by changes).

The architecture prioritizes simplicity and low resource usage. A single Rust crate organized into modules handles both CLI queries and background indexing. The daemon process shares the SQLite database with CLI invocations — no IPC protocol is needed. Concurrency uses sync Rust with `rayon` for parallel indexing; no async runtime is required since all workloads are CPU-bound or event-driven (filesystem watching). V2's Ollama HTTP calls use `ureq`, a sync blocking HTTP client that fits the no-async constraint.

**V3 adds source display, code summaries, and call graph analysis.** `wonk show` collapses symbol lookup + file reading into a single call that returns exactly the source span tree-sitter already knows — halving round-trip latency for LLM agents. `wonk summary` provides structural metrics and optional LLM-generated descriptions of files and directories via Ollama's `/api/generate` endpoint. `wonk callers`, `wonk callees`, and `wonk callpath` enable symbol-level call graph navigation by recording the enclosing function for each call-site reference during indexing, enabling agents to trace execution paths and understand blast radius at the function level.

Key technology choices: Rust for single static binary distribution and native Tree-sitter/SQLite FFI, SQLite with FTS5 for persistent symbol storage, the `grep` and `ignore` crates from ripgrep for text search and file filtering, `notify` for cross-platform filesystem watching, `ureq` for sync HTTP to Ollama, and `linfa-clustering` for K-Means clustering.

---

## 2) Architectural Drivers

### 2.1 Business Drivers
- **Token efficiency:** Raw grep is the #1 token burner in LLM coding agents. Wonk returns ranked, deduplicated, structure-aware results that use ≥ 50% fewer tokens while preserving ≥ 95% of relevant results.
- Drop-in grep replacement for LLM coding agents — zero integration work
- Zero-config first use — auto-initializes on first query
- Single binary, no external dependencies (V1) — trivial to install and distribute
- **Vocabulary gap bridging (V2):** Semantic search finds functionally related code even when terminology doesn't overlap — essential for LLM agents searching by intent rather than exact names
- **Round-trip reduction (V3):** `wonk show` eliminates the symbol-lookup-then-file-read round-trip; `wonk summary` provides high-level orientation without reading every file; call graph commands trace execution paths without manual grep chains

### 2.2 Quality Attributes (from PRD NFRs)

| Attribute | Requirement | Architecture Response |
|-----------|-------------|----------------------|
| Latency (warm) | < 100ms query response | SQLite indexed lookups + FTS5 for symbol name search |
| Latency (cold) | < 5s first query on 5k-file repo | Parallel Tree-sitter parsing via rayon |
| Latency (contention) | < 50ms blocking during daemon writes | SQLite busy_timeout handles brief write contention |
| Latency (embedding) | Dependent on Ollama throughput | Batch embedding during init; incremental via daemon; block-and-wait with progress for queries |
| Latency (semantic query) | < 200ms for 50k vectors | Ollama query embed ~10-50ms + brute-force dot product ~25-100ms = ~35-150ms total |
| Index freshness | < 1s after file save | 500ms debounce + ~50ms parse/write = ~550ms typical |
| Daemon idle memory | < 15 MB (V1); ~20 MB with V2 ureq client loaded | No async runtime overhead; sync Rust + rayon; ureq adds minimal baseline |
| Daemon idle CPU | ~0% | Blocked on OS filesystem events (inotify/FSEvents) |
| Binary size | < 30 MB | Static binary with bundled SQLite, Tree-sitter grammars, grep engine |
| Storage | ~1 MB per 10k symbols | SQLite with appropriate indexes |
| Storage (embeddings) | ~3 KB per symbol (768 × f32) | BLOBs in SQLite; ~146 MB for 50k vectors |
| Storage (V3 additions) | ~4 bytes per reference (caller_id); ~0.5 KB per summary cache entry | Additive: ~4 MB per 1M references; summaries table negligible for typical repos |
| Latency (show) | < 50ms for source display | Index lookup + file read; no re-parse needed |
| Latency (summary) | < 100ms structural; 1-5s with `--semantic` | Index aggregation; LLM generation cached in SQLite |
| Latency (callers/callees) | < 100ms depth-1; < 500ms depth-10 | SQL JOIN on `caller_id`; BFS traversal for callpath |

### 2.3 Constraints
- **Language:** Rust (required for single static binary, native Tree-sitter FFI, grep crate access)
- **No async runtime:** Sync Rust + rayon only (DR-002); ureq for sync HTTP (DR-009)
- **No IPC:** CLI and daemon communicate only via shared SQLite (DR-003)
- **WAL mode:** SQLite WAL journal mode for concurrent reader/writer access (DR-004)
- **Conditional network dependency (V2):** Ollama required only for semantic features; all V1 features remain fully offline

---

## 3) System Overview

### 3.1 High-Level Architecture Diagram

```
┌─────────────────────────────────────────────────────────────┐
│                      CLI (wonk)                              │
│  clap-derived command parser                                 │
│  Subcommands: search, sym, ref, sig, ls, deps, rdeps,       │
│               init, update, status, daemon, repos,           │
│               ask, cluster, impact                  [V2]     │
│               show, summary, callers, callees,      [V3]     │
│               callpath                              [V3]     │
├─────────────────────────────────────────────────────────────┤
│                     Query Router                              │
│  Routes queries to index, grep, or semantic backends          │
├─────────────────────────────────────────────────────────────┤
│             Smart Search Ranker                               │
│  Ranks, deduplicates, and budget-caps results                 │
│  Blends structural + semantic results for --semantic  [V2]    │
├──────────────────┬──────────────────┬───────────────────────┤
│ Structural Index │   Text Search    │  Semantic Engine [V2]  │
│ (Tree-sitter +   │   (grep crate)   │  (Embedding + Cosine)  │
│  SQLite + FTS5)  │                  │                        │
├──────────────────┴──────────────────┴───────────────────────┤
│                     SQLite Database                           │
│  symbols, references, files, symbols_fts,                     │
│  daemon_status, embeddings [V2]                               │
├─────────────────────────────────────────────────────────────┤
│                   Background Daemon                           │
│  notify + crossbeam-channel + rayon                           │
│  File watcher → debounce → re-index → SQLite                  │
│  Embedding re-index on file change (if Ollama up) [V2]        │
├─────────────────────────────────────────────────────────────┤
│                   Ollama (external) [V2]                       │
│  nomic-embed-text model, localhost:11434                      │
│  POST /api/embed — batch embedding                            │
└─────────────────────────────────────────────────────────────┘
```

### 3.2 Component Summary

| Component | Responsibility | Technology |
|-----------|---------------|------------|
| CLI | Parse commands, dispatch to query router, format output | clap 4.5 (derive), serde_json, serde_toon2 |
| Query Router | Route queries to SQLite index, grep fallback, or semantic engine | Custom module |
| Smart Search Ranker | Rank, deduplicate, and budget-cap search results using structural metadata | Custom module |
| Structural Index | Parse files, extract symbols/references, manage index | tree-sitter 0.26, rusqlite 0.38 |
| Text Search | Grep-compatible text search across files | grep 0.4, ignore 0.4 |
| SQLite Database | Persistent storage for symbols, references, metadata, embeddings | rusqlite 0.38 (bundled + FTS5) |
| Background Daemon | Watch filesystem, debounce events, re-index changed files, re-embed changed chunks | notify 8.x, notify-debouncer-mini, crossbeam-channel, rayon |
| Configuration | Load and merge global/per-repo TOML config | toml 0.8 |
| Embedding Engine [V2] | Chunk symbols, call Ollama API, store/retrieve vectors | ureq 3.1, bytemuck |
| Semantic Search [V2] | Cosine similarity search, result blending, dependency-scoped queries | rayon, custom module |
| Clustering Engine [V2] | K-Means clustering of symbol embeddings | linfa-clustering 0.8, ndarray |
| Impact Analyzer [V2] | Detect changed symbols, find semantically similar code | Custom module (git CLI, embedding comparison) |
| Source Display [V3] | Look up symbol in index, read source span from file | Custom module (index query + file read) |
| Code Summary Engine [V3] | Structural metrics aggregation, LLM description generation + caching | Custom module, ureq (Ollama `/api/generate`) |
| Call Graph [V3] | Record enclosing callers during indexing, traverse caller/callee relationships | Custom module (Tree-sitter parent traversal, BFS) |

---

## 4) Component Details

### 4.1 CLI

**Responsibility:** Parse user commands, dispatch to query router, format and print results.

**Technology:** clap 4.5 (derive API), serde + serde_json for JSON output, serde_toon2 for TOON output

**Interfaces:**
- Exposes: `wonk` binary with subcommands (search, sym, ref, sig, ls, deps, rdeps, init, update, status, daemon, repos, ask [V2], cluster [V2], impact [V2])
- Consumes: Query Router, Configuration

**Key Design Notes:**
- Global `--format` flag available on all commands (grep, json, toon)
- Global `--raw` flag bypasses the Smart Search Ranker, returning unranked grep-style output (PRD-SSRCH-REQ-006)
- Default output format is `file:line:content` (grep-compatible)
- On invocation, checks for running daemon and auto-spawns if needed (PRD-DMN-REQ-002)
- On first use with no index, triggers auto-initialization (PRD-AUT-REQ-001)
- Daemon management subcommands: `wonk daemon start`, `wonk daemon stop`, `wonk daemon status`, `wonk daemon list` (PRD-DMN-REQ-014), `wonk daemon stop --all` (PRD-DMN-REQ-015)
- V2 subcommands:
  - `wonk ask <query>` — semantic search with `--budget`, `--json`, `--from`, `--to` flags
  - `wonk cluster <path>` — semantic clustering with `--json` flag
  - `wonk impact <file>` — change impact analysis with `--since`, `--json` flags
  - `wonk search --semantic` — blended structural + semantic results
- V3 subcommands:
  - `wonk show <name>` — source display with `--file`, `--kind`, `--exact`, `--shallow`, `--budget` flags
  - `wonk summary <path>` — structural summary with `--detail`, `--depth`, `--recursive`, `--semantic` flags
  - `wonk callers <symbol>` — list caller functions with `--depth` flag
  - `wonk callees <symbol>` — list callee functions with `--depth` flag
  - `wonk callpath <from> <to>` — find call chain between two symbols
- **Auto-init embedding delegation (PRD-SEM-REQ-009):** When auto-init is triggered by a query, the CLI builds the structural index synchronously, then writes a `embedding_build_requested = 1` flag to the `daemon_status` table. The daemon reads this flag on startup and begins embedding generation in the background.
- **Block-and-wait for incomplete embeddings (PRD-SEM-REQ-013):** When `wonk ask` detects embeddings are incomplete, the CLI calls `Embedding Engine::embed_repo()` directly with a progress callback that prints to stderr, blocking until complete, then proceeds with the semantic query.

**Related Requirements:** PRD-OUT-REQ-001, PRD-OUT-REQ-002, PRD-OUT-REQ-003, PRD-AUT-REQ-001, PRD-DMN-REQ-002, PRD-DMN-REQ-011 through PRD-DMN-REQ-015, PRD-SSRCH-REQ-006, PRD-SEM-REQ-001 through PRD-SEM-REQ-005, PRD-SEM-REQ-009, PRD-SEM-REQ-013, PRD-SCLST-REQ-001 through PRD-SCLST-REQ-003, PRD-SIMP-REQ-001 through PRD-SIMP-REQ-004, PRD-SHOW-REQ-001 through PRD-SHOW-REQ-013, PRD-SUM-REQ-001 through PRD-SUM-REQ-018, PRD-CGR-REQ-001 through PRD-CGR-REQ-014

### 4.2 Query Router

**Responsibility:** Route each query to the appropriate backend (SQLite index, grep crate, or semantic engine) and manage fallback logic.

**Technology:** Custom Rust module

**Interfaces:**
- Exposes: Unified query API consumed by CLI
- Consumes: Structural Index (SQLite), Text Search (grep crate), Semantic Search (V2)

**Key Design Notes:**
- Routing table:
  | Command | Primary | Fallback |
  |---------|---------|----------|
  | `wonk search` | grep crate (always) | — |
  | `wonk search --semantic` | grep crate + semantic engine | — |
  | `wonk sym` | SQLite symbols table | grep with heuristic patterns |
  | `wonk ref` | SQLite references table | grep for name occurrences |
  | `wonk deps` | SQLite import data | grep for import/require statements |
  | `wonk ls` | SQLite symbols by file | Tree-sitter on-demand parse |
  | `wonk sig` | SQLite symbols table | grep with heuristic patterns |
  | `wonk rdeps` | SQLite import data | grep for import/require statements |
  | `wonk ask` [V2] | Semantic engine (embeddings) | Error if Ollama unavailable |
  | `wonk cluster` [V2] | Clustering engine (embeddings) | Error if no embeddings |
  | `wonk impact` [V2] | Impact analyzer (embeddings + git) | Error if no embeddings |
- Fallback is triggered when primary returns no results
- Error types from `thiserror` enable matching on `NoIndex` vs `QueryFailed` vs `NoEmbeddings` (V2)

**Related Requirements:** PRD-FBK-REQ-001 through PRD-FBK-REQ-005, PRD-SIG-REQ-001, PRD-LST-REQ-001, PRD-LST-REQ-002, PRD-DEP-REQ-001, PRD-DEP-REQ-002, PRD-SEM-REQ-001, PRD-SEM-REQ-002, PRD-SEM-REQ-012

### 4.3 Smart Search Ranker

**Responsibility:** Take raw search results (from either SQLite index or grep fallback), enrich them with structural metadata, rank by relevance, deduplicate, and enforce token budgets.

**Technology:** Custom Rust module, no additional dependencies

**Interfaces:**
- Exposes: `rank_results(raw_results, index_metadata, options) -> RankedResults`
- Consumes: SQLite Database (for symbol/reference metadata), raw grep results

**Key Design Notes:**
- **Ranking tiers** (highest to lowest priority):
  1. Symbol definitions (function, class, type declarations)
  2. Call sites / direct usages
  3. Import/export statements
  4. Comments and documentation
  5. Test files (detected by path heuristics: `test/`, `tests/`, `*_test.*`, `*.test.*`, `*.spec.*`)
- **Deduplication:** When the same symbol appears in multiple locations due to re-exports, barrel files, or type declaration files, collapse to the canonical definition and note `(+N other locations)`
- **Token budget mode (`--budget <n>`):** Estimate tokens per result line (~4 chars/token heuristic), emit results in rank order until budget exhausted, append `-- N more results truncated --` summary
- **Category headers:** When ranked mode is active, insert headers between tiers: `-- definitions --`, `-- usages --`, `-- imports --`, `-- tests --`
- **Bypass:** `--raw` flag skips the ranker entirely, returning unranked grep-style output
- **Symbol detection:** On `wonk search <pattern>`, check if pattern matches any symbol name in the index. If yes, use ranked mode. If no, use plain text search (no ranking).
- Applied as a post-processing step — does not change the underlying search engines
- **V2 blending (PRD-SEM-REQ-002):** When `--semantic` is provided on `wonk search`, structural matches are presented first, followed by additional semantic matches not already present, each annotated with cosine similarity score.

**Related Requirements:** PRD-SSRCH-REQ-001 through PRD-SSRCH-REQ-006, PRD-SEM-REQ-002, PRD-SEM-REQ-004

### 4.4 Structural Index (Indexer)

**Responsibility:** Parse source files with Tree-sitter, extract symbols and references, write to SQLite.

**Technology:** tree-sitter 0.26, per-language grammar crates (10 languages), rayon for parallelism

**Interfaces:**
- Exposes: `index_repo(path) -> Result<()>`, `index_file(path) -> Result<()>`
- Consumes: SQLite Database, File Walker (ignore crate)

**Key Design Notes:**
- Full index (`wonk init`): Walk files with `ignore` crate (respects .gitignore, .wonkignore, default exclusions), parse in parallel with rayon, batch-insert into SQLite
- Incremental index (daemon): Re-parse single files, delete old data, insert new data in a transaction
- Extracts per the PRD: functions, methods, classes, structs, interfaces, enums, traits, type aliases, constants, exported symbols, function calls, type annotations, import statements
- Records file metadata: language, line count, content hash (xxhash), import/export list
- Tree-sitter grammars bundled at compile time — one `tree-sitter-{lang}` crate per language
- **Worktree boundary exclusion (DR-008):** The `WalkBuilder` uses a `filter_entry` callback that checks each directory for a `.git` entry (file or directory). If found and the directory is not the repo root itself, the entire subtree is skipped — treating it as a separate repository or worktree boundary.

**Related Requirements:** PRD-IDX-REQ-001 through PRD-IDX-REQ-011, PRD-SYM-REQ-001 through PRD-SYM-REQ-004, PRD-REF-REQ-001 through PRD-REF-REQ-003, PRD-WKT-REQ-003

### 4.5 Text Search

**Responsibility:** Grep-compatible text search across files, used as primary backend for `wonk search` and as fallback for structural queries.

**Technology:** grep 0.4 (ripgrep internals), ignore 0.4 (file walking)

**Interfaces:**
- Exposes: `text_search(pattern, options) -> Results`
- Consumes: Filesystem (via ignore crate walker)

**Key Design Notes:**
- `wonk search` always goes through the grep crate, never the index
- Supports regex mode (`--regex`), case-insensitive (`-i`), path restriction (`-- <path>`)
- Used as fallback backend by Query Router with heuristic patterns (e.g., `fn <name>`, `def <name>`, `function <name>` for symbol fallback)
- ignore crate handles .gitignore, .wonkignore, hidden files, and default exclusions

**Related Requirements:** PRD-SRCH-REQ-001 through PRD-SRCH-REQ-005, PRD-FBK-REQ-001 through PRD-FBK-REQ-003

### 4.6 Background Daemon

**Responsibility:** Watch filesystem for changes, keep the structural index current via incremental re-indexing, and keep embeddings current by re-embedding changed files when Ollama is reachable (V2).

**Technology:** notify 8.x, notify-debouncer-mini, crossbeam-channel, rayon

**Interfaces:**
- Exposes: None (standalone background process)
- Consumes: SQLite Database, Structural Index, Embedding Engine (V2)

**Key Design Notes:**
- Spawned as a separate OS process by the CLI (fork/exec or `std::process::Command`)
- Event loop: `notify` → `notify-debouncer-mini` (500ms window) → `crossbeam-channel` → process batch
- On file change: hash file (xxhash), compare to stored hash, skip if unchanged, else re-parse and update index
- On file delete: remove all symbols, references, metadata, and embeddings for that file
- On new file: detect language, parse if supported, add to index
- Writes heartbeat/status to `daemon_status` table in SQLite (DR-003)
- **Runs indefinitely** — no idle timeout (PRD-DMN-REQ-003 removed; daemons persist until explicitly stopped)
- Single instance per repo enforced via PID file
- Detaches from parent process (daemonizes) so CLI can exit immediately
- **Multi-daemon management (DR-013):** `wonk daemon list` scans PID files via glob `~/.wonk/repos/*/daemon.pid` to discover all running daemons. `wonk daemon stop --all` iterates and sends SIGTERM to each (PRD-DMN-REQ-014, PRD-DMN-REQ-015).
- **Worktree boundary filtering (DR-008):** The `should_process` event filter checks whether an event path falls within a nested worktree boundary by walking ancestor directories (between the event path and the repo root) for `.git` entries. Events inside a nested boundary are discarded. Cost is O(depth) `exists()` calls per event, negligible since events are debounced.
- **V2 embedding re-indexing:** After structural re-indexing of changed files, if Ollama is reachable, re-generate chunks and re-embed all symbols belonging to the changed files (PRD-SEM-REQ-010). If Ollama is unreachable, skip embedding update silently and mark affected files as stale in the embeddings table (PRD-SEM-REQ-011).
- **V3 caller_id population:** Incremental re-indexing of changed files must also populate `caller_id` on new reference rows using the same Tree-sitter parent traversal logic as the full index build (DR-021). This ensures call graph data stays current as files change.

**Related Requirements:** PRD-DMN-REQ-001 through PRD-DMN-REQ-015, PRD-WKT-REQ-004, PRD-SEM-REQ-009, PRD-SEM-REQ-010, PRD-SEM-REQ-011

### 4.7 Configuration

**Responsibility:** Load, merge, and provide configuration values to all components.

**Technology:** toml 0.8

**Interfaces:**
- Exposes: `Config` struct consumed by all components
- Consumes: `~/.wonk/config.toml` (global), `.wonk/config.toml` (per-repo)

**Key Design Notes:**
- Load order: defaults → global config → per-repo config (last wins)
- All config is optional — sensible defaults baked in
- Config sections: `[daemon]`, `[index]`, `[output]`, `[ignore]`, `[llm]` [V3]
- **V2 change:** `daemon.idle_timeout_minutes` config key removed — daemons now run indefinitely (PRD-DMN-REQ-003 removed, PRD-CFG-REQ-004 struck through). See DR-013 for rationale.
- **V3 change:** `[llm]` section added with `model` key (default: `"llama3.2:3b"`) for `wonk summary --semantic` text generation (DR-018, PRD-SUM-REQ-014).

**Related Requirements:** PRD-CFG-REQ-001 through PRD-CFG-REQ-010, PRD-SUM-REQ-014

### 4.8 Embedding Engine [V2]

**Responsibility:** Generate embedding chunks from indexed symbols, call Ollama to embed them, store and retrieve embedding vectors from SQLite.

**Technology:** ureq 3.1 (sync HTTP), bytemuck (zero-copy BLOB cast), serde + serde_json

**Interfaces:**
- Exposes: `embed_repo(db) -> Result<()>`, `embed_file(db, file) -> Result<()>`, `embed_query(query) -> Result<Vec<f32>>`, `get_embedding(symbol_id) -> Result<Vec<f32>>`
- Consumes: SQLite Database (symbols + embeddings tables), Ollama API

**Key Design Notes:**
- **Chunking strategy (PRD-SEM-REQ-006):** One chunk per tree-sitter symbol definition. Each chunk includes: file path, parent scope, import context, and the symbol's source code. This context-rich format gives the embedding model enough information to understand what the code does.
  ```
  File: src/auth/middleware.ts
  Scope: AuthMiddleware
  Imports: jwt, UserRepo
  ---
  async verifyToken(token: string): Promise<User | null> {
    const decoded = jwt.verify(token, this.secret);
    return this.userRepo.findById(decoded.sub);
  }
  ```
- **Full-file fallback (PRD-SEM-REQ-007):** Files with no extractable tree-sitter symbols (config files, markdown, scripts) are treated as a single chunk for embedding.
- **Ollama API (DR-009):** POST to `http://localhost:11434/api/embed` with `{"model": "nomic-embed-text", "input": [...]}`. Batch multiple chunks per request. Response contains `{"embeddings": [[...], ...]}`.
- **Vector storage (DR-010):** Embeddings stored as raw little-endian f32 BLOBs in the `embeddings` table. Read back with `bytemuck::cast_slice::<u8, f32>()` for zero-copy deserialization.
- **Pre-normalization:** All vectors are L2-normalized before storage so cosine similarity reduces to a dot product at query time.
- **Embedding dimensions (DR-012):** Full 768 dimensions from `nomic-embed-text` — no truncation.
- **Build flow:**
  - Explicit `wonk init` with Ollama reachable: build embeddings alongside structural index, display progress (PRD-SEM-REQ-008)
  - Explicit `wonk init` with Ollama unreachable: skip embeddings with warning, structural index only (PRD-SEM-REQ-014)
  - Auto-init triggered by query: structural index only, delegate embedding to daemon (PRD-SEM-REQ-009)
- **Stale tracking:** Each row in `embeddings` has a `stale` flag. When a file changes and Ollama is unreachable, the daemon sets `stale = 1` for that file's embeddings. On next query, stale embeddings are still searched but may be less accurate.

**Related Requirements:** PRD-SEM-REQ-006 through PRD-SEM-REQ-016

### 4.9 Semantic Search [V2]

**Responsibility:** Perform cosine similarity search against stored embeddings, optionally scope results by dependency graph, and blend with structural results.

**Technology:** rayon (parallel dot product), custom module

**Interfaces:**
- Exposes: `semantic_search(query_vec, options) -> Vec<SemanticResult>`, `blended_search(structural_results, semantic_results) -> Vec<BlendedResult>`
- Consumes: SQLite Database (embeddings + symbols tables), Embedding Engine (for query embedding)

**Key Design Notes:**
- **Brute-force cosine similarity (DR-010, PRD-SEM-REQ-016):** Load all embeddings into memory, compute dot product (vectors are pre-normalized) in parallel with rayon. Expected performance: ~25-100ms for 50K vectors on 8 cores.
- **Result format (PRD-SEM-REQ-003):** Each result includes file path, line number, symbol name, symbol kind, and cosine similarity score.
- **Block-and-wait (PRD-SEM-REQ-013):** If embeddings are incomplete when `wonk ask` is run, block and display progress while building embeddings, then return results.
- **Dependency scoping (PRD-SDEP):**
  - `--from <file>`: Filter semantic results to symbols in files reachable via forward dependencies from the specified file (PRD-SDEP-REQ-001)
  - `--to <file>`: Filter semantic results to symbols in files that transitively import the specified file (PRD-SDEP-REQ-002)
  - **Transitive traversal algorithm (PRD-SDEP-REQ-003):** Compute reachable file set using BFS/DFS over the file-level dependency graph stored in SQLite. Starting from the specified file, iteratively follow import edges (forward for `--from`, reverse for `--to`) until no new files are discovered. The reachable set is then used to filter embedding results before ranking. Implemented in Rust (not SQL recursive CTE) to avoid SQLite recursion limits on deep dependency chains.
- **Blending (PRD-SEM-REQ-002):** On `wonk search --semantic`, structural matches presented first, then additional semantic matches not already present, each with similarity score.
- **Ollama unavailable (PRD-SEM-REQ-012):** Return clear error message stating Ollama is required for semantic search.

**Related Requirements:** PRD-SEM-REQ-001 through PRD-SEM-REQ-005, PRD-SEM-REQ-012, PRD-SEM-REQ-013, PRD-SEM-REQ-016, PRD-SDEP-REQ-001 through PRD-SDEP-REQ-003

### 4.10 Clustering Engine [V2]

**Responsibility:** Group symbol embeddings by semantic similarity using K-Means clustering and present labeled clusters.

**Technology:** linfa-clustering 0.8.1, ndarray (DR-011)

**Interfaces:**
- Exposes: `cluster_path(db, path, options) -> Vec<Cluster>`
- Consumes: SQLite Database (embeddings + symbols tables)

**Key Design Notes:**
- **Algorithm (DR-011):** K-Means with K-Means++ initialization via `linfa-clustering`. Pure Rust, no BLAS dependency, no async.
- **Auto-k selection:** Run K-Means for k = 2..√n, compute silhouette score for each, select k with highest silhouette. Cap k at a reasonable maximum (e.g., 20).
- **Cluster labeling (PRD-SCLST-REQ-002):** Each cluster lists its most representative symbols — those closest to the cluster centroid — and the files they belong to. Default: top 5 representative symbols per cluster, ranked by ascending distance to centroid. Configurable via `--top <n>` flag.
- **Output (PRD-SCLST-REQ-001, PRD-SCLST-REQ-003):** Default output shows labeled groups; `--json` outputs structured JSON with cluster members, centroids, and distances.
- **Data preparation:** Load embeddings for all symbols within the specified path, construct ndarray matrix, run K-Means fitting.

**Related Requirements:** PRD-SCLST-REQ-001 through PRD-SCLST-REQ-003

### 4.11 Impact Analyzer [V2]

**Responsibility:** Detect symbols that changed in a file (or set of files), find semantically similar symbols elsewhere, and present them as potentially impacted code.

**Technology:** Custom module, git CLI (for `--since`), embedding comparison

**Interfaces:**
- Exposes: `analyze_impact(db, file, options) -> Vec<ImpactResult>`
- Consumes: SQLite Database (embeddings + symbols tables), Embedding Engine, git CLI

**Key Design Notes:**
- **Symbol change detection (DR-014):** For `wonk impact <file>`, re-parse the file with Tree-sitter and compare current symbols against the indexed version (by name, kind, and content hash). Changed or new symbols are identified without shelling out to git.
- **`--since <commit>` (DR-014, PRD-SIMP-REQ-002):** Shell out to `git diff --name-only <commit>` to get the list of changed files, then analyze each file as above.
- **Semantic similarity:** For each changed symbol, embed its current source (via Ollama) and compare against all stored embeddings. Results ranked by descending similarity (PRD-SIMP-REQ-001).
- **Result format (PRD-SIMP-REQ-003):** Each result is an `ImpactResult` containing: `changed_symbol` (name, kind, file, line), `impacted_symbol` (name, kind, file, line), `similarity_score` (f32), and `file_path` (of the impacted symbol). Defined in `types.rs`.
- **Output (PRD-SIMP-REQ-004):** Default human-readable format; `--json` for structured JSON matching the `ImpactResult` fields.

**Related Requirements:** PRD-SIMP-REQ-001 through PRD-SIMP-REQ-004

### 4.12 Source Display [V3]

**Responsibility:** Look up symbols by name in the index, read their source spans from the source file, and format output with line numbers.

**Technology:** Custom Rust module, no additional dependencies

**Interfaces:**
- Exposes: `show_symbol(name, options) -> Vec<ShowResult>`
- Consumes: SQLite Database (symbols table), Filesystem (source files)

**Key Design Notes:**
- **Source span reading (PRD-SHOW-REQ-001):** Query `symbols` table for matching name, read file lines `line..end_line` for each match. Prefix each line with its 1-based file line number (PRD-SHOW-REQ-008).
- **Multiple matches (PRD-SHOW-REQ-002):** Display all matches, each preceded by a file header showing `file:start_line-end_line`.
- **Filtering:** `--file <path>` restricts to symbols in that file (PRD-SHOW-REQ-003). `--kind <kind>` restricts to symbol kind (PRD-SHOW-REQ-004). `--exact` requires exact name match (PRD-SHOW-REQ-005).
- **Shallow mode (DR-017, PRD-SHOW-REQ-006):** For container types (class, struct, enum, trait, interface), query child symbols via `scope` column match in the same file, then display the container's signature line followed by each child's `signature` field. No Tree-sitter re-parse needed — uses existing index data.
- **Budget truncation (PRD-SHOW-REQ-007):** Uses the existing budget module (~4 chars/token heuristic) to truncate output and indicate omission.
- **No index fallback (PRD-SHOW-REQ-009):** Unlike other commands, `wonk show` requires an index. Returns an error directing user to `wonk init` if no index exists.
- **Missing end_line (PRD-SHOW-REQ-010):** If a symbol has no `end_line`, fall back to displaying the `signature` text from the index.
- **Missing source file (PRD-SHOW-REQ-011):** If the source file no longer exists, skip the result and emit a warning to stderr.
- **Structured output (PRD-SHOW-REQ-012):** JSON/TOON response includes fields: `name` (string), `kind` (string), `file` (string), `line` (integer), `end_line` (integer|null), `source` (string — full source body), `language` (string). Defined as `ShowResult` in `types.rs`.
- **MCP exposure (PRD-SHOW-REQ-013):** `wonk_show` tool with parameters: name (required), kind (optional), file (optional), exact (boolean), shallow (boolean), budget (integer), format (json|toon).

**Related Requirements:** PRD-SHOW-REQ-001 through PRD-SHOW-REQ-013

### 4.13 Code Summary Engine [V3]

**Responsibility:** Aggregate structural metrics for files and directories from the index, optionally generate natural-language descriptions via Ollama LLM, and cache descriptions in SQLite.

**Technology:** Custom Rust module, ureq 3.1 (Ollama `/api/generate`), serde_json

**Interfaces:**
- Exposes: `summarize_path(db, path, options) -> SummaryResult`
- Consumes: SQLite Database (symbols, files, summaries tables), Ollama API (`/api/generate`)

**Key Design Notes:**
- **Structural metrics (PRD-SUM-REQ-001, PRD-SUM-REQ-002):** Query `files` and `symbols` tables to aggregate: file count, line count, symbol count by kind, language breakdown, dependency count. Three detail levels control which metrics are included (PRD-SUM-REQ-003 through PRD-SUM-REQ-005).
- **Recursion (PRD-SUM-REQ-006 through PRD-SUM-REQ-008):** `--depth N` recursively summarizes children up to N levels. `--recursive` is unlimited depth. Default is depth 0 (target only).
- **LLM description (DR-018, PRD-SUM-REQ-009):** When `--semantic` is provided, construct a prompt containing structural metrics and symbol signatures, send to Ollama's `POST /api/generate` endpoint with the configured model. Default model is `llama3.2:3b`, overridable via `[llm].model` in config.toml.
- **LLM prompt construction (PRD-SUM-REQ-010):** The prompt includes: path being summarized, language breakdown, symbol signatures grouped by kind, and import/export relationships. Asks for a 2-3 sentence description of the path's purpose and contents.
- **Description caching (DR-019, DR-020, PRD-SUM-REQ-011, PRD-SUM-REQ-012):** Cache LLM descriptions in the `summaries` table keyed by path + content hash. Content hash is computed from sorted `(symbol.id, file.hash)` pairs under the path. Cache hit returns instantly without calling Ollama.
- **Ollama unavailable (PRD-SUM-REQ-013):** Display warning and return structural summary without description.
- **Model not found (PRD-SUM-REQ-015):** Distinct from Ollama-unreachable. If Ollama is reachable but responds with model-not-found for the configured (or default) model, return an error instructing the user to run `ollama pull <model>` or set `[llm].model` in config.toml. This is an error (not a warning with fallback) because the user explicitly requested `--semantic`.
- **Structured output (PRD-SUM-REQ-017):** JSON/TOON response includes fields: `path` (string), `type` (`"file"` | `"directory"`), `detail_level` (`"rich"` | `"light"` | `"symbols"`), `metrics` (object: `file_count`, `line_count`, `symbol_count_by_kind`, `language_breakdown`, `dependency_count` — subset varies by detail level), `children` (array of `SummaryResult`, present if depth > 0 or `--recursive`), `description` (string, present only if `--semantic` and Ollama successful). Defined as `SummaryResult` in `types.rs`.
- **Semantic + recursion interaction (PRD-SUM-REQ-009, PRD-SUM-REQ-006):** When both `--semantic` and `--depth N` (or `--recursive`) are specified, the LLM description is generated only for the top-level target path, not for each child. Generating per-child descriptions would be prohibitively slow (1-5s per Ollama call).
- **Configuration (DR-018):** `[llm]` section in config.toml with `model` key (default: `"llama3.2:3b"`). Additional keys: `generate_url` (default: `"http://localhost:11434/api/generate"`).
- **Auto-init (PRD-SUM-REQ-016):** Consistent with PRD-AUT behavior — builds index on first use.
- **MCP exposure (PRD-SUM-REQ-018):** `wonk_summary` tool with parameters: path (required), detail (optional: rich|light|symbols), depth (optional integer), recursive (optional boolean), semantic (optional boolean), format (json|toon).

**Related Requirements:** PRD-SUM-REQ-001 through PRD-SUM-REQ-018

### 4.14 Call Graph [V3]

**Responsibility:** Record enclosing caller symbols during indexing, query caller/callee relationships, and find call paths between symbols via BFS traversal.

**Technology:** Custom Rust module, no additional dependencies beyond existing Tree-sitter and SQLite

**Interfaces:**
- Exposes: `callers(db, name, depth) -> Vec<CallerResult>`, `callees(db, name, depth) -> Vec<CalleeResult>`, `callpath(db, from, to) -> Option<Vec<CallPathHop>>`
- Consumes: SQLite Database (references + symbols tables via `caller_id` JOIN)

**Key Design Notes:**
- **Enclosing symbol detection (DR-021, PRD-CGR-REQ-001):** During Tree-sitter parsing in the indexer, when a call-site node is encountered, walk up `node.parent()` to find the nearest enclosing function/method node. Record its `symbols.id` as `caller_id` on the reference row. File-scope calls (no enclosing function) get `caller_id = NULL`, treated as `<module>` scope at query time (PRD-CGR-REQ-002).
- **Data model (DR-015):** `caller_id INTEGER REFERENCES symbols(id)` added to the `references` table. Nullable for file-scope calls. Indexed for efficient JOIN queries.
- **Callers query (PRD-CGR-REQ-003):** `SELECT DISTINCT s.* FROM references r JOIN symbols s ON r.caller_id = s.id WHERE r.name = ?` — returns all functions whose bodies contain a call to the named symbol.
- **Callees query (PRD-CGR-REQ-004):** `SELECT DISTINCT r.name, ... FROM references r WHERE r.caller_id IN (SELECT id FROM symbols WHERE name LIKE ?)` — returns all symbols called within the named function's body.
- **Transitive expansion (PRD-CGR-REQ-005, PRD-CGR-REQ-006):** `--depth N` iteratively expands at each level: depth 1 = direct only, depth 2 = callers/callees of callers/callees, etc. Default depth 1 (PRD-CGR-REQ-007). Cap at depth 10 with warning (PRD-CGR-REQ-008).
- **Call path (DR-016, PRD-CGR-REQ-009):** BFS from `<from>` symbol expanding callees at each level. Maintains a visited set and parent map to reconstruct the shortest path when `<to>` is reached. Returns the chain of intermediate hops. Reports "no path found" if BFS exhausts the graph (PRD-CGR-REQ-010). Depth capped at 10.
- **Multiple definitions (PRD-CGR-REQ-011):** When the named symbol has multiple definitions (e.g., same name in different files), include callers/callees from all definitions and indicate which definition each result corresponds to.
- **Auto-init (PRD-CGR-REQ-012):** Consistent with PRD-AUT behavior — auto-initializes the structural index on first use. Fresh indexes include `caller_id` data from the start. Existing indexes without `caller_id` return empty call graph results with a hint to re-index via `wonk update`.
- **Index rebuild requirement:** Existing indexes lack `caller_id` data. Repos must re-index (`wonk update`) to populate caller relationships. Call graph queries on old indexes return empty results with a hint to re-index.
- **MCP exposure (PRD-CGR-REQ-013, PRD-CGR-REQ-014):** `wonk_callers` and `wonk_callees` tools with parameters: name (required), depth (optional, default 1, max 10), format (json|toon). `wonk_callpath` tool with parameters: from (required), to (required), format (json|toon).

**Related Requirements:** PRD-CGR-REQ-001 through PRD-CGR-REQ-014

### 4.15 MCP Server

**Responsibility:** Expose wonk query capabilities as MCP (Model Context Protocol) tools over JSON-RPC 2.0 stdio, enabling AI coding assistants to use wonk without CLI invocation.

**Technology:** Custom Rust module, serde_json for JSON-RPC serialization

**Interfaces:**
- Exposes: 14 MCP tools over stdio (JSON-RPC 2.0)
- Consumes: All query backends (Query Router, Semantic Search, Clustering Engine, Impact Analyzer, Source Display, Code Summary Engine, Call Graph)

**Key Design Notes:**
- **Tool manifest (14 tools):**
  | Tool | Backend | Added |
  |------|---------|-------|
  | `wonk_search` | Text Search + Smart Search Ranker | V1 |
  | `wonk_sym` | Structural Index | V1 |
  | `wonk_ref` | Structural Index | V1 |
  | `wonk_sig` | Structural Index | V1 |
  | `wonk_ls` | Structural Index | V1 |
  | `wonk_deps` | Structural Index | V1 |
  | `wonk_rdeps` | Structural Index | V1 |
  | `wonk_init` | Pipeline | V1 |
  | `wonk_status` | SQLite Database | V1 |
  | `wonk_show` | Source Display | V3 |
  | `wonk_summary` | Code Summary Engine | V3 |
  | `wonk_callers` | Call Graph | V3 |
  | `wonk_callees` | Call Graph | V3 |
  | `wonk_callpath` | Call Graph | V3 |
- **Routing:** Each tool handler delegates to its backend component using the same code paths as the CLI subcommands. MCP tools and CLI commands produce identical results.
- **Parameter mapping:** MCP tool parameters map 1:1 to CLI flags. See sections 4.12, 4.13, 4.14 for parameter specifications per tool.
- **Error handling:** Backend errors are returned as JSON-RPC error responses with human-readable messages.

**Related Requirements:** PRD-SHOW-REQ-013, PRD-SUM-REQ-018, PRD-CGR-REQ-013, PRD-CGR-REQ-014

---

## 5) Data Architecture

### 5.1 Data Stores

| Store | Type | Purpose | Location |
|-------|------|---------|----------|
| SQLite index.db | Relational (SQLite) | Symbols, references, file metadata, FTS5 index, daemon status, embeddings (V2) | `~/.wonk/repos/<sha256-short>/index.db` (central) or `.wonk/index.db` (local) |
| meta.json | JSON file | Repo path, creation time, detected languages | Alongside index.db |
| daemon.pid | Text file | PID of running daemon process | Alongside index.db |
| config.toml | TOML file | User configuration overrides | `~/.wonk/config.toml` (global) or `.wonk/config.toml` (per-repo) |

### 5.2 SQLite Schema

```sql
-- Core symbol table
CREATE TABLE symbols (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,          -- function, class, method, type, constant, variable, module, interface, enum, struct, trait
    file TEXT NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    end_line INTEGER,
    scope TEXT,                  -- parent symbol (e.g., class name for a method)
    signature TEXT,              -- full signature text for display
    language TEXT NOT NULL
);

-- Name-based references (all usages of a symbol name)
CREATE TABLE references (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,          -- matched by name to symbols
    file TEXT NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    context TEXT,                -- the full line of source for display
    caller_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL  -- [V3] enclosing function (DR-015); NULL for file-scope calls
);

-- File metadata
CREATE TABLE files (
    path TEXT PRIMARY KEY,
    language TEXT,
    hash TEXT NOT NULL,          -- content hash (xxhash) for change detection
    last_indexed INTEGER NOT NULL,
    line_count INTEGER,
    symbols_count INTEGER
);

-- Daemon status (DR-003: status table for CLI to read)
CREATE TABLE daemon_status (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);
-- Keys: 'pid', 'state', 'last_activity', 'files_queued', 'last_error', 'uptime_start'

-- Full-text search on symbol names
CREATE VIRTUAL TABLE symbols_fts USING fts5(name, kind, file, content=symbols, content_rowid=id);

-- [V2] Embedding vectors for semantic search (DR-010, PRD-SEM-REQ-015)
CREATE TABLE embeddings (
    id INTEGER PRIMARY KEY,
    symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    file TEXT NOT NULL,          -- denormalized for efficient per-file operations
    chunk_text TEXT NOT NULL,    -- the context-rich chunk that was embedded
    vector BLOB NOT NULL,        -- 768 × f32 = 3072 bytes, little-endian, L2-normalized
    stale INTEGER NOT NULL DEFAULT 0,  -- 1 if file changed but re-embedding failed
    created_at INTEGER NOT NULL,
    UNIQUE(symbol_id)
);

-- [V2] Index for per-file embedding operations (daemon re-embedding, file deletion)
CREATE INDEX idx_embeddings_file ON embeddings(file);

-- [V3] Cached LLM descriptions for wonk summary --semantic (DR-020)
CREATE TABLE summaries (
    path TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,  -- hash of sorted (symbol.id, file.hash) pairs under path (DR-019)
    description TEXT NOT NULL,   -- LLM-generated natural-language description
    created_at INTEGER NOT NULL
);

-- Indexes
CREATE INDEX idx_symbols_name ON symbols(name);
CREATE INDEX idx_symbols_file ON symbols(file);
CREATE INDEX idx_symbols_kind ON symbols(kind);
CREATE INDEX idx_references_name ON references(name);
CREATE INDEX idx_references_file ON references(file);
CREATE INDEX idx_references_caller ON references(caller_id);  -- [V3] for callers/callees queries (DR-015)
```

### 5.3 Data Flow

**Index build (`wonk init`):**
1. Walk repo with `ignore` crate (respects .gitignore, .wonkignore, default exclusions)
2. Parallel parse with rayon: each file → Tree-sitter → symbols + references + metadata
3. Batch insert into SQLite (within transactions for atomicity)
4. Populate FTS5 index
5. Write meta.json
6. Spawn daemon
7. [V2] If Ollama is reachable: generate chunks from each symbol, batch-embed via Ollama, store vectors in `embeddings` table, display progress (PRD-SEM-REQ-008)
8. [V2] If Ollama is unreachable: skip embedding with warning, structural index only (PRD-SEM-REQ-014)

**Incremental update (daemon):**
1. `notify` detects filesystem event
2. `notify-debouncer-mini` batches events over 500ms window
3. For each file: hash → compare → skip if unchanged → re-parse → delete old rows → insert new rows (single transaction per file)
4. Update `daemon_status` table
5. [V2] If Ollama is reachable: re-generate chunks for changed symbols, re-embed, update `embeddings` table (PRD-SEM-REQ-010)
6. [V2] If Ollama is unreachable: set `stale = 1` on affected embeddings (PRD-SEM-REQ-011)

**Query (`wonk sym <name>`):**
1. CLI opens read-only SQLite connection with `busy_timeout=5000`
2. Query Router checks index: `SELECT * FROM symbols WHERE name LIKE '%<name>%'` (or FTS5 for performance)
3. If results found → format and print
4. If no results → fall back to grep crate with heuristic patterns

**Source display (`wonk show <name>`) [V3]:**
1. CLI opens read-only SQLite connection
2. Query `symbols` table for matching name (with optional `--kind`, `--file`, `--exact` filters)
3. For each matching symbol: read source file lines `line..end_line`
4. If `--shallow` and symbol is a container type: query child symbols via `scope` match, display container signature + child signatures (DR-017)
5. If `--budget` specified: truncate output to token budget
6. Format with line numbers and file headers

**Code summary (`wonk summary <path>`) [V3]:**
1. CLI opens SQLite connection
2. Query `files` and `symbols` tables to aggregate metrics for the target path
3. If `--depth N` or `--recursive`: recursively aggregate for child paths
4. If `--semantic`: compute content hash from `(symbol.id, file.hash)` pairs (DR-019), check `summaries` table for cache hit
5. On cache miss: construct prompt with structural metrics + symbol signatures, call Ollama `POST /api/generate` with configured model (DR-018), store result in `summaries` table (DR-020)
6. If Ollama unreachable: return structural summary with warning

**Callers/callees query (`wonk callers <symbol>`) [V3]:**
1. CLI opens read-only SQLite connection
2. Query `references` JOIN `symbols` on `caller_id` to find callers (or callees via reverse join)
3. If `--depth > 1`: iteratively expand at each level using BFS, up to depth cap of 10
4. Format results with file path, line, symbol name, kind

**Call path query (`wonk callpath <from> <to>`) [V3]:**
1. CLI opens read-only SQLite connection
2. Resolve `<from>` and `<to>` to symbol IDs via `symbols` table
3. BFS from `<from>` expanding callees at each level (DR-016), maintaining visited set + parent map
4. If `<to>` reached: reconstruct shortest path via parent map, return chain of hops
5. If BFS exhausts graph or depth cap reached: return "no path found"

**Semantic query (`wonk ask <query>`) [V2]:**
1. CLI opens SQLite connection
2. Check if embeddings exist; if incomplete, block and build with progress (PRD-SEM-REQ-013)
3. Embed the query string via Ollama (`POST /api/embed`); error if Ollama unreachable (PRD-SEM-REQ-012)
4. L2-normalize the query vector
5. Load all stored embedding vectors from `embeddings` table
6. Compute dot product (= cosine similarity for normalized vectors) in parallel with rayon
7. Sort by descending similarity
8. If `--from` or `--to` specified: filter by dependency reachability (PRD-SDEP)
9. If `--budget` specified: truncate to token budget (PRD-SEM-REQ-004)
10. Format output with file path, line, symbol name, kind, similarity score (PRD-SEM-REQ-003)

### 5.4 Index Location Strategy

Central mode (default):
```
~/.wonk/
  repos/
    <sha256-short-of-repo-path>/
      index.db
      meta.json
      daemon.pid
  config.toml
```

Local mode (`wonk init --local`):
```
.wonk/
  index.db
  meta.json
  daemon.pid
  config.toml       # per-repo overrides
```

Repo root discovery: walk up from CWD looking for `.git` or `.wonk`. Accepts both `.git` directories (regular repos) and `.git` files (linked worktrees) as valid markers (PRD-WKT-REQ-001). The nearest match wins, so a worktree nested inside another repo resolves to the worktree's own root (PRD-WKT-REQ-002).

Repo path hash: SHA256 of the canonical repo root path, truncated to first 16 hex chars. Each worktree has its own root path and therefore its own hash — producing a separate index directory automatically (PRD-WKT-REQ-005).

---

## 6) Integration Architecture

### 6.1 External Integrations

**V1:** None. Wonk is a standalone CLI tool with no network dependencies.

**V2:**

| System | Protocol | Purpose | Failure Handling |
|--------|----------|---------|------------------|
| Ollama | HTTP (localhost:11434) | Embedding generation via `nomic-embed-text` | Graceful degradation: V1 features work without Ollama; `wonk ask` returns clear error; daemon skips re-embedding and marks files stale |
| Ollama [V3] | HTTP (localhost:11434) | Text generation via `/api/generate` for `wonk summary --semantic` | Graceful degradation: structural summary returned without description; warning emitted |

**Ollama API details:**
- Embed endpoint: `POST http://localhost:11434/api/embed`
  - Request: `{"model": "nomic-embed-text", "input": ["chunk1", "chunk2", ...]}`
  - Response: `{"embeddings": [[f32; 768], ...]}`
  - Batch size: Multiple chunks per request for throughput
- Generate endpoint [V3]: `POST http://localhost:11434/api/generate`
  - Request: `{"model": "<configured model>", "prompt": "<summary prompt>", "stream": false}`
  - Response: `{"response": "<generated text>", ...}`
  - Default model: `llama3.2:3b` (configurable via `[llm].model` in config.toml)
- Reachability check: `GET http://localhost:11434/` (returns 200 if running)
- No authentication required (localhost-only)

### 6.2 Internal Communication

| From | To | Mechanism | Notes |
|------|----|-----------|-------|
| CLI | SQLite | Direct file access (rusqlite) | Read-only connection with busy_timeout |
| Daemon | SQLite | Direct file access (rusqlite) | Read-write connection with busy_timeout |
| CLI | Daemon | PID file + OS signals | SIGTERM for stop, PID file for status check |
| CLI | Daemon status | SQLite daemon_status table | Daemon writes status, CLI reads it |
| CLI | Ollama [V2] | HTTP via ureq | Query embedding for `wonk ask` |
| Daemon | Ollama [V2] | HTTP via ureq | Batch embedding for incremental re-indexing |

No sockets, no IPC protocols, no serialization between processes.

---

## 7) Security Architecture

### 7.1 File Access
- Wonk operates with the user's filesystem permissions — no privilege escalation
- Index is stored in user-owned directories (`~/.wonk/` or `.wonk/`)
- Daemon runs as the invoking user

### 7.2 Data Protection
- No sensitive data is stored (source code is already on disk)
- No encryption at rest (index is a cache, not a store of record)
- V1: No network communication — no encryption in transit needed
- V2: Ollama communication is localhost-only (127.0.0.1:11434) — no data leaves the machine. Embedding vectors are derived from source code already on disk. No TLS needed for localhost connections.

### 7.3 PID File Safety
- PID file is checked for stale PIDs (process no longer running) before spawning a new daemon
- PID file is removed on clean daemon shutdown

### 7.4 Ollama Trust Model [V2]
- Ollama is assumed to be a trusted local service controlled by the user
- No authentication is performed (Ollama doesn't support it for localhost)
- Source code chunks are sent to localhost only — never transmitted over a network
- If Ollama is compromised, the impact is limited to incorrect embeddings (no write access to index)

---

## 8) Infrastructure & Deployment

### 8.1 Build Targets

| Platform | Target Triple | Priority | Build Method |
|----------|---------------|----------|-------------|
| macOS ARM | aarch64-apple-darwin | P0 | cross |
| macOS x86_64 | x86_64-apple-darwin | P0 | cross |
| Linux x86_64 | x86_64-unknown-linux-musl | P0 | cross |
| Linux ARM | aarch64-unknown-linux-musl | P1 | cross |
| Windows x86_64 | x86_64-pc-windows-msvc | P2 | cross |

Note: Linux targets use musl for fully static binaries.

### 8.2 CI/CD Pipeline

```
GitHub Actions workflow:
  on: [push to main, pull request, release tag]

  jobs:
    test:
      - cargo test (Linux)
      - cargo clippy
      - cargo fmt --check

    build:
      matrix: [5 platform targets]
      - cross build --release --target <triple>
      - Strip binary
      - Verify binary size < 30 MB
      - Upload artifact

    release (on tag):
      - Download all artifacts
      - Create GitHub Release with binaries
      - Publish to crates.io (cargo publish)
```

### 8.3 Install Methods

| Method | Command | Notes |
|--------|---------|-------|
| Cargo | `cargo install wonk` | Builds from source |
| Homebrew | `brew install wonk` | Prebuilt binary via tap |
| Direct download | `curl -fsSL https://wonk.dev/install.sh \| sh` | Platform-detected binary |
| npm | `npm install -g @wonk/cli` | Wrapper package for JS ecosystem |

---

## 9) Observability

### 9.1 Logging
- Daemon logs to stderr (captured if redirected) or syslog
- CLI prints hints to stderr (e.g., "run `wonk init` to enable fast structural search")
- No structured logging in V1 — keep it simple

### 9.2 Metrics
- `wonk status` displays: file count, symbol count, reference count, index size, last indexed time, daemon state
- V2 additions to `wonk status`: embedding count, stale embedding count, Ollama reachability
- Daemon writes status to `daemon_status` table: PID, state, last activity, queue depth, last error

### 9.3 Tracing
- Not applicable for V1 (single-process CLI, no distributed system)

### 9.4 Alerting
- Not applicable for V1 (local tool, no server)

---

## 10) Cost Model

| Component | Cost |
|-----------|------|
| GitHub Actions CI | Free (public repo) or included minutes (private) |
| Distribution hosting | GitHub Releases (free) |
| Homebrew tap | GitHub repo (free) |
| Ollama (V2) | Free, open-source, runs locally — no API costs |
| **Total** | **$0/month** |

---

## 11) Decision Records

### DR-001: Project Structure

**Status:** Accepted
**Date:** 2026-02-11
**Context:** Wonk has several logical components (CLI, indexer, search, db, daemon, config). Need to decide how to organize the Rust codebase. (Affects all features)

**Options Considered:**
1. **Single crate with modules** - One Cargo.toml, modules in src/
   - Pros: Simplest setup, easy refactoring
   - Cons: Full recompile on any change, harder to enforce API boundaries
2. **Cargo workspace from the start** - 3-4 crates (wonk-cli, wonk-core, wonk-daemon)
   - Pros: Clean API boundaries, faster incremental compilation
   - Cons: More boilerplate, premature boundaries may shift
3. **Single crate now, workspace later** - Start simple, refactor when boundaries stabilize
   - Pros: Fast initial development, refactor with confidence later
   - Cons: Refactoring cost later (mitigated by Rust's type system)

**Decision:** Option 3 — Single crate now, workspace later

**Rationale:** Boundaries between indexer, search, daemon, and CLI will become clearer once there's a working prototype. Premature workspace setup often leads to wrong boundaries. Follows ripgrep's evolution pattern.

**Consequences:**
- Initial project is a single `Cargo.toml` with `src/` modules
- Will refactor to workspace when module boundaries stabilize (likely after core features work end-to-end)
- Refactoring is safe due to Rust's type system

---

### DR-002: Concurrency Model

**Status:** Accepted
**Date:** 2026-02-11
**Context:** Wonk needs parallelism for indexing (CPU-bound Tree-sitter parsing) and an event loop for the daemon (filesystem watching). PRD requires < 5s cold init, < 50ms single-file re-index, < 15 MB daemon idle memory.

**Options Considered:**
1. **Sync Rust + rayon** - No async runtime. rayon for parallel indexing, blocking event loop with notify + crossbeam-channel
   - Pros: Minimal memory, rayon ideal for CPU-bound work, simpler mental model, file I/O not truly async
   - Cons: No built-in timeout/cancellation, would need to add tokio if V2 needs network I/O
2. **Tokio async runtime** - tokio for daemon event loop and spawn_blocking for Tree-sitter
   - Pros: Built-in timeouts/cancellation, familiar to many Rust devs
   - Cons: 2-5 MB runtime overhead, mixing tokio+rayon causes deadlocks, file I/O gains nothing from async
3. **Tokio for daemon, rayon for indexing** - Hybrid
   - Pros: tokio's select!/timers for daemon, rayon for CPU work
   - Cons: Two concurrency models, deadlock risk when mixing

**Decision:** Option 1 — Sync Rust + rayon

**Rationale:** The daemon watches files and writes to SQLite — no network I/O. File I/O is not truly async on Linux/macOS. rayon is purpose-built for the CPU-bound Tree-sitter parsing workload. No async runtime keeps idle memory well under 15 MB and avoids the tokio/rayon mixing pitfall entirely.

**Consequences:**
- No `.await` anywhere in the codebase
- Daemon event loop uses `crossbeam-channel::select!` for timeouts
- V2 network calls (Ollama) use ureq (sync/blocking) — no async runtime needed (DR-009)
- Timeout/cancellation handled via crossbeam channels and atomic flags

---

### DR-003: Daemon Architecture

**Status:** Accepted
**Date:** 2026-02-11
**Context:** The daemon is a background process keeping the index fresh. CLI needs to query the index and check daemon status. Need to decide how CLI and daemon communicate. (PRD-DMN)

**Options Considered:**
1. **Shared SQLite, no IPC** - CLI and daemon both access SQLite directly. PID file for lifecycle.
   - Pros: Simplest, CLI works even if daemon is down, aligns with graceful degradation
   - Cons: Limited daemon status info (just PID), no real-time progress streaming
2. **Unix domain socket IPC** - Daemon listens on a socket. CLI connects for queries/status.
   - Pros: Rich status, could route queries through daemon
   - Cons: Wire protocol, platform differences, adds failure mode, undermines graceful degradation
3. **Shared SQLite + status table** - Option 1 plus daemon writes status to a `daemon_status` table
   - Pros: Simplicity of Option 1, richer status than PID file alone, one access pattern for CLI
   - Cons: Status slightly stale (periodic writes), adds a table

**Decision:** Option 3 — Shared SQLite + status table

**Rationale:** All the simplicity of no IPC, with useful daemon status info. The daemon writes heartbeat, queue depth, and last error to SQLite periodically. CLI reads it like any other query. Fully aligned with graceful degradation — CLI never depends on daemon being reachable.

**Consequences:**
- `daemon_status` table added to schema
- Daemon writes status on each index update and periodically (heartbeat)
- `wonk daemon status` reads from this table + checks PID file
- No socket, no wire protocol, no serialization between processes

---

### DR-004: SQLite Concurrency Strategy

**Status:** Accepted
**Date:** 2026-02-11
**Context:** Daemon writes index updates while CLI reads for queries. Need to choose SQLite journal mode and concurrency strategy. PRD requires < 100ms query latency (under typical conditions) and < 50ms single-file re-index.

**Options Considered:**
1. **WAL mode with busy timeout** - Readers never block writers, writers never block readers
   - Pros: Best concurrency, proven pattern
   - Cons: WAL file can grow, slightly more disk usage
2. **WAL mode with connection pooling** - WAL plus r2d2-sqlite pool
   - Pros: Could parallelize batch inserts
   - Cons: SQLite only allows one writer regardless of pool, added complexity for no benefit
3. **Rollback journal (default SQLite)** - Default mode, serialize all access
   - Pros: Zero configuration, simplest possible
   - Cons: Writers block readers during commits (brief, ~5-20ms per file transaction)

**Decision:** Option 1 — WAL mode with busy timeout

**Rationale:** WAL mode allows concurrent readers and a single writer without blocking. Write transactions are small (one file's symbols at a time, ~5-20ms), so contention is minimal. `PRAGMA busy_timeout=5000` ensures retries rather than failures if two writers coincide. This is the proven pattern for daemon+CLI workloads sharing a SQLite database.

**Consequences:**
- CLI queries proceed without blocking during daemon writes (readers never block)
- `busy_timeout=5000` set on all connections to handle writer contention gracefully
- WAL file may grow during sustained writes; SQLite checkpoints automatically
- Slightly more disk usage than rollback journal (WAL + shared-memory files)

---

### DR-005: Crate Selections

**Status:** Accepted
**Date:** 2026-02-11 (updated 2026-02-13 for V2 additions)
**Context:** Need to select key Rust dependencies aligned with architecture decisions (sync + rayon, WAL mode SQLite, single binary).

**Options Considered:** For each role, the selected crate is the ecosystem standard. Alternatives considered and rejected inline.

**Decision:** Full crate selection:

| Role | Crate | Version | Notes |
|------|-------|---------|-------|
| CLI parsing | clap (derive) | 4.5.x | Standard, declarative |
| SQLite | rusqlite (bundled) | 0.38.x | Statically links SQLite with FTS5 support |
| Tree-sitter | tree-sitter | 0.26.x | Official Rust bindings |
| TS grammars | tree-sitter-{lang} | latest | 10 language crates bundled at compile time |
| Text search | grep | 0.4.x | Ripgrep internals as library |
| File filtering | ignore | 0.4.x | Gitignore-compatible walker from ripgrep |
| Parallel indexing | rayon | 1.x | CPU-bound parallel iteration |
| File watching | notify | 8.x | Cross-platform filesystem events |
| Event debouncing | notify-debouncer-mini | 0.5.x | Deduplicates rapid filesystem events |
| Channels | crossbeam-channel | 0.5.x | Blocking channels for daemon event loop |
| Content hashing | xxhash-rust | 0.8.x | Fast file content hashing for change detection |
| Repo path hashing | sha2 | 0.10.x | SHA256-short for central index directory names |
| Config parsing | toml | 0.8.x | Parse config.toml files |
| JSON output | serde + serde_json | 1.x | Structured output for --format json |
| TOON output | serde_toon2 | 0.1.x | Structured output for --format toon |
| Error handling (app) | anyhow | 1.x | Ergonomic errors for CLI/application code |
| Error handling (lib) | thiserror | 2.x | Typed errors for component boundaries |
| HTTP client [V2] | ureq | 3.1.x | Sync/blocking HTTP for Ollama API; `features = ["json"]` (DR-009) |
| Zero-copy cast [V2] | bytemuck | 1.x | Cast `&[u8]` BLOB ↔ `&[f32]` slice without copying (DR-010) |
| Clustering [V2] | linfa-clustering | 0.8.x | K-Means++ with silhouette scoring (DR-011) |
| Numeric arrays [V2] | ndarray | 0.16.x | Matrix representation for linfa input (DR-011) |

**Rationale:** Each crate is the ecosystem standard for its role. `rusqlite` bundled feature includes FTS5. `grep` and `ignore` are from ripgrep, ensuring compatibility with grep-style output. `xxhash-rust` for fast content hashing, `sha2` for repo path hashing (matching PRD's SHA256 specification). V2 additions: `ureq` maintains the no-async constraint (DR-002) while adding network capability; `bytemuck` enables zero-copy BLOB deserialization; `linfa-clustering` provides pure-Rust K-Means without BLAS dependency.

**Consequences:**
- All grammars compiled into binary (adds ~10-15 MB, within 30 MB budget)
- `rusqlite` bundled feature compiles SQLite from source (adds to build time but ensures FTS5)
- `grep` crate documentation is sparse — may need to reference ripgrep source for usage patterns
- tree-sitter 0.26.x: avoid deprecated `set_timeout_micros` and `set_cancellation_flag` APIs
- V2 crates add minimal binary size impact (~1-2 MB estimated)

---

### DR-006: Error Handling Strategy

**Status:** Accepted
**Date:** 2026-02-11
**Context:** Wonk needs to distinguish between error types for fallback logic (e.g., "no index" triggers grep fallback, "query failed" is a real error). Also needs good error messages for CLI users. (PRD-FBK)

**Options Considered:**
1. **anyhow everywhere** - anyhow::Result throughout
   - Pros: Minimal boilerplate, great .context() chains
   - Cons: Can't match on specific errors for fallback logic
2. **thiserror for library, anyhow for CLI** - Typed errors at component boundaries, anyhow in CLI glue
   - Pros: Fallback logic can match on DbError::NoIndex vs QueryFailed, clean separation
   - Cons: Slightly more boilerplate
3. **Custom error types only** - thiserror only, no anyhow
   - Pros: Full control
   - Cons: Excessive boilerplate for a CLI tool

**Decision:** Option 2 — thiserror for library boundaries, anyhow for CLI

**Rationale:** The Query Router needs to match on error variants to decide whether to fall back to grep search. `thiserror` at component boundaries (db, indexer, search) enables this. `anyhow` in CLI code provides ergonomic error context and formatting. Standard Rust pattern for applications with library-like internals.

**Consequences:**
- Define error enums: `DbError`, `IndexError`, `SearchError`, `EmbeddingError` (V2) with `thiserror`
- Query Router matches on `DbError::NoIndex` to trigger fallback
- V2: Query Router matches on `EmbeddingError::OllamaUnreachable` to return clear user-facing error
- CLI wraps everything in `anyhow::Result` for display
- Error messages are user-friendly (no raw panics or debug output)

---

### DR-007: Cross-Compilation & CI/CD

**Status:** Accepted
**Date:** 2026-02-11
**Context:** Need to build static binaries for 5 platform targets (PRD-DST-REQ-004 through 007). Binary includes C dependencies (SQLite, Tree-sitter grammars) that need cross-compilation support.

**Options Considered:**
1. **GitHub Actions + cross** - Docker-based cross-compilation for all targets
   - Pros: Handles C toolchains automatically, simple build matrix, free for public repos
   - Cons: Docker builds slower than native, Windows cross-compilation can be finicky
2. **Native runners per platform** - macos-latest, ubuntu-latest, windows-latest
   - Pros: No cross-compilation issues, faster per-build
   - Cons: macOS x86_64 still needs cross, Linux ARM still needs cross, more CI config
3. **Hybrid** - Native for P0, cross for P1/P2
   - Pros: Most reliable for important targets
   - Cons: More complex CI config

**Decision:** Option 1 — GitHub Actions + cross

**Rationale:** Simplest to configure and maintain. `cross` handles the C toolchain complexity for SQLite and Tree-sitter FFI across all 5 targets. Docker overhead is acceptable for release builds. Local development uses native `cargo build`.

**Consequences:**
- CI workflow uses build matrix with 5 target triples
- Linux targets use musl for fully static binaries
- Release workflow triggered by git tags
- Binary size verified in CI (< 30 MB assertion)
- May need to revisit Windows cross-compilation if C FFI issues arise

---

### DR-008: Worktree Boundary Detection Strategy

**Status:** Accepted
**Date:** 2026-02-12
**Context:** Git worktrees and nested repositories inside a parent repo's directory tree must not be indexed or watched by the parent. The walker and file watcher need a mechanism to detect `.git` entries in subdirectories and treat them as boundaries. (PRD-WKT-REQ-001 through PRD-WKT-REQ-005; boundary detection specifically addresses PRD-WKT-REQ-003 and PRD-WKT-REQ-004, while repo root discovery in section 5.4 addresses PRD-WKT-REQ-001 and PRD-WKT-REQ-002, and index location hashing addresses PRD-WKT-REQ-005)

**Options Considered:**
1. **Inline boundary check (filter callback)** — Per-directory `exists()` check in walker's `filter_entry()` and watcher's `should_process()`
   - Pros: Simplest (~20 lines per component), no cache, always correct when worktrees are added/removed
   - Cons: One `exists()` syscall per directory in walker; O(depth) checks per watcher event batch
2. **Pre-computed boundary set** — Scan for nested `.git` entries at startup, maintain `HashSet<PathBuf>`
   - Pros: O(1) per-event lookup
   - Cons: Stale when worktrees created/deleted, extra startup cost, more state to manage
3. **Hybrid (inline walker, pre-computed watcher)** — Different mechanisms per component
   - Pros: Fast watcher lookups
   - Cons: Two mechanisms for same concept, over-engineered for V1

**Decision:** Option 1 — Inline boundary check

**Rationale:** The cost is negligible — the walker already performs stat calls for gitignore processing, and watcher events are debounced (batched over 500ms). Always correct without caching concerns. If profiling later shows the watcher check is a bottleneck, upgrading to Option 3 is a backward-compatible change.

**Consequences:**
- Walker's `WalkBuilder` gains a `filter_entry` callback that skips directories containing `.git` (unless it's the repo root)
- Watcher's `should_process` gains ancestor-path checking for `.git` boundaries between the event path and repo root
- No new data structures or caches
- Automatically handles dynamic worktree creation/deletion

---

### DR-009: HTTP Client for Ollama Communication [V2]

**Status:** Accepted
**Date:** 2026-02-13
**Context:** V2 semantic features require HTTP communication with Ollama for embedding generation. Need to select an HTTP client that fits the existing no-async architecture (DR-002). (PRD-SEM-REQ-006 through PRD-SEM-REQ-016)

**Options Considered:**
1. **ureq** — Sync/blocking HTTP client, no async runtime
   - Pros: Fits DR-002 (no async), `features = ["json"]` for easy `send_json()`/`read_json()`, ~73M downloads, rustls TLS backend, minimal dependencies
   - Cons: Blocks the calling thread during HTTP calls (acceptable for CLI and daemon)
2. **reqwest (blocking)** — reqwest with `features = ["blocking"]`
   - Pros: Popular, well-documented, cookie/redirect support
   - Cons: Pulls in tokio even in blocking mode (~2-5 MB binary impact), conflicts with DR-002
3. **minreq** — Minimal HTTP client
   - Pros: Tiny dependency footprint
   - Cons: No JSON support, less maintained, manual serialization

**Decision:** Option 1 — ureq

**Rationale:** ureq is purpose-built for sync/blocking HTTP — exactly what the no-async architecture requires. The `json` feature enables `request.send_json(&body).and_then(|r| r.read_json())` for clean Ollama API calls. No TLS needed since Ollama is localhost-only, but rustls is available if needed later. No tokio dependency keeps binary small and avoids DR-002 conflicts.

**Consequences:**
- All Ollama HTTP calls are blocking — CLI blocks during query embedding, daemon blocks during batch embedding
- Daemon embedding runs on its own thread (not the watcher thread) to avoid blocking file event processing
- Connection timeout and read timeout configured via ureq builder
- No TLS overhead for localhost connections

---

### DR-010: Vector Storage Strategy [V2]

**Status:** Accepted
**Date:** 2026-02-13
**Context:** Need to store and retrieve 768-dimensional f32 embedding vectors for semantic search. Expected scale: up to 50K symbols per large repo. (PRD-SEM-REQ-015, PRD-SEM-REQ-016)

**Options Considered:**
1. **Plain BLOB in SQLite** — Store embeddings as raw little-endian f32 BLOBs, brute-force cosine similarity in Rust
   - Pros: Zero additional dependencies, zero-copy with bytemuck, rayon-parallelized brute-force is fast enough (~25-100ms for 50K vectors), no SQLite version compatibility issues
   - Cons: O(n) search, loads all vectors into memory for search
2. **sqlite-vec extension** — Loadable SQLite extension for vector search
   - Pros: SQL-level vector operations, ANN indexing for larger scale
   - Cons: Incompatible with rusqlite's `bundled` feature (SQLite version mismatch between compiled-in and extension), would require dynamic SQLite linking
3. **Naive SQL (individual floats)** — Store each dimension as a column or row
   - Pros: Pure SQL
   - Cons: 768 columns or rows per vector is impractical, terrible performance

**Decision:** Option 1 — Plain BLOB in SQLite

**Rationale:** For the expected scale (5K-50K symbols), brute-force cosine similarity with rayon is well within latency targets (~25-100ms on 8 cores). `bytemuck::cast_slice::<u8, f32>()` provides zero-copy deserialization of BLOBs. Pre-normalizing vectors at storage time reduces cosine similarity to a dot product. This avoids the sqlite-vec compatibility issue with rusqlite's bundled mode entirely.

**Consequences:**
- Embeddings table stores vectors as 3072-byte BLOBs (768 × 4 bytes)
- All vectors loaded into memory for search (~146 MB for 50K vectors at 768 dims)
- Brute-force search parallelized with rayon
- Pre-normalize all vectors at storage time (cosine sim = dot product)
- If scale exceeds ~100K vectors, may need ANN indexing (revisit then)

---

### DR-011: Clustering Algorithm [V2]

**Status:** Accepted
**Date:** 2026-02-13
**Context:** `wonk cluster <path>` needs to group symbol embeddings by semantic similarity. Need to choose an algorithm that works in 768-dimensional space with typical symbol counts (100-5000 per directory). (PRD-SCLST-REQ-001 through PRD-SCLST-REQ-003)

**Options Considered:**
1. **K-Means via linfa-clustering** — K-Means++ initialization, pure Rust, silhouette scoring for auto-k
   - Pros: Fast (O(n·k·d·i)), handles 768 dims well, deterministic-ish with K-Means++, linfa-clustering 0.8.1 is pure Rust with no BLAS requirement, ndarray for data representation
   - Cons: Requires choosing k (mitigated by silhouette auto-selection), assumes spherical clusters
2. **DBSCAN** — Density-based clustering, no k required
   - Pros: Auto-determines cluster count, finds arbitrary shapes
   - Cons: Curse of dimensionality — distance metrics break down in 768-dim space without PCA preprocessing, epsilon parameter hard to tune
3. **Hierarchical (agglomerative)** — Bottom-up merging
   - Pros: Dendogram output, no k required
   - Cons: O(n³) time complexity, impractical for > 5000 points

**Decision:** Option 1 — K-Means via linfa-clustering

**Rationale:** K-Means with K-Means++ initialization is the most practical choice for 768-dim embeddings at the expected scale. The silhouette method for auto-selecting k (try k = 2..√n, pick highest silhouette score) avoids requiring users to specify cluster count. DBSCAN fails without PCA in high dimensions, and hierarchical is too slow at O(n³). linfa-clustering 0.8.1 is pure Rust, no BLAS, no async — fits the architecture perfectly.

**Consequences:**
- `wonk cluster` runs K-Means for multiple k values and selects the best via silhouette scoring
- ndarray used for matrix representation of embeddings
- May produce suboptimal clusters for non-spherical distributions (acceptable for code similarity)
- Clustering is a batch operation (not incremental) — re-runs from scratch each time

---

### DR-012: Embedding Dimensions [V2]

**Status:** Accepted
**Date:** 2026-02-13
**Context:** `nomic-embed-text` supports Matryoshka dimension reduction (768/512/256/128/64). Need to decide whether to use full 768-dim vectors or truncate for smaller storage and faster search. (PRD-SEM-REQ-015, PRD-SEM-REQ-016)

**Options Considered:**
1. **Full 768 dimensions** — Use the complete embedding output
   - Pros: Maximum semantic fidelity, best similarity accuracy, recommended by model authors for code
   - Cons: ~3 KB per vector, ~146 MB for 50K vectors in memory during search
2. **Truncated to 256 dimensions** — Use Matryoshka truncation
   - Pros: 1 KB per vector, 3× faster brute-force, ~49 MB for 50K vectors
   - Cons: ~5-10% accuracy loss, less differentiation between similar symbols
3. **Configurable** — User chooses dimension count
   - Pros: Flexibility
   - Cons: Config complexity, all embeddings must use same dimension, re-embed on change

**Decision:** Option 1 — Full 768 dimensions

**Rationale:** Brute-force search at 768 dims is already within latency targets (~25-100ms for 50K vectors with rayon). Memory usage (~146 MB) is acceptable for a CLI tool running on developer machines. The marginal accuracy gain of full dimensions is worth more than the marginal performance gain of truncation, especially for distinguishing semantically similar code symbols.

**Consequences:**
- Each embedding vector is 3072 bytes (768 × f32)
- Search loads ~146 MB for 50K vectors (acceptable on modern dev machines)
- If memory becomes an issue for very large repos, truncation can be added as an opt-in later
- Embedding model choice is hardcoded to nomic-embed-text; dimension is always 768

---

### DR-013: Daemon Lifecycle & Multi-Daemon Management [V2]

**Status:** Accepted
**Date:** 2026-02-13
**Context:** V2 removes the 30-minute idle timeout (daemons run indefinitely to keep embeddings fresh). With daemons persisting across repos, users need visibility and control over all running daemons. (PRD-DMN-REQ-014, PRD-DMN-REQ-015)

**Options Considered:**
1. **PID file scanning** — `wonk daemon list` globs `~/.wonk/repos/*/daemon.pid`, reads each PID, checks if process is alive
   - Pros: No new infrastructure, works with existing PID file convention, simple implementation
   - Cons: O(n) filesystem scan per invocation (negligible for expected repo count)
2. **Central registry** — SQLite database at `~/.wonk/daemons.db` tracking all running daemons
   - Pros: O(1) lookup, richer metadata (start time, repo path, resource usage)
   - Cons: New database to manage, consistency issues if daemon crashes without cleanup, over-engineered

**Decision:** Option 1 — PID file scanning

**Rationale:** The existing convention of one PID file per repo in `~/.wonk/repos/<hash>/daemon.pid` already provides all the information needed. A glob + PID check takes < 10ms even for 100 repos. The `meta.json` alongside each PID file provides the repo path for display. No new infrastructure needed.

**Consequences:**
- `wonk daemon list` implementation: glob `~/.wonk/repos/*/daemon.pid`, read PID, verify alive, read `meta.json` for repo path
- `wonk daemon stop --all` implementation: iterate list, SIGTERM each
- Stale PID files (crashed daemons) are detected and cleaned up automatically
- Works identically for central and local mode indexes

---

### DR-014: Git Integration for Change Impact Analysis [V2]

**Status:** Accepted
**Date:** 2026-02-13
**Context:** `wonk impact` needs to detect which symbols changed in a file (for impact analysis) and which files changed since a commit (for `--since`). Need to decide between using the git2 crate, shelling out to git CLI, or a hybrid approach. (PRD-SIMP-REQ-001, PRD-SIMP-REQ-002)

**Options Considered:**
1. **git2 crate** — libgit2 Rust bindings for all git operations
   - Pros: In-process, type-safe, no external dependency
   - Cons: Heavy dependency (~5 MB binary impact), libgit2 lags behind git features, complex API for simple operations
2. **Git CLI** — Shell out to `git` for everything
   - Pros: Always up-to-date with latest git, simple for file listing
   - Cons: External dependency (git must be installed), parsing overhead, not great for symbol-level diffs
3. **Hybrid** — Index-based diff for symbol changes + git CLI for file listing
   - Pros: Symbol change detection uses existing Tree-sitter parse (no git needed), git CLI only for simple file listing (`git diff --name-only`), minimal external dependency
   - Cons: Two mechanisms, but each is the right tool for its job

**Decision:** Option 3 — Hybrid (index diff + git CLI)

**Rationale:** For `wonk impact <file>`: re-parse the file with Tree-sitter and compare current symbols against the indexed version (by name, kind, and content hash). This is fast, uses existing infrastructure, and doesn't need git at all. For `--since <commit>`: shell out to `git diff --name-only <commit>` to get the list of changed files — this is a simple, well-understood operation that doesn't justify pulling in libgit2. The hybrid approach avoids the ~5 MB binary impact of git2 while using each mechanism for what it does best.

**Consequences:**
- No git2 dependency — keeps binary lean
- `wonk impact <file>` works without git installed (purely index-based symbol diff)
- `wonk impact --since <commit>` requires git to be installed (reasonable assumption for developers)
- Symbol change detection compares: current Tree-sitter parse vs. stored symbols (name + kind + content hash)
- File change detection: `std::process::Command::new("git").args(["diff", "--name-only", commit])`

---

### DR-015: Call Graph Data Model [V3]

**Status:** Accepted
**Date:** 2026-02-24
**Context:** `wonk callers`, `wonk callees`, and `wonk callpath` require knowing which function contains each call-site reference. The current `references` table records name, file, line, col, and context — but no link to the enclosing symbol. (PRD-CGR-REQ-001 through PRD-CGR-REQ-014)

**Options Considered:**
1. **Add `caller_id` column to `references` table** — INTEGER column referencing `symbols(id)` for the enclosing function/method
   - Pros: Single table, simple JOINs, minimal schema change (one new nullable column), backward compatible (existing refs get NULL)
   - Cons: Nullable for file-scope refs; requires index rebuild to populate
2. **Separate `call_edges` table** — New table `call_edges(caller_symbol_id, callee_name, file, line)`
   - Pros: Clean separation, no nullable columns, call-graph-specific indexes
   - Cons: Duplicates data already in references, two tables to maintain, more complex indexer
3. **Add `caller_name` + `caller_file` columns to `references`** — Denormalized enclosing symbol info
   - Pros: Fast callers queries without JOIN
   - Cons: Denormalized duplication, harder to resolve to full symbol info

**Decision:** Option 1 — Add `caller_id` column to `references` table

**Rationale:** Simplest schema change that uses proper normalization with the existing `symbols` table. Enables rich queries via a single JOIN. The nullable `caller_id` for file-scope refs is a clean representation of PRD-CGR-REQ-002's `<module>` case. Existing indexes without `caller_id` simply return empty call graph results with a re-index hint.

**Consequences:**
- `references` table gains `caller_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL`
- New index: `idx_references_caller ON references(caller_id)` for efficient callers queries
- Existing repos must re-index (`wonk update`) to populate caller relationships
- Call graph queries on old indexes return empty results with hint to re-index

---

### DR-016: Call Graph Traversal Algorithm [V3]

**Status:** Accepted
**Date:** 2026-02-24
**Context:** `wonk callpath <from> <to>` needs to find a chain of calls connecting two symbols. `wonk callers --depth N` and `wonk callees --depth N` need transitive expansion at each depth level. The call graph is a directed graph where edges are caller→callee relationships. Typical codebase graphs are sparse with depth rarely exceeding 10-15 hops. PRD caps traversal at depth 10. (PRD-CGR-REQ-005 through PRD-CGR-REQ-010)

**Options Considered:**
1. **BFS (Breadth-First Search)** — BFS from `<from>` expanding callees, stopping when `<to>` found
   - Pros: Guarantees shortest path, simple queue + visited set, natural depth limiting, matches existing BFS pattern in `semantic.rs`
   - Cons: Explores all nodes at each depth level (memory proportional to branching factor)
2. **Bidirectional BFS** — BFS from both ends simultaneously
   - Pros: Much faster for deep graphs (√n exploration)
   - Cons: More complex, marginal benefit given depth-10 cap
3. **DFS with depth limit** — Depth-first with backtracking
   - Pros: Lower memory usage
   - Cons: Does NOT guarantee shortest path, may explore deep dead-ends first

**Decision:** Option 1 — BFS

**Rationale:** Guarantees shortest call path, matches the existing BFS pattern used for dependency traversal in `semantic.rs`, and the depth-10 cap keeps memory trivial. Bidirectional BFS is over-engineered for this scale. The same BFS approach applies to transitive callers/callees expansion: each BFS level corresponds to one depth increment, iteratively expanding from the starting symbol(s). Application-level BFS is preferred over SQL recursive CTEs to avoid SQLite recursion limits on deep call chains.

**Consequences:**
- `callpath` uses a simple BFS with queue + visited set + parent map
- Returns the shortest call chain (fewest hops)
- `callers --depth N` and `callees --depth N` use the same iterative BFS pattern, expanding one level per iteration
- Depth capped at 10 — BFS level corresponds directly to hop count
- Consistent pattern with `semantic.rs` dependency traversal

---

### DR-017: Source Display Shallow Mode [V3]

**Status:** Accepted
**Date:** 2026-02-24
**Context:** `wonk show --shallow` for container types (class, struct, enum, trait, interface) should display the container signature and member signatures without member bodies. Need to decide how to extract member signatures. (PRD-SHOW-REQ-006)

**Options Considered:**
1. **File read + Tree-sitter re-parse** — Re-parse the source span and extract direct children's signatures
   - Pros: Accurate even if index is slightly stale, works from live file
   - Cons: Adds Tree-sitter parse (~1-5ms per symbol), more complex code path
2. **Index-based child lookup** — Query symbols where `scope` matches the container name in the same file, display each child's `signature` field
   - Pros: No re-parse needed, pure index query, fast, uses existing data (`scope` + `signature` columns)
   - Cons: Depends on `scope` being correctly populated

**Decision:** Option 2 — Index-based child lookup

**Rationale:** Simpler implementation that leverages existing index data. The `scope` column is already populated by the indexer for methods under classes, and the `signature` column stores the text needed for display. Avoids an unnecessary Tree-sitter re-parse and aligns with the tool's philosophy of leveraging the index.

**Consequences:**
- Shallow mode queries: `SELECT signature FROM symbols WHERE scope = ? AND file = ?`
- Falls back to reading just the symbol's start line if `signature` is empty
- Depends on `scope` correctness — already validated by existing structural queries

---

### DR-018: LLM Generation Model Configuration [V3]

**Status:** Accepted
**Date:** 2026-02-24
**Context:** `wonk summary --semantic` generates descriptions via Ollama's `/api/generate` endpoint. Need to decide whether to require explicit model configuration or provide a sensible default. (PRD-SUM-REQ-014, PRD-SUM-REQ-015)

**Options Considered:**
1. **No default model — require explicit config** — `--semantic` without `[llm].model` returns error with instructions
   - Pros: User consciously chooses model, no surprise resource usage, matches original PRD
   - Cons: Extra friction on first use
2. **Default model with config override** — Ship with `llama3.2:3b` as default, overridable via `[llm].model`
   - Pros: Works out of the box if Ollama has the model pulled, less friction
   - Cons: Default model may not be pulled (clear error), opinionated choice

**Decision:** Option 2 — Default model (`llama3.2:3b`) with config override

**Rationale:** Reduces first-use friction. If the default model isn't pulled, Ollama returns a clear error that guides the user to pull it. A small model (3B) is a sensible default that runs on most developer machines. Power users override via config.

**Consequences:**
- `[llm].model` in config.toml defaults to `"llama3.2:3b"` if not specified
- PRD-SUM-REQ-015 updated to reflect default model behavior instead of error-on-missing
- If Ollama returns model-not-found error, display message instructing user to `ollama pull` or configure a different model

---

### DR-019: Summary Description Cache Invalidation [V3]

**Status:** Accepted
**Date:** 2026-02-24
**Context:** LLM-generated descriptions are expensive (~1-5s per call). Need a cache invalidation strategy that correctly regenerates when content changes but avoids unnecessary regeneration. (PRD-SUM-REQ-011, PRD-SUM-REQ-012)

**Options Considered:**
1. **Hash of symbol IDs + file content hashes** — Compute hash over sorted `(symbol.id, file.hash)` pairs under the target path
   - Pros: Precise invalidation, uses existing `files.hash` (xxhash), cheap to compute
   - Cons: Adding/removing files invalidates (correct behavior)
2. **Hash of structural metrics only** — Cache key is aggregate metrics (file count, symbol count, line count)
   - Pros: Very cheap to compute
   - Cons: Misses meaningful content changes that don't alter counts
3. **Timestamp-based TTL** — Cache with expiry (e.g., 1 hour)
   - Pros: Simplest implementation
   - Cons: Stale descriptions within TTL, unnecessary regeneration when nothing changed

**Decision:** Option 1 — Hash of symbol IDs + file content hashes

**Rationale:** Content-based invalidation using data the indexer already maintains. Correct, cheap (query + hash), and avoids both false positives (unnecessary regeneration) and false negatives (stale descriptions after content changes).

**Consequences:**
- Cache key: `(path, SHA256(sorted [(symbol.id, file.hash), ...]))` for all files under path
- Cache hit: instant return without Ollama call
- Cache miss: generate description, store in `summaries` table
- File content changes detected via existing xxhash values in `files` table

---

### DR-020: Summary Cache Storage [V3]

**Status:** Accepted
**Date:** 2026-02-24
**Context:** Need to store cached LLM descriptions for `wonk summary --semantic` in SQLite. (PRD-SUM-REQ-011)

**Options Considered:**
1. **Dedicated `summaries` table** — `summaries(path TEXT PRIMARY KEY, content_hash TEXT, description TEXT, created_at INTEGER)`
   - Pros: Clean separation, simple queries, easy to clear without touching other tables
   - Cons: One more table in the schema
2. **Reuse key-value pattern** — Store in a generic metadata table (like `daemon_status`)
   - Pros: No new table
   - Cons: Awkward compound keys, mixes concerns

**Decision:** Option 1 — Dedicated `summaries` table

**Rationale:** Clean, purpose-built, simple queries. A `SELECT WHERE path = ? AND content_hash = ?` is the entire cache lookup. Easy to `DELETE FROM summaries` to clear all cached descriptions without risk.

**Consequences:**
- New table: `summaries(path TEXT PRIMARY KEY, content_hash TEXT NOT NULL, description TEXT NOT NULL, created_at INTEGER NOT NULL)`
- Cache lookup: `SELECT description FROM summaries WHERE path = ? AND content_hash = ?`
- Cache miss (hash mismatch): `INSERT OR REPLACE` with new description
- `wonk update` can optionally clear cached summaries

---

### DR-021: Call Graph Enclosing Symbol Detection [V3]

**Status:** Accepted
**Date:** 2026-02-24
**Context:** The indexer must record the enclosing function/method for each call-site reference to populate `caller_id` (DR-015). Need to decide how to identify the enclosing symbol during Tree-sitter parsing. (PRD-CGR-REQ-001, PRD-CGR-REQ-002)

**Options Considered:**
1. **Tree-sitter parent traversal at parse time** — Walk `node.parent()` from each call-site to find nearest enclosing function/method node
   - Pros: Simple, uses Tree-sitter's concrete syntax tree natively, O(depth) per call site, happens during existing parse pass
   - Cons: Must map Tree-sitter node kinds to symbol kinds per language (already done in indexer)
2. **Post-processing pass using line ranges** — After extracting symbols and references, match each reference to the symbol whose `line..end_line` range contains it
   - Pros: Decoupled from Tree-sitter traversal
   - Cons: Requires second pass, line-range containment ambiguous for nested scopes, more complex

**Decision:** Option 1 — Tree-sitter parent traversal at parse time

**Rationale:** Uses the tree structure for exactly what it's designed for. The indexer already maps node kinds to symbol kinds per language, so identifying "is this parent a function?" is a reuse of existing logic. No second pass, no ambiguity, no additional data structures.

**Consequences:**
- Indexer's reference extraction code gains a `find_enclosing_function(node)` helper that walks `node.parent()` upward
- Maps parent node kinds to symbol IDs using the already-extracted symbols for the current file
- File-scope calls (no enclosing function found) get `caller_id = NULL`
- All 11 supported languages need their function/method node kinds mapped (most already are)

---

### DR-022: MCP Server V3 Tool Expansion [V3]

**Status:** Accepted
**Date:** 2026-02-24
**Context:** V3 adds 5 new CLI subcommands (show, summary, callers, callees, callpath) that should be accessible to AI coding assistants via MCP. Need to decide whether to extend the existing MCP server or create a versioned/separate interface. (PRD-SHOW-REQ-013, PRD-SUM-REQ-018, PRD-CGR-REQ-013, PRD-CGR-REQ-014)

**Options Considered:**
1. **Extend existing MCP server** — Add 5 new tool handlers to `mcp.rs`, increasing tool count from 9 to 14
   - Pros: Single server, additive change (no breaking changes), MCP clients automatically discover new tools, each tool maps 1:1 to its CLI subcommand
   - Cons: Growing handler count in one file (manageable at 14)
2. **Versioned MCP server** — Separate V3 tool manifest, clients must opt in
   - Pros: Backward compatibility guaranteed
   - Cons: Over-engineered — MCP tool addition is inherently additive and non-breaking

**Decision:** Option 1 — Extend existing MCP server

**Rationale:** MCP tool addition is additive — existing tools remain unchanged, new tools are discovered automatically by clients. There's no breaking change to justify versioning. Each new tool reuses the same routing pattern as existing tools, delegating to its backend component. The 1:1 mapping between CLI subcommands and MCP tools keeps the interface predictable.

**Consequences:**
- `mcp.rs` gains 5 new tool handler functions routing to `show.rs`, `summary.rs`, and `callgraph.rs`
- Tool count increases from 9 to 14
- No changes to existing tool contracts
- MCP clients (Claude Code, Aider, etc.) discover new tools automatically via `tools/list`

---

## 12) Open Questions & Risks

| ID | Question/Risk | Impact | Mitigation | Owner |
|----|---------------|--------|------------|-------|
| AR-001 | grep crate documentation is sparse — may be hard to use correctly | M | Reference ripgrep source code for usage patterns | Eng |
| AR-002 | WAL file growth under sustained heavy writes (e.g., initial index of 50k files) | L | SQLite auto-checkpoints; busy_timeout handles writer contention | Eng |
| AR-003 | Binary size budget (30 MB) with 10 bundled grammars + SQLite + grep engine + V2 crates + V3 modules (show, summary, callgraph) | M | V3 adds no new crates — three pure-Rust modules add ~0.1-0.3 MB; ureq already present for V2; monitor in CI; strip binaries; consider LTO | Eng |
| AR-004 | Windows cross-compilation with C FFI deps (SQLite, Tree-sitter) | L | P2 priority; can switch to native Windows runner if cross fails | Eng |
| AR-005 | tree-sitter 0.26 deprecated APIs (set_timeout_micros, set_allocator) | L | Use progress callbacks instead; monitor for 0.27 migration | Eng |
| AR-006 | Similarity threshold calibration — should there be a minimum cosine similarity cutoff? | M | Test with real queries; may need empirical calibration before setting a default | Eng |
| AR-007 | Memory usage for 50K+ vector brute-force search (~146 MB) may be high on constrained machines | M | Monitor; truncation (DR-012) can be added as opt-in if needed | Eng |
| AR-008 | Ollama availability — users must install and run Ollama separately for V2 features | M | Clear error messages; all V1 features work without Ollama; installation docs | Eng |
| AR-009 | Call graph accuracy for dynamic dispatch — virtual calls, trait objects, function pointers cannot be resolved statically | M | Document limitation; out of scope per PRD-CGR; name-based matching catches most cases | Eng |
| AR-010 | Index migration for `caller_id` — existing repos need re-index for call graph features | L | Detect missing `caller_id` at query time; display re-index hint; non-breaking for existing features | Eng |
| AR-011 | Default LLM model (`llama3.2:3b`) may not be pulled in Ollama | L | Clear error message guiding user to `ollama pull` or configure alternative model | Eng |
| AR-012 | Summary cache invalidation precision — content hash based on symbol IDs + file hashes may trigger regeneration on unrelated file changes within the target path | L | Acceptable: regeneration is correct behavior; cost is 1-5s per call | Eng |

---

## 13) Appendices

### A. Glossary

| Term | Definition |
|------|------------|
| Symbol | A named code entity: function, class, method, type, constant, variable, etc. |
| Reference | A usage/mention of a symbol name in source code |
| FTS5 | SQLite Full-Text Search extension version 5 |
| Tree-sitter | Incremental parsing library that builds concrete syntax trees |
| WAL | Write-Ahead Logging — SQLite journal mode enabling concurrent reads during writes (see DR-004) |
| Rollback journal | SQLite's default journal mode — serializes reads and writes |
| Embedding | A dense vector representation of code that captures semantic meaning (V2) |
| Chunk | A context-rich text block (symbol + file path + scope + imports) prepared for embedding (V2) |
| Cosine similarity | A measure of similarity between two vectors, computed as their dot product when L2-normalized (V2) |
| Ollama | Open-source local LLM/embedding server; provides nomic-embed-text model (V2) |
| nomic-embed-text | Embedding model producing 768-dim vectors; optimized for text and code (V2) |
| K-Means | Clustering algorithm that partitions data into k groups by minimizing within-cluster variance (V2) |
| Silhouette score | Metric measuring how well each point fits its assigned cluster vs. neighboring clusters (V2) |
| Call graph | Directed graph of caller→callee relationships between functions/methods (V3) |
| `caller_id` | Foreign key in `references` table pointing to the enclosing function's `symbols.id` (V3) |
| BFS | Breadth-First Search — graph traversal that explores all neighbors at each depth before going deeper (V3) |

### B. Module Layout

```
src/
  main.rs           # Entry point, clap setup, dispatch
  cli.rs            # Command definitions and output formatting
  router.rs         # Query routing and fallback logic
  ranker.rs         # Smart search ranking, deduplication, token budgeting
  db.rs             # SQLite connection management, schema, queries
  indexer.rs         # Tree-sitter parsing, symbol/reference extraction
  search.rs          # grep crate text search wrapper
  daemon.rs          # Daemon process: file watching, event loop, lifecycle
  config.rs          # TOML config loading and merging
  types.rs           # Shared types (Symbol, Reference, FileMetadata, SemanticResult, ImpactResult, Cluster, etc.)
  errors.rs          # thiserror error types (DbError, IndexError, SearchError, EmbeddingError)
  embedding.rs       # [V2] Ollama API client, chunking, vector storage/retrieval
  semantic.rs        # [V2] Cosine similarity search, result blending, dependency scoping
  cluster.rs         # [V2] K-Means clustering via linfa, silhouette auto-k
  impact.rs          # [V2] Change detection, semantic impact analysis
  show.rs            # [V3] Source display — symbol lookup + file read + shallow mode
  summary.rs         # [V3] Structural metrics aggregation, LLM description generation + caching
  callgraph.rs       # [V3] Caller/callee queries, BFS call path traversal
```

### C. References
- PRD: `specs/product_specs.md`
- Original PRD: `/mnt/c/Users/elect/Downloads/csi-v1-prd.md`
- ripgrep architecture: https://github.com/BurntSushi/ripgrep
- Tree-sitter docs: https://tree-sitter.github.io/tree-sitter/
- SQLite WAL vs rollback: https://sqlite.org/wal.html
- Ollama API docs: https://github.com/ollama/ollama/blob/main/docs/api.md
- nomic-embed-text: https://huggingface.co/nomic-ai/nomic-embed-text-v1.5
- linfa-clustering: https://docs.rs/linfa-clustering/
