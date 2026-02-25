# Implementation Tasks

**Generated from:**
- PRD: `specs/product_specs.md`
- Architecture: `specs/architecture.md`

**Last updated:** 2026-02-25
**Status:** In Progress

---

## Overview

**Total Tasks:** 74
**Milestones:** 25

### Milestone Summary

| Milestone | Description | Tasks | Status |
|-----------|-------------|-------|--------|
| M1 | Project Scaffold & Text Search | 5 | Complete |
| M2 | Indexing Engine | 6 | Complete |
| M3 | Structural Queries | 5 | Complete |
| M4 | Background Daemon | 5 | Complete |
| M5 | Auto-Init, Dependencies & Configuration | 4 | Complete |
| M6 | Smart Search | 4 | Complete |
| M7 | Polish & Distribution | 5 | Complete |
| M8 | Git Worktree Support | 3 | Complete |
| M9 | Embedding Infrastructure | 5 | Complete |
| M10 | Semantic Search (`wonk ask`) | 3 | Complete |
| M11 | Daemon Embedding & Lifecycle Updates | 4 | Complete |
| M12 | Semantic Blending & Dependency Scoping | 3 | Complete |
| M13 | Semantic Clustering (`wonk cluster`) | 2 | Complete |
| M14 | Change Impact Analysis (`wonk impact`) | 2 | Complete |
| M15 | Call Graph Data Model & Indexing | 2 | Not Started |
| M16 | Source Display (`wonk show`) | 2 | Not Started |
| M17 | Call Graph Commands | 2 | Not Started |
| M18 | Code Summary Engine (`wonk summary`) | 2 | Not Started |
| M19 | Edge Confidence & Inheritance Infrastructure | 3 | Not Started |
| M20 | Hybrid Search Fusion (RRF) | 1 | Not Started |
| M21 | Execution Flow Detection (`wonk flows`) | 1 | Not Started |
| M22 | Blast Radius Analysis (`wonk blast`) | 1 | Not Started |
| M23 | Scoped Change Detection (`wonk changes`) | 2 | Not Started |
| M24 | Unified Symbol Context (`wonk context`) | 1 | Not Started |
| M25 | Multi-Repo MCP | 1 | Not Started |

### Dependency Graph

```
M1–M8: V1 [Complete] ✅

M9: Embedding Infrastructure (depends: M1–M8)
├── TASK-038 ──┬── TASK-039 ──┐
│              └── TASK-040 ──┼── TASK-042
│                             │
│              TASK-041 ──────┘
│
M10: Semantic Search (depends: M9)
├── TASK-043 ── TASK-044 ── TASK-045
│
M11: Daemon Embedding & Lifecycle (depends: M9)
├── TASK-046
├── TASK-047
├── TASK-048
├── TASK-049
│
M12: Semantic Blending & Dependency Scoping (depends: M10)
├── TASK-050
├── TASK-051 ── TASK-052
│
M13: Semantic Clustering (depends: M9)
├── TASK-053 ── TASK-054
│
M14: Change Impact Analysis (depends: M10)
├── TASK-055 ── TASK-056

M15: Call Graph Data Model & Indexing (independent)
├── TASK-057 ── TASK-058
│
M16: Source Display (independent, parallel with M15)
├── TASK-059 ── TASK-060
│
M17: Call Graph Commands (depends: M15)
├── TASK-061 ── TASK-062
│
M18: Code Summary Engine (independent, parallel with M15/M16)
├── TASK-063 ── TASK-064

M19: Edge Confidence & Inheritance Infrastructure (independent)
├── TASK-065 ──┬── TASK-066
│              └── TASK-067 ←── TASK-065 + TASK-066
│
M20: Hybrid Search Fusion (independent, parallel with M19)
├── TASK-068
│
M21: Execution Flow Detection (depends: V3 M15 + M19)
├── TASK-069 ←── TASK-058 + TASK-067
│
M22: Blast Radius Analysis (depends: V3 M15 + M19)
├── TASK-070 ←── TASK-058 + TASK-067
│
M23: Scoped Change Detection (depends: M21 + M22)
├── TASK-071 ── TASK-072 ←── TASK-071 + TASK-069 + TASK-070
│
M24: Unified Symbol Context (depends: M21 + M19)
├── TASK-073 ←── TASK-069 + TASK-067
│
M25: Multi-Repo MCP (independent)
├── TASK-074
```

### Critical Path

**V1 (Complete):**
TASK-001 → TASK-002 → TASK-004 → TASK-005 (M1) ✅
→ TASK-007 → TASK-008 → TASK-010 → TASK-011 (M2) ✅
→ TASK-012 → TASK-013 (M3) ✅
→ TASK-017 → TASK-018 → TASK-019 (M4) ✅
→ TASK-022 (M5) ✅
→ TASK-031 → TASK-032 → TASK-033 (M6) ✅
→ TASK-029 → TASK-030 (M7) ✅

**V2 (Complete):**
TASK-038 → TASK-039 → TASK-042 (M9) ✅
→ TASK-043 → TASK-044 → TASK-045 (M10) ✅
→ TASK-051 → TASK-052 (M12) ✅

**V3 Critical Path (Call Graph):**
TASK-057 → TASK-058 (M15) → TASK-061 → TASK-062 (M17)

**V3 Parallel Tracks:**
Track A: TASK-059 → TASK-060 (M16 — Source Display)
Track B: TASK-063 → TASK-064 (M18 — Code Summary)

**V4 Critical Path (Graph Intelligence):**
TASK-065 → TASK-066 → TASK-067 (M19)
→ TASK-069 (M21) + TASK-070 (M22) [also depends: TASK-058 from V3]
→ TASK-072 (M23) + TASK-073 (M24)

**V4 Parallel Tracks:**
Track A: TASK-068 (M20 — RRF) — can start immediately
Track B: TASK-074 (M25 — Multi-Repo MCP) — can start immediately
Track C: TASK-071 (M23 — Hunk-to-symbol mapping) — can start immediately

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
- [x] `cargo init` with binary target
- [x] Add all dependencies to Cargo.toml (clap, rusqlite with bundled, tree-sitter, grep, ignore, rayon, notify, notify-debouncer-mini, crossbeam-channel, xxhash-rust, sha2, toml, serde, serde_json, anyhow, thiserror, fork, signal-hook)
- [x] Add tree-sitter language grammar crates (10 languages)
- [x] Create initial module files (cli.rs, router.rs, db.rs, indexer.rs, search.rs, daemon.rs, config.rs, types.rs, errors.rs)
- [x] Verify `cargo build` succeeds

**Dependencies:**
- Blocked by: None
- Blocks: TASK-002, TASK-003, TASK-006, TASK-007, TASK-024, TASK-029

**Acceptance Criteria:**
- `cargo build` succeeds with all dependencies
- Module files exist with basic structure
- Typecheck passes

**Related Requirements:** PRD-DST-REQ-001, PRD-DST-REQ-002
**Related Decisions:** DR-001, DR-005

**Status:** Complete

---

### TASK-002: CLI skeleton with clap derive

**Milestone:** M1 - Project Scaffold & Text Search
**Component:** CLI
**Estimate:** M

**Goal:**
Define all subcommands and global flags with clap derive, dispatching to stub handlers.

**Action Items:**
- [x] Define top-level `Cli` struct with `#[arg(global = true)]` `--json` flag
- [x] Define subcommand enum: search, sym, ref, sig, ls, deps, rdeps, init, update, status, daemon (start/stop/status), repos (list/clean)
- [x] Define argument structs for each subcommand (pattern, flags, path args)
- [x] Wire up dispatch from main.rs to stub functions that print "not yet implemented"
- [x] Implement `--` separator for path restriction on search/ref

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

**Status:** Complete

---

### TASK-003: File walker with gitignore support

**Milestone:** M1 - Project Scaffold & Text Search
**Component:** Text Search
**Estimate:** M

**Goal:**
Build a file walker using the `ignore` crate that respects .gitignore and default exclusions.

**Action Items:**
- [x] Create `walker` module wrapping the `ignore` crate's `WalkBuilder`
- [x] Configure default exclusions (node_modules, vendor, target, build, dist, __pycache__, .venv)
- [x] Skip hidden files/directories except .github
- [x] Support path restriction (walk from a subdirectory)
- [x] Use ignore crate's internal parallelism (`WalkParallel`) for file enumeration

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

**Status:** Complete

---

### TASK-004: Text search engine (wonk search)

**Milestone:** M1 - Project Scaffold & Text Search
**Component:** Text Search
**Estimate:** M

**Goal:**
Implement `wonk search <pattern>` using the `grep` crate with regex, case-insensitive, and path restriction support.

**Action Items:**
- [x] Create `search` module wrapping the `grep` crate (grep-searcher, grep-regex, grep-matcher)
- [x] Implement literal and regex pattern matching
- [x] Implement case-insensitive flag (`-i`)
- [x] Integrate file walker (TASK-003) for file enumeration
- [x] Wire up to CLI dispatch from TASK-002

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

**Status:** Complete

---

### TASK-005: Output formatting (grep-compatible + JSON)

**Milestone:** M1 - Project Scaffold & Text Search
**Component:** CLI
**Estimate:** M

**Goal:**
Implement dual output formatting: grep-compatible default and structured JSON via `--json`.

**Action Items:**
- [x] Define output types (SearchResult, SymbolResult, RefResult, etc.) with serde derives
- [x] Implement grep-style formatter: `file:line:content`
- [x] Implement JSON formatter: one JSON object per line
- [x] Wire `--json` global flag to formatter selection
- [x] Ensure output goes to stdout, hints/errors to stderr

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

**Status:** Complete

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
- [x] Create `db` module with connection open/create logic
- [x] Implement schema creation: `symbols`, `references`, `files`, `daemon_status` tables with all indexes
- [x] Create FTS5 content-sync virtual table (`symbols_fts`) with proper triggers (INSERT with 'delete' command for deletions — never raw DELETE)
- [x] Set `PRAGMA busy_timeout=5000` on all connections
- [x] Implement repo path hashing (SHA256-short, first 16 hex chars) for central index directory
- [x] Support both central (`~/.wonk/repos/<hash>/`) and local (`.wonk/`) index locations
- [x] Write `meta.json` alongside index (repo_path, created timestamp, detected languages)
- [x] Implement repo root discovery (walk up from CWD looking for `.git` or `.wonk`)

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

**Status:** Complete

---

### TASK-007: Tree-sitter parsing infrastructure

**Milestone:** M2 - Indexing Engine
**Component:** Structural Index
**Estimate:** M

**Goal:**
Build a multi-language Tree-sitter dispatcher that detects file language and parses with the correct grammar.

**Action Items:**
- [x] Create `indexer` module with language detection by file extension
- [x] Register all 10 bundled grammars using compile-time loading (`tree_sitter_rust::LANGUAGE.into()`, etc.)
- [x] Implement `parse_file(path) -> Option<tree_sitter::Tree>` that selects the correct parser
- [x] Handle unsupported languages gracefully (return None, don't error)
- [x] Write Tree-sitter S-expression queries for symbol extraction per language (function/method definitions, class/struct/interface/enum/trait definitions, type aliases, constants, exports)
- [x] Avoid deprecated APIs (set_timeout_micros, set_cancellation_flag) — use progress callbacks if needed

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

**Status:** Complete

---

### TASK-008: Symbol extraction across all languages

**Milestone:** M2 - Indexing Engine
**Component:** Structural Index
**Estimate:** L

**Goal:**
Extract symbol definitions (functions, classes, methods, types, constants) from parsed Tree-sitter trees for all 10 languages.

**Action Items:**
- [x] Define `Symbol` struct (name, kind, file, line, col, end_line, scope, signature, language)
- [x] Write extraction queries per language for: functions/methods, classes/structs/interfaces/enums/traits, type aliases, module-level constants/variables, exported symbols
- [x] Extract `scope` (parent symbol name, e.g., class name for a method)
- [x] Extract `signature` (full signature text for display)
- [x] Test against real-world code samples for each language

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

**Status:** Complete

---

### TASK-009: Reference and import extraction

**Milestone:** M2 - Indexing Engine
**Component:** Structural Index
**Estimate:** M

**Goal:**
Extract references (function calls, type annotations, imports) from parsed trees and record them with context lines.

**Action Items:**
- [x] Define `Reference` struct (name, file, line, col, context)
- [x] Write extraction queries per language for: function/method calls, type annotations, import statements
- [x] Capture the full source line as `context` for display
- [x] Extract import/export data for the `files` table (for dependency graph in M5)

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

**Status:** Complete

---

### TASK-010: Full index build pipeline (wonk init + wonk update)

**Milestone:** M2 - Indexing Engine
**Component:** Structural Index, SQLite Database
**Estimate:** M

**Goal:**
Wire everything together into `wonk init` and `wonk update` commands that build a complete index.

**Action Items:**
- [x] Implement `wonk init`: walk files (TASK-003), parse with Tree-sitter (TASK-007/008/009), batch-insert symbols/references/files into SQLite (TASK-006)
- [x] Parallelize file parsing with rayon across available CPU cores
- [x] Use transactions for atomicity (one transaction per batch)
- [x] Compute content hash (xxhash) per file for change detection
- [x] Populate FTS5 index via content-sync triggers
- [x] Implement `wonk init --local` for local index mode
- [x] Implement `wonk update` as force re-index (drop and rebuild)
- [x] Wire to CLI dispatch from TASK-002

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

**Status:** Complete

---

### TASK-011: Index status and repo management

**Milestone:** M2 - Indexing Engine
**Component:** CLI, SQLite Database
**Estimate:** S

**Goal:**
Implement `wonk status`, `wonk repos list`, and `wonk repos clean` commands.

**Action Items:**
- [x] `wonk status`: query SQLite for file count, symbol count, reference count, index size (file size of index.db), last indexed time
- [x] `wonk repos list`: scan `~/.wonk/repos/`, read each `meta.json`, display repo paths with stats
- [x] `wonk repos clean`: check each repo path still exists, remove index directories for missing repos
- [x] Support `--json` output for all three commands

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

**Status:** Complete

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
- [x] Create `router` module with a `QueryRouter` that holds both a db connection and search engine
- [x] Define `thiserror` error types: `DbError::NoIndex`, `DbError::QueryFailed`, `SearchError`
- [x] Implement routing logic: try primary (SQLite), if no results fall back to grep with heuristic patterns
- [x] Define heuristic grep patterns for symbol fallback (e.g., `fn <name>`, `def <name>`, `function <name>`, `class <name>`)
- [x] Define heuristic grep patterns for import fallback (e.g., `import.*<name>`, `require.*<name>`, `use <name>`)

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

**Status:** Complete

---

### TASK-013: Symbol lookup command (wonk sym)

**Milestone:** M3 - Structural Queries
**Component:** Query Router, CLI
**Estimate:** M

**Goal:**
Implement `wonk sym <name>` with substring/exact matching, kind filtering, and fallback.

**Action Items:**
- [x] Implement SQLite query: substring match via FTS5 or `LIKE '%name%'`
- [x] Implement `--exact` flag: exact name match via `WHERE name = ?`
- [x] Implement `--kind <kind>` flag: filter by symbol kind
- [x] Format output: `file:line:  signature` (grep-compatible) and JSON with all fields
- [x] Wire through query router for fallback to grep heuristics
- [x] Wire to CLI dispatch

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

**Status:** Complete

---

### TASK-014: Reference finding command (wonk ref)

**Milestone:** M3 - Structural Queries
**Component:** Query Router, CLI
**Estimate:** M

**Goal:**
Implement `wonk ref <name>` with path restriction and fallback.

**Action Items:**
- [x] Implement SQLite query: match references by name
- [x] Implement path restriction via `-- <path>` (filter by file prefix)
- [x] Format output: `file:line:  context_line` (grep-compatible) and JSON with all fields
- [x] Wire through query router for fallback to grep (plain name search)
- [x] Wire to CLI dispatch

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

**Status:** Complete

---

### TASK-015: Signature display command (wonk sig)

**Milestone:** M3 - Structural Queries
**Component:** Query Router, CLI
**Estimate:** S

**Goal:**
Implement `wonk sig <name>` that displays just the signature lines for matching symbols.

**Action Items:**
- [x] Implement SQLite query: select signature from symbols matching name
- [x] Format output: `file:line:  signature` (grep-compatible) and JSON
- [x] Wire through query router for fallback to grep heuristics
- [x] Wire to CLI dispatch

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

**Status:** Complete

---

### TASK-016: Symbol listing command (wonk ls)

**Milestone:** M3 - Structural Queries
**Component:** Query Router, CLI
**Estimate:** M

**Goal:**
Implement `wonk ls <path>` with flat and tree views, including on-demand Tree-sitter fallback.

**Action Items:**
- [x] Implement SQLite query: select symbols filtered by file path (exact file or directory prefix)
- [x] Implement flat view: list symbols sorted by file and line
- [x] Implement `--tree` flag: group symbols by scope hierarchy (e.g., class → methods)
- [x] Format output: flat list (grep-compatible) and JSON
- [x] Wire fallback: if no symbols in index for a file, perform on-demand Tree-sitter parse
- [x] Wire to CLI dispatch

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

**Status:** Complete

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
- [x] Implement double-fork daemonization using `fork` crate (detach from parent, new session)
- [x] Write PID to `daemon.pid` alongside index.db
- [x] Check for stale PID files on startup (process no longer running → remove and proceed)
- [x] Enforce single instance per repo (check PID file before spawning)
- [x] Register SIGTERM handler via `signal-hook` for graceful shutdown
- [x] On shutdown: clean up PID file, close SQLite connection

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

**Status:** Complete

---

### TASK-018: File watcher with debounced events

**Milestone:** M4 - Background Daemon
**Component:** Background Daemon
**Estimate:** M

**Goal:**
Set up filesystem watching with notify and notify-debouncer-mini, feeding debounced events into a crossbeam channel.

**Action Items:**
- [x] Initialize `notify` recommended watcher for the repo root
- [x] Wrap with `notify-debouncer-mini` configured for 500ms debounce window
- [x] Feed debounced events into a `crossbeam-channel` sender
- [x] Implement the daemon event loop: receive from channel, dispatch to re-indexer
- [x] Respect file filtering rules (gitignore, default exclusions) when processing events
- [x] Handle event types: create, modify, delete, rename

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

**Status:** Complete

---

### TASK-019: Incremental re-indexing pipeline

**Milestone:** M4 - Background Daemon
**Component:** Background Daemon, Structural Index
**Estimate:** M

**Goal:**
Process file change events by re-hashing, re-parsing, and updating the index incrementally.

**Action Items:**
- [x] On file modify: compute xxhash, compare to stored hash in `files` table, skip if unchanged
- [x] On changed hash: re-parse with Tree-sitter, delete old symbols/references for that file, insert new ones (single transaction)
- [x] On file delete: remove all symbols, references, and file metadata for that file
- [x] On new file: detect language, parse if supported, insert into index
- [x] Update `files` table metadata (hash, last_indexed, line_count, symbols_count)
- [x] Update FTS5 via content-sync triggers (ensure INSERT-with-delete pattern, not raw DELETE)

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

**Status:** Complete

---

### TASK-020: Daemon status table and heartbeat

**Milestone:** M4 - Background Daemon
**Component:** Background Daemon, SQLite Database
**Estimate:** S

**Goal:**
Write daemon status (heartbeat, queue depth, errors) to the `daemon_status` SQLite table for CLI to read.

**Action Items:**
- [x] Write status on daemon start: pid, state='running', uptime_start
- [x] Update last_activity timestamp on each index update
- [x] Write files_queued count when processing batches
- [x] Write last_error on indexing failures
- [x] Periodic heartbeat write (every 30 seconds) so CLI can detect stale daemons
- [x] Clear status on clean shutdown

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

**Status:** Complete

---

### TASK-021: Daemon lifecycle commands and auto-spawn

**Milestone:** M4 - Background Daemon
**Component:** CLI, Background Daemon
**Estimate:** M

**Goal:**
Implement `wonk daemon start/stop/status` and auto-spawn the daemon on any CLI command when an index exists.

**Action Items:**
- [x] `wonk daemon start`: spawn daemon if not running, report if already running
- [x] `wonk daemon stop`: send SIGTERM to PID from PID file, wait for exit, clean up
- [x] `wonk daemon status`: read `daemon_status` table + check PID file, display state/PID/uptime/last activity
- [x] Implement idle timeout: daemon exits after 30 minutes of no filesystem activity (uses config value from TASK-024 when available)
- [x] Auto-spawn logic: on any CLI query command, check PID file → if daemon not running and index exists → spawn daemon
- [x] Wire `wonk init` to spawn daemon after index build
- [x] Support `--json` output for daemon status

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

**Status:** Complete

---

## Milestone 5: Auto-Init, Dependencies & Configuration

**Goal:** Wonk auto-initializes on first use. `wonk deps`/`wonk rdeps` work. Config files are loaded and applied.
**Exit Criteria:** Run `wonk sym foo` in an uninitialized repo — index builds automatically. Config overrides take effect.

### TASK-022: Auto-initialization on first query

**Status:** Complete

**Milestone:** M5 - Auto-Init, Dependencies & Configuration
**Component:** CLI, Structural Index
**Estimate:** M

**Goal:**
When any query command is run and no index exists, automatically build the index with a progress indicator before returning results.

**Action Items:**
- [x] Detect missing index at query dispatch time (no index.db for current repo)
- [x] Run full index build inline (same as `wonk init` pipeline from TASK-010)
- [x] Display progress indicator to stderr during indexing (file count, percentage)
- [x] Spawn daemon after auto-init completes
- [x] Return query results after index is ready
- [x] Print hint to stderr: "Indexed N files in Xs. Daemon started."

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

**Status:** Complete

---

### TASK-023: Dependency graph commands (wonk deps/rdeps)

**Milestone:** M5 - Auto-Init, Dependencies & Configuration
**Component:** Query Router, CLI
**Estimate:** M

**Goal:**
Implement `wonk deps <file>` and `wonk rdeps <file>` using import/export data from the index with grep fallback.

**Action Items:**
- [x] Query `files` table import/export data for forward dependencies (`wonk deps`)
- [x] Query reverse: find all files whose imports include the target file (`wonk rdeps`)
- [x] Resolve import paths to actual file paths (language-specific: JS/TS relative imports, Python module paths, etc.)
- [x] Format output: one file path per line (grep-compatible) and JSON
- [x] Wire through query router: fall back to grep for import/require patterns when index has no data
- [x] Wire to CLI dispatch

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

**Status:** Complete

---

### TASK-024: Configuration loading and merging

**Milestone:** M5 - Auto-Init, Dependencies & Configuration
**Component:** Configuration
**Estimate:** M

**Goal:**
Load and merge global (`~/.wonk/config.toml`) and per-repo (`.wonk/config.toml`) configuration with sensible defaults.

**Action Items:**
- [x] Define `Config` struct with all sections: `[daemon]` (idle_timeout_minutes, debounce_ms), `[index]` (max_file_size_kb, additional_extensions), `[output]` (default_format, color), `[ignore]` (patterns)
- [x] Implement defaults for all fields (30 min timeout, 500ms debounce, 1024kb max file size, grep format, color=true)
- [x] Load global config from `~/.wonk/config.toml` if it exists
- [x] Load per-repo config from `.wonk/config.toml` if it exists
- [x] Merge: defaults → global → per-repo (last wins)
- [x] Wire config into all components: daemon uses timeout/debounce, indexer uses max_file_size/additional_extensions, CLI uses output format/color, walker uses ignore patterns
- [x] Ensure tool works identically when no config files exist

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

**Status:** Complete

---

### TASK-025: Custom ignore patterns (.wonkignore + config)

**Milestone:** M5 - Auto-Init, Dependencies & Configuration
**Component:** Text Search, Structural Index
**Estimate:** S

**Goal:**
Support `.wonkignore` files and `[ignore].patterns` from config for excluding files from indexing and search.

**Action Items:**
- [x] Add `.wonkignore` support to the file walker (TASK-003) via ignore crate's custom ignore file feature
- [x] Add `[ignore].patterns` from config (TASK-024) as additional ignore rules
- [x] Ensure both apply to indexing (`wonk init`) and text search (`wonk search`)
- [x] `.wonkignore` uses same syntax as `.gitignore`

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

**Status:** Complete

---

## Milestone 6: Smart Search

**Goal:** `wonk search` returns ranked, deduplicated, token-efficient results when the query matches known symbols. `--budget` and `--raw` flags work.
**Exit Criteria:** For queries matching known symbols, output contains ≥ 50% fewer lines than equivalent `rg` while preserving ≥ 95% of relevant results. `--budget` caps output. `--raw` bypasses ranking.

### TASK-031: Result classification engine

**Milestone:** M6 - Smart Search
**Component:** Smart Search Ranker
**Estimate:** M

**Goal:**
Classify each search result line into a category (definition, call site, import, comment, test) using index metadata and path heuristics.

**Action Items:**
- [x] Create `ranker` module with `ResultCategory` enum: Definition, CallSite, Import, Comment, Test, Other
- [x] For each grep result line, check if the file+line matches a symbol definition in the index → Definition
- [x] Check if the file+line matches a reference in the index → CallSite
- [x] Check if the line contains import/require/use patterns → Import
- [x] Check if the line is inside a comment (using Tree-sitter node types from the index, or heuristic patterns like `//`, `#`, `/* */`)
- [x] Check if the file path matches test heuristics (`test/`, `tests/`, `__tests__/`, `*_test.*`, `*.test.*`, `*.spec.*`) → Test
- [x] Default unclassified results to Other

**Dependencies:**
- Blocked by: TASK-012, TASK-013
- Blocks: TASK-032

**Acceptance Criteria:**
- Symbol definitions are correctly classified as Definition
- Function calls and usages are classified as CallSite
- Import statements are classified as Import
- Test files are detected by path heuristics
- Unclassified lines default to Other
- Classification adds < 10ms overhead per 100 results
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SSRCH-REQ-001
**Related Decisions:** DR-001

**Status:** Complete

---

### TASK-032: Result ranking and deduplication

**Milestone:** M6 - Smart Search
**Component:** Smart Search Ranker
**Estimate:** M

**Goal:**
Sort classified results by relevance tier, deduplicate re-exported/aliased symbols, and insert category headers.

**Action Items:**
- [x] Sort results by category tier: Definition > CallSite > Import > Other > Comment > Test
- [x] Within each tier, sort by file path then line number
- [x] Deduplicate: when the same symbol name appears in multiple files as re-exports or barrel file entries, keep the canonical definition and collapse others into `(+N other locations)`
- [x] Insert category headers between tiers: `-- definitions --`, `-- usages --`, `-- imports --`, `-- tests --`
- [x] Support `--raw` flag to bypass all ranking/deduplication
- [x] Ensure grep-compatible output format is preserved (headers go to stderr or are prefixed with `--` to not break parsers)

**Dependencies:**
- Blocked by: TASK-031
- Blocks: TASK-033

**Acceptance Criteria:**
- Definitions always appear first in output
- Re-exported symbols are deduplicated with count annotation
- Category headers are visible and don't break grep-compatible parsing
- `--raw` returns unranked, undeduped results
- For queries matching known symbols, output ≥ 50% fewer lines than `rg`
- Relevant results preserved at ≥ 95% recall
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SSRCH-REQ-001, PRD-SSRCH-REQ-002, PRD-SSRCH-REQ-003, PRD-SSRCH-REQ-006
**Related Decisions:** DR-001

**Status:** Complete

---

### TASK-033: Token budget mode

**Milestone:** M6 - Smart Search
**Component:** Smart Search Ranker, CLI
**Estimate:** S

**Goal:**
Implement `--budget <n>` flag that limits output to approximately `n` tokens, prioritizing higher-ranked results.

**Action Items:**
- [x] Add `--budget <n>` flag to CLI (global, applies to all search/query commands)
- [x] Implement token estimation: ~4 characters per token heuristic (simple, fast)
- [x] Emit results in rank order, tracking cumulative token count
- [x] When budget is exhausted, stop and append summary line: `-- N more results truncated (budget: <n> tokens) --`
- [x] Budget summary goes to stderr so it doesn't break piped output parsing
- [x] Ensure `--json` mode respects budget (truncate JSON array, add metadata object with truncation info)

**Dependencies:**
- Blocked by: TASK-032
- Blocks: None

**Acceptance Criteria:**
- `--budget 500` limits output to approximately 500 tokens
- Higher-ranked results are preserved, lower-ranked are truncated
- Truncation summary is visible
- Works with both grep-style and JSON output
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SSRCH-REQ-004
**Related Decisions:** DR-001

**Status:** Complete

---

### TASK-034: Symbol detection for automatic smart mode

**Milestone:** M6 - Smart Search
**Component:** Smart Search Ranker, Query Router
**Estimate:** S

**Goal:**
Automatically detect whether a `wonk search` pattern matches known symbol names and engage smart ranking when it does.

**Action Items:**
- [x] On `wonk search <pattern>`, check if pattern matches any symbol name in the FTS5 index
- [x] If match found: run grep search AND enrich results with structural metadata, then rank
- [x] If no match: run plain grep search, skip ranking (pattern is likely a string literal, error message, or config value)
- [x] Display mode indicator to stderr: `(smart: N symbols matched)` or `(text search)`
- [x] Allow explicit override: `--smart` forces ranked mode, `--raw` forces unranked

**Dependencies:**
- Blocked by: TASK-012
- Blocks: None

**Acceptance Criteria:**
- `wonk search processPayment` detects symbol match and ranks results
- `wonk search "connection refused"` detects no symbol match and returns plain grep results
- Mode indicator is visible on stderr
- `--smart` and `--raw` overrides work
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SSRCH-REQ-005, PRD-SSRCH-REQ-006
**Related Decisions:** DR-001

**Status:** Complete

---

## Milestone 7: Polish & Distribution

**Goal:** Production-ready CLI with progress indicators, colorized output, helpful error messages, and cross-compiled binaries.
**Exit Criteria:** Prebuilt binaries for all P0 platforms. `wonk` provides clear feedback on every operation.

### TASK-026: Progress indicators for indexing operations

**Milestone:** M7 - Polish & Distribution
**Component:** CLI
**Estimate:** S

**Goal:**
Show progress feedback during `wonk init`, `wonk update`, and auto-initialization.

**Action Items:**
- [x] Count total files before indexing starts (fast pre-scan via walker)
- [x] Display progress to stderr: `Indexing... [1234/5678 files]` updated in-place
- [x] Show completion summary: `Indexed 5678 files (4521 symbols, 12340 references) in 3.2s`
- [x] Suppress progress when stdout is not a TTY (piped output)
- [x] Ensure progress output doesn't interfere with `--json` mode

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

**Status:** Complete

---

### TASK-027: Colorized output and terminal detection

**Milestone:** M7 - Polish & Distribution
**Component:** CLI
**Estimate:** S

**Goal:**
Colorize grep-style output (file paths, line numbers, matches) with terminal detection and config override.

**Action Items:**
- [x] Detect TTY on stdout (disable color when piped)
- [x] Colorize file paths, line numbers, match highlights — matching ripgrep conventions
- [x] Ensure color scheme does not rely solely on red/green distinction (accessible for deuteranopia/protanopia)
- [x] Use additional visual indicators beyond color (bold, underline, positioning) so information is never conveyed by color alone
- [x] Respect `output.color` config setting (true/false/auto)
- [x] Respect `NO_COLOR`, `CLICOLOR=0`, and `CLICOLOR_FORCE=1` environment variables (NO_COLOR takes precedence)
- [x] Apply color to all commands (search, sym, ref, sig, ls, deps, rdeps, status)

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

**Status:** Complete

---

### TASK-028: Error messages and hint system

**Milestone:** M7 - Polish & Distribution
**Component:** CLI
**Estimate:** S

**Goal:**
Provide clear, actionable error messages and contextual hints on stderr.

**Action Items:**
- [x] Implement user-facing error formatter (no raw panic output, no debug formatting)
- [x] Add hints for common situations: no index, stale daemon, unsupported language, no results
- [x] Print hints to stderr so they don't pollute piped output
- [x] Suppress hints in `--json` mode

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

**Status:** Complete

---

### TASK-029: CI/CD pipeline with GitHub Actions + cross

**Milestone:** M7 - Polish & Distribution
**Component:** Infrastructure
**Estimate:** M

**Goal:**
Set up GitHub Actions workflow for testing, building, and cross-compiling for all 5 platform targets.

**Action Items:**
- [x] Create `.github/workflows/ci.yml`: cargo test, cargo clippy, cargo fmt --check on push/PR
- [x] Create `.github/workflows/release.yml`: triggered on version tags
- [x] Set up build matrix with `cross` for 5 targets: aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-musl, aarch64-unknown-linux-musl, x86_64-pc-windows-msvc
- [x] Strip binaries post-build
- [x] Assert binary size < 30 MB in CI
- [x] Upload build artifacts per platform

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

**Status:** Complete

---

### TASK-030: Release workflow and install methods

**Milestone:** M7 - Polish & Distribution
**Component:** Infrastructure
**Estimate:** M

**Goal:**
Automate GitHub Releases with platform binaries and set up Homebrew tap and install script.

**Action Items:**
- [x] Create GitHub Release on tag push with all platform binaries attached
- [x] Name binaries consistently: `wonk-<version>-<target>`
- [x] Create Homebrew tap repo with formula pointing to GitHub Release assets
- [x] Create `install.sh` script: detect platform, download correct binary, install to `/usr/local/bin`
- [x] Create npm wrapper package (`@wonk/cli`) that downloads the correct binary on install
- [x] Add install instructions to README

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

**Status:** Complete

---

## Milestone 8: Git Worktree Support

**Goal:** Worktrees are fully isolated — each worktree gets its own index and daemon, with no cross-worktree contamination during indexing or file watching.
**Exit Criteria:** Two worktrees of the same repo produce separate indexes. A nested worktree does not pollute the parent's index. The parent daemon ignores events from nested worktree files.

### TASK-035: Walker worktree boundary exclusion

**Milestone:** M8 - Git Worktree Support
**Component:** Structural Index
**Estimate:** S

**Goal:**
Add a `filter_entry` callback to the `WalkBuilder` that skips subdirectories containing a `.git` entry, preventing cross-worktree contamination during indexing.

**Action Items:**
- [x] Add `filter_entry` callback to `WalkBuilder` in `walker.rs` that checks each directory for `.git` existence
- [x] Skip the directory if `.git` is found AND the directory is not the repo root itself
- [x] Handle both `.git` as file (linked worktree) and `.git` as directory (nested repo)
- [x] Add unit tests with a mock nested `.git` directory structure
- [x] Verify default exclusions (node_modules, etc.) still work alongside worktree exclusion

**Dependencies:**
- Blocked by: None
- Blocks: TASK-037

**Acceptance Criteria:**
- Walker skips directories containing `.git` that are not the repo root
- Both `.git` files and `.git` directories are detected as boundaries
- Existing exclusions (gitignore, default exclusions) still work
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-WKT-REQ-003
**Related Decisions:** DR-008

**Status:** Complete

---

### TASK-036: Watcher worktree boundary filtering

**Milestone:** M8 - Git Worktree Support
**Component:** Background Daemon
**Estimate:** S

**Goal:**
Extend the `should_process` event filter to discard filesystem events originating from within a nested worktree boundary.

**Action Items:**
- [x] Add ancestor-path boundary check to `should_process` in `watcher.rs`
- [x] For each event path, walk ancestor directories between the event path and repo root
- [x] If any ancestor directory contains a `.git` entry (file or directory), discard the event
- [x] Accept repo root as parameter so the root's own `.git` is not treated as a boundary
- [x] Add unit tests simulating nested worktree events

**Dependencies:**
- Blocked by: None
- Blocks: TASK-037

**Acceptance Criteria:**
- Events from files inside nested worktree boundaries are discarded
- Events from the repo's own files are processed normally
- Events from the repo root's `.git` are still filtered (existing behavior preserved)
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-WKT-REQ-004
**Related Decisions:** DR-008

**Status:** Complete

---

### TASK-037: Git worktree integration tests

**Milestone:** M8 - Git Worktree Support
**Component:** All
**Estimate:** M

**Goal:**
Verify end-to-end worktree support: repo root detection accepts `.git` files, nearest root wins when nested, indexes are independent per worktree, and cross-worktree contamination is prevented.

**Action Items:**
- [x] Create test fixture: initialize a git repo, add a linked worktree via `git worktree add`
- [x] Test REQ-001: `find_repo_root` correctly identifies the worktree root when `.git` is a file
- [x] Test REQ-002: When CWD is inside a nested worktree, `find_repo_root` returns the worktree root (not the parent)
- [x] Test REQ-003: Running `wonk init` from the parent repo does not index files from the nested worktree
- [x] Test REQ-004: The parent repo's daemon ignores file changes inside the nested worktree
- [x] Test REQ-005: Two worktrees of the same repo produce separate index directories with different content

**Dependencies:**
- Blocked by: TASK-035, TASK-036
- Blocks: None

**Acceptance Criteria:**
- All 5 PRD-WKT requirements verified with integration tests
- Tests use real `git worktree` commands (not mocks)
- Running `wonk search` inside a linked worktree returns only that worktree's results
- Two worktrees produce separate indexes
- A nested worktree does not pollute the parent's index
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-WKT-REQ-001 through PRD-WKT-REQ-005
**Related Decisions:** DR-008

**Status:** Complete

---

## Milestone 9: Embedding Infrastructure

**Goal:** `wonk init` builds embedding vectors alongside the structural index when Ollama is reachable. Vectors stored in SQLite, retrievable via zero-copy deserialization.
**Exit Criteria:** After `wonk init`, the `embeddings` table contains one row per symbol with a 768-dim f32 BLOB vector. Vectors are L2-normalized. Ollama-unavailable case handled gracefully.

### TASK-038: V2 dependencies, schema migration, and error types

**Milestone:** M9 - Embedding Infrastructure
**Component:** SQLite Database, All
**Estimate:** S

**Goal:**
Add V2 crate dependencies, extend the SQLite schema with the `embeddings` table, and add `EmbeddingError` type.

**Action Items:**
- [x] Add to Cargo.toml: `ureq = { version = "3.1", features = ["json"] }`, `bytemuck = { version = "1", features = ["derive"] }`
- [x] Add `embeddings` table to schema creation in `db.rs`: `id`, `symbol_id` (FK → symbols, ON DELETE CASCADE), `file` (TEXT), `chunk_text` (TEXT), `vector` (BLOB), `stale` (INTEGER DEFAULT 0), `created_at` (INTEGER), `UNIQUE(symbol_id)`
- [x] Add `idx_embeddings_file` index on `embeddings(file)`
- [x] Add `EmbeddingError` enum to `errors.rs` with variants: `OllamaUnreachable`, `OllamaError(String)`, `InvalidResponse`, `NoEmbeddings`, `ChunkingFailed`
- [x] Add `QueryRouter` error matching for `EmbeddingError::NoEmbeddings` in `router.rs`
- [x] Handle schema migration: detect if `embeddings` table exists, create if missing (for upgrading V1 indexes)

**Dependencies:**
- Blocked by: None
- Blocks: TASK-039, TASK-040, TASK-041

**Acceptance Criteria:**
- `cargo build` succeeds with new dependencies
- Schema creates `embeddings` table with all columns and constraints
- `ON DELETE CASCADE` works: deleting a symbol row also deletes its embedding
- `EmbeddingError` variants enable pattern matching in router
- Existing V1 indexes upgrade gracefully (table created on first V2 use)
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SEM-REQ-015
**Related Decisions:** DR-005, DR-006, DR-010

**Status:** Complete

---

### TASK-039: Ollama API client

**Milestone:** M9 - Embedding Infrastructure
**Component:** Embedding Engine
**Estimate:** M

**Goal:**
Implement a sync HTTP client for Ollama's embedding API with health checking, batch embedding, and error handling.

**Action Items:**
- [x] Create `embedding.rs` module
- [x] Implement `OllamaClient` struct with configurable base URL (default: `http://localhost:11434`)
- [x] Implement health check: `GET /` → returns true if 200 OK
- [x] Implement `embed_batch(texts: &[String]) -> Result<Vec<Vec<f32>>>`: POST to `/api/embed` with `{"model": "nomic-embed-text", "input": [...]}`
- [x] Implement `embed_single(text: &str) -> Result<Vec<f32>>`: convenience wrapper
- [x] Parse response: extract `embeddings` array from JSON response
- [x] Configure connection timeout (2s) and read timeout (60s) via ureq agent builder
- [x] Return `EmbeddingError::OllamaUnreachable` on connection failure
- [x] Return `EmbeddingError::OllamaError` on non-200 responses with error detail

**Dependencies:**
- Blocked by: TASK-038
- Blocks: TASK-042

**Acceptance Criteria:**
- Health check correctly detects Ollama running/not running
- Batch embedding returns 768-dim f32 vectors for each input
- Single embedding convenience method works
- Connection errors return `OllamaUnreachable`
- API errors return `OllamaError` with message
- Timeouts are enforced (no hanging on unresponsive Ollama)
- Typecheck passes
- Tests pass (with mock or integration tests against real Ollama)

**Related Requirements:** PRD-SEM-REQ-008, PRD-SEM-REQ-012, PRD-SEM-REQ-014
**Related Decisions:** DR-009, DR-012

**Status:** Complete

---

### TASK-040: Symbol chunking engine

**Milestone:** M9 - Embedding Infrastructure
**Component:** Embedding Engine
**Estimate:** M

**Goal:**
Generate context-rich text chunks from indexed symbols, suitable for embedding by `nomic-embed-text`.

**Action Items:**
- [x] Implement `chunk_symbol(symbol: &Symbol, file_imports: &[String], source_code: &str) -> String` in `embedding.rs`
- [x] Chunk format: `File: <path>\nScope: <scope>\nImports: <imports>\n---\n<source_code>` where source_code is extracted from line to end_line
- [x] For symbols with no scope, omit the Scope line
- [x] For files with no imports, omit the Imports line
- [x] Implement `chunk_file_fallback(path: &str, content: &str) -> String` for files with no extractable symbols (PRD-SEM-REQ-007)
- [x] Read source code from disk for each symbol's line range (line to end_line)
- [x] Implement `chunk_all_symbols(db, repo_root) -> Vec<(i64, String)>` returning (symbol_id, chunk_text) pairs
- [x] Truncate chunks that exceed model context limit (8192 tokens ≈ 32KB for nomic-embed-text)

**Dependencies:**
- Blocked by: TASK-038
- Blocks: TASK-042

**Acceptance Criteria:**
- Chunks include file path, scope, imports, and source code
- Full-file fallback generates a single chunk for symbol-less files
- Chunks are well-formed for the embedding model (not truncated mid-token)
- Long files/symbols truncated to model context limit
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SEM-REQ-006, PRD-SEM-REQ-007
**Related Decisions:** DR-010

**Status:** Complete

---

### TASK-041: Vector storage and retrieval

**Milestone:** M9 - Embedding Infrastructure
**Component:** Embedding Engine, SQLite Database
**Estimate:** M

**Goal:**
Store embedding vectors as BLOBs in SQLite and retrieve them with zero-copy deserialization via bytemuck.

**Action Items:**
- [x] Implement `store_embedding(db, symbol_id, file, chunk_text, vector: &[f32]) -> Result<()>`: L2-normalize vector, write as little-endian f32 BLOB
- [x] Implement `store_embeddings_batch(db, embeddings: &[(i64, &str, &str, &[f32])]) -> Result<()>`: batch insert within a transaction
- [x] Implement `load_all_embeddings(db) -> Result<Vec<(i64, Vec<f32>)>>`: load all (symbol_id, vector) pairs
- [x] Use `bytemuck::cast_slice::<u8, f32>()` for zero-copy BLOB → f32 slice conversion
- [x] Implement `delete_embeddings_for_file(db, file: &str) -> Result<()>`: delete all embeddings for a file
- [x] Implement `mark_embeddings_stale(db, file: &str) -> Result<()>`: set `stale = 1` for a file's embeddings
- [x] Implement `embedding_stats(db) -> Result<(usize, usize)>`: return (total_count, stale_count)
- [x] Implement L2 normalization: `normalize(vec: &mut [f32])` — divide each element by the L2 norm

**Dependencies:**
- Blocked by: TASK-038
- Blocks: TASK-042

**Acceptance Criteria:**
- Vectors round-trip correctly: store as BLOB, retrieve as identical f32 slice
- All stored vectors are L2-normalized (norm ≈ 1.0)
- Batch insert uses a single transaction for atomicity
- Zero-copy deserialization via bytemuck works (no data corruption)
- Stale marking and deletion work for per-file operations
- Embedding stats return correct counts
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SEM-REQ-015
**Related Decisions:** DR-010, DR-012

**Status:** Complete

---

### TASK-042: Embedding build pipeline in `wonk init`

**Milestone:** M9 - Embedding Infrastructure
**Component:** Embedding Engine, CLI
**Estimate:** L

**Goal:**
Wire chunking, Ollama embedding, and vector storage into the `wonk init` flow, with progress display and graceful handling of Ollama unavailability.

**Action Items:**
- [x] After structural index build in `wonk init`, check Ollama reachability via health check
- [x] If reachable: generate chunks for all symbols (TASK-040), batch-embed via Ollama (TASK-039), store vectors (TASK-041)
- [x] Display embedding progress to stderr: `Embedding... [1234/5678 symbols]`
- [x] Batch size: embed ~50 chunks per Ollama API call for throughput
- [x] If Ollama unreachable during `wonk init`: print warning to stderr "Ollama not available — skipping embedding generation. Semantic search will not be available until embeddings are built.", continue with structural index only
- [x] Handle partial failures: if Ollama goes down mid-batch, store what was completed, report count
- [x] Wire embedding stats into `wonk status` output: show embedding count, stale count, Ollama reachability

**Dependencies:**
- Blocked by: TASK-039, TASK-040, TASK-041
- Blocks: TASK-043, TASK-044, TASK-045, TASK-047, TASK-049, TASK-053, TASK-055

**Acceptance Criteria:**
- `wonk init` with Ollama running: builds structural index AND embeddings
- `wonk init` without Ollama: builds structural index only, prints warning
- Progress indicator shows during embedding
- `wonk status` shows embedding count and Ollama reachability
- Partial failures save completed embeddings
- Re-running `wonk init` rebuilds all embeddings
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SEM-REQ-008, PRD-SEM-REQ-014
**Related Decisions:** DR-009, DR-010, DR-012

**Status:** Complete

---

## Milestone 10: Semantic Search (`wonk ask`)

**Goal:** `wonk ask "authentication"` finds `verifyToken`, `checkCredentials`, and similar symbols via cosine similarity, even though "authentication" doesn't appear in any symbol name.
**Exit Criteria:** Semantic search returns relevant results ranked by similarity. `--budget` and `--json` work. Clear error when Ollama unavailable.

### TASK-043: Brute-force cosine similarity engine

**Milestone:** M10 - Semantic Search
**Component:** Semantic Search
**Estimate:** M

**Goal:**
Implement parallel brute-force cosine similarity search over all stored embedding vectors.

**Action Items:**
- [x] Create `semantic.rs` module
- [x] Implement `semantic_search(query_vec: &[f32], all_embeddings: &[(i64, Vec<f32>)], limit: usize) -> Vec<(i64, f32)>`: compute dot product (vectors are pre-normalized) for each stored vector, return top-N by descending score
- [x] Parallelize dot product computation with rayon (`par_iter()`)
- [x] Sort results by descending similarity score
- [x] Define `SemanticResult` struct in `types.rs`: `symbol_id`, `file`, `line`, `symbol_name`, `symbol_kind`, `similarity_score`
- [x] Implement `resolve_results(db, scored: &[(i64, f32)]) -> Vec<SemanticResult>`: join symbol_id with symbols table to get file, line, name, kind

**Dependencies:**
- Blocked by: TASK-042
- Blocks: TASK-044

**Acceptance Criteria:**
- Cosine similarity (dot product on normalized vectors) is computed correctly
- Results are sorted by descending similarity
- rayon parallelism is used (measurable speedup on multi-core)
- Search over 50K vectors completes in < 200ms
- SemanticResult includes all required fields (file, line, name, kind, score)
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SEM-REQ-016
**Related Decisions:** DR-010, DR-012

**Status:** Complete

---

### TASK-044: `wonk ask` CLI subcommand

**Milestone:** M10 - Semantic Search
**Component:** CLI, Query Router, Semantic Search
**Estimate:** M

**Goal:**
Implement the `wonk ask <query>` CLI command that performs semantic search and displays results with similarity scores.

**Action Items:**
- [x] Add `ask` subcommand to CLI with args: `<query>` (required), `--budget <n>` (optional), `--json` (global), `--from <file>` (optional, wired in M12), `--to <file>` (optional, wired in M12)
- [x] Wire through Query Router: `wonk ask` → Semantic Search engine
- [x] Flow: embed query via Ollama → normalize → brute-force search (TASK-043) → format results
- [x] Default output format: `file:line  symbol_name (kind) [score]`
- [x] JSON output: include all SemanticResult fields plus similarity_score
- [x] `--budget`: apply token budget to semantic results (reuse budget logic from TASK-033)
- [x] Print result count and top score summary to stderr

**Dependencies:**
- Blocked by: TASK-043
- Blocks: TASK-045, TASK-050, TASK-052, TASK-055

**Acceptance Criteria:**
- `wonk ask "authentication"` returns semantically related symbols with scores
- Results sorted by descending similarity
- Each result shows file, line, symbol name, kind, and similarity score
- `--json` outputs valid JSON with all fields
- `--budget` limits output token count
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SEM-REQ-001, PRD-SEM-REQ-003, PRD-SEM-REQ-004, PRD-SEM-REQ-005
**Related Decisions:** DR-009, DR-010

**Status:** Complete

---

### TASK-045: Block-and-wait and Ollama error handling

**Milestone:** M10 - Semantic Search
**Component:** CLI, Embedding Engine
**Estimate:** M

**Goal:**
When `wonk ask` is run with incomplete embeddings, block and build them with progress. When Ollama is unavailable, return a clear error.

**Action Items:**
- [x] On `wonk ask`, check embedding completeness: compare symbol count in `symbols` table vs `embeddings` table
- [x] If embeddings are incomplete: call `Embedding Engine::embed_repo()` directly from CLI with a progress callback that prints to stderr, blocking until complete
- [x] After embedding completes, proceed with the semantic query
- [x] If Ollama is not reachable when `wonk ask` is run: return `error: Ollama is required for semantic search. Start Ollama with 'ollama serve' and ensure nomic-embed-text is available.`
- [x] If Ollama becomes unreachable mid-embedding (block-and-wait): report partial progress and error

**Dependencies:**
- Blocked by: TASK-044
- Blocks: None

**Acceptance Criteria:**
- `wonk ask` with no embeddings: blocks, shows progress, builds embeddings, then returns results
- `wonk ask` with partial embeddings: builds remaining, then returns results
- `wonk ask` with complete embeddings: returns results immediately (no delay)
- Ollama unavailable: clear, actionable error message
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SEM-REQ-012, PRD-SEM-REQ-013
**Related Decisions:** DR-009

**Status:** Complete

---

## Milestone 11: Daemon Embedding & Lifecycle Updates

**Goal:** Daemon keeps embeddings fresh on file changes. Multi-daemon management works. Idle timeout removed.
**Exit Criteria:** Edit a file with Ollama running → embeddings update within 1s. `wonk daemon list` shows all running daemons. `wonk daemon stop --all` stops them all.

### TASK-046: Remove daemon idle timeout

**Milestone:** M11 - Daemon Embedding & Lifecycle Updates
**Component:** Background Daemon, Configuration
**Estimate:** S

**Goal:**
Remove the 30-minute idle timeout so daemons run indefinitely until explicitly stopped.

**Action Items:**
- [ ] Remove idle timeout logic from daemon event loop in `daemon.rs`
- [ ] Remove `idle_timeout_minutes` from `Config` struct and config parsing in `config.rs`
- [ ] Update `wonk daemon status` output to no longer show timeout remaining
- [ ] Ensure daemon still exits cleanly on SIGTERM
- [ ] Update any tests that depend on idle timeout behavior

**Dependencies:**
- Blocked by: None
- Blocks: None

**Acceptance Criteria:**
- Daemon runs indefinitely without auto-exiting
- `wonk daemon stop` still works (SIGTERM)
- `idle_timeout_minutes` config key is ignored if present (no error)
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-CFG-REQ-004 (removed)
**Related Decisions:** DR-013

**Status:** Complete

---

### TASK-047: Daemon incremental embedding re-indexing

**Milestone:** M11 - Daemon Embedding & Lifecycle Updates
**Component:** Background Daemon, Embedding Engine
**Estimate:** L

**Goal:**
After structural re-indexing of changed files, re-generate and store embeddings for those files when Ollama is reachable.

**Action Items:**
- [x] After incremental structural re-index (existing pipeline), check Ollama reachability
- [x] If Ollama reachable: delete old embeddings for the changed file, generate new chunks from updated symbols, embed via Ollama, store new vectors
- [x] If Ollama unreachable: call `mark_embeddings_stale(db, file)` to set `stale = 1` for the file's embeddings (PRD-SEM-REQ-011)
- [x] Run embedding work on a separate thread from the watcher thread to avoid blocking file event processing
- [x] On file delete: CASCADE handles embedding cleanup (from FK constraint)
- [x] Log embedding re-index activity to daemon status table

**Dependencies:**
- Blocked by: TASK-042
- Blocks: None

**Acceptance Criteria:**
- File change with Ollama running: embeddings updated within 1s of structural re-index
- File change with Ollama down: embeddings marked stale, no error visible to user
- File deletion: embeddings removed via CASCADE
- Embedding work doesn't block file event processing (runs on separate thread)
- Daemon status shows embedding activity
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SEM-REQ-010, PRD-SEM-REQ-011
**Related Decisions:** DR-009, DR-010, DR-013

**Status:** Complete

---

### TASK-048: Multi-daemon management (`daemon list`, `stop --all`)

**Milestone:** M11 - Daemon Embedding & Lifecycle Updates
**Component:** CLI, Background Daemon
**Estimate:** M

**Goal:**
Implement `wonk daemon list` to show all running daemons and `wonk daemon stop --all` to stop them all.

**Action Items:**
- [x] Implement `wonk daemon list`: glob `~/.wonk/repos/*/daemon.pid`, read each PID, check if process is alive (`kill(pid, 0)`), read `meta.json` for repo path
- [x] Display format: `PID    REPO PATH    UPTIME    STATUS`
- [x] Clean up stale PID files (process not running) during listing
- [x] Implement `wonk daemon stop --all`: iterate daemon list, send SIGTERM to each, wait for exit, report results
- [x] Support `--json` output for daemon list
- [x] Handle local-mode indexes: also check `.wonk/daemon.pid` in current repo

**Dependencies:**
- Blocked by: None
- Blocks: None

**Acceptance Criteria:**
- `wonk daemon list` shows all running daemons with repo paths and PIDs
- Stale PID files are detected and cleaned up
- `wonk daemon stop --all` stops all running daemons
- `--json` outputs structured daemon list
- Works for both central and local index modes
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-DMN-REQ-014, PRD-DMN-REQ-015
**Related Decisions:** DR-013

**Status:** Complete

---

### TASK-049: Auto-init embedding delegation to daemon

**Milestone:** M11 - Daemon Embedding & Lifecycle Updates
**Component:** CLI, Background Daemon
**Estimate:** S

**Goal:**
When auto-init is triggered by a query, build structural index only, then delegate embedding generation to the daemon.

**Action Items:**
- [x] In auto-init path (triggered by `wonk ask` or `wonk search --semantic` with no index): build structural index synchronously
- [x] After structural index build, write `embedding_build_requested = 1` to `daemon_status` table
- [x] In daemon startup, check for `embedding_build_requested` flag, begin embedding generation in background if Ollama is reachable
- [x] Clear the flag after embedding build completes
- [x] If `wonk ask` is run before daemon finishes embeddings, block-and-wait logic (TASK-045) takes over

**Dependencies:**
- Blocked by: TASK-042
- Blocks: None

**Acceptance Criteria:**
- Auto-init builds structural index immediately, not embeddings
- Daemon picks up embedding_build_requested flag and starts embedding
- Flag is cleared after completion
- If user runs `wonk ask` before daemon finishes, block-and-wait works
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SEM-REQ-009
**Related Decisions:** DR-013

**Status:** Complete

---

## Milestone 12: Semantic Blending & Dependency Scoping

**Goal:** `wonk search --semantic` blends structural and semantic results. `--from`/`--to` filters semantic results by dependency graph reachability.
**Exit Criteria:** `wonk search --semantic <pattern>` returns structural matches first, then semantic matches. `wonk ask "auth" --from src/routes/api.ts` returns only reachable symbols.

### TASK-050: `wonk search --semantic` blending

**Milestone:** M12 - Semantic Blending & Dependency Scoping
**Component:** Smart Search Ranker, CLI
**Estimate:** M

**Goal:**
Add `--semantic` flag to `wonk search` that blends structural results with semantic results.

**Action Items:**
- [x] Add `--semantic` flag to `wonk search` CLI
- [x] When `--semantic` is provided: run structural search as normal, then run semantic search for the same pattern
- [x] Deduplicate: remove semantic results that match structural results (same file+line)
- [x] Blend: present structural matches first (with existing ranking), then additional semantic matches with similarity scores
- [x] Semantic matches formatted with `[semantic: 0.87]` annotation
- [x] `--budget` applies to blended result set

**Dependencies:**
- Blocked by: TASK-044
- Blocks: None

**Acceptance Criteria:**
- `wonk search --semantic verifyToken` returns structural matches first, then semantic matches
- Semantic matches include similarity score annotation
- No duplicate results (same file+line appears only once)
- `--budget` limits total blended output
- `--json` includes both result types with a `source` field ("structural" or "semantic")
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SEM-REQ-002
**Related Decisions:** DR-010

**Status:** Complete

---

### TASK-051: Dependency graph transitive traversal

**Milestone:** M12 - Semantic Blending & Dependency Scoping
**Component:** Semantic Search
**Estimate:** M

**Goal:**
Implement BFS/DFS traversal over the file-level dependency graph to compute reachable file sets for `--from` and `--to` scoping.

**Action Items:**
- [x] Implement `reachable_from(db, file: &str) -> HashSet<String>`: BFS forward traversal following import edges from the given file
- [x] Implement `reachable_to(db, file: &str) -> HashSet<String>`: BFS reverse traversal finding all files that transitively import the given file
- [x] Load file-level dependency graph from SQLite (files table import data) into an adjacency list
- [x] Use `VecDeque` for BFS, `HashSet` for visited tracking
- [x] Handle cycles (files that import each other) — visited set prevents infinite loops
- [x] Return the file as part of its own reachable set

**Dependencies:**
- Blocked by: TASK-042
- Blocks: TASK-052

**Acceptance Criteria:**
- Forward traversal: A imports B imports C → reachable_from(A) = {A, B, C}
- Reverse traversal: A imports B, C imports B → reachable_to(B) = {A, B, C}
- Cycles handled correctly (no infinite loop)
- Traversal completes in < 50ms for typical dependency graphs (< 10K files)
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SDEP-REQ-003
**Related Decisions:** DR-010

**Status:** Complete

---

### TASK-052: `--from` / `--to` dependency scoping on `wonk ask`

**Milestone:** M12 - Semantic Blending & Dependency Scoping
**Component:** Semantic Search, CLI
**Estimate:** S

**Goal:**
Wire `--from` and `--to` flags on `wonk ask` to filter semantic results by dependency reachability.

**Action Items:**
- [x] Wire `--from <file>` flag: compute `reachable_from(file)` (TASK-051), filter semantic results to symbols in reachable files only
- [x] Wire `--to <file>` flag: compute `reachable_to(file)` (TASK-051), filter semantic results to symbols in reachable files only
- [x] Apply filtering BEFORE ranking/budget (so budget counts only relevant results)
- [x] If `--from` and `--to` are both specified, intersect reachable sets
- [x] If specified file doesn't exist in index, return clear error

**Dependencies:**
- Blocked by: TASK-044, TASK-051
- Blocks: None

**Acceptance Criteria:**
- `wonk ask "auth" --from src/routes/api.ts` returns only symbols reachable from that file
- `wonk ask "auth" --to src/utils/db.ts` returns only symbols that can reach that file
- Results still include similarity scores
- Non-existent file produces clear error
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SDEP-REQ-001, PRD-SDEP-REQ-002
**Related Decisions:** DR-010

**Status:** Complete

---

## Milestone 13: Semantic Clustering (`wonk cluster`)

**Goal:** `wonk cluster src/auth/` groups related symbols by semantic similarity using K-Means, revealing conceptual groupings within a directory.
**Exit Criteria:** Clustering produces meaningful groups. Auto-k selection via silhouette scoring works. Output shows representative symbols per cluster.

### TASK-053: K-Means clustering with silhouette auto-k

**Milestone:** M13 - Semantic Clustering
**Component:** Clustering Engine
**Estimate:** L

**Goal:**
Implement K-Means clustering of symbol embeddings with automatic k selection via silhouette scoring.

**Action Items:**
- [x] Add to Cargo.toml: `linfa-clustering = "0.8"`, `linfa = "0.8"`, `ndarray = "0.16"`
- [x] Create `cluster.rs` module
- [x] Implement `cluster_embeddings(embeddings: &[(i64, Vec<f32>)], max_k: usize) -> Vec<Cluster>`: load embeddings into ndarray matrix, run K-Means for k = 2..min(√n, max_k), compute silhouette score for each k, select best k
- [x] Use K-Means++ initialization via `linfa-clustering::KMeans::params_with_rng(k).init_method(KMeansPlusPlus)`
- [x] Define `Cluster` struct in `types.rs`: `cluster_id`, `centroid: Vec<f32>`, `members: Vec<ClusterMember>`, `representative_symbols: Vec<ClusterMember>` (top 5 closest to centroid)
- [x] Define `ClusterMember` struct: `symbol_id`, `symbol_name`, `symbol_kind`, `file`, `line`, `distance_to_centroid`
- [x] Implement silhouette scoring: for each point, compute (b - a) / max(a, b) where a = avg distance to same-cluster points, b = avg distance to nearest other cluster points
- [x] Cap max_k at 20

**Dependencies:**
- Blocked by: TASK-042
- Blocks: TASK-054

**Acceptance Criteria:**
- K-Means correctly partitions embeddings into k clusters
- Silhouette scoring selects a reasonable k (verified on sample data)
- Cluster representatives are the 5 symbols closest to centroid
- Clustering completes in < 5s for 5000 symbols
- Handles edge cases: < 3 symbols (return single cluster), all identical embeddings
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SCLST-REQ-001, PRD-SCLST-REQ-002
**Related Decisions:** DR-011, DR-012

**Status:** Complete

---

### TASK-054: `wonk cluster` CLI subcommand

**Milestone:** M13 - Semantic Clustering
**Component:** CLI, Query Router, Clustering Engine
**Estimate:** M

**Goal:**
Implement the `wonk cluster <path>` CLI command that displays semantic clusters of symbols within a directory.

**Action Items:**
- [x] Add `cluster` subcommand to CLI with args: `<path>` (required), `--json` (global), `--top <n>` (optional, default 5, number of representative symbols per cluster)
- [x] Wire through Query Router: load embeddings filtered by path prefix, pass to Clustering Engine (TASK-053)
- [x] Default output format: numbered cluster groups with representative symbols
  ```
  Cluster 1 (15 symbols):
    src/auth/middleware.ts:15  verifyToken (function) [0.12]
    src/auth/session.ts:8     validateSession (function) [0.15]
    ...
  Cluster 2 (8 symbols):
    ...
  ```
- [x] JSON output: structured cluster data with members, centroids, distances
- [x] If no embeddings exist for path, return error with hint to run `wonk init`
- [x] If fewer than 3 symbols in path, return all symbols in a single group

**Dependencies:**
- Blocked by: TASK-053
- Blocks: None

**Acceptance Criteria:**
- `wonk cluster src/auth/` groups related auth symbols together
- Output clearly separates distinct concerns
- Each cluster shows top representative symbols with distance to centroid
- `--json` outputs structured cluster data
- `--top 10` shows 10 representatives per cluster
- Clear error when no embeddings exist
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SCLST-REQ-001, PRD-SCLST-REQ-002, PRD-SCLST-REQ-003
**Related Decisions:** DR-011

**Status:** Complete

---

## Milestone 14: Change Impact Analysis (`wonk impact`)

**Goal:** `wonk impact <file>` finds semantically similar code that might be affected by changes. `--since <commit>` analyzes all files changed since a commit.
**Exit Criteria:** Changing `verifyToken` surfaces `validateSession` and `checkCredentials` as potentially impacted. `--since HEAD~3` works.

### TASK-055: Symbol change detection

**Milestone:** M14 - Change Impact Analysis
**Component:** Impact Analyzer
**Estimate:** M

**Goal:**
Detect which symbols changed in a file by comparing a fresh Tree-sitter parse against the indexed version.

**Action Items:**
- [x] Create `impact.rs` module
- [x] Implement `detect_changed_symbols(db, file: &str) -> Result<Vec<ChangedSymbol>>`: re-parse the file with Tree-sitter, extract current symbols, compare against stored symbols by (name, kind, content_hash)
- [x] Define `ChangedSymbol` struct in `types.rs`: `name`, `kind`, `file`, `line`, `change_type` (Added, Modified, Removed)
- [x] A symbol is "Modified" if name+kind match but content hash differs
- [x] A symbol is "Added" if it exists in current parse but not in index
- [x] A symbol is "Removed" if it exists in index but not in current parse
- [x] Implement `detect_changed_files_since(commit: &str) -> Result<Vec<String>>`: shell out to `git diff --name-only <commit>` and parse output
- [x] Handle git not installed: return clear error for `--since` only (file-level impact works without git)

**Dependencies:**
- Blocked by: TASK-042
- Blocks: TASK-056

**Acceptance Criteria:**
- Modified symbols detected correctly (same name+kind, different content)
- Added symbols detected (new symbol not in index)
- Removed symbols detected (in index but not in current file)
- `git diff --name-only` integration works for `--since`
- Git not installed: error only for `--since`, not for file-level analysis
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SIMP-REQ-001, PRD-SIMP-REQ-002
**Related Decisions:** DR-014

**Status:** Complete

---

### TASK-056: `wonk impact` CLI subcommand

**Milestone:** M14 - Change Impact Analysis
**Component:** CLI, Query Router, Impact Analyzer, Semantic Search
**Estimate:** L

**Goal:**
Implement `wonk impact <file>` that finds semantically similar code that might be affected by changes in the specified file.

**Action Items:**
- [x] Add `impact` subcommand to CLI with args: `<file>` (required), `--since <commit>` (optional), `--json` (global)
- [x] Wire through Query Router: detect changed symbols (TASK-055), embed each changed symbol's current source via Ollama, compare against all stored embeddings (TASK-043)
- [x] For `--since <commit>`: get changed files list (TASK-055), analyze each file, aggregate results
- [x] Define `ImpactResult` struct in `types.rs`: `changed_symbol` (name, kind, file, line), `impacted_symbol` (name, kind, file, line), `similarity_score`, `file_path`
- [x] Exclude the changed symbol itself from impact results (don't report a symbol as impacted by itself)
- [x] Sort results by descending similarity score
- [x] Default output format:
  ```
  Changed: verifyToken (function) in src/auth/middleware.ts:15
    → src/auth/session.ts:8      validateSession (function) [0.89]
    → src/auth/credentials.ts:22 checkCredentials (function) [0.84]
  ```
- [x] JSON output: structured ImpactResult array
- [x] If no embeddings exist, return error with hint

**Dependencies:**
- Blocked by: TASK-044, TASK-055
- Blocks: None

**Acceptance Criteria:**
- `wonk impact src/auth/middleware.ts` finds semantically related code
- Results ranked by similarity to the changed symbols
- `--since HEAD~3` analyzes all files changed in last 3 commits
- Each result shows changed symbol, impacted symbol, similarity, file
- Changed symbol is not reported as impacted by itself
- `--json` outputs valid structured data
- Clear error when no embeddings exist
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SIMP-REQ-001, PRD-SIMP-REQ-002, PRD-SIMP-REQ-003, PRD-SIMP-REQ-004
**Related Decisions:** DR-014, DR-010

**Status:** Complete

---

## Milestone 15: Call Graph Data Model & Indexing

**Goal:** The Tree-sitter indexer records which enclosing function each call-site reference lives inside, stored as `caller_id` in the references table. This is foundational data for callers/callees/callpath commands.
**Exit Criteria:** After `wonk init` or `wonk update`, `SELECT COUNT(*) FROM references WHERE caller_id IS NOT NULL` returns a non-zero count for repos with function calls. Daemon incremental re-indexing also populates caller_id.

### TASK-057: Call graph schema and enclosing function detection

**Milestone:** M15 - Call Graph Data Model & Indexing
**Component:** SQLite Database, Structural Index
**Estimate:** M

**Goal:**
Add `caller_id` column to the references table and implement Tree-sitter parent traversal to detect the enclosing function for each call-site reference.

**Action Items:**
- [ ] Add `caller_id INTEGER REFERENCES symbols(id)` column to `references` table (nullable for file-scope calls)
- [ ] Add index on `caller_id` for efficient JOIN queries (DR-015)
- [ ] Handle schema migration: detect existing indexes without caller_id, add column via ALTER TABLE
- [ ] Implement enclosing symbol detection (DR-021): when a call-site node is encountered during Tree-sitter parsing, walk `node.parent()` to find the nearest enclosing function/method node
- [ ] Map enclosing node to its `symbols.id` (match by file, name, line range)
- [ ] Set `caller_id = NULL` for file-scope calls (no enclosing function) — treated as `<module>` at query time (PRD-CGR-REQ-002)
- [ ] Support all 11 languages for enclosing function detection

**Dependencies:**
- Blocked by: None
- Blocks: TASK-058, TASK-061, TASK-062

**Acceptance Criteria:**
- `caller_id` column exists in references table with FK to symbols.id
- Index on caller_id is created for query performance
- Existing indexes without caller_id upgrade gracefully (ALTER TABLE)
- Enclosing function detection correctly identifies parent function/method for call sites
- File-scope calls have caller_id = NULL
- Works for all 11 supported languages
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-CGR-REQ-001, PRD-CGR-REQ-002
**Related Decisions:** DR-015, DR-021

**Status:** Not Started

---

### TASK-058: Populate caller_id in build pipeline and daemon

**Milestone:** M15 - Call Graph Data Model & Indexing
**Component:** Pipeline, Background Daemon
**Estimate:** M

**Goal:**
Wire enclosing function detection into the full index build (`wonk init`/`wonk update`) and daemon incremental re-indexing so caller_id is populated on all reference rows.

**Action Items:**
- [ ] During full index build (pipeline.rs), after extracting references for each file, resolve enclosing functions and set caller_id on each reference row
- [ ] Use two-pass approach within each file: first extract symbols (to get their IDs), then extract references with caller_id resolution
- [ ] During daemon incremental re-indexing, populate caller_id on new reference rows using the same logic
- [ ] For `wonk update` on existing indexes: full rebuild includes caller_id population
- [ ] When indexes lack caller_id data, call graph queries should return empty results with a hint to re-index
- [ ] Log caller_id population stats during init (e.g., "Populated N caller relationships")

**Dependencies:**
- Blocked by: TASK-057
- Blocks: TASK-061, TASK-062

**Acceptance Criteria:**
- After `wonk init`, references table has non-null caller_id for calls inside functions
- After daemon re-indexes a file, new references have caller_id populated
- `wonk update` rebuilds all caller_id relationships
- Fresh indexes include caller_id from the start
- Caller_id stats shown during init progress
- Progress/stat messages emitted to stderr, not stdout
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-CGR-REQ-001
**Related Decisions:** DR-015, DR-021

**Status:** Not Started

---

## Milestone 16: Source Display (`wonk show`)

**Goal:** `wonk show <name>` looks up symbols in the index, reads their source code from the file, and returns it with line numbers — collapsing the symbol-lookup + file-read round-trip into a single call.
**Exit Criteria:** `wonk show processPayment` returns the full function body. Filtering, shallow mode, budget, and MCP tool all work.

### TASK-059: `wonk show` core implementation

**Milestone:** M16 - Source Display
**Component:** Source Display, CLI
**Estimate:** L

**Goal:**
Implement the core `wonk show <name>` command that looks up symbols in the index, reads their source spans from source files, and formats output with line numbers.

**Action Items:**
- [ ] Create `show.rs` module with `show_symbol(db, name, options) -> Vec<ShowResult>` function
- [ ] Add `show` subcommand to CLI with args: `<name>` (required), `--file <path>`, `--kind <kind>`, `--exact`, `--format` (grep|json|toon)
- [ ] Query symbols table for matching name (substring by default, exact with --exact)
- [ ] Filter by --file (file path prefix match) and --kind (symbol kind filter)
- [ ] For each match: read source file lines from `line` to `end_line`, prefix each with 1-based line number (PRD-SHOW-REQ-008)
- [ ] Multiple matches: display all, each preceded by file header `file:start_line-end_line` (PRD-SHOW-REQ-002)
- [ ] No end_line fallback: display signature text from index (PRD-SHOW-REQ-010)
- [ ] Missing source file: skip result, emit warning to stderr (PRD-SHOW-REQ-011)
- [ ] No index fallback: return error directing user to `wonk init` (PRD-SHOW-REQ-009)
- [ ] Define `ShowResult` in types.rs: name, kind, file, line, end_line, source, language (PRD-SHOW-REQ-012)
- [ ] JSON/TOON output includes all ShowResult fields
- [ ] Wire to CLI dispatch

**Dependencies:**
- Blocked by: None
- Blocks: TASK-060

**Acceptance Criteria:**
- `wonk show processPayment` returns full function body with line numbers
- `wonk show --kind class StripeClient` filters to class only
- `wonk show --file src/auth.ts login` restricts to symbols in that file
- `wonk show --exact foo` requires exact name match
- Multiple matches shown with file headers
- Missing end_line falls back to signature display
- Missing source file produces stderr warning
- No index produces error with init guidance
- JSON output includes all ShowResult fields
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SHOW-REQ-001 through PRD-SHOW-REQ-005, PRD-SHOW-REQ-008 through PRD-SHOW-REQ-012 (PRD-SHOW-REQ-006, REQ-007, REQ-013 covered by TASK-060)
**Related Decisions:** DR-017, DR-022

**Status:** Not Started

---

### TASK-060: `wonk show` shallow mode, budget, and MCP tool

**Milestone:** M16 - Source Display
**Component:** Source Display, CLI, MCP Server
**Estimate:** M

**Goal:**
Add shallow mode for container types, budget truncation, and expose `wonk_show` as an MCP tool.

**Action Items:**
- [ ] Implement `--shallow` flag (PRD-SHOW-REQ-006, DR-017): for container types (class, struct, enum, trait, interface), query child symbols via `scope` column match in the same file
- [ ] Shallow display: container's signature line followed by each child's `signature` field (no bodies)
- [ ] No Tree-sitter re-parse needed — uses existing index data
- [ ] Implement `--budget <n>` flag (PRD-SHOW-REQ-007): use existing budget module (~4 chars/token heuristic) to truncate output and indicate omission
- [ ] Add `wonk_show` MCP tool with parameters: name (required), kind (optional), file (optional), exact (boolean), shallow (boolean), budget (integer), format (json|toon) (PRD-SHOW-REQ-013)
- [ ] Wire MCP tool handler to existing show_symbol backend

**Dependencies:**
- Blocked by: TASK-059
- Blocks: None

**Acceptance Criteria:**
- `wonk show --shallow StripeClient` shows class signature + method signatures without bodies
- Shallow mode works for all container types (class, struct, enum, trait, interface)
- `wonk show --budget 100 LargeClass` truncates and notes omission
- MCP tool `wonk_show` works with all parameters
- MCP returns identical results to CLI
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SHOW-REQ-006, PRD-SHOW-REQ-007, PRD-SHOW-REQ-013
**Related Decisions:** DR-017, DR-022

**Status:** Not Started

---

## Milestone 17: Call Graph Commands

**Goal:** `wonk callers`, `wonk callees`, and `wonk callpath` enable symbol-level call graph navigation, letting agents trace execution paths and understand blast radius at the function level.
**Exit Criteria:** All three commands return correct results. Transitive expansion works. BFS call path finds shortest chains. MCP tools work.

### TASK-061: `wonk callers` and `wonk callees` with transitive expansion

**Milestone:** M17 - Call Graph Commands
**Component:** Call Graph, CLI, MCP Server
**Estimate:** L

**Goal:**
Implement `wonk callers <symbol>` and `wonk callees <symbol>` commands with transitive depth expansion and MCP tool exposure.

**Action Items:**
- [ ] Create `callgraph.rs` module
- [ ] Implement `callers(db, name, depth) -> Vec<CallerResult>`: SQL query `SELECT DISTINCT s.* FROM references r JOIN symbols s ON r.caller_id = s.id WHERE r.name = ?` (PRD-CGR-REQ-003)
- [ ] Implement `callees(db, name, depth) -> Vec<CalleeResult>`: SQL query `SELECT DISTINCT r.name, ... FROM references r WHERE r.caller_id IN (SELECT id FROM symbols WHERE name LIKE ?)` (PRD-CGR-REQ-004)
- [ ] Implement transitive expansion (PRD-CGR-REQ-005, PRD-CGR-REQ-006): --depth N iteratively expands at each level
- [ ] Default depth 1 (PRD-CGR-REQ-007), cap at 10 with warning (PRD-CGR-REQ-008)
- [ ] Handle multiple definitions (PRD-CGR-REQ-011): include results from all definitions, indicate which definition
- [ ] Handle file-scope callers: display as `<module>` scope
- [ ] Auto-init: consistent with PRD-AUT behavior (PRD-CGR-REQ-012)
- [ ] Old indexes without caller_id: return empty results with hint to re-index via `wonk update`
- [ ] Add `callers` and `callees` subcommands to CLI with args: `<symbol>` (required), `--depth <n>` (optional, default 1)
- [ ] Output formatting: grep-compatible + JSON/TOON
- [ ] Add MCP tools `wonk_callers` and `wonk_callees` with parameters: name, depth, format (PRD-CGR-REQ-013)

**Dependencies:**
- Blocked by: TASK-057, TASK-058
- Blocks: TASK-062

**Acceptance Criteria:**
- `wonk callers dispatch` lists all functions whose bodies call `dispatch`
- `wonk callers dispatch --depth 2` lists direct callers and their callers
- `wonk callees main` lists all functions called within `main`
- `--depth 15` warns about depth cap and uses depth 10
- Multiple definitions handled correctly
- Auto-init works on unindexed repos
- Old indexes without caller_id show hint to re-index
- MCP tools work with all parameters
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-CGR-REQ-002 through PRD-CGR-REQ-008, PRD-CGR-REQ-011, PRD-CGR-REQ-012, PRD-CGR-REQ-013
**Related Decisions:** DR-015, DR-021, DR-022

**Status:** Not Started

---

### TASK-062: `wonk callpath` BFS call chain finder

**Milestone:** M17 - Call Graph Commands
**Component:** Call Graph, CLI, MCP Server
**Estimate:** M

**Goal:**
Implement `wonk callpath <from> <to>` that finds call chains between two symbols via BFS traversal.

**Action Items:**
- [ ] Implement `callpath(db, from, to) -> Option<Vec<CallPathHop>>` (DR-016): BFS from `<from>` expanding callees at each level
- [ ] Maintain visited set (HashSet) and parent map (HashMap) for path reconstruction
- [ ] When `<to>` is reached, reconstruct shortest path via parent map
- [ ] If BFS exhausts the graph without reaching `<to>`, report "no path found" (PRD-CGR-REQ-010)
- [ ] Cap BFS depth at 10 (consistent with callers/callees cap)
- [ ] Define `CallPathHop` struct: symbol_name, symbol_kind, file, line
- [ ] Add `callpath` subcommand to CLI with args: `<from>` (required), `<to>` (required)
- [ ] Output formatting: chain display `from -> hop1 -> hop2 -> to` with file:line per hop (use ASCII `->`, not Unicode arrows, for terminal compatibility)
- [ ] JSON/TOON output: array of CallPathHop structs
- [ ] Add MCP tool `wonk_callpath` with parameters: from, to, format (PRD-CGR-REQ-014)

**Dependencies:**
- Blocked by: TASK-061
- Blocks: None

**Acceptance Criteria:**
- `wonk callpath main dispatch` shows the call chain from main to dispatch
- `wonk callpath foo bar` where no path exists prints "no path found"
- BFS finds shortest path
- Depth capped at 10
- JSON output includes all hop details
- MCP tool `wonk_callpath` works with all parameters
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-CGR-REQ-009, PRD-CGR-REQ-010, PRD-CGR-REQ-014
**Related Decisions:** DR-016, DR-022

**Status:** Not Started

---

## Milestone 18: Code Summary Engine (`wonk summary`)

**Goal:** `wonk summary <path>` provides structural metrics and optional LLM-generated descriptions of files and directories, giving agents and developers a quick orientation without reading every file.
**Exit Criteria:** Structural summaries at all detail levels. Recursive depth works. LLM descriptions generated and cached. MCP tool works.

### TASK-063: Structural summary engine and `wonk summary` CLI

**Milestone:** M18 - Code Summary Engine
**Component:** Code Summary Engine, CLI, MCP Server
**Estimate:** L

**Goal:**
Implement `wonk summary <path>` with structural metrics aggregation, three detail levels, recursive depth, and MCP tool exposure.

**Action Items:**
- [ ] Create `summary.rs` module
- [ ] Implement `summarize_path(db, path, options) -> SummaryResult`: query files and symbols tables to aggregate metrics
- [ ] Structural metrics: file count, line count, symbol count by kind, language breakdown, dependency count (PRD-SUM-REQ-001, PRD-SUM-REQ-002)
- [ ] Detail levels: `--detail rich` (default, all metrics), `--detail light` (file count, symbol count, languages), `--detail symbols` (symbol counts by kind only) (PRD-SUM-REQ-003 through PRD-SUM-REQ-005)
- [ ] Recursion: `--depth N` summarizes nested directories/files up to N levels (PRD-SUM-REQ-006), default depth 0 (PRD-SUM-REQ-007), `--recursive` for unlimited depth (PRD-SUM-REQ-008)
- [ ] Define `SummaryResult` in types.rs: path, type (file|directory), detail_level, metrics, children (array), description (optional) (PRD-SUM-REQ-017)
- [ ] Add `summary` subcommand to CLI with args: `<path>` (required), `--detail`, `--depth`, `--recursive`, `--semantic` (wired in TASK-064)
- [ ] Output formatting: human-readable default, JSON/TOON structured output
- [ ] Auto-init: consistent with PRD-AUT behavior (PRD-SUM-REQ-016)
- [ ] Add MCP tool `wonk_summary` with parameters: path, detail, depth, recursive, semantic, format (PRD-SUM-REQ-018)

**Dependencies:**
- Blocked by: None
- Blocks: TASK-064

**Acceptance Criteria:**
- `wonk summary src/` displays file count, line count, symbol counts, language breakdown, dependency count
- `wonk summary src/ --detail light` shows only file count, symbol count, languages
- `wonk summary src/ --detail symbols` shows only symbol counts by kind
- `wonk summary src/ --depth 2` shows target + children + grandchildren
- `wonk summary src/ --recursive` shows full hierarchy
- JSON output includes all SummaryResult fields
- Auto-init works on unindexed repos
- MCP tool works with all parameters
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SUM-REQ-001 through PRD-SUM-REQ-008, PRD-SUM-REQ-016, PRD-SUM-REQ-017, PRD-SUM-REQ-018
**Related Decisions:** DR-018, DR-019, DR-020, DR-022

**Status:** Not Started

---

### TASK-064: LLM description generation and caching

**Milestone:** M18 - Code Summary Engine
**Component:** Code Summary Engine, Configuration, SQLite Database
**Estimate:** M

**Goal:**
Add `--semantic` flag to `wonk summary` that generates LLM descriptions via Ollama, with caching in SQLite and configurable model selection.

**Action Items:**
- [ ] Add `[llm]` section to Config with `model` key (default: `"llama3.2:3b"`) and `generate_url` (default: `"http://localhost:11434/api/generate"`) (DR-018, PRD-SUM-REQ-014)
- [ ] Add `summaries` table to SQLite schema: path, content_hash, description, created_at (DR-020)
- [ ] Implement content hash computation: sorted `(symbol.id, file.hash)` pairs under the path (DR-019)
- [ ] Implement prompt construction (PRD-SUM-REQ-010): path, language breakdown, symbol signatures by kind, import/export relationships — ask for 2-3 sentence description
- [ ] Implement Ollama `/api/generate` call via ureq: POST with model, prompt, stream=false
- [ ] Cache hit: return cached description when path + content_hash match (PRD-SUM-REQ-012)
- [ ] Ollama unreachable: display warning to stderr, return structural summary without description on stdout (PRD-SUM-REQ-013)
- [ ] Model not found: return error with instructions to `ollama pull <model>` or configure `[llm].model` (PRD-SUM-REQ-015)
- [ ] Semantic + recursion interaction: LLM description only for top-level path, not per-child
- [ ] Default model llama3.2:3b when no config (PRD-SUM-REQ-015)

**Dependencies:**
- Blocked by: TASK-063
- Blocks: None

**Acceptance Criteria:**
- `wonk summary src/ --semantic` includes LLM-generated description
- Repeated call on unchanged code returns cached description instantly
- `wonk summary src/ --semantic` without config uses llama3.2:3b; error with instructions if model unavailable
- `wonk summary src/ --semantic` with Ollama down shows warning on stderr + structural only on stdout
- Cache invalidated when content changes (different hash)
- `[llm].model` config override works
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-SUM-REQ-009 through PRD-SUM-REQ-015
**Related Decisions:** DR-018, DR-019, DR-020

**Status:** Not Started

---

## Milestone 19: Edge Confidence & Inheritance Infrastructure

**Goal:** Enrich the call graph with confidence scores on reference edges and inheritance (extends/implements) relationships, enabling V4 commands to filter low-confidence edges and traverse type hierarchies.
**Exit Criteria:** `references` table has `confidence` column populated during indexing, `type_edges` table stores inheritance relationships, daemon re-index propagates both.

### TASK-065: V4 schema migration and edge confidence scoring

**Milestone:** M19 - Edge Confidence & Inheritance Infrastructure
**Component:** SQLite Database, Indexer
**Estimate:** M

**Goal:**
Add `confidence REAL` column to the `references` table, create the `type_edges` table, and implement confidence scoring logic in the indexer.

**Action Items:**
- [ ] Add `confidence REAL DEFAULT 0.5` column to `references` table via `ALTER TABLE` (O(1) migration, no row rewriting) (DR-028)
- [ ] Create index `idx_references_confidence` on the new column
- [ ] Create `type_edges` table with columns: `id INTEGER PRIMARY KEY`, `child_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE`, `parent_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE`, `relationship TEXT NOT NULL` (DR-029)
- [ ] Add `UNIQUE(child_id, parent_id, relationship)` constraint on `type_edges`
- [ ] Create indexes on both `child_id` and `parent_id` for bidirectional queries
- [ ] Implement confidence scoring logic in `indexer.rs`: import-resolved → 0.95 (PRD-CONF-REQ-002), same-file definition → 0.85 (PRD-CONF-REQ-003), same-scope → 0.80, cross-file name match → 0.50 (PRD-CONF-REQ-004)
- [ ] During reference extraction, check import resolution evidence and assign confidence per reference (PRD-CONF-REQ-001)
- [ ] Add `--min-confidence <N>` flag to graph traversal CLI commands: `blast`, `flows`, `callers`, `callees`, `callpath`, `context` (PRD-CONF-REQ-005)
- [ ] Include `confidence` field in JSON/TOON output for all graph commands (PRD-CONF-REQ-006)
- [ ] Backward compatibility: existing indexes get all refs at 0.5 (the DEFAULT); re-index recalculates

**Dependencies:**
- Blocked by: None
- Blocks: TASK-066, TASK-067

**Acceptance Criteria:**
- `ALTER TABLE` migration succeeds on existing indexes without row rewriting
- `type_edges` table created with proper constraints and indexes
- Import-resolved references have confidence >= 0.9
- Same-file references have confidence >= 0.8
- Fuzzy cross-file name-matched references have confidence <= 0.5
- `wonk callers foo --min-confidence 0.8` excludes low-confidence matches
- JSON output includes confidence field on all graph edges
- Existing indexes work without re-index (all refs get 0.5)
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-CONF-REQ-001 through PRD-CONF-REQ-006
**Related Decisions:** DR-028, DR-029

**Status:** Not Started

---

### TASK-066: Inheritance extraction across OOP languages

**Milestone:** M19 - Edge Confidence & Inheritance Infrastructure
**Component:** Indexer
**Estimate:** L

**Goal:**
Extract `extends` and `implements` relationships from Tree-sitter parse trees for 8+ OOP languages and store them as typed edges in the `type_edges` table.

**Action Items:**
- [ ] TypeScript/JavaScript: extract `class_heritage` → `extends_clause` for extends, `implements_clause` for implements (PRD-HRTG-REQ-001, PRD-HRTG-REQ-002)
- [ ] Python: extract `class_definition` → `argument_list` for superclass (PRD-HRTG-REQ-001)
- [ ] Java: extract `superclass` node for extends, `super_interfaces` node for implements (PRD-HRTG-REQ-001, PRD-HRTG-REQ-002)
- [ ] C#: extract `base_list` → class types for extends, interface types for implements (PRD-HRTG-REQ-001, PRD-HRTG-REQ-002)
- [ ] C++: extract `base_class_clause` for extends (PRD-HRTG-REQ-001)
- [ ] Ruby: extract `superclass` node for extends (PRD-HRTG-REQ-001)
- [ ] Rust: extract `impl_item` for trait implementation → implements edge (PRD-HRTG-REQ-002)
- [ ] PHP: extract `class_declaration` → `base_clause` for extends, `class_interface_clause` for implements (PRD-HRTG-REQ-001, PRD-HRTG-REQ-002)
- [ ] C and Go: skip (no class inheritance; Go interfaces are implicit)
- [ ] Parent resolution: look up parent symbol in same-file symbols or resolved via imports; skip edge if no match found
- [ ] Store edges with `relationship` = `"extends"` or `"implements"` (PRD-HRTG-REQ-005)
- [ ] Include `relationship` field in JSON/TOON output for type edges (PRD-HRTG-REQ-005)

**Dependencies:**
- Blocked by: TASK-065
- Blocks: TASK-067

**Acceptance Criteria:**
- `wonk init` on a TypeScript project extracts extends/implements relationships
- `wonk init` on a Java project extracts class hierarchy and interface implementations
- `wonk init` on a Rust project extracts trait implementations
- Parent resolution correctly links child to parent symbol
- Unresolvable parents are silently skipped (no edge stored)
- Type edges include `relationship` field in output
- C and Go projects produce no type edges
- Tests cover at least TypeScript, Python, Java, Rust, C# extraction
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-HRTG-REQ-001, PRD-HRTG-REQ-002, PRD-HRTG-REQ-005
**Related Decisions:** DR-029

**Status:** Not Started

---

### TASK-067: Wire confidence + inheritance into build pipeline and daemon

**Milestone:** M19 - Edge Confidence & Inheritance Infrastructure
**Component:** Pipeline, Background Daemon
**Estimate:** M

**Goal:**
Ensure full index builds (`wonk init`/`wonk update`) and daemon incremental re-indexing populate confidence scores on all references and extract/store inheritance edges in `type_edges`.

**Action Items:**
- [ ] During full index build (pipeline.rs), compute confidence for each reference after extraction (using the import-resolution evidence from TASK-065)
- [ ] During full index build, extract inheritance relationships per file and batch-insert into `type_edges`
- [ ] During daemon incremental re-indexing, compute confidence for new/updated references
- [ ] During daemon incremental re-indexing, delete stale type_edges for re-indexed files and insert fresh ones
- [ ] For `wonk update`: full rebuild recalculates all confidence scores and rebuilds type_edges
- [ ] Log confidence and inheritance stats during init (e.g., "Scored N references, extracted M type edges")
- [ ] Stats messages emitted to stderr, not stdout

**Dependencies:**
- Blocked by: TASK-065, TASK-066
- Blocks: TASK-069, TASK-070, TASK-073

**Acceptance Criteria:**
- After `wonk init`, references have varied confidence values (not all 0.5)
- After `wonk init`, type_edges table populated for OOP codebases
- After daemon re-indexes a file, new references have confidence recalculated
- After daemon re-indexes a file, stale type_edges removed, fresh ones inserted
- `wonk update` rebuilds all confidence and type_edge data
- Stats displayed during init progress
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-CONF-REQ-001, PRD-HRTG-REQ-001, PRD-HRTG-REQ-002
**Related Decisions:** DR-028, DR-029

**Status:** Not Started

---

## Milestone 20: Hybrid Search Fusion (RRF)

**Goal:** Replace simple structural-first/semantic-append blending with Reciprocal Rank Fusion for `wonk search --semantic`, producing optimally ranked interleaved results.
**Exit Criteria:** `wonk search --semantic "auth"` returns results ranked by RRF score, with high-relevance semantic matches interleaved above low-relevance structural matches.

### TASK-068: Reciprocal Rank Fusion for `wonk search --semantic`

**Milestone:** M20 - Hybrid Search Fusion
**Component:** Ranker, Router, Configuration
**Estimate:** S

**Goal:**
Implement `fuse_rrf()` in `ranker.rs` that merges structural and semantic result lists using the RRF formula, and wire it into the search pipeline replacing the existing blending logic.

**Action Items:**
- [ ] Add `fuse_rrf(structural: &[RankedResult], semantic: &[SemanticResult], k: f32) -> Vec<FusedResult>` to `ranker.rs` (~40 lines) (PRD-RRF-REQ-001)
- [ ] Implement RRF formula: `score(d) = Sum 1/(K + rank_i(d))` across all result lists (PRD-RRF-REQ-001)
- [ ] Default K=60 (PRD-RRF-REQ-002)
- [ ] Add `rrf_k` to `[search]` section in config.toml schema; use configured value when present (PRD-RRF-REQ-003)
- [ ] Define `FusedResult` with: result data, rrf_score, source tracking (Structural, Semantic, or Both)
- [ ] Sort output by descending RRF score (PRD-RRF-REQ-004)
- [ ] Replace existing `blended_search()` call in `router.rs` with `fuse_rrf()` call
- [ ] Apply existing budget/ranking post-processing after fusion

**Dependencies:**
- Blocked by: None
- Blocks: None

**Acceptance Criteria:**
- `wonk search --semantic "auth"` returns interleaved results ranked by RRF score
- A high-ranked semantic result can appear before a low-ranked structural result
- Default K=60 produces reasonable interleaving
- Custom `rrf_k` value from config.toml is respected
- Existing non-semantic search (`wonk search "auth"`) is unaffected
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-RRF-REQ-001 through PRD-RRF-REQ-004
**Related Decisions:** DR-027

**Status:** Not Started

---

## Milestone 21: Execution Flow Detection (`wonk flows`)

**Goal:** `wonk flows` detects entry point symbols and traces execution paths through the call graph via BFS.
**Exit Criteria:** `wonk flows` lists entry points, `wonk flows main` traces the full flow, `--from` file filtering works, MCP tool exposed.

### TASK-069: `wonk flows` entry point detection and flow tracing

**Milestone:** M21 - Execution Flow Detection
**Component:** Flow Detection, CLI, MCP Server
**Estimate:** L

**Goal:**
Implement `flows.rs` module with entry point detection via SQL anti-join and forward BFS flow tracing, plus CLI subcommand and MCP tool.

**Action Items:**
- [ ] Create `flows.rs` module (~200 lines) (DR-023)
- [ ] Implement `detect_entry_points(db, options) -> Vec<Symbol>`: SQL anti-join to find functions/methods with no indexed callers (PRD-FLOW-REQ-001)
  - Query: `SELECT s.* FROM symbols s WHERE s.kind IN ('function', 'method') AND s.id NOT IN (SELECT DISTINCT caller_id FROM "references" WHERE caller_id IS NOT NULL)`
- [ ] Implement `trace_flow(db, entry, options) -> ExecutionFlow`: BFS from entry point expanding callees at each level (PRD-FLOW-REQ-002, PRD-FLOW-REQ-003)
- [ ] `--depth N` caps BFS traversal (default: 10, maximum: 20) (PRD-FLOW-REQ-004)
- [ ] `--branching N` limits callees followed per symbol (default: 4), sorted by confidence descending (PRD-FLOW-REQ-005)
- [ ] Exclude flows with fewer than 2 steps (PRD-FLOW-REQ-006)
- [ ] Each step includes: symbol name, kind, file path, line number, depth (PRD-FLOW-REQ-007)
- [ ] `--from <file>` restricts entry point detection to symbols in the specified file (PRD-FLOW-REQ-008)
- [ ] Honor `--min-confidence` to exclude low-confidence edges during traversal (PRD-CONF-REQ-005)
- [ ] Define `ExecutionFlow` in types.rs: entry_point, steps (ordered array), step_count (PRD-FLOW-REQ-009)
- [ ] Add `flows` subcommand to CLI with args: `[entry]` (optional), `--from`, `--depth`, `--branching`, `--min-confidence`, `--format`
- [ ] JSON/TOON output includes structured fields (PRD-FLOW-REQ-009)
- [ ] Add MCP tool `wonk_flows` with parameters: entry, from, depth, branching, min_confidence, format (PRD-FLOW-REQ-010)
- [ ] Auto-init: consistent with PRD-AUT behavior

**Dependencies:**
- Blocked by: TASK-058 (V3 — caller_id population), TASK-067
- Blocks: TASK-072, TASK-073

**Acceptance Criteria:**
- `wonk flows` lists all detected entry points with call depth
- `wonk flows main` traces the full execution flow from `main`
- `wonk flows --from src/api.ts` shows flows starting from that file only
- Flows with only 1 step are excluded
- `--depth 5` limits BFS to 5 levels
- `--branching 2` follows at most 2 callees per symbol
- `--min-confidence 0.8` excludes fuzzy-matched edges
- MCP tool `wonk_flows` works through Claude Code
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-FLOW-REQ-001 through PRD-FLOW-REQ-010, PRD-CONF-REQ-005
**Related Decisions:** DR-023

**Status:** Not Started

---

## Milestone 22: Blast Radius Analysis (`wonk blast`)

**Goal:** `wonk blast <symbol>` traverses the call graph outward from a symbol, grouping results by depth-based severity tiers with risk level assessment.
**Exit Criteria:** `wonk blast processPayment` shows callers grouped by severity, risk levels computed, test files excluded by default, MCP tool exposed.

### TASK-070: `wonk blast` depth-annotated traversal with severity tiers

**Milestone:** M22 - Blast Radius Analysis
**Component:** Blast Radius, CLI, MCP Server
**Estimate:** L

**Goal:**
Implement `blast.rs` module with depth-annotated BFS, severity tiers, risk levels, inheritance integration, and test exclusion, plus CLI subcommand and MCP tool.

**Action Items:**
- [ ] Create `blast.rs` module (~200 lines) (DR-024)
- [ ] Implement `analyze_blast(db, symbol, options) -> BlastAnalysis`: depth-annotated BFS from target symbol (PRD-BLAST-REQ-001)
- [ ] Direction control: upstream (default) traverses callers + type_edges children, downstream traverses callees (PRD-BLAST-REQ-004, PRD-BLAST-REQ-005)
- [ ] Severity tiers by depth: depth 1 = "WILL BREAK", depth 2 = "LIKELY AFFECTED", depth 3+ = "MAY NEED TESTING" (PRD-BLAST-REQ-002)
- [ ] Risk level from total affected count: LOW ≤3, MEDIUM 4-10, HIGH 11-25, CRITICAL >25 (PRD-BLAST-REQ-003)
- [ ] `--depth N` caps traversal (default: 3, maximum: 10) (PRD-BLAST-REQ-006)
- [ ] Affected files summary: deduplicated list of files containing affected symbols (PRD-BLAST-REQ-007)
- [ ] Test exclusion by default: reuse ranker.rs path heuristics (test/, tests/, *_test.*, *.test.*, *.spec.*); `--include-tests` overrides (PRD-BLAST-REQ-008)
- [ ] Inheritance integration: query `type_edges WHERE parent_id = ?` to include child classes as depth-1 dependants during upstream traversal (PRD-HRTG-REQ-003)
- [ ] Honor `--min-confidence` to exclude low-confidence edges (PRD-CONF-REQ-005)
- [ ] Define `BlastAnalysis` in types.rs: target, direction, risk_level, total_affected, tiers[], affected_files[] (PRD-BLAST-REQ-009)
- [ ] Add `blast` subcommand to CLI with args: `<symbol>` (required), `--direction`, `--depth`, `--include-tests`, `--min-confidence`, `--format`
- [ ] JSON/TOON output includes all BlastAnalysis fields (PRD-BLAST-REQ-009)
- [ ] Add MCP tool `wonk_blast` with parameters: symbol, direction, depth, include_tests, min_confidence, format (PRD-BLAST-REQ-010)
- [ ] Auto-init: consistent with PRD-AUT behavior

**Dependencies:**
- Blocked by: TASK-058 (V3 — caller_id population), TASK-067
- Blocks: TASK-072

**Acceptance Criteria:**
- `wonk blast processPayment` shows callers grouped by depth with severity labels
- `wonk blast processPayment --direction downstream` shows callees grouped by depth
- Risk levels correctly reflect affected symbol counts
- Test files excluded by default; included with `--include-tests`
- `wonk blast IPaymentProvider` includes all implementors in depth-1 tier (inheritance)
- `--min-confidence 0.8` excludes fuzzy-matched edges
- Affected files summary included in output
- MCP tool works through Claude Code
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-BLAST-REQ-001 through PRD-BLAST-REQ-010, PRD-HRTG-REQ-003, PRD-CONF-REQ-005
**Related Decisions:** DR-024

**Status:** Not Started

---

## Milestone 23: Scoped Change Detection (`wonk changes`)

**Goal:** `wonk changes` maps git diff hunks to indexed symbols and optionally chains into blast radius and flow analysis.
**Exit Criteria:** `wonk changes` detects symbols affected by unstaged changes, `--scope` variants work, `--blast` and `--flows` chaining produce aggregated impact.

### TASK-071: Hunk-to-symbol mapping for scoped change detection

**Milestone:** M23 - Scoped Change Detection
**Component:** Change Detection (impact.rs)
**Estimate:** M

**Goal:**
Extend `impact.rs` with `ChangeScope` enum and git diff hunk-to-symbol mapping that identifies which indexed symbols overlap with changed line ranges.

**Action Items:**
- [ ] Add `ChangeScope` enum to `impact.rs`: `Unstaged` (default), `Staged`, `All`, `Compare(ref)` (PRD-CHG-REQ-001 through PRD-CHG-REQ-004)
- [ ] Implement git diff scoping commands:
  - Unstaged: `git diff --name-only` (PRD-CHG-REQ-001)
  - Staged: `git diff --cached --name-only` (PRD-CHG-REQ-002)
  - All: `git diff HEAD --name-only` (PRD-CHG-REQ-003)
  - Compare: `git diff <ref> --name-only` (PRD-CHG-REQ-004)
- [ ] Implement hunk-to-symbol mapping (PRD-CHG-REQ-005):
  1. Run `git diff --unified=0 [flags] <file>` to get precise line ranges
  2. Parse diff output to extract changed line ranges from hunk headers (`@@ -start,count +start,count @@`)
  3. Query indexed symbols for the file: `SELECT * FROM symbols WHERE file = ?`
  4. Overlap check: symbol is Modified if any changed line range overlaps `line..end_line`
  5. Re-parse file with Tree-sitter for Added (new) and Removed (absent) symbols
- [ ] Reuse existing `detect_changed_symbols()` for Added/Removed detection
- [ ] Define `ChangeAnalysis` in types.rs: scope, changed_symbols[] (each with name, kind, file, line, change_type)

**Dependencies:**
- Blocked by: None
- Blocks: TASK-072

**Acceptance Criteria:**
- `detect_changes(db, Unstaged, options)` correctly identifies symbols affected by unstaged changes
- `detect_changes(db, Staged, options)` works for staged changes
- `detect_changes(db, Compare("main"), options)` works for branch comparison
- Hunk-to-symbol mapping correctly identifies Modified symbols from overlapping line ranges
- Added and Removed symbols detected via Tree-sitter re-parse
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-CHG-REQ-001 through PRD-CHG-REQ-005
**Related Decisions:** DR-025

**Status:** Not Started

---

### TASK-072: `wonk changes` CLI with blast/flow chaining

**Milestone:** M23 - Scoped Change Detection
**Component:** CLI, MCP Server, Change Detection (impact.rs)
**Estimate:** M

**Goal:**
Add `wonk changes` CLI subcommand with `--blast` and `--flows` chaining that calls blast radius and flow detection for each changed symbol, plus MCP tool exposure.

**Action Items:**
- [ ] Add `changes` subcommand to CLI with args: `--scope` (unstaged|staged|all|compare), `--base <ref>` (required when scope=compare), `--blast`, `--flows`, `--min-confidence`, `--format`
- [ ] Wire CLI to `detect_changes()` from TASK-071
- [ ] `--blast` chaining (PRD-CHG-REQ-006): for each changed symbol, call `analyze_blast()` from `blast.rs` and include aggregated per-symbol blast radius in output; compute combined risk level
- [ ] `--flows` chaining (PRD-CHG-REQ-007): identify execution flows (from `flows.rs`) containing any changed symbols; a flow is "affected" if any of its steps match a changed symbol
- [ ] JSON/TOON output includes: scope, changed_symbols[], blast_radius (optional), affected_flows (optional) (PRD-CHG-REQ-008)
- [ ] Add MCP tool `wonk_changes` with parameters: scope, base, blast, flows, min_confidence, format (PRD-CHG-REQ-009)
- [ ] Auto-init: consistent with PRD-AUT behavior

**Dependencies:**
- Blocked by: TASK-071, TASK-069, TASK-070
- Blocks: None

**Acceptance Criteria:**
- `wonk changes` shows symbols affected by unstaged changes
- `wonk changes --scope staged` shows symbols in staged changes
- `wonk changes --scope compare --base main` shows changes vs. main
- `wonk changes --blast` includes blast radius per changed symbol with aggregated risk level
- `wonk changes --flows` lists affected execution flows
- MCP tool works through Claude Code
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-CHG-REQ-006 through PRD-CHG-REQ-009
**Related Decisions:** DR-025

**Status:** Not Started

---

## Milestone 24: Unified Symbol Context (`wonk context`)

**Goal:** `wonk context <name>` aggregates definition, categorized incoming/outgoing references, flow participation, and children into a single response.
**Exit Criteria:** `wonk context processPayment` shows definition, callers, callees, importers, flows, and children in one response. MCP tool exposed.

### TASK-073: `wonk context` symbol information aggregation

**Milestone:** M24 - Unified Symbol Context
**Component:** Context, CLI, MCP Server
**Estimate:** L

**Goal:**
Implement `context.rs` orchestration module that aggregates definition, categorized incoming/outgoing references, flow participation, and children for a symbol, plus CLI subcommand and MCP tool.

**Action Items:**
- [ ] Create `context.rs` module (~150 lines) (DR-026)
- [ ] Implement `symbol_context(db, name, options) -> Vec<SymbolContext>` (PRD-CTX-REQ-001)
- [ ] Aggregate definition: file, line, end_line, kind, signature (from symbols table)
- [ ] Incoming references categorized as (PRD-CTX-REQ-005):
  - Callers: functions whose body calls this symbol (`references JOIN symbols ON caller_id`)
  - Importers: files that import this symbol (`file_imports WHERE name = ?`)
  - Type Users: symbols referencing this symbol's type in annotations/signatures
- [ ] Outgoing references categorized as (PRD-CTX-REQ-006):
  - Callees: symbols called within this function's body (`references WHERE caller_id = self.id`)
  - Imports: modules/symbols imported by this symbol's file
- [ ] Flow participation: which execution flows include this symbol and at which step (PRD-CTX-REQ-007)
- [ ] Children: classes extending or implementing this symbol from `type_edges WHERE parent_id = ?` (PRD-HRTG-REQ-004)
- [ ] `--file <path>` restricts to symbols in that file (PRD-CTX-REQ-002)
- [ ] `--kind <kind>` restricts to symbol kind (PRD-CTX-REQ-003)
- [ ] Multiple matches: return context for all, clearly labeled (PRD-CTX-REQ-004)
- [ ] Honor `--min-confidence` to filter low-confidence edges (PRD-CONF-REQ-005)
- [ ] Define `SymbolContext` in types.rs: symbol, incoming {callers[], importers[], type_users[]}, outgoing {callees[], imports[]}, flows[], children[] (PRD-CTX-REQ-008)
- [ ] Add `context` subcommand to CLI with args: `<name>` (required), `--file`, `--kind`, `--min-confidence`, `--format`
- [ ] JSON/TOON output includes all SymbolContext fields (PRD-CTX-REQ-008)
- [ ] Add MCP tool `wonk_context` with parameters: name, file, kind, min_confidence, format (PRD-CTX-REQ-009)
- [ ] Auto-init: consistent with PRD-AUT behavior

**Dependencies:**
- Blocked by: TASK-069, TASK-067
- Blocks: None

**Acceptance Criteria:**
- `wonk context processPayment` shows definition, callers, callees, importers, and flows in one response
- `wonk context --file src/auth.ts verifyToken` narrows to that file
- `wonk context --kind class StripeClient` narrows to class only
- `wonk context BaseHandler` shows extending classes under "Children"
- Categories are clearly separated in output
- Multiple matching symbols each get full context
- MCP tool works through Claude Code
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-CTX-REQ-001 through PRD-CTX-REQ-009, PRD-HRTG-REQ-004, PRD-CONF-REQ-005
**Related Decisions:** DR-026

**Status:** Not Started

---

## Milestone 25: Multi-Repo MCP

**Goal:** A single MCP server instance can serve queries across all indexed repositories via an optional `repo` parameter.
**Exit Criteria:** `wonk_repos` lists all indexed repos, `repo` parameter routes queries correctly, lazy-loaded connections, backward compatible default.

### TASK-074: Multi-repo MCP discovery and routing

**Milestone:** M25 - Multi-Repo MCP
**Component:** MCP Server
**Estimate:** M

**Goal:**
Extend `mcp.rs` to discover all indexed repositories at startup, lazy-load connections per repo, add optional `repo` parameter to all existing tools, and expose `wonk_repos` tool.

**Action Items:**
- [ ] Repo discovery at startup: glob `~/.wonk/repos/*/meta.json` to find all indexed repositories (PRD-MREP-REQ-001)
- [ ] Build repo registry: map of repo name (last path component) → repo metadata (path, index location)
- [ ] Implement lazy-load `HashMap<String, Connection>`: open connection on first query, cache for session lifetime (PRD-MREP-REQ-006)
- [ ] Default behavior: when no `repo` parameter provided, use working directory repo (PRD-MREP-REQ-002)
- [ ] Repo routing: when `repo` parameter provided, look up in registry, open/reuse connection, route query (PRD-MREP-REQ-003)
- [ ] Name matching: match by last path component of repo root; return error listing all matches for ambiguous names (PRD-MREP-REQ-004)
- [ ] Add `wonk_repos` MCP tool: lists all available repos with name, path, file count, symbol count, last indexed time (PRD-MREP-REQ-005)
- [ ] Add optional `repo` string parameter to all 18 existing MCP tool definitions
- [ ] Server restart required to discover newly indexed repos (documented limitation)

**Dependencies:**
- Blocked by: None
- Blocks: None

**Acceptance Criteria:**
- Single MCP server can answer queries about multiple repos
- `wonk_repos` lists all indexed repos with stats
- `wonk_search` with `repo: "other-project"` queries the other project's index
- Default (no `repo` param) uses working directory repo — backward compatible
- Lazy-loaded connections don't block server startup
- Ambiguous repo names return an error with all matching paths
- First query to a new repo opens connection (~5ms latency, negligible)
- Typecheck passes
- Tests pass

**Related Requirements:** PRD-MREP-REQ-001 through PRD-MREP-REQ-006
**Related Decisions:** DR-030

**Status:** Not Started

---

## Parking Lot

Tasks identified but not yet scheduled:

| ID | Description | Reason Deferred |
|----|-------------|-----------------|
| - | LSP server integration | Future version |
| - | Cross-language call graphs | Future version — V3 call graph is same-language only |
| - | Editor integrations | Future version |
| - | Remote/monorepo support | Future version |
| - | Web UI | Future version |
| - | Bundled/offline embedding model (ONNX) | Future version — would remove Ollama dependency |
| - | Configurable embedding models | Future version — single model for V2 |
| - | ANN indexing for >100K vectors | Future version — brute-force sufficient for V2 scale |
| - | Dynamic dispatch resolution for call graph | Future version — V3 tracks static calls only |

---

## Change Log

| Date | Change | Author |
|------|--------|--------|
| 2026-02-11 | Initial task breakdown — 30 tasks across 6 milestones | TBD |
| 2026-02-11 | Added Smart Search milestone (M6, TASK-031 to TASK-034). Renumbered Polish to M7. Updated milestone statuses. Total tasks: 34 across 7 milestones. Reframed around token-efficiency value proposition. | TBD |
| 2026-02-12 | Added Git Worktree Support milestone (M8, TASK-035 to TASK-037). 3 tasks: walker boundary exclusion, watcher boundary filtering, integration tests. Total tasks: 37 across 8 milestones. | TBD |
| 2026-02-13 | Added V2 semantic search milestones (M9-M14, TASK-038 to TASK-056). 19 tasks across 6 milestones: Embedding Infrastructure, Semantic Search, Daemon Embedding & Lifecycle, Semantic Blending & Dependency Scoping, Semantic Clustering, Change Impact Analysis. Total tasks: 56 across 14 milestones. | TBD |
| 2026-02-24 | Added V3 milestones (M15-M18, TASK-057 to TASK-064). 8 tasks across 4 milestones: Call Graph Data Model & Indexing, Source Display, Call Graph Commands, Code Summary Engine. Marked M11/M12 as Complete. Updated parking lot. Total tasks: 64 across 18 milestones. | TBD |
| 2026-02-25 | Added V4 milestones (M19-M25, TASK-065 to TASK-074). 10 tasks across 7 milestones: Edge Confidence & Inheritance Infrastructure, Hybrid Search Fusion (RRF), Execution Flow Detection, Blast Radius Analysis, Scoped Change Detection, Unified Symbol Context, Multi-Repo MCP. Total tasks: 74 across 25 milestones. | TBD |
