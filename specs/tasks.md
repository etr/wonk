# Implementation Tasks

**Generated from:**
- PRD: `specs/product_specs.md`
- Architecture: `specs/architecture.md`

**Last updated:** 2026-02-11
**Status:** Draft

---

## Overview

**Total Tasks:** 30
**Milestones:** 6

### Milestone Summary

| Milestone | Description | Tasks | Status |
|-----------|-------------|-------|--------|
| M1 | Project Scaffold & Text Search | 5 | Not Started |
| M2 | Indexing Engine | 6 | Not Started |
| M3 | Structural Queries | 5 | Not Started |
| M4 | Background Daemon | 5 | Not Started |
| M5 | Auto-Init, Dependencies & Configuration | 4 | Not Started |
| M6 | Polish & Distribution | 5 | Not Started |

### Dependency Graph

```
M1: Project Scaffold & Text Search
├── TASK-001 ──┬── TASK-002 ──┬── TASK-004 ── TASK-005
│              │              │
│              └── TASK-003 ──┘
│
M2: Indexing Engine (depends: M1)
├── TASK-006 ──────────────────────────────┐
├── TASK-007 ──┬── TASK-008 ──┐            │
│              └── TASK-009 ──┼── TASK-010 ── TASK-011
│                             │
M3: Structural Queries (depends: M2)
├── TASK-012 ──┬── TASK-013
│              ├── TASK-014
│              ├── TASK-015
│              └── TASK-016
│
M4: Background Daemon (depends: M2)
├── TASK-017 ──┬── TASK-018 ── TASK-019
│              └── TASK-020 ──┐
│                             └── TASK-021
│
M5: Auto-Init, Dependencies & Configuration (depends: M3, M4)
├── TASK-022
├── TASK-023
├── TASK-024 ── TASK-025
│
M6: Polish & Distribution (depends: M5)
├── TASK-026
├── TASK-027
├── TASK-028
├── TASK-029 ── TASK-030
```

### Critical Path

TASK-001 → TASK-002 → TASK-004 → TASK-005 (M1)
→ TASK-007 → TASK-008 → TASK-010 → TASK-011 (M2)
→ TASK-012 → TASK-013 (M3)
→ TASK-017 → TASK-018 → TASK-019 (M4)
→ TASK-022 (M5)
→ TASK-029 → TASK-030 (M6)

---

## Milestone 1: Project Scaffold & Text Search

**Goal:** `wonk search <pattern>` works as a grep-compatible text search CLI.
**Exit Criteria:** Binary compiles, `wonk search` returns matches in `file:line:content` format with regex, case-insensitive, and path restriction support.

### TASK-001: Initialize Rust project with dependencies

**Milestone:** M1 - Project Scaffold & Text Search
**Component:** All
**Estimate:** S

**Goal:**
Set up Cargo project with all crate dependencies from DR-005.

**Action Items:**
- [ ] `cargo init` with binary target
- [ ] Add all dependencies to Cargo.toml (clap, rusqlite with bundled, tree-sitter, grep, ignore, rayon, notify, notify-debouncer-mini, crossbeam-channel, xxhash-rust, sha2, toml, serde, serde_json, anyhow, thiserror, fork, signal-hook)
- [ ] Add tree-sitter language grammar crates (10 languages)
- [ ] Create initial module files (cli.rs, router.rs, db.rs, indexer.rs, search.rs, daemon.rs, config.rs, types.rs, errors.rs)
- [ ] Verify `cargo build` succeeds

**Dependencies:**
- Blocked by: None
- Blocks: TASK-002, TASK-003, TASK-006, TASK-007, TASK-024, TASK-029

**Acceptance Criteria:**
- `cargo build` succeeds with all dependencies
- Module files exist with basic structure
- Typecheck passes

**Related Requirements:** PRD-DST-REQ-001, PRD-DST-REQ-002
**Related Decisions:** DR-001, DR-005

**Status:** Not Started

---

### TASK-002: CLI skeleton with clap derive

**Milestone:** M1 - Project Scaffold & Text Search
**Component:** CLI
**Estimate:** M

**Goal:**
Define all subcommands and global flags with clap derive, dispatching to stub handlers.

**Action Items:**
- [ ] Define top-level `Cli` struct with `#[arg(global = true)]` `--json` flag
- [ ] Define subcommand enum: search, sym, ref, sig, ls, deps, rdeps, init, update, status, daemon (start/stop/status), repos (list/clean)
- [ ] Define argument structs for each subcommand (pattern, flags, path args)
- [ ] Wire up dispatch from main.rs to stub functions that print "not yet implemented"
- [ ] Implement `--` separator for path restriction on search/ref

**Dependencies:**
- Blocked by: TASK-001
- Blocks: TASK-004, TASK-005

**Acceptance Criteria:**
- All subcommands parse correctly
- `wonk --help` shows all commands
- `wonk search --help` shows search-specific flags
- `wonk sym --kind function --exact foo` parses correctly
- Typecheck passes

**Related Requirements:** PRD-SRCH-REQ-001 through PRD-SRCH-REQ-004, PRD-SYM-REQ-001 through PRD-SYM-REQ-003, PRD-REF-REQ-001 through PRD-REF-REQ-002, PRD-SIG-REQ-001, PRD-LST-REQ-001, PRD-LST-REQ-002, PRD-DEP-REQ-001, PRD-DEP-REQ-002
**Related Decisions:** DR-001, DR-005

**Status:** Not Started

---

### TASK-003: File walker with gitignore support

**Milestone:** M1 - Project Scaffold & Text Search
**Component:** Text Search
**Estimate:** M

**Goal:**
Build a file walker using the `ignore` crate that respects .gitignore and default exclusions.

**Action Items:**
- [ ] Create `walker` module wrapping the `ignore` crate's `WalkBuilder`
- [ ] Configure default exclusions (node_modules, vendor, target, build, dist, __pycache__, .venv)
- [ ] Skip hidden files/directories except .github
- [ ] Support path restriction (walk from a subdirectory)
- [ ] Use ignore crate's internal parallelism (`WalkParallel`) for file enumeration

**Dependencies:**
- Blocked by: TASK-001
- Blocks: TASK-004, TASK-010, TASK-018, TASK-025

**Acceptance Criteria:**
- Walker respects .gitignore rules
- Default exclusions (node_modules, vendor, etc.) are skipped
- Hidden dirs except .github are skipped
- Path restriction works
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-IDX-REQ-009, PRD-IDX-REQ-011
**Related Decisions:** DR-005

**Status:** Not Started

---

### TASK-004: Text search engine (wonk search)

**Milestone:** M1 - Project Scaffold & Text Search
**Component:** Text Search
**Estimate:** M

**Goal:**
Implement `wonk search <pattern>` using the `grep` crate with regex, case-insensitive, and path restriction support.

**Action Items:**
- [ ] Create `search` module wrapping the `grep` crate (grep-searcher, grep-regex, grep-matcher)
- [ ] Implement literal and regex pattern matching
- [ ] Implement case-insensitive flag (`-i`)
- [ ] Integrate file walker (TASK-003) for file enumeration
- [ ] Wire up to CLI dispatch from TASK-002

**Dependencies:**
- Blocked by: TASK-002, TASK-003
- Blocks: TASK-005, TASK-012

**Acceptance Criteria:**
- `wonk search <pattern>` returns matching lines
- `--regex` enables regex mode
- `-i` enables case-insensitive matching
- `-- src/` restricts search to path
- Results match ripgrep output for the same pattern
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SRCH-REQ-001 through PRD-SRCH-REQ-004
**Related Decisions:** DR-005

**Status:** Not Started

---

### TASK-005: Output formatting (grep-compatible + JSON)

**Milestone:** M1 - Project Scaffold & Text Search
**Component:** CLI
**Estimate:** M

**Goal:**
Implement dual output formatting: grep-compatible default and structured JSON via `--json`.

**Action Items:**
- [ ] Define output types (SearchResult, SymbolResult, RefResult, etc.) with serde derives
- [ ] Implement grep-style formatter: `file:line:content`
- [ ] Implement JSON formatter: one JSON object per line
- [ ] Wire `--json` global flag to formatter selection
- [ ] Ensure output goes to stdout, hints/errors to stderr

**Dependencies:**
- Blocked by: TASK-004
- Blocks: TASK-027

**Acceptance Criteria:**
- Default output matches `file:line:content` format
- `--json` outputs valid JSON Lines format (newline-delimited JSON). Errors during streaming are emitted as JSON error objects
- Hints and errors print to stderr
- Output is parseable by tools expecting ripgrep format
- Output respects terminal width when available. Long paths/content truncate gracefully
- Binary file content is skipped or safely indicated
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-OUT-REQ-001, PRD-OUT-REQ-002, PRD-OUT-REQ-003, PRD-SRCH-REQ-005
**Related Decisions:** DR-005

**Status:** Not Started

---

## Milestone 2: Indexing Engine

**Goal:** `wonk init` builds a full SQLite index with Tree-sitter parsing. `wonk status` shows stats.
**Exit Criteria:** Running `wonk init` on a real repo indexes all supported files. `wonk status` displays file count, symbol count, and storage size.

### TASK-006: SQLite schema and connection management

**Milestone:** M2 - Indexing Engine
**Component:** SQLite Database
**Estimate:** M

**Goal:**
Create the SQLite database with full schema including FTS5 content-sync table and provide a connection manager.

**Action Items:**
- [ ] Create `db` module with connection open/create logic
- [ ] Implement schema creation: `symbols`, `references`, `files`, `daemon_status` tables with all indexes
- [ ] Create FTS5 content-sync virtual table (`symbols_fts`) with proper triggers (INSERT with 'delete' command for deletions — never raw DELETE)
- [ ] Set `PRAGMA busy_timeout=5000` on all connections
- [ ] Implement repo path hashing (SHA256-short, first 16 hex chars) for central index directory
- [ ] Support both central (`~/.wonk/repos/<hash>/`) and local (`.wonk/`) index locations
- [ ] Write `meta.json` alongside index (repo_path, created timestamp, detected languages)
- [ ] Implement repo root discovery (walk up from CWD looking for `.git` or `.wonk`)

**Dependencies:**
- Blocked by: TASK-001
- Blocks: TASK-010, TASK-012, TASK-017, TASK-020

**Acceptance Criteria:**
- Schema creates successfully with all tables, indexes, and FTS5
- FTS5 triggers sync correctly on insert/update/delete (using INSERT with 'delete' pattern)
- busy_timeout is set on all connections
- Repo root discovery works (finds .git or .wonk walking up)
- Central and local index paths are computed correctly
- meta.json is written with correct fields
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-IDX-REQ-002, PRD-IDX-REQ-003
**Related Decisions:** DR-004, DR-005

**Status:** Not Started

---

### TASK-007: Tree-sitter parsing infrastructure

**Milestone:** M2 - Indexing Engine
**Component:** Structural Index
**Estimate:** M

**Goal:**
Build a multi-language Tree-sitter dispatcher that detects file language and parses with the correct grammar.

**Action Items:**
- [ ] Create `indexer` module with language detection by file extension
- [ ] Register all 10 bundled grammars using compile-time loading (`tree_sitter_rust::LANGUAGE.into()`, etc.)
- [ ] Implement `parse_file(path) -> Option<tree_sitter::Tree>` that selects the correct parser
- [ ] Handle unsupported languages gracefully (return None, don't error)
- [ ] Write Tree-sitter S-expression queries for symbol extraction per language (function/method definitions, class/struct/interface/enum/trait definitions, type aliases, constants, exports)
- [ ] Avoid deprecated APIs (set_timeout_micros, set_cancellation_flag) — use progress callbacks if needed

**Dependencies:**
- Blocked by: TASK-001
- Blocks: TASK-008, TASK-009, TASK-016

**Acceptance Criteria:**
- All 10 languages parse without errors on valid source files
- Language detection maps extensions correctly (.ts→TypeScript, .tsx→TSX, .py→Python, .rs→Rust, .go→Go, .java→Java, .c→C, .cpp→C++, .rb→Ruby, .php→PHP)
- Unsupported extensions return None
- No deprecated tree-sitter APIs used
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-IDX-REQ-004
**Related Decisions:** DR-005

**Status:** Not Started

---

### TASK-008: Symbol extraction across all languages

**Milestone:** M2 - Indexing Engine
**Component:** Structural Index
**Estimate:** L

**Goal:**
Extract symbol definitions (functions, classes, methods, types, constants) from parsed Tree-sitter trees for all 10 languages.

**Action Items:**
- [ ] Define `Symbol` struct (name, kind, file, line, col, end_line, scope, signature, language)
- [ ] Write extraction queries per language for: functions/methods, classes/structs/interfaces/enums/traits, type aliases, module-level constants/variables, exported symbols
- [ ] Extract `scope` (parent symbol name, e.g., class name for a method)
- [ ] Extract `signature` (full signature text for display)
- [ ] Test against real-world code samples for each language

**Dependencies:**
- Blocked by: TASK-007
- Blocks: TASK-010, TASK-019

**Acceptance Criteria:**
- Symbols extracted correctly for all 10 languages
- Each symbol has name, kind, file, line, col, signature
- Scope correctly identifies parent (e.g., method → class)
- Tested against sample files for each language
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-IDX-REQ-005, PRD-SYM-REQ-004
**Related Decisions:** DR-005

**Status:** Not Started

---

### TASK-009: Reference and import extraction

**Milestone:** M2 - Indexing Engine
**Component:** Structural Index
**Estimate:** M

**Goal:**
Extract references (function calls, type annotations, imports) from parsed trees and record them with context lines.

**Action Items:**
- [ ] Define `Reference` struct (name, file, line, col, context)
- [ ] Write extraction queries per language for: function/method calls, type annotations, import statements
- [ ] Capture the full source line as `context` for display
- [ ] Extract import/export data for the `files` table (for dependency graph in M5)

**Dependencies:**
- Blocked by: TASK-007
- Blocks: TASK-010, TASK-019, TASK-023

**Acceptance Criteria:**
- References extracted correctly for all 10 languages
- Each reference includes name, location, and full source line context
- Import/export data captured per file
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-IDX-REQ-006, PRD-IDX-REQ-007, PRD-REF-REQ-003
**Related Decisions:** DR-005

**Status:** Not Started

---

### TASK-010: Full index build pipeline (wonk init + wonk update)

**Milestone:** M2 - Indexing Engine
**Component:** Structural Index, SQLite Database
**Estimate:** M

**Goal:**
Wire everything together into `wonk init` and `wonk update` commands that build a complete index.

**Action Items:**
- [ ] Implement `wonk init`: walk files (TASK-003), parse with Tree-sitter (TASK-007/008/009), batch-insert symbols/references/files into SQLite (TASK-006)
- [ ] Parallelize file parsing with rayon across available CPU cores
- [ ] Use transactions for atomicity (one transaction per batch)
- [ ] Compute content hash (xxhash) per file for change detection
- [ ] Populate FTS5 index via content-sync triggers
- [ ] Implement `wonk init --local` for local index mode
- [ ] Implement `wonk update` as force re-index (drop and rebuild)
- [ ] Wire to CLI dispatch from TASK-002

**Dependencies:**
- Blocked by: TASK-003, TASK-006, TASK-008, TASK-009
- Blocks: TASK-011, TASK-022, TASK-026

**Acceptance Criteria:**
- `wonk init` indexes a real repo with all supported languages
- Symbols, references, and file metadata are in SQLite
- FTS5 is populated and queryable
- Content hashes stored per file
- Small repos (< 1k files) index in < 1 second
- Medium repos (1k-10k files) index in 1-5 seconds
- Parallel parsing utilizes multiple CPU cores (verified via timing comparison with single-threaded baseline)
- `wonk update` forces full re-index
- `--local` stores index in `.wonk/`
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-IDX-REQ-001, PRD-IDX-REQ-002, PRD-IDX-REQ-003, PRD-IDX-REQ-008, PRD-IDX-REQ-012
**Related Decisions:** DR-001, DR-002, DR-004

**Status:** Not Started

---

### TASK-011: Index status and repo management

**Milestone:** M2 - Indexing Engine
**Component:** CLI, SQLite Database
**Estimate:** S

**Goal:**
Implement `wonk status`, `wonk repos list`, and `wonk repos clean` commands.

**Action Items:**
- [ ] `wonk status`: query SQLite for file count, symbol count, reference count, index size (file size of index.db), last indexed time
- [ ] `wonk repos list`: scan `~/.wonk/repos/`, read each `meta.json`, display repo paths with stats
- [ ] `wonk repos clean`: check each repo path still exists, remove index directories for missing repos
- [ ] Support `--json` output for all three commands

**Dependencies:**
- Blocked by: TASK-010
- Blocks: None

**Acceptance Criteria:**
- `wonk status` shows correct counts matching actual index contents
- `wonk repos list` shows all indexed repos
- `wonk repos clean` removes stale indexes
- All three support `--json`
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-IDX-REQ-013, PRD-IDX-REQ-014, PRD-IDX-REQ-015
**Related Decisions:** DR-003

**Status:** Not Started

---

## Milestone 3: Structural Queries

**Goal:** `wonk sym`, `wonk ref`, `wonk sig`, `wonk ls` return results from the index with grep-based fallback.
**Exit Criteria:** All four query commands return correct results. Fallback to grep works when index has no results.

### TASK-012: Query router with fallback logic

**Milestone:** M3 - Structural Queries
**Component:** Query Router
**Estimate:** M

**Goal:**
Build the routing layer that dispatches queries to SQLite index or grep-based fallback depending on availability and results.

**Action Items:**
- [ ] Create `router` module with a `QueryRouter` that holds both a db connection and search engine
- [ ] Define `thiserror` error types: `DbError::NoIndex`, `DbError::QueryFailed`, `SearchError`
- [ ] Implement routing logic: try primary (SQLite), if no results fall back to grep with heuristic patterns
- [ ] Define heuristic grep patterns for symbol fallback (e.g., `fn <name>`, `def <name>`, `function <name>`, `class <name>`)
- [ ] Define heuristic grep patterns for import fallback (e.g., `import.*<name>`, `require.*<name>`, `use <name>`)

**Dependencies:**
- Blocked by: TASK-006, TASK-004
- Blocks: TASK-013, TASK-014, TASK-015, TASK-016, TASK-023, TASK-028

**Acceptance Criteria:**
- Router dispatches to index when available
- Router falls back to grep when index returns no results
- Error types enable pattern matching for fallback decisions
- Heuristic patterns cover all 10 supported languages
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-FBK-REQ-001 through PRD-FBK-REQ-005
**Related Decisions:** DR-006

**Status:** Not Started

---

### TASK-013: Symbol lookup command (wonk sym)

**Milestone:** M3 - Structural Queries
**Component:** Query Router, CLI
**Estimate:** M

**Goal:**
Implement `wonk sym <name>` with substring/exact matching, kind filtering, and fallback.

**Action Items:**
- [ ] Implement SQLite query: substring match via FTS5 or `LIKE '%name%'`
- [ ] Implement `--exact` flag: exact name match via `WHERE name = ?`
- [ ] Implement `--kind <kind>` flag: filter by symbol kind
- [ ] Format output: `file:line:  signature` (grep-compatible) and JSON with all fields
- [ ] Wire through query router for fallback to grep heuristics
- [ ] Wire to CLI dispatch

**Dependencies:**
- Blocked by: TASK-012
- Blocks: None

**Acceptance Criteria:**
- `wonk sym processPayment` finds matching symbols as substring
- `--exact` returns only exact matches
- `--kind function` filters to functions only
- `--json` includes all symbol fields (file, line, col, kind, name, signature, language)
- Falls back to grep patterns when index has no results
- Precision > 90% for correct definitions returned
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SYM-REQ-001 through PRD-SYM-REQ-004
**Related Decisions:** DR-005

**Status:** Not Started

---

### TASK-014: Reference finding command (wonk ref)

**Milestone:** M3 - Structural Queries
**Component:** Query Router, CLI
**Estimate:** M

**Goal:**
Implement `wonk ref <name>` with path restriction and fallback.

**Action Items:**
- [ ] Implement SQLite query: match references by name
- [ ] Implement path restriction via `-- <path>` (filter by file prefix)
- [ ] Format output: `file:line:  context_line` (grep-compatible) and JSON with all fields
- [ ] Wire through query router for fallback to grep (plain name search)
- [ ] Wire to CLI dispatch

**Dependencies:**
- Blocked by: TASK-012
- Blocks: None

**Acceptance Criteria:**
- `wonk ref processPayment` returns all references with context lines
- `-- src/` restricts results to path
- `--json` includes all reference fields
- Falls back to grep name search when index has no results
- Recall > 80% vs grep baseline
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-REF-REQ-001 through PRD-REF-REQ-003
**Related Decisions:** DR-005

**Status:** Not Started

---

### TASK-015: Signature display command (wonk sig)

**Milestone:** M3 - Structural Queries
**Component:** Query Router, CLI
**Estimate:** S

**Goal:**
Implement `wonk sig <name>` that displays just the signature lines for matching symbols.

**Action Items:**
- [ ] Implement SQLite query: select signature from symbols matching name
- [ ] Format output: `file:line:  signature` (grep-compatible) and JSON
- [ ] Wire through query router for fallback to grep heuristics
- [ ] Wire to CLI dispatch

**Dependencies:**
- Blocked by: TASK-012
- Blocks: None

**Acceptance Criteria:**
- `wonk sig processPayment` shows function signatures with file and line
- `--json` outputs structured data
- Falls back to grep patterns when index has no results
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SIG-REQ-001
**Related Decisions:** DR-005

**Status:** Not Started

---

### TASK-016: Symbol listing command (wonk ls)

**Milestone:** M3 - Structural Queries
**Component:** Query Router, CLI
**Estimate:** M

**Goal:**
Implement `wonk ls <path>` with flat and tree views, including on-demand Tree-sitter fallback.

**Action Items:**
- [ ] Implement SQLite query: select symbols filtered by file path (exact file or directory prefix)
- [ ] Implement flat view: list symbols sorted by file and line
- [ ] Implement `--tree` flag: group symbols by scope hierarchy (e.g., class → methods)
- [ ] Format output: flat list (grep-compatible) and JSON
- [ ] Wire fallback: if no symbols in index for a file, perform on-demand Tree-sitter parse
- [ ] Wire to CLI dispatch

**Dependencies:**
- Blocked by: TASK-012, TASK-007
- Blocks: None

**Acceptance Criteria:**
- `wonk ls src/main.rs` lists all symbols in the file
- `wonk ls src/` lists symbols recursively for directory
- `--tree` shows nesting (methods under classes)
- Falls back to on-demand Tree-sitter parse when index is empty for that file
- `--json` outputs structured data
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-LST-REQ-001, PRD-LST-REQ-002, PRD-FBK-REQ-004
**Related Decisions:** DR-005

**Status:** Not Started

---

## Milestone 4: Background Daemon

**Goal:** File watcher keeps index current automatically. Daemon commands work. Auto-spawns on CLI use.
**Exit Criteria:** Edit a file, run `wonk sym` — the updated symbol is found within 1 second. Daemon auto-exits after idle timeout.

### TASK-017: Daemon process management (spawn, PID, signals)

**Milestone:** M4 - Background Daemon
**Component:** Background Daemon
**Estimate:** M

**Goal:**
Implement daemon spawning via double-fork, PID file management, and graceful shutdown via SIGTERM.

**Action Items:**
- [ ] Implement double-fork daemonization using `fork` crate (detach from parent, new session)
- [ ] Write PID to `daemon.pid` alongside index.db
- [ ] Check for stale PID files on startup (process no longer running → remove and proceed)
- [ ] Enforce single instance per repo (check PID file before spawning)
- [ ] Register SIGTERM handler via `signal-hook` for graceful shutdown
- [ ] On shutdown: clean up PID file, close SQLite connection

**Dependencies:**
- Blocked by: TASK-006
- Blocks: TASK-018, TASK-020, TASK-021

**Acceptance Criteria:**
- Daemon spawns as a background process detached from parent
- PID file is written and cleaned up on exit
- Stale PID files are detected and replaced
- Only one daemon per repo
- SIGTERM triggers graceful shutdown
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-DMN-REQ-011
**Related Decisions:** DR-003, DR-005

**Status:** Not Started

---

### TASK-018: File watcher with debounced events

**Milestone:** M4 - Background Daemon
**Component:** Background Daemon
**Estimate:** M

**Goal:**
Set up filesystem watching with notify and notify-debouncer-mini, feeding debounced events into a crossbeam channel.

**Action Items:**
- [ ] Initialize `notify` recommended watcher for the repo root
- [ ] Wrap with `notify-debouncer-mini` configured for 500ms debounce window
- [ ] Feed debounced events into a `crossbeam-channel` sender
- [ ] Implement the daemon event loop: receive from channel, dispatch to re-indexer
- [ ] Respect file filtering rules (gitignore, default exclusions) when processing events
- [ ] Handle event types: create, modify, delete, rename

**Dependencies:**
- Blocked by: TASK-017, TASK-003
- Blocks: TASK-019

**Acceptance Criteria:**
- File changes are detected within 500ms debounce window
- Rapid saves produce a single debounced event
- Events are correctly categorized (create/modify/delete)
- Ignored files/directories don't trigger re-indexing
- Event loop blocks efficiently (near-zero CPU when idle)
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-DMN-REQ-004, PRD-DMN-REQ-009
**Related Decisions:** DR-002, DR-005

**Status:** Not Started

---

### TASK-019: Incremental re-indexing pipeline

**Milestone:** M4 - Background Daemon
**Component:** Background Daemon, Structural Index
**Estimate:** M

**Goal:**
Process file change events by re-hashing, re-parsing, and updating the index incrementally.

**Action Items:**
- [ ] On file modify: compute xxhash, compare to stored hash in `files` table, skip if unchanged
- [ ] On changed hash: re-parse with Tree-sitter, delete old symbols/references for that file, insert new ones (single transaction)
- [ ] On file delete: remove all symbols, references, and file metadata for that file
- [ ] On new file: detect language, parse if supported, insert into index
- [ ] Update `files` table metadata (hash, last_indexed, line_count, symbols_count)
- [ ] Update FTS5 via content-sync triggers (ensure INSERT-with-delete pattern, not raw DELETE)

**Dependencies:**
- Blocked by: TASK-018, TASK-008, TASK-009
- Blocks: None

**Acceptance Criteria:**
- File modify events: re-indexed only when content hash changes
- File delete events: all symbols, references, and metadata removed from index
- File create events: new files detected, language identified, parsed and indexed if supported
- Single-file re-index completes in < 50ms (benchmarked per PRD-DMN-REQ-010)
- FTS5 stays in sync via triggers (INSERT-with-delete pattern, never raw DELETE)
- Index freshness after file save < 1 second (end-to-end: event → debounce → re-index)
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-DMN-REQ-005, PRD-DMN-REQ-006, PRD-DMN-REQ-007, PRD-DMN-REQ-008, PRD-DMN-REQ-010
**Related Decisions:** DR-004

**Status:** Not Started

---

### TASK-020: Daemon status table and heartbeat

**Milestone:** M4 - Background Daemon
**Component:** Background Daemon, SQLite Database
**Estimate:** S

**Goal:**
Write daemon status (heartbeat, queue depth, errors) to the `daemon_status` SQLite table for CLI to read.

**Action Items:**
- [ ] Write status on daemon start: pid, state='running', uptime_start
- [ ] Update last_activity timestamp on each index update
- [ ] Write files_queued count when processing batches
- [ ] Write last_error on indexing failures
- [ ] Periodic heartbeat write (every 30 seconds) so CLI can detect stale daemons
- [ ] Clear status on clean shutdown

**Dependencies:**
- Blocked by: TASK-017, TASK-006
- Blocks: TASK-021

**Acceptance Criteria:**
- daemon_status table is populated while daemon runs
- Heartbeat updates every 30 seconds
- last_activity reflects most recent index operation
- CLI can read status independently via SQLite
- Status is cleared on clean shutdown
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-DMN-REQ-014
**Related Decisions:** DR-003

**Status:** Not Started

---

### TASK-021: Daemon lifecycle commands and auto-spawn

**Milestone:** M4 - Background Daemon
**Component:** CLI, Background Daemon
**Estimate:** M

**Goal:**
Implement `wonk daemon start/stop/status` and auto-spawn the daemon on any CLI command when an index exists.

**Action Items:**
- [ ] `wonk daemon start`: spawn daemon if not running, report if already running
- [ ] `wonk daemon stop`: send SIGTERM to PID from PID file, wait for exit, clean up
- [ ] `wonk daemon status`: read `daemon_status` table + check PID file, display state/PID/uptime/last activity
- [ ] Implement idle timeout: daemon exits after 30 minutes of no filesystem activity (uses config value from TASK-024 when available)
- [ ] Auto-spawn logic: on any CLI query command, check PID file → if daemon not running and index exists → spawn daemon
- [ ] Wire `wonk init` to spawn daemon after index build
- [ ] Support `--json` output for daemon status

**Dependencies:**
- Blocked by: TASK-017, TASK-020
- Blocks: TASK-022

**Acceptance Criteria:**
- `wonk daemon start` starts the daemon
- `wonk daemon stop` cleanly stops it
- `wonk daemon status` shows running state, PID, and last activity
- Daemon auto-exits after 30 min idle
- `wonk init` spawns daemon on completion
- Any query command auto-spawns daemon if not running
- `--json` output works for daemon status
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-DMN-REQ-001, PRD-DMN-REQ-002, PRD-DMN-REQ-003, PRD-DMN-REQ-012, PRD-DMN-REQ-013, PRD-DMN-REQ-014
**Related Decisions:** DR-003

**Status:** Not Started

---

## Milestone 5: Auto-Init, Dependencies & Configuration

**Goal:** Wonk auto-initializes on first use. `wonk deps`/`wonk rdeps` work. Config files are loaded and applied.
**Exit Criteria:** Run `wonk sym foo` in an uninitialized repo — index builds automatically. Config overrides take effect.

### TASK-022: Auto-initialization on first query

**Milestone:** M5 - Auto-Init, Dependencies & Configuration
**Component:** CLI, Structural Index
**Estimate:** M

**Goal:**
When any query command is run and no index exists, automatically build the index with a progress indicator before returning results.

**Action Items:**
- [ ] Detect missing index at query dispatch time (no index.db for current repo)
- [ ] Run full index build inline (same as `wonk init` pipeline from TASK-010)
- [ ] Display progress indicator to stderr during indexing (file count, percentage)
- [ ] Spawn daemon after auto-init completes
- [ ] Return query results after index is ready
- [ ] Print hint to stderr: "Indexed N files in Xs. Daemon started."

**Dependencies:**
- Blocked by: TASK-010, TASK-021
- Blocks: TASK-026

**Acceptance Criteria:**
- `wonk sym foo` on an uninitialized repo builds index then returns results
- Progress indicator is visible during indexing
- Daemon spawns after auto-init
- First query on a 5k-file repo returns in < 5 seconds
- Subsequent queries hit warm index (< 100ms)
- Auto-init hint is suppressible via `--quiet` flag
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-AUT-REQ-001, PRD-AUT-REQ-002, PRD-AUT-REQ-003
**Related Decisions:** DR-002

**Status:** Not Started

---

### TASK-023: Dependency graph commands (wonk deps/rdeps)

**Milestone:** M5 - Auto-Init, Dependencies & Configuration
**Component:** Query Router, CLI
**Estimate:** M

**Goal:**
Implement `wonk deps <file>` and `wonk rdeps <file>` using import/export data from the index with grep fallback.

**Action Items:**
- [ ] Query `files` table import/export data for forward dependencies (`wonk deps`)
- [ ] Query reverse: find all files whose imports include the target file (`wonk rdeps`)
- [ ] Resolve import paths to actual file paths (language-specific: JS/TS relative imports, Python module paths, etc.)
- [ ] Format output: one file path per line (grep-compatible) and JSON
- [ ] Wire through query router: fall back to grep for import/require patterns when index has no data
- [ ] Wire to CLI dispatch

**Dependencies:**
- Blocked by: TASK-009, TASK-012
- Blocks: None

**Acceptance Criteria:**
- `wonk deps src/main.ts` lists files imported by that file
- `wonk rdeps src/utils.ts` lists files that import it
- Falls back to grep import patterns when index has no data
- `--json` outputs structured data
- Works for all 10 supported languages
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-DEP-REQ-001, PRD-DEP-REQ-002, PRD-FBK-REQ-003
**Related Decisions:** DR-005

**Status:** Not Started

---

### TASK-024: Configuration loading and merging

**Milestone:** M5 - Auto-Init, Dependencies & Configuration
**Component:** Configuration
**Estimate:** M

**Goal:**
Load and merge global (`~/.wonk/config.toml`) and per-repo (`.wonk/config.toml`) configuration with sensible defaults.

**Action Items:**
- [ ] Define `Config` struct with all sections: `[daemon]` (idle_timeout_minutes, debounce_ms), `[index]` (max_file_size_kb, additional_extensions), `[output]` (default_format, color), `[ignore]` (patterns)
- [ ] Implement defaults for all fields (30 min timeout, 500ms debounce, 1024kb max file size, grep format, color=true)
- [ ] Load global config from `~/.wonk/config.toml` if it exists
- [ ] Load per-repo config from `.wonk/config.toml` if it exists
- [ ] Merge: defaults → global → per-repo (last wins)
- [ ] Wire config into all components: daemon uses timeout/debounce, indexer uses max_file_size/additional_extensions, CLI uses output format/color, walker uses ignore patterns
- [ ] Ensure tool works identically when no config files exist

**Dependencies:**
- Blocked by: TASK-001
- Blocks: TASK-025, TASK-027

**Acceptance Criteria:**
- Tool works with zero config (all defaults applied)
- Global config overrides defaults
- Per-repo config overrides global
- All config keys are respected (timeout, debounce, max file size, additional extensions, output format, color, ignore patterns)
- Files larger than `max_file_size_kb` are skipped during indexing with a warning message
- Files with `additional_extensions` are correctly detected and indexed with appropriate language grammar
- Invalid config produces clear error message
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-CFG-REQ-001 through PRD-CFG-REQ-010
**Related Decisions:** DR-005

**Status:** Not Started

---

### TASK-025: Custom ignore patterns (.wonkignore + config)

**Milestone:** M5 - Auto-Init, Dependencies & Configuration
**Component:** Text Search, Structural Index
**Estimate:** S

**Goal:**
Support `.wonkignore` files and `[ignore].patterns` from config for excluding files from indexing and search.

**Action Items:**
- [ ] Add `.wonkignore` support to the file walker (TASK-003) via ignore crate's custom ignore file feature
- [ ] Add `[ignore].patterns` from config (TASK-024) as additional ignore rules
- [ ] Ensure both apply to indexing (`wonk init`) and text search (`wonk search`)
- [ ] `.wonkignore` uses same syntax as `.gitignore`

**Dependencies:**
- Blocked by: TASK-003, TASK-024
- Blocks: None

**Acceptance Criteria:**
- Files matching `.wonkignore` patterns are excluded from index and search
- Files matching config `ignore.patterns` are excluded
- Both use gitignore syntax
- Exclusions apply to both `wonk init` and `wonk search`
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-IDX-REQ-010, PRD-CFG-REQ-010
**Related Decisions:** DR-005

**Status:** Not Started

---

## Milestone 6: Polish & Distribution

**Goal:** Production-ready CLI with progress indicators, colorized output, helpful error messages, and cross-compiled binaries.
**Exit Criteria:** Prebuilt binaries for all P0 platforms. `wonk` provides clear feedback on every operation.

### TASK-026: Progress indicators for indexing operations

**Milestone:** M6 - Polish & Distribution
**Component:** CLI
**Estimate:** S

**Goal:**
Show progress feedback during `wonk init`, `wonk update`, and auto-initialization.

**Action Items:**
- [ ] Count total files before indexing starts (fast pre-scan via walker)
- [ ] Display progress to stderr: `Indexing... [1234/5678 files]` updated in-place
- [ ] Show completion summary: `Indexed 5678 files (4521 symbols, 12340 references) in 3.2s`
- [ ] Suppress progress when stdout is not a TTY (piped output)
- [ ] Ensure progress output doesn't interfere with `--json` mode

**Dependencies:**
- Blocked by: TASK-010, TASK-022
- Blocks: None

**Acceptance Criteria:**
- Progress indicator updates during indexing
- Completion summary shows file/symbol/reference counts and elapsed time
- Progress suppressed when piped or when `TERM=dumb`
- Screen-reader friendly: periodic line-based updates (not just in-place cursor manipulation) when terminal doesn't support cursor control
- No interference with --json output
- Typecheck passes

**Related Requirements:** PRD-AUT-REQ-002
**Related Decisions:** DR-001

**Status:** Not Started

---

### TASK-027: Colorized output and terminal detection

**Milestone:** M6 - Polish & Distribution
**Component:** CLI
**Estimate:** S

**Goal:**
Colorize grep-style output (file paths, line numbers, matches) with terminal detection and config override.

**Action Items:**
- [ ] Detect TTY on stdout (disable color when piped)
- [ ] Colorize file paths, line numbers, match highlights — matching ripgrep conventions
- [ ] Ensure color scheme does not rely solely on red/green distinction (accessible for deuteranopia/protanopia)
- [ ] Use additional visual indicators beyond color (bold, underline, positioning) so information is never conveyed by color alone
- [ ] Respect `output.color` config setting (true/false/auto)
- [ ] Respect `NO_COLOR`, `CLICOLOR=0`, and `CLICOLOR_FORCE=1` environment variables (NO_COLOR takes precedence)
- [ ] Apply color to all commands (search, sym, ref, sig, ls, deps, rdeps, status)

**Dependencies:**
- Blocked by: TASK-005, TASK-024
- Blocks: None

**Acceptance Criteria:**
- Output is colorized in TTY
- Color disabled when piped
- `output.color = false` in config disables color
- `NO_COLOR` env var disables color
- `CLICOLOR=0` disables color, `CLICOLOR_FORCE=1` forces color
- All information conveyed by color is also available through structure or formatting (bold, position) even without color
- Color scheme avoids sole reliance on red/green distinction
- Typecheck passes

**Related Requirements:** PRD-OUT-REQ-001, PRD-OUT-REQ-003, PRD-CFG-REQ-009

**Status:** Not Started

---

### TASK-028: Error messages and hint system

**Milestone:** M6 - Polish & Distribution
**Component:** CLI
**Estimate:** S

**Goal:**
Provide clear, actionable error messages and contextual hints on stderr.

**Action Items:**
- [ ] Implement user-facing error formatter (no raw panic output, no debug formatting)
- [ ] Add hints for common situations: no index, stale daemon, unsupported language, no results
- [ ] Print hints to stderr so they don't pollute piped output
- [ ] Suppress hints in `--json` mode

**Dependencies:**
- Blocked by: TASK-012
- Blocks: None

**Acceptance Criteria:**
- All errors are human-readable (no panics, no debug output)
- Error messages follow consistent format: `error: <message>` with optional `hint: <suggestion>` on next line
- Exit codes are consistent: 0=success, 1=general error, 2=usage error
- Hints print to stderr
- Hints are contextual and actionable
- Hints suppressed in --json mode
- Typecheck passes

**Related Requirements:** PRD-FBK-REQ-005
**Related Decisions:** DR-006

**Status:** Not Started

---

### TASK-029: CI/CD pipeline with GitHub Actions + cross

**Milestone:** M6 - Polish & Distribution
**Component:** Infrastructure
**Estimate:** M

**Goal:**
Set up GitHub Actions workflow for testing, building, and cross-compiling for all 5 platform targets.

**Action Items:**
- [ ] Create `.github/workflows/ci.yml`: cargo test, cargo clippy, cargo fmt --check on push/PR
- [ ] Create `.github/workflows/release.yml`: triggered on version tags
- [ ] Set up build matrix with `cross` for 5 targets: aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-musl, aarch64-unknown-linux-musl, x86_64-pc-windows-msvc
- [ ] Strip binaries post-build
- [ ] Assert binary size < 30 MB in CI
- [ ] Upload build artifacts per platform

**Dependencies:**
- Blocked by: TASK-001
- Blocks: TASK-030

**Acceptance Criteria:**
- CI runs tests and lints on every push/PR
- Release workflow builds all 5 targets
- Binaries are stripped
- Binary size < 30 MB verified in CI
- Artifacts uploaded
- Typecheck passes

**Related Requirements:** PRD-DST-REQ-001 through PRD-DST-REQ-007
**Related Decisions:** DR-007

**Status:** Not Started

---

### TASK-030: Release workflow and install methods

**Milestone:** M6 - Polish & Distribution
**Component:** Infrastructure
**Estimate:** M

**Goal:**
Automate GitHub Releases with platform binaries and set up Homebrew tap and install script.

**Action Items:**
- [ ] Create GitHub Release on tag push with all platform binaries attached
- [ ] Name binaries consistently: `wonk-<version>-<target>`
- [ ] Create Homebrew tap repo with formula pointing to GitHub Release assets
- [ ] Create `install.sh` script: detect platform, download correct binary, install to `/usr/local/bin`
- [ ] Create npm wrapper package (`@wonk/cli`) that downloads the correct binary on install
- [ ] Add install instructions to README

**Dependencies:**
- Blocked by: TASK-029
- Blocks: None

**Acceptance Criteria:**
- `brew install wonk` installs the correct binary for the platform
- `curl -fsSL .../install.sh | sh` installs correctly
- `cargo install wonk` builds from source
- npm package installs correctly
- All P0 platforms have working install paths
- Typecheck passes

**Related Requirements:** PRD-DST-REQ-001, PRD-DST-REQ-003, PRD-DST-REQ-004, PRD-DST-REQ-005

**Status:** Not Started

---

## Parking Lot

Tasks identified but not yet scheduled:

| ID | Description | Reason Deferred |
|----|-------------|-----------------|
| - | LSP server integration | V2 feature |
| - | Semantic/embedding search | V2 feature |
| - | Directory summaries | V2 feature |
| - | Cross-language call graphs | V2 feature |
| - | Editor integrations | V2 feature |
| - | Remote/monorepo support | V2 feature |
| - | Web UI | V2 feature |

---

## Change Log

| Date | Change | Author |
|------|--------|--------|
| 2026-02-11 | Initial task breakdown — 30 tasks across 6 milestones | TBD |
