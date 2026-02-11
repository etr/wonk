# System Architecture

**Version:** 0.1
**Last updated:** 2026-02-11
**Status:** Draft
**Owner:** TBD

---

## 1) Executive Summary

Wonk is a single-binary Rust CLI tool that provides fast, structure-aware code search for LLM coding agents. It combines a Tree-sitter-based structural indexer with the `grep` crate (ripgrep internals) for text search, backed by SQLite for persistent storage.

The architecture prioritizes simplicity and low resource usage. A single Rust crate organized into modules handles both CLI queries and background indexing. The daemon process shares the SQLite database with CLI invocations — no IPC protocol is needed. Concurrency uses sync Rust with `rayon` for parallel indexing; no async runtime is required since all workloads are CPU-bound or event-driven (filesystem watching).

Key technology choices: Rust for single static binary distribution and native Tree-sitter/SQLite FFI, SQLite with FTS5 for persistent symbol storage, the `grep` and `ignore` crates from ripgrep for text search and file filtering, and `notify` for cross-platform filesystem watching.

---

## 2) Architectural Drivers

### 2.1 Business Drivers
- Drop-in grep replacement for LLM coding agents — zero integration work
- Zero-config first use — auto-initializes on first query
- Single binary, no external dependencies — trivial to install and distribute

### 2.2 Quality Attributes (from PRD NFRs)

| Attribute | Requirement | Architecture Response |
|-----------|-------------|----------------------|
| Latency (warm) | < 100ms query response | SQLite indexed lookups + FTS5 for symbol name search |
| Latency (cold) | < 5s first query on 5k-file repo | Parallel Tree-sitter parsing via rayon |
| Latency (contention) | < 50ms blocking during daemon writes | SQLite busy_timeout handles brief write contention |
| Index freshness | < 1s after file save | notify-based file watcher with 500ms debounce |
| Daemon idle memory | < 15 MB | No async runtime overhead; sync Rust + rayon |
| Daemon idle CPU | ~0% | Blocked on OS filesystem events (inotify/FSEvents) |
| Binary size | < 30 MB | Static binary with bundled SQLite, Tree-sitter grammars, grep engine |
| Storage | ~1 MB per 10k symbols | SQLite with appropriate indexes |

### 2.3 Constraints
- **Language:** Rust (required for single static binary, native Tree-sitter FFI, grep crate access)
- **No async runtime:** Sync Rust + rayon only (DR-002)
- **No IPC:** CLI and daemon communicate only via shared SQLite (DR-003)
- **Rollback journal:** SQLite default journal mode, not WAL (DR-004)

---

## 3) System Overview

### 3.1 High-Level Architecture Diagram

```
┌─────────────────────────────────────────────────┐
│                  CLI (wonk)                      │
│  clap-derived command parser                     │
├─────────────────────────────────────────────────┤
│               Query Router                       │
│  Routes queries to index or grep fallback        │
├────────────────────┬────────────────────────────┤
│  Structural Index  │      Text Search            │
│  (Tree-sitter +    │      (grep crate)           │
│   SQLite + FTS5)   │                             │
├────────────────────┴────────────────────────────┤
│               SQLite Database                    │
│  symbols, references, files, symbols_fts,        │
│  daemon_status                                   │
├─────────────────────────────────────────────────┤
│             Background Daemon                    │
│  notify + crossbeam-channel + rayon              │
│  File watcher → debounce → re-index → SQLite     │
└─────────────────────────────────────────────────┘
```

### 3.2 Component Summary

| Component | Responsibility | Technology |
|-----------|---------------|------------|
| CLI | Parse commands, dispatch to query router, format output | clap 4.5 (derive), serde_json |
| Query Router | Route queries to SQLite index or grep fallback | Custom module |
| Structural Index | Parse files, extract symbols/references, manage index | tree-sitter 0.26, rusqlite 0.38 |
| Text Search | Grep-compatible text search across files | grep 0.4, ignore 0.4 |
| SQLite Database | Persistent storage for symbols, references, metadata | rusqlite 0.38 (bundled + FTS5) |
| Background Daemon | Watch filesystem, debounce events, re-index changed files | notify 8.x, notify-debouncer-mini, crossbeam-channel, rayon |
| Configuration | Load and merge global/per-repo TOML config | toml 0.8 |

---

## 4) Component Details

### 4.1 CLI

**Responsibility:** Parse user commands, dispatch to query router, format and print results.

**Technology:** clap 4.5 (derive API), serde + serde_json for `--json` output

**Interfaces:**
- Exposes: `wonk` binary with subcommands (search, sym, ref, sig, ls, deps, rdeps, init, update, status, daemon, repos)
- Consumes: Query Router, Configuration

**Key Design Notes:**
- Global `--json` flag available on all commands
- Default output format is `file:line:content` (grep-compatible)
- On invocation, checks for running daemon and auto-spawns if needed (PRD-DMN-REQ-002)
- On first use with no index, triggers auto-initialization (PRD-AUT-REQ-001)

**Related Requirements:** PRD-OUT-REQ-001, PRD-OUT-REQ-002, PRD-OUT-REQ-003, PRD-AUT-REQ-001, PRD-DMN-REQ-002

### 4.2 Query Router

**Responsibility:** Route each query to the appropriate backend (SQLite index vs grep crate) and manage fallback logic.

**Technology:** Custom Rust module

**Interfaces:**
- Exposes: Unified query API consumed by CLI
- Consumes: Structural Index (SQLite), Text Search (grep crate)

**Key Design Notes:**
- Routing table:
  | Command | Primary | Fallback |
  |---------|---------|----------|
  | `wonk search` | grep crate (always) | — |
  | `wonk sym` | SQLite symbols table | grep with heuristic patterns |
  | `wonk ref` | SQLite references table | grep for name occurrences |
  | `wonk deps` | SQLite import data | grep for import/require statements |
  | `wonk ls` | SQLite symbols by file | Tree-sitter on-demand parse |
  | `wonk sig` | SQLite symbols table | grep with heuristic patterns |
  | `wonk rdeps` | SQLite import data | grep for import/require statements |
- Fallback is triggered when primary returns no results
- Error types from `thiserror` enable matching on `NoIndex` vs `QueryFailed`

**Related Requirements:** PRD-FBK-REQ-001 through PRD-FBK-REQ-005, PRD-SIG-REQ-001, PRD-LST-REQ-001, PRD-LST-REQ-002, PRD-DEP-REQ-001, PRD-DEP-REQ-002

### 4.3 Structural Index (Indexer)

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

**Related Requirements:** PRD-IDX-REQ-001 through PRD-IDX-REQ-011, PRD-SYM-REQ-001 through PRD-SYM-REQ-004, PRD-REF-REQ-001 through PRD-REF-REQ-003

### 4.4 Text Search

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

### 4.5 Background Daemon

**Responsibility:** Watch filesystem for changes and keep the index current via incremental re-indexing.

**Technology:** notify 8.x, notify-debouncer-mini, crossbeam-channel, rayon

**Interfaces:**
- Exposes: None (standalone background process)
- Consumes: SQLite Database, Structural Index

**Key Design Notes:**
- Spawned as a separate OS process by the CLI (fork/exec or `std::process::Command`)
- Event loop: `notify` → `notify-debouncer-mini` (500ms window) → `crossbeam-channel` → process batch
- On file change: hash file (xxhash), compare to stored hash, skip if unchanged, else re-parse and update index
- On file delete: remove all symbols, references, metadata for that file
- On new file: detect language, parse if supported, add to index
- Writes heartbeat/status to `daemon_status` table in SQLite (DR-003)
- Auto-exits after configurable idle timeout (default 30 minutes)
- Single instance per repo enforced via PID file
- Detaches from parent process (daemonizes) so CLI can exit immediately

**Related Requirements:** PRD-DMN-REQ-001 through PRD-DMN-REQ-014

### 4.6 Configuration

**Responsibility:** Load, merge, and provide configuration values to all components.

**Technology:** toml 0.8

**Interfaces:**
- Exposes: `Config` struct consumed by all components
- Consumes: `~/.wonk/config.toml` (global), `.wonk/config.toml` (per-repo)

**Key Design Notes:**
- Load order: defaults → global config → per-repo config (last wins)
- All config is optional — sensible defaults baked in
- Config sections: `[daemon]`, `[index]`, `[output]`, `[ignore]`

**Related Requirements:** PRD-CFG-REQ-001 through PRD-CFG-REQ-010

---

## 5) Data Architecture

### 5.1 Data Stores

| Store | Type | Purpose | Location |
|-------|------|---------|----------|
| SQLite index.db | Relational (SQLite) | Symbols, references, file metadata, FTS5 index, daemon status | `~/.wonk/repos/<sha256-short>/index.db` (central) or `.wonk/index.db` (local) |
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
    context TEXT                 -- the full line of source for display
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

-- Indexes
CREATE INDEX idx_symbols_name ON symbols(name);
CREATE INDEX idx_symbols_file ON symbols(file);
CREATE INDEX idx_symbols_kind ON symbols(kind);
CREATE INDEX idx_references_name ON references(name);
CREATE INDEX idx_references_file ON references(file);
```

### 5.3 Data Flow

**Index build (`wonk init`):**
1. Walk repo with `ignore` crate (respects .gitignore, .wonkignore, default exclusions)
2. Parallel parse with rayon: each file → Tree-sitter → symbols + references + metadata
3. Batch insert into SQLite (within transactions for atomicity)
4. Populate FTS5 index
5. Write meta.json
6. Spawn daemon

**Incremental update (daemon):**
1. `notify` detects filesystem event
2. `notify-debouncer-mini` batches events over 500ms window
3. For each file: hash → compare → skip if unchanged → re-parse → delete old rows → insert new rows (single transaction per file)
4. Update `daemon_status` table

**Query (`wonk sym <name>`):**
1. CLI opens read-only SQLite connection with `busy_timeout=5000`
2. Query Router checks index: `SELECT * FROM symbols WHERE name LIKE '%<name>%'` (or FTS5 for performance)
3. If results found → format and print
4. If no results → fall back to grep crate with heuristic patterns

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

Repo root discovery: walk up from CWD looking for `.git` or `.wonk`.
Repo path hash: SHA256 of the canonical repo root path, truncated to first 16 hex chars.

---

## 6) Integration Architecture

### 6.1 External Integrations

None. Wonk is a standalone CLI tool with no network dependencies.

### 6.2 Internal Communication

| From | To | Mechanism | Notes |
|------|----|-----------|-------|
| CLI | SQLite | Direct file access (rusqlite) | Read-only connection with busy_timeout |
| Daemon | SQLite | Direct file access (rusqlite) | Read-write connection with busy_timeout |
| CLI | Daemon | PID file + OS signals | SIGTERM for stop, PID file for status check |
| CLI | Daemon status | SQLite daemon_status table | Daemon writes status, CLI reads it |

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
- No network communication — no encryption in transit needed

### 7.3 PID File Safety
- PID file is checked for stale PIDs (process no longer running) before spawning a new daemon
- PID file is removed on clean daemon shutdown

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
- If V2 adds network features (LSP, remote indexing), tokio would be added then
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

**Decision:** Option 3 — Rollback journal with busy timeout

**Rationale:** Simplest approach. Write transactions are small (one file's symbols at a time, ~5-20ms), so CLI queries only block briefly if they coincide with a daemon commit. `PRAGMA busy_timeout=5000` ensures retries rather than failures. Product spec updated to accept brief blocking (< 50ms) during concurrent daemon writes.

**Consequences:**
- CLI queries may experience brief blocking (~5-20ms) during daemon writes
- `busy_timeout=5000` set on all connections to handle contention gracefully
- Product spec NFR updated: "Brief blocking (< 50ms) is acceptable during concurrent daemon writes"
- Can upgrade to WAL mode later if contention proves problematic in practice

---

### DR-005: Crate Selections

**Status:** Accepted
**Date:** 2026-02-11
**Context:** Need to select key Rust dependencies aligned with architecture decisions (sync + rayon, rollback journal SQLite, single binary).

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
| JSON output | serde + serde_json | 1.x | Structured output for --json flag |
| Error handling (app) | anyhow | 1.x | Ergonomic errors for CLI/application code |
| Error handling (lib) | thiserror | 2.x | Typed errors for component boundaries |

**Rationale:** Each crate is the ecosystem standard for its role. `rusqlite` bundled feature includes FTS5. `grep` and `ignore` are from ripgrep, ensuring compatibility with grep-style output. `xxhash-rust` for fast content hashing, `sha2` for repo path hashing (matching PRD's SHA256 specification).

**Consequences:**
- All grammars compiled into binary (adds ~10-15 MB, within 30 MB budget)
- `rusqlite` bundled feature compiles SQLite from source (adds to build time but ensures FTS5)
- `grep` crate documentation is sparse — may need to reference ripgrep source for usage patterns
- tree-sitter 0.26.x: avoid deprecated `set_timeout_micros` and `set_cancellation_flag` APIs

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
- Define error enums: `DbError`, `IndexError`, `SearchError` with `thiserror`
- Query Router matches on `DbError::NoIndex` to trigger fallback
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

## 12) Open Questions & Risks

| ID | Question/Risk | Impact | Mitigation | Owner |
|----|---------------|--------|------------|-------|
| AR-001 | grep crate documentation is sparse — may be hard to use correctly | M | Reference ripgrep source code for usage patterns | Eng |
| AR-002 | Rollback journal contention under heavy writes (e.g., initial index of 50k files while CLI queries) | L | busy_timeout handles it; can upgrade to WAL if needed | Eng |
| AR-003 | Binary size budget (30 MB) with 10 bundled grammars + SQLite + grep engine | M | Monitor in CI; strip binaries; consider LTO | Eng |
| AR-004 | Windows cross-compilation with C FFI deps (SQLite, Tree-sitter) | L | P2 priority; can switch to native Windows runner if cross fails | Eng |
| AR-005 | tree-sitter 0.26 deprecated APIs (set_timeout_micros, set_allocator) | L | Use progress callbacks instead; monitor for 0.27 migration | Eng |

---

## 13) Appendices

### A. Glossary

| Term | Definition |
|------|------------|
| Symbol | A named code entity: function, class, method, type, constant, variable, etc. |
| Reference | A usage/mention of a symbol name in source code |
| FTS5 | SQLite Full-Text Search extension version 5 |
| Tree-sitter | Incremental parsing library that builds concrete syntax trees |
| WAL | Write-Ahead Logging — SQLite journal mode (not used in V1, see DR-004) |
| Rollback journal | SQLite's default journal mode — serializes writes |

### B. Module Layout (initial)

```
src/
  main.rs           # Entry point, clap setup, dispatch
  cli.rs            # Command definitions and output formatting
  router.rs         # Query routing and fallback logic
  db.rs             # SQLite connection management, schema, queries
  indexer.rs         # Tree-sitter parsing, symbol/reference extraction
  search.rs          # grep crate text search wrapper
  daemon.rs          # Daemon process: file watching, event loop, lifecycle
  config.rs          # TOML config loading and merging
  types.rs           # Shared types (Symbol, Reference, FileMetadata, etc.)
  errors.rs          # thiserror error types (DbError, IndexError, SearchError)
```

### C. References
- PRD: `specs/product_specs.md`
- Original PRD: `/mnt/c/Users/elect/Downloads/csi-v1-prd.md`
- ripgrep architecture: https://github.com/BurntSushi/ripgrep
- Tree-sitter docs: https://tree-sitter.github.io/tree-sitter/
- SQLite WAL vs rollback: https://sqlite.org/wal.html
