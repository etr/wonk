# EARS-based Product Requirements

**Doc status:** Draft 0.1
**Last updated:** 2026-02-11
**Owner:** TBD
**Audience:** Exec, Eng, Design, Data, QA, Sec

---

## 0) How we'll write requirements (EARS cheat sheet)
- **Ubiquitous form:** "When <trigger> then the system shall <response>."
- **Optional elements:** [when/while/until/as soon as] <trigger>, [the] system shall <response> [<object>].
- **Style:** Clear, atomic, testable, technology-agnostic.

---

## 1) Product context

- **Vision:** A single-binary CLI tool that provides structure-aware code search optimized for LLM coding agents. The core problem it solves is **token burn**: agents like Claude Code grep aggressively, stuffing hundreds of noisy, unranked lines into their context window. Wonk pre-indexes a codebase using Tree-sitter, understands code structure (definitions vs. usages, symbol kinds, scopes, imports), and returns results that are **filtered, ranked, and deduplicated** — delivering higher signal in fewer tokens. It maintains the index via a background file watcher and exposes results through a grep-compatible interface that existing tools can use with zero integration work.
- **Target users / segments:**
  - **Primary:** LLM coding agents (Claude Code, Aider, Continue, Cursor agent mode) that invoke CLI tools for codebase navigation.
  - **Secondary:** Developers who want fast structural code search from the terminal.
- **Key JTBDs:**
  - Reduce token consumption by returning ranked, deduplicated, structure-aware search results instead of raw grep output.
  - Find symbol definitions instantly without scanning every file.
  - Find all usages of a symbol name across the codebase.
  - Understand file-level dependency relationships.
  - Get grep-compatible output that LLM agents can parse without changes.
- **North-star metrics:**
  - Token reduction: `wonk search` returns ≥ 50% fewer lines than equivalent `rg` for the same query, while preserving ≥ 95% of relevant results.
  - Time to first result (warm index) < 100ms
  - Precision of `wonk sym` (correct definitions returned) > 90%
  - Recall of `wonk ref` (usages found vs grep baseline) > 80%
- **Release strategy:** V1 is CLI-only. Editor integrations, LSP backends, and cross-language call graphs are deferred to V2. V2 semantic search features (embedding-based search, clustering, impact analysis) are now specified below. V3 adds source display, code summary, and call graph analysis. V4 adds graph intelligence features: execution flow detection, blast radius analysis, scoped change detection, unified symbol context, hybrid search fusion, edge confidence scoring, inheritance tracking, and multi-repo MCP.

---

## 2) Non-functional & cross-cutting requirements

- **Latency:** Warm-index queries shall return results in < 100ms under typical conditions. Brief blocking (< 50ms) is acceptable during concurrent daemon writes. Cold auto-init on a 5k-file repo shall return results in < 5 seconds.
- **Storage:** Index size shall be approximately 1 MB per 10k symbols.
- **Repo discovery:** The system shall discover the repo root by walking up from the current directory looking for `.git` or `.wonk`.
- **Graceful degradation:** A stale index shall still return results. Queries shall tolerate brief write contention from the daemon without failing.
- **Binary size:** < 30 MB including all bundled Tree-sitter grammars.
- **Daemon idle resources:** < 15 MB memory, near-zero CPU.

---

## 3) Feature list (living backlog)

### 3.1 Text Search (PRD-SRCH)

**Problem / outcome**
LLM agents and developers need fast, grep-compatible text search across indexed files.

**In scope**
- Pattern matching via the grep crate
- Case-insensitive, regex, and path-restricted modes

**Out of scope**
- Semantic/embedding search (V2)

**EARS Requirements**
- `PRD-SRCH-REQ-001` When the user runs `wonk search <pattern>` then the system shall search all indexed files and return matching lines in `file:line:content` format.
- `PRD-SRCH-REQ-002` When the user provides `--regex` then the system shall interpret the pattern as a regular expression.
- `PRD-SRCH-REQ-003` When the user provides `-i` then the system shall perform case-insensitive matching.
- `PRD-SRCH-REQ-004` When the user provides a path after `--` then the system shall restrict search to files within that path.
- `PRD-SRCH-REQ-005` When the user provides `--json` then the system shall output results as JSON objects with file, line, col, and content fields.

**Acceptance criteria**
- Search returns correct matches identical to ripgrep for the same pattern
- Case-insensitive flag works across all file types
- Path restriction correctly limits results
- JSON output is valid and parseable

---

### 3.2 Smart Search (PRD-SSRCH)

**Problem / outcome**
Raw grep results are the #1 source of token burn for LLM coding agents. A search for a common symbol name returns every occurrence — definitions, usages, imports, re-exports, comments, test fixtures — with no ranking or deduplication. The agent stuffs all of this into its context window, wasting tokens on noise.

**In scope**
- Structure-aware result ranking (definitions first, then call sites, then other usages)
- Result grouping by symbol kind (definitions, references, imports, comments, tests)
- Deduplication (same symbol re-exported or aliased appears once)
- Token-budget mode: limit output to an approximate token count
- Automatic detection of whether a query matches a known symbol (use structural results) or is a plain string (use text search)

**Out of scope**
- Semantic/embedding search (V2)
- Type-aware deduplication (V2 — requires LSP)

**EARS Requirements**
- `PRD-SSRCH-REQ-001` When the user runs `wonk search <pattern>` and the pattern matches known symbol names in the index then the system shall return results ranked by relevance: definitions first, then call sites, then imports, then other usages, then comments and test files.
- `PRD-SSRCH-REQ-002` When returning ranked results then the system shall group results by category and display a category header (e.g., `-- definitions --`, `-- usages --`, `-- tests --`).
- `PRD-SSRCH-REQ-003` When the same symbol appears multiple times due to re-exports, type declarations, or import aliases then the system shall deduplicate results, showing the canonical definition and noting the count of duplicates.
- `PRD-SSRCH-REQ-004` When the user provides `--budget <n>` then the system shall limit output to approximately `n` tokens, prioritizing higher-ranked results.
- `PRD-SSRCH-REQ-005` When the pattern does not match any known symbol names then the system shall fall back to unranked text search (equivalent to grep).
- `PRD-SSRCH-REQ-006` When the user provides `--raw` then the system shall skip ranking and deduplication, returning unranked grep-style results.

**Acceptance criteria**
- For queries matching known symbols, output contains ≥ 50% fewer lines than equivalent `rg` while preserving ≥ 95% of relevant results
- Definitions always appear before usages in ranked output
- Duplicate re-exports are collapsed
- `--budget` limits output length
- `--raw` bypasses smart filtering
- Test files and comments are ranked lowest

---

### 3.3 Symbol Lookup (PRD-SYM)

**Problem / outcome**
Users need to quickly find symbol definitions (functions, classes, types) by name without scanning entire files.

**In scope**
- Name-based symbol lookup with substring and exact matching
- Kind filtering (function, class, method, type, etc.)

**Out of scope**
- Type-aware resolution (V2)

**EARS Requirements**
- `PRD-SYM-REQ-001` When the user runs `wonk sym <name>` then the system shall return all symbol definitions matching the name as a substring.
- `PRD-SYM-REQ-002` When the user provides `--kind <kind>` then the system shall filter results to symbols of that kind.
- `PRD-SYM-REQ-003` When the user provides `--exact` then the system shall match the name exactly.
- `PRD-SYM-REQ-004` When returning symbol results then each result shall include file path, line number, symbol kind, and signature.

**Acceptance criteria**
- Substring matching finds partial name matches
- Kind filter correctly limits to specified symbol type
- Exact match returns only symbols with identical names
- Results include all required fields

---

### 3.4 Reference Finding (PRD-REF)

**Problem / outcome**
Users need to find all usages of a symbol name across the codebase.

**In scope**
- Name-based reference lookup, path restriction

**Out of scope**
- Type-aware reference resolution, heuristic disambiguation

**EARS Requirements**
- `PRD-REF-REQ-001` When the user runs `wonk ref <name>` then the system shall return all indexed references matching the name.
- `PRD-REF-REQ-002` When the user provides a path after `--` then the system shall restrict results to files within that path.
- `PRD-REF-REQ-003` When returning reference results then each result shall include file path, line number, and the full source line.
- `PRD-REF-REQ-004` Where `output=files` is provided then the system shall return only unique file paths containing references, without per-reference line details.

**Acceptance criteria**
- References are found by name matching
- Path restriction limits results correctly
- Context lines are displayed for each reference

---

### 3.5 Signature Display (PRD-SIG)

**Problem / outcome**
Users need to view function/method signatures without opening files.

**In scope**
- Signature text display for matching symbols

**Out of scope**
- Docstring/documentation extraction

**EARS Requirements**
- `PRD-SIG-REQ-001` When the user runs `wonk sig <name>` then the system shall return the signature text for all matching symbol definitions.

**Acceptance criteria**
- Signatures are displayed for matching symbols
- Output includes file and line for each signature

---

### 3.6 Symbol Listing (PRD-LST) — DEPRECATED

Absorbed into PRD-SUM. The `wonk summary` command with `--detail rich` now provides
full symbol listing (with line, col, end_line, scope, signature) and `--tree` support,
superseding the former `wonk ls` subcommand.

Original requirements PRD-LST-REQ-001 and PRD-LST-REQ-002 are now satisfied by:
- PRD-SUM-REQ-001/002 (symbol listing via rich detail)
- PRD-SUM-REQ-019 (tree display via --tree flag)
- PRD-SUM-REQ-020 (per-symbol location metadata)

**Acceptance criteria**
- Lists all symbols in a file
- Recursively lists symbols for directories
- Tree view correctly shows nesting (e.g., methods under classes)

---

### 3.7 Dependency Graph (PRD-DEP)

**Problem / outcome**
Users need to understand file-level dependency relationships.

**In scope**
- Forward dependencies (imports) and reverse dependencies (importers)

**Out of scope**
- Cross-language call graphs (V2), transitive dependency trees

**EARS Requirements**
- `PRD-DEP-REQ-001` When the user runs `wonk deps <file>` then the system shall return all files imported by the specified file.
- `PRD-DEP-REQ-002` When the user runs `wonk rdeps <file>` then the system shall return all files that import the specified file.

**Acceptance criteria**
- Forward deps correctly identify imports/requires
- Reverse deps correctly identify importers
- Works for all supported languages

---

### 3.8 Index Build (PRD-IDX)

**Problem / outcome**
The system needs to build and maintain a structural index of the codebase using Tree-sitter parsing and persistent storage.

**In scope**
- Full index build, central and local storage, parallel indexing
- Tree-sitter parsing for 11 languages
- File filtering (gitignore, wonkignore, default exclusions)
- Force re-index, status reporting, repo management

**Out of scope**
- Remote indexing, multi-root workspace support

**EARS Requirements**
- `PRD-IDX-REQ-001` When the user runs `wonk init` then the system shall build a full structural index of the current repository.
- `PRD-IDX-REQ-002` When `wonk init` is run without `--local` then the system shall store the index centrally at `~/.wonk/repos/<hash>/`.
- `PRD-IDX-REQ-003` When `wonk init --local` is run then the system shall store the index in `.wonk/` inside the repository root.
- `PRD-IDX-REQ-004` When indexing then the system shall detect and parse files using bundled Tree-sitter grammars for TypeScript/TSX, JavaScript/JSX, Python, Rust, Go, Java, C, C++, Ruby, PHP, and C#.
- `PRD-IDX-REQ-005` When indexing files then the system shall extract symbol definitions including functions, methods, classes, structs, interfaces, enums, traits, type aliases, constants, and exported symbols.
- `PRD-IDX-REQ-006` When indexing files then the system shall extract references including function calls, type annotations, and import statements.
- `PRD-IDX-REQ-007` When indexing files then the system shall record file metadata including language, line count, content hash, and import/export list.
- `PRD-IDX-REQ-008` When indexing then the system shall parallelize file parsing across available CPU cores.
- `PRD-IDX-REQ-009` When indexing then the system shall respect `.gitignore` rules and skip hidden files and directories except `.github`.
- `PRD-IDX-REQ-010` When a `.wonkignore` file exists then the system shall additionally exclude files matching its patterns.
- `PRD-IDX-REQ-011` When indexing then the system shall always exclude `node_modules`, `vendor`, `target`, `build`, `dist`, `__pycache__`, and `.venv` directories.
- `PRD-IDX-REQ-012` When the user runs `wonk update` then the system shall force a full re-index of the repository.
- `PRD-IDX-REQ-013` When the user runs `wonk status` then the system shall display index statistics including file count, symbol count, index freshness, and storage size.
- `PRD-IDX-REQ-014` When the user runs `wonk repos list` then the system shall display all indexed repositories with their paths and index metadata.
- `PRD-IDX-REQ-015` When the user runs `wonk repos clean` then the system shall remove indexes for repositories whose paths no longer exist.

**Acceptance criteria**
- Small repos (< 1k files) index in < 1 second
- Medium repos (1k-10k files) index in 1-5 seconds
- Large repos (10k-50k files) index in 5-15 seconds
- All 11 language families are correctly parsed
- Gitignore, wonkignore, and default exclusions are respected

---

### 3.9 Background Daemon (PRD-DMN)

**Problem / outcome**
The index must stay current as files change without requiring manual re-indexing.

**In scope**
- OS-native file watching, debounced incremental updates
- Auto-start/auto-stop lifecycle, PID-based single-instance enforcement
- Resource-efficient idle behavior

**Out of scope**
- Network-based file watching, multi-repo daemon

**EARS Requirements**
- `PRD-DMN-REQ-001` When `wonk init` completes then the system shall automatically start the background daemon.
- `PRD-DMN-REQ-002` When any CLI command is run and no daemon is running but an index exists then the system shall auto-spawn the daemon.
- `PRD-DMN-REQ-003` When the daemon receives filesystem change events then the system shall batch them over a 500ms debounce window before processing.
- `PRD-DMN-REQ-004` When processing a changed file then the system shall re-hash the file, compare to the stored hash, and skip re-indexing if unchanged.
- `PRD-DMN-REQ-005` When a changed file has a new content hash then the system shall re-parse it and update its symbols, references, and metadata in the index.
- `PRD-DMN-REQ-006` When a file is deleted then the system shall remove all its symbols, references, and metadata from the index.
- `PRD-DMN-REQ-007` When a new file is created then the system shall detect its language, parse it, and add it to the index if supported.
- `PRD-DMN-REQ-008` While the daemon is idle the system shall use less than 15 MB memory and near-zero CPU.
- `PRD-DMN-REQ-009` When re-indexing a single file then the system shall complete in less than 50ms.
- `PRD-DMN-REQ-010` The system shall enforce only one daemon per repository via a PID file.
- `PRD-DMN-REQ-011` When the user runs `wonk daemon start` then the system shall start the daemon if not already running.
- `PRD-DMN-REQ-012` When the user runs `wonk daemon stop` then the system shall stop the running daemon.
- `PRD-DMN-REQ-013` When the user runs `wonk daemon status` then the system shall display the daemon's running state and process ID.
- `PRD-DMN-REQ-014` When the user runs `wonk daemon list` then the system shall display all running daemons across all repositories with their repo paths and process IDs.
- `PRD-DMN-REQ-015` When the user runs `wonk daemon stop --all` then the system shall stop all running daemons across all repositories.

**Acceptance criteria**
- Index freshness after file save < 1 second
- Daemon idles at < 15 MB memory
- Single-file re-index < 50ms
- Only one daemon runs per repository

---

### 3.10 Auto-Initialization (PRD-AUT)

**Problem / outcome**
Users should get results on first use without explicit setup steps, regardless of repo size.

**In scope**
- Transparent first-use indexing with progress indication
- No repo size cap

**Out of scope**
- Partial/streaming results during indexing

**EARS Requirements**
- `PRD-AUT-REQ-001` When a query command is run and no index exists for the current repository then the system shall automatically build the index before returning results.
- `PRD-AUT-REQ-002` While auto-initialization is in progress the system shall display a progress indicator.
- `PRD-AUT-REQ-003` When auto-initialization completes then the system shall spawn the background daemon.

**Acceptance criteria**
- First query on a 5k-file repo returns results in < 5 seconds (including index build)
- Progress indicator is visible during indexing

---

### 3.11 Query Fallback (PRD-FBK)

**Problem / outcome**
The tool must always return useful results even when the index is incomplete or structural data is unavailable.

**In scope**
- Fallback from index queries to grep-crate search
- Hint messages for uninitialized repos
- Graceful handling of unsupported languages

**Out of scope**
- Partial index results

**EARS Requirements**
- `PRD-FBK-REQ-001` If `wonk sym` finds no results in the index then the system shall fall back to grep-based search with heuristic patterns for definitions.
- `PRD-FBK-REQ-002` If `wonk ref` finds no results in the index then the system shall fall back to grep-based search for name occurrences.
- `PRD-FBK-REQ-003` If `wonk deps` finds no import data in the index then the system shall fall back to grep-based search for import/require statements.
- `PRD-FBK-REQ-004` ~~If `wonk ls` finds no symbols in the index for a file then the system shall perform an on-demand Tree-sitter parse.~~ (Deprecated: `wonk ls` merged into `wonk summary`)
- `PRD-FBK-REQ-005` If a file's language is not supported by Tree-sitter then the system shall still include it in text search results.

**Acceptance criteria**
- All structural commands return results even without an index
- Unsupported language files are searchable via text search
- Precision of `wonk sym` > 90%
- Recall of `wonk ref` > 80% vs grep baseline

---

### 3.12 Configuration (PRD-CFG)

**Problem / outcome**
Users need to customize behavior without requiring config for default usage.

**In scope**
- Global config at `~/.wonk/config.toml`
- Per-repo config at `.wonk/config.toml`
- Daemon, index, output, and ignore settings

**Out of scope**
- Per-language config, config import/export

**EARS Requirements**
- `PRD-CFG-REQ-001` When no configuration file exists then the system shall operate with sensible defaults requiring zero configuration.
- `PRD-CFG-REQ-002` Where a global config file exists at `~/.wonk/config.toml` then the system shall apply its settings to all repositories.
- `PRD-CFG-REQ-003` Where a per-repo config file exists at `.wonk/config.toml` then the system shall apply its settings, overriding global config for that repository.
- `PRD-CFG-REQ-004` ~~Where `daemon.idle_timeout_minutes` is configured then the system shall use that value instead of the default 30 minutes.~~ Removed — daemon no longer auto-exits on idle (see PRD-DMN-REQ-003 removal).
- `PRD-CFG-REQ-005` Where `daemon.debounce_ms` is configured then the system shall use that value instead of the default 500ms.
- `PRD-CFG-REQ-006` Where `index.max_file_size_kb` is configured then the system shall skip files larger than that size.
- `PRD-CFG-REQ-007` Where `index.additional_extensions` is configured then the system shall treat files with those extensions as indexable.
- `PRD-CFG-REQ-008` Where `output.default_format` is set to `"json"` then the system shall output JSON format by default.
- `PRD-CFG-REQ-009` Where `output.color` is configured then the system shall enable or disable colorized output accordingly.
- `PRD-CFG-REQ-010` Where `ignore.patterns` is configured then the system shall exclude matching files from indexing.

**Acceptance criteria**
- Tool works with zero config out of the box
- Per-repo config overrides global config
- All config keys are respected when set

---

### 3.13 Distribution (PRD-DST)

**Problem / outcome**
The tool must be easily installable across platforms as a single binary with no dependencies.

**In scope**
- Single static Rust binary with all grammars bundled
- Multiple install methods (Homebrew, Cargo, direct download, npm)
- Platform support tiers

**Out of scope**
- Editor integrations (V2), Web UI

**EARS Requirements**
- `PRD-DST-REQ-001` The system shall be distributed as a single static binary with no external runtime dependencies.
- `PRD-DST-REQ-002` The system shall bundle all 11 Tree-sitter grammars within the binary.
- `PRD-DST-REQ-003` The binary size shall not exceed 30 MB including all bundled grammars.
- `PRD-DST-REQ-004` The system shall support macOS ARM and x86_64 as P0 platforms.
- `PRD-DST-REQ-005` The system shall support Linux x86_64 as a P0 platform.
- `PRD-DST-REQ-006` The system shall support Linux ARM as a P1 platform.
- `PRD-DST-REQ-007` The system shall support Windows x86_64 as a P2 platform.

**Acceptance criteria**
- Binary runs without installing any runtime or shared library
- Binary size < 30 MB on all platforms
- Builds and runs on all P0 platforms

---

### 3.14 Output Formats (PRD-OUT)

**Problem / outcome**
All commands must support consistent output formats for both human and machine consumers.

**In scope**
- Grep-compatible default output
- JSON structured output via `--format json`
- TOON structured output via `--format toon`

**Out of scope**
- Custom output templates

**EARS Requirements**
- `PRD-OUT-REQ-001` When returning results then the system shall default to `file:line:content` format, identical to ripgrep output.
- `PRD-OUT-REQ-002` When the user provides `--format json` on any command then the system shall output results as structured JSON objects.
- `PRD-OUT-REQ-003` When color output is enabled then the system shall colorize grep-style output for terminal readability.
- `PRD-OUT-REQ-004` When the user provides `--format toon` on any command then the system shall output results in TOON (Tree Object Oriented Notation) format.

**Acceptance criteria**
- Default output is parseable by any tool that parses ripgrep output
- JSON output is valid and includes all relevant fields per command
- TOON output is valid and includes all relevant fields per command
- Color output respects terminal capability and config

---

### 3.15 Git Worktree Support (PRD-WKT)

**Problem / outcome**
Developers who use git worktrees to work on multiple branches simultaneously cannot use wonk reliably. A linked worktree has a `.git` file (not directory) pointing to the main repo's git directory. Without explicit worktree awareness, the tool may conflate worktrees — indexing files from one worktree into another's index, or returning search results from a different branch's checkout.

**In scope**
- Correct repo root detection for linked worktrees (`.git` file)
- Separate index per worktree (natural from path-based hashing)
- Separate daemon per worktree
- Exclusion of nested worktree directories during indexing and file watching

**Out of scope**
- Shared/branch-aware indexes across worktrees
- Cross-worktree search or reference finding

**EARS Requirements**
- `PRD-WKT-REQ-001` When the system discovers a `.git` entry during repo root detection then it shall accept both a `.git` directory (regular repo) and a `.git` file (linked worktree) as valid repo root markers.
- `PRD-WKT-REQ-002` When a worktree is nested inside another repository's directory tree then the system shall use the nearest worktree root (the first `.git` or `.wonk` encountered walking upward from the working directory).
- `PRD-WKT-REQ-003` When indexing a repository then the system shall skip any subdirectory that contains a `.git` entry (file or directory), treating it as a separate repository or worktree boundary.
- `PRD-WKT-REQ-004` When the file watcher receives events from a path within a nested worktree boundary then the system shall ignore those events.
- `PRD-WKT-REQ-005` When a linked worktree is indexed then the system shall store its index independently, keyed by the worktree's own root path (not the main repository's path).

**Acceptance criteria**
- Running `wonk search` inside a linked worktree returns only results from that worktree's checked-out files
- Two worktrees of the same repo produce separate indexes with different content
- A worktree nested inside another repo's directory does not pollute the parent repo's index
- The daemon for a parent repo does not re-index files belonging to a nested worktree

---

### 3.16 Semantic Search (PRD-SEM)

**Problem / outcome**
Structural and text search can only find code that matches syntactically — searching for "authentication" won't find `verifyToken`, `checkCredentials`, or `validateSession`. Developers and LLM agents need to search by intent rather than exact names. Semantic search uses embeddings to bridge this vocabulary gap, finding functionally related code even when terminology doesn't overlap.

**In scope**
- `wonk ask <query>` — dedicated semantic search command
- `wonk search --semantic` — blend structural + semantic results in smart search
- Tree-sitter-based chunking (one chunk per symbol definition, with file/scope/import context)
- Embedding via Ollama `nomic-embed-text` (external, optional dependency)
- Vector storage in the existing SQLite index DB
- Cosine similarity scoring displayed in all output formats
- Embedding build during explicit `wonk init`; background daemon build for auto-init scenarios
- Incremental re-embedding via daemon when files change
- Block-and-wait with progress when embeddings are incomplete

**Out of scope**
- Bundled/offline embedding model (would require ONNX runtime in binary)
- Custom/configurable embedding models (single model for V2)
- Semantic search for non-code files (markdown, config) beyond full-file fallback

**EARS Requirements**
- `PRD-SEM-REQ-001` When the user runs `wonk ask <query>` then the system shall embed the query via Ollama, perform cosine similarity search against all stored symbol embeddings, and return results ranked by descending similarity score.
- `PRD-SEM-REQ-002` When the user provides `--semantic` on `wonk search` then the system shall blend structural results with semantic results, presenting structural matches first followed by additional semantic matches not already present. *Superseded by PRD-RRF-REQ-001 in V4 — replaced by Reciprocal Rank Fusion.*
- `PRD-SEM-REQ-003` When returning semantic search results then each result shall include file path, line number, symbol name, symbol kind, and cosine similarity score.
- `PRD-SEM-REQ-004` When the user provides `--budget <n>` on `wonk ask` then the system shall limit output to approximately `n` tokens, prioritizing results with highest similarity.
- `PRD-SEM-REQ-005` When the user provides `--format json` on `wonk ask` then the system shall output results as JSON objects including all fields plus the similarity score.
- `PRD-SEM-REQ-006` When building embeddings then the system shall create one chunk per tree-sitter symbol definition, including the file path, parent scope, import context, and the symbol's source code.
- `PRD-SEM-REQ-007` When a file has no extractable tree-sitter symbols then the system shall treat the full file content as a single chunk for embedding.
- `PRD-SEM-REQ-008` When the user runs `wonk init` explicitly and Ollama is reachable then the system shall build embeddings alongside the structural index, displaying progress.
- `PRD-SEM-REQ-009` When auto-initialization is triggered by a query then the system shall build the structural index only, then delegate embedding generation to the background daemon.
- `PRD-SEM-REQ-010` When the daemon detects file changes and Ollama is reachable then the system shall re-embed all chunks belonging to the changed files.
- `PRD-SEM-REQ-011` If Ollama is unreachable during daemon re-embedding then the system shall skip embedding updates silently and mark affected files as stale in the index.
- `PRD-SEM-REQ-012` If Ollama is not reachable when `wonk ask` is run then the system shall return a clear error message stating that Ollama is required for semantic search.
- `PRD-SEM-REQ-013` When `wonk ask` is run and embeddings are incomplete then the system shall block and display embedding build progress until ready, then return results.
- `PRD-SEM-REQ-014` If Ollama is not reachable when `wonk init` is run then the system shall skip embedding generation with a warning and build only the structural index.
- `PRD-SEM-REQ-015` When storing embeddings then the system shall write vectors to a dedicated table in the existing SQLite index database.
- `PRD-SEM-REQ-016` When computing similarity then the system shall use brute-force cosine similarity over all stored vectors.

**Acceptance criteria**
- `wonk ask "authentication"` finds `verifyToken`, `checkCredentials`, and similar symbols even though the word "authentication" doesn't appear in them
- Similarity scores are displayed for every result
- `wonk search --semantic <pattern>` returns structural matches first, then semantic matches
- `--budget` correctly limits output token count
- Embeddings survive daemon restart and are incrementally updated
- Clear error message when Ollama is unavailable
- `wonk init` completes structural index even if Ollama is down

---

### 3.17 Semantic Dependency Analysis (PRD-SDEP)

**Problem / outcome**
Semantic search alone returns results from across the entire codebase. Developers often need results scoped to a specific execution path — "find authentication-related code reachable from this endpoint." Combining semantic search with the dependency graph enables intent-aware, scope-limited queries.

**In scope**
- Semantic search filtered by dependency reachability
- Forward scope (code reachable from a file) and reverse scope (code that reaches a file)

**Out of scope**
- Cross-language dependency resolution
- Function-level call graph (see PRD-CGR for standalone call graph feature)

**EARS Requirements**
- `PRD-SDEP-REQ-001` When the user provides `--from <file>` on `wonk ask` then the system shall restrict semantic results to symbols in files reachable via forward dependencies from the specified file.
- `PRD-SDEP-REQ-002` When the user provides `--to <file>` on `wonk ask` then the system shall restrict semantic results to symbols in files that transitively import the specified file.
- `PRD-SDEP-REQ-003` When computing reachability then the system shall traverse the file-level dependency graph transitively (not just direct imports).

**Acceptance criteria**
- `wonk ask "auth" --from src/routes/api.ts` returns only semantically related symbols reachable from that route
- Transitive dependencies are followed (A imports B imports C → C is reachable from A)
- Results still include similarity scores

---

### 3.18 Semantic Clustering (PRD-SCLST)

**Problem / outcome**
Developers joining a codebase or navigating an unfamiliar directory need a high-level map of what concerns exist. Current tools list files or symbols, but don't reveal the conceptual groupings within a directory. Clustering embeddings surfaces these groupings automatically.

**In scope**
- Cluster symbols in a directory by semantic similarity
- Labeled cluster output (representative symbols per cluster)

**Out of scope**
- LLM-generated cluster labels/summaries
- Interactive/visual cluster exploration

**EARS Requirements**
- `PRD-SCLST-REQ-001` When the user runs `wonk cluster <path>` then the system shall cluster all symbol embeddings within the specified path by semantic similarity and display labeled groups.
- `PRD-SCLST-REQ-002` When displaying clusters then each cluster shall list its most representative symbols (closest to cluster centroid) and the files they belong to.
- `PRD-SCLST-REQ-003` When the user provides `--json` on `wonk cluster` then the system shall output cluster data as structured JSON.

**Acceptance criteria**
- `wonk cluster src/auth/` groups related auth symbols together
- Output clearly separates distinct concerns (e.g., token validation vs. session management vs. user lookup)
- Each cluster shows its top representative symbols

---

### 3.19 Semantic Change Impact Analysis (PRD-SIMP)

**Problem / outcome**
When a developer modifies code, they need to know what other code might be affected beyond what the dependency graph shows. A renamed concept, changed algorithm, or modified interface might impact semantically related code in files with no direct import relationship.

**In scope**
- Find code semantically similar to recently changed symbols
- Git-aware: detect changes since a commit or on unstaged files

**Out of scope**
- Automatic modification suggestions
- Cross-repo impact analysis

**EARS Requirements**
- `PRD-SIMP-REQ-001` When the user runs `wonk impact <file>` then the system shall identify symbols that changed in the file (vs. indexed version), find semantically similar symbols in other files, and display them ranked by similarity.
- `PRD-SIMP-REQ-002` When the user provides `--since <commit>` on `wonk impact` then the system shall analyze all files changed since that commit.
- `PRD-SIMP-REQ-003` When displaying impact results then each result shall include the changed symbol, the potentially impacted symbol, the similarity score, and the file path.
- `PRD-SIMP-REQ-004` When the user provides `--json` on `wonk impact` then the system shall output impact data as structured JSON.

**Acceptance criteria**
- Changing `verifyToken` surfaces `validateSession` and `checkCredentials` as potentially impacted
- `--since HEAD~3` analyzes all files changed in the last 3 commits
- Results are ranked by similarity to the changed code

### 3.20 Source Display (PRD-SHOW)

**Problem / outcome**
LLM coding agents need two tool calls to see a symbol's source code: one to look up the symbol (getting file path and line number), then another to read the file at those lines. This doubles round-trip latency and often results in over-reading — agents guess at line ranges and pull in irrelevant code. `wonk show` collapses this into a single call that returns exactly the source span tree-sitter already knows.

**In scope**
- Display full source body of any indexed symbol (functions, methods, classes, structs, enums, traits, interfaces, etc.)
- Disambiguation by file path (`--file`) and symbol kind (`--kind`)
- Shallow mode for container types (signature + member signatures, no bodies)
- Budget-aware truncation
- Line-numbered output
- MCP tool exposure (`wonk_show`)

**Out of scope**
- Grep-based fallback when no index exists (index is required)
- Syntax highlighting / colorization of source
- Cross-file expansion (following imports to show dependencies)

**EARS Requirements**
- `PRD-SHOW-REQ-001` When user runs `wonk show <name>` then the system shall look up matching symbols in the index and display their source code by reading lines `line` through `end_line` from the source file.
- `PRD-SHOW-REQ-002` When multiple symbols match the name then the system shall display all matches, each preceded by a file header showing `file:start_line-end_line`.
- `PRD-SHOW-REQ-003` Where `--file <path>` is provided then the system shall restrict results to symbols defined in that file.
- `PRD-SHOW-REQ-004` Where `--kind <kind>` is provided then the system shall restrict results to symbols of that kind.
- `PRD-SHOW-REQ-005` Where `--exact` is provided then the system shall require an exact name match instead of substring matching.
- `PRD-SHOW-REQ-006` Where `--shallow` is provided and the symbol is a container type (class, struct, enum, trait, interface) then the system shall display the container signature and member signatures without member bodies.
- `PRD-SHOW-REQ-007` Where `--budget <n>` is provided then the system shall truncate output to approximately n tokens and indicate what was omitted.
- `PRD-SHOW-REQ-008` When displaying source lines then the system shall prefix each line with its 1-based file line number.
- `PRD-SHOW-REQ-009` When no index exists for the repository then the system shall return an error directing the user to run `wonk init`.
- `PRD-SHOW-REQ-010` If a matched symbol has no `end_line` recorded then the system shall fall back to displaying only the symbol's signature text.
- `PRD-SHOW-REQ-011` If the source file for a matched symbol no longer exists or is unreadable then the system shall skip that result and emit a warning.
- `PRD-SHOW-REQ-012` When output format is JSON or TOON then the system shall include structured fields: name, kind, file, line, end_line, source, language.
- `PRD-SHOW-REQ-013` The system shall expose `wonk_show` as an MCP tool with parameters: name (required), kind (optional), file (optional), exact (boolean), shallow (boolean), budget (integer), format (json|toon).

**Acceptance criteria**
- `wonk show processPayment` on a TypeScript repo returns the full function body with correct line numbers
- `wonk show --kind class StripeClient` returns only the class, not variables with the same name
- `wonk show --shallow StripeClient` returns class signature + method signatures without bodies
- `wonk show --file src/auth.ts login` returns only the `login` symbol from that specific file
- `wonk show nonexistent` returns empty result set
- `wonk show` on an unindexed repo shows error with init guidance
- Budget truncation: `wonk show --budget 100 LargeClass` truncates and notes omission
- JSON output includes `source` field with the full code body
- MCP tool `wonk_show` works through Claude Code

---

### 3.21 Code Summary (PRD-SUM)

**Problem / outcome**
LLM coding agents and developers need a quick structural and semantic overview of files and directories — what's in them, how complex they are, and what they do — without manually reading every file. This command also serves as the symbol listing tool (formerly `wonk ls`).

**In scope**
- `wonk summary <path>` subcommand (also replaces `wonk ls`)
- Three detail levels for structural metrics: rich (default), light, symbols-only
- Per-symbol location metadata (line, col, end_line, scope) in rich detail
- `--tree` flag for scope-grouped symbol display
- Configurable recursion depth (`--depth N`) for hierarchical summaries
- LLM-generated natural-language descriptions via `--semantic` flag (Ollama `/api/generate`)
- LLM generation model configuration in `[llm]` section of existing layered config.toml
- Caching of LLM descriptions in SQLite index (invalidated on content changes)
- MCP tool exposure (`wonk_summary`)

**Out of scope**
- Bundled/offline LLM (must use external Ollama)
- Automatic LLM description without explicit `--semantic` flag
- Code quality metrics (complexity, test coverage)
- Streaming LLM output

**EARS Requirements**
- `PRD-SUM-REQ-001` When user runs `wonk summary <path>` on a file then the system shall display a structural summary including the file's language, line count, and symbols grouped by kind.
- `PRD-SUM-REQ-002` When user runs `wonk summary <path>` on a directory then the system shall display an aggregate structural summary including file count, total line count, language breakdown, and total symbols grouped by kind.
- `PRD-SUM-REQ-003` Where `--detail rich` is specified or no `--detail` flag is provided then the system shall include file count, line count, symbol count by kind, language breakdown, and dependency count in the summary.
- `PRD-SUM-REQ-004` Where `--detail light` is specified then the system shall include file count, symbol count, and language breakdown only.
- `PRD-SUM-REQ-005` Where `--detail symbols` is specified then the system shall include symbol counts grouped by kind only.
- `PRD-SUM-REQ-006` Where `--depth N` is provided then the system shall recursively summarize nested directories and files up to N levels deep, presenting each child's summary indented under its parent.
- `PRD-SUM-REQ-007` While no `--depth` flag is specified then the system shall summarize only the target path (depth 0, no recursion).
- `PRD-SUM-REQ-008` Where `--recursive` is provided then the system shall summarize all nested directories and files to unlimited depth.
- `PRD-SUM-REQ-009` Where the `--semantic` flag is provided then the system shall generate a natural-language description of the path's purpose and contents using the configured LLM model via Ollama's `/api/generate` endpoint and include it in the summary output.
- `PRD-SUM-REQ-010` When generating an LLM description then the system shall construct a prompt containing the structural metrics and symbol signatures for the target path as context.
- `PRD-SUM-REQ-011` When an LLM description is successfully generated then the system shall store it in the SQLite index keyed by path and a content hash derived from the path's indexed symbols.
- `PRD-SUM-REQ-012` When an LLM description is requested and a cached entry with a matching content hash exists then the system shall return the cached description without calling Ollama.
- `PRD-SUM-REQ-013` If Ollama is unreachable when `--semantic` is requested then the system shall display a warning and return the structural summary without the LLM description.
- `PRD-SUM-REQ-014` When the user configures an `[llm]` section with a `model` key in `config.toml` then the system shall use the specified model name for text generation requests to Ollama.
- `PRD-SUM-REQ-015` If no `[llm].model` is configured when `--semantic` is requested then the system shall use `llama3.2:3b` as the default generation model. If the model is not available in Ollama then the system shall display an error instructing the user to pull the model or configure an alternative in `config.toml`.
- `PRD-SUM-REQ-016` When `wonk summary` is invoked without an existing index then the system shall auto-initialize the index consistent with PRD-AUT behavior.
- `PRD-SUM-REQ-017` When output format is JSON or TOON then the system shall include structured fields: path, type (file|directory), detail_level, metrics (object), children (array, if recursive), description (string, if `--semantic`).
- `PRD-SUM-REQ-018` The system shall expose `wonk_summary` as an MCP tool with parameters: path (required), detail (optional: rich|light|symbols), depth (optional integer), recursive (optional boolean), semantic (optional boolean), tree (optional boolean), format (json|toon).
- `PRD-SUM-REQ-019` When `--tree` is provided with `--detail rich` then the system shall display symbols with scope-based nesting hierarchy (methods indented under classes).
- `PRD-SUM-REQ-020` When `--detail rich` is used then each symbol in the output shall include `line`, `col`, `end_line`, and `scope` fields in addition to `name`, `kind`, and `signature`.

**Acceptance criteria**
- `wonk summary src/` displays file count, line count, symbol counts by kind, language breakdown, dependency count
- `wonk summary src/ --detail light` displays only file count, symbol count, language breakdown
- `wonk summary src/ --detail symbols` displays only symbol counts grouped by kind
- `wonk summary src/ --depth 2` shows target summary plus summaries of children and grandchildren
- `wonk summary src/ --recursive` shows full hierarchy to leaves
- `wonk summary src/ --semantic` includes an LLM-generated natural-language description (Ollama must be running with configured model)
- Repeated `wonk summary src/ --semantic` on unchanged code returns cached description instantly
- `wonk summary src/ --semantic` without `[llm]` config uses default model `llama3.2:3b`; shows error with pull/config instructions if model not available
- `wonk summary src/ --semantic` with Ollama down shows warning + structural summary only
- JSON output includes all structured fields
- MCP tool `wonk_summary` works through Claude Code

---

### 3.22 Call Graph Analysis (PRD-CGR)

**Problem / outcome**
Wonk provides flat reference lookup (`wonk ref`) and file-level dependency graphs (`wonk deps`/`wonk rdeps`), but there is no way to navigate **symbol-level caller/callee relationships**. When an LLM agent or developer asks "who calls this function?" or "what does this function call?", they get name-occurrence matches without knowing which enclosing function contains the call. A proper call graph enables tracing execution paths, understanding blast radius of changes at the function level, and scoping refactoring work precisely.

**In scope**
- Indexer enhancement: store the enclosing caller symbol for each call-site reference
- `wonk callers <symbol>` — list functions/methods that call the given symbol
- `wonk callees <symbol>` — list functions/methods called by the given symbol
- `wonk callpath <from> <to>` — find call chains connecting two symbols
- Transitive traversal with `--depth N` for callers and callees
- MCP tool exposure (`wonk_callers`, `wonk_callees`, `wonk_callpath`)

**Out of scope**
- Dynamic dispatch resolution (virtual calls, trait objects, function pointers)
- Cross-language call graph edges
- Call frequency or hot-path analysis

**EARS Requirements**
- `PRD-CGR-REQ-001` When indexing a source file then the system shall record, for each call-site reference, the enclosing function or method symbol that contains the call.
- `PRD-CGR-REQ-002` When a call-site reference occurs at file scope (outside any function) then the system shall record the enclosing caller as `<module>` (the file's implicit top-level scope).
- `PRD-CGR-REQ-003` When the user runs `wonk callers <symbol>` then the system shall display all functions and methods whose bodies contain a call-site reference to the named symbol.
- `PRD-CGR-REQ-004` When the user runs `wonk callees <symbol>` then the system shall display all symbols that are called within the body of the named function or method.
- `PRD-CGR-REQ-005` Where `--depth N` is provided on `wonk callers` then the system shall transitively expand callers up to N levels (depth 1 = direct callers only, depth 2 = callers of callers, etc.).
- `PRD-CGR-REQ-006` Where `--depth N` is provided on `wonk callees` then the system shall transitively expand callees up to N levels.
- `PRD-CGR-REQ-007` While no `--depth` flag is specified on `wonk callers` or `wonk callees` then the system shall default to depth 1 (direct relationships only).
- `PRD-CGR-REQ-008` Where `--depth N` exceeds 10 then the system shall cap traversal at depth 10 and emit a warning indicating the cap was applied.
- `PRD-CGR-REQ-009` When the user runs `wonk callpath <from> <to>` then the system shall find and display at least one call chain from the `<from>` symbol to the `<to>` symbol, showing each intermediate caller/callee hop.
- `PRD-CGR-REQ-010` If no call path exists between the two symbols then the system shall display a message indicating no path was found.
- `PRD-CGR-REQ-011` When the named symbol has multiple definitions (e.g., overloaded methods across files) then the system shall include callers or callees from all definitions and indicate which definition each result corresponds to.
- `PRD-CGR-REQ-012` When `wonk callers`, `wonk callees`, or `wonk callpath` is invoked without an existing index then the system shall auto-initialize the index consistent with PRD-AUT behavior.
- `PRD-CGR-REQ-013` The system shall expose `wonk_callers` and `wonk_callees` as MCP tools with parameters: name (required string), depth (optional integer, default 1, max 10), format (json|toon).
- `PRD-CGR-REQ-014` The system shall expose `wonk_callpath` as an MCP tool with parameters: from (required string), to (required string), format (json|toon).

**Acceptance criteria**
- `wonk callers dispatch` lists all functions whose bodies call `dispatch`
- `wonk callers dispatch --depth 2` lists direct callers and their callers
- `wonk callees main` lists all functions called within `main`
- `wonk callpath main dispatch` shows the call chain from `main` to `dispatch`
- `wonk callpath foo bar` where no path exists prints "no path found"
- `wonk callers X --depth 15` warns about depth cap and uses depth 10
- Symbol with multiple definitions shows callers from all definitions
- MCP tools `wonk_callers`, `wonk_callees`, `wonk_callpath` work through Claude Code
- Running any call graph command on an unindexed repo triggers auto-init

---

### 3.23 Execution Flow Detection (PRD-FLOW) [V4]

**Problem / outcome**
LLM agents need to understand how code executes end-to-end — "how does an API request flow through the system?" Currently, wonk provides flat reference lists and file-level dependencies, but no way to trace an execution path from entry point to leaf. Flow detection identifies entry points (functions with no internal callers), traces outward through the call graph, and produces named execution flows.

**In scope**
- Entry point detection (functions/methods with no indexed callers)
- BFS tracing from entry points through call graph edges (PRD-CGR)
- Configurable depth and branching limits
- File-scoped entry point filtering
- MCP tool exposure

**Out of scope**
- Dynamic dispatch resolution (virtual calls, trait objects)
- Cross-language flow tracing
- LLM-generated flow labels/descriptions
- Framework-aware entry point detection (e.g., Express routes, Spring controllers)

**EARS Requirements**
- `PRD-FLOW-REQ-001` When the user runs `wonk flows` then the system shall identify entry point symbols (functions/methods with no indexed callers) and display them with their call depth.
- `PRD-FLOW-REQ-002` When the user runs `wonk flows <entry>` then the system shall trace the call graph forward from the named symbol via BFS and display the ordered sequence of symbols forming the execution flow.
- `PRD-FLOW-REQ-003` When tracing a flow then the system shall follow call-graph edges recorded in the index (PRD-CGR-REQ-001) up to a configurable maximum depth.
- `PRD-FLOW-REQ-004` Where `--depth N` is provided then the system shall limit BFS traversal to N levels (default: 10, maximum: 20).
- `PRD-FLOW-REQ-005` Where `--branching N` is provided then the system shall follow at most N callees per symbol during BFS (default: 4).
- `PRD-FLOW-REQ-006` When a traced flow has fewer than 2 steps then the system shall exclude it from flow output.
- `PRD-FLOW-REQ-007` When displaying a flow then each step shall include the symbol name, kind, file path, and line number.
- `PRD-FLOW-REQ-008` Where `--from <file>` is provided then the system shall restrict entry point detection to symbols defined in the specified file.
- `PRD-FLOW-REQ-009` When output format is JSON or TOON then the system shall include structured fields: entry_point, steps (ordered array of {name, kind, file, line, depth}), step_count.
- `PRD-FLOW-REQ-010` The system shall expose `wonk_flows` as an MCP tool with parameters: entry (optional string), from (optional file path), depth (optional integer, default 10, max 20), branching (optional integer, default 4), format (json|toon).

**Acceptance criteria**
- `wonk flows` lists all detected entry points with call depth
- `wonk flows main` traces the full execution flow from `main`
- `wonk flows --from src/api.ts` shows flows starting from that file only
- Flows with only 1 step are excluded
- MCP tool `wonk_flows` works through Claude Code

---

### 3.24 Blast Radius Impact Analysis (PRD-BLAST) [V4]

**Problem / outcome**
When modifying a symbol, developers need to know the consequence — not just "what changed" (PRD-SIMP) but "what would break." The current `wonk impact` detects Added/Modified/Removed symbols but doesn't trace the call graph outward. Blast radius analysis walks the dependency graph from a symbol, grouping results by depth to indicate severity.

**In scope**
- Symbol-level blast radius via call graph traversal
- Depth-based severity tiers
- Risk level assessment
- Direction control (upstream dependants vs. downstream dependencies)
- MCP tool exposure

**Out of scope**
- Automatic fix suggestions
- Cross-repo impact analysis
- Semantic similarity impact (existing PRD-SIMP handles this)

**EARS Requirements**
- `PRD-BLAST-REQ-001` When the user runs `wonk blast <symbol>` then the system shall traverse the call graph outward from the named symbol and display all directly and transitively dependent symbols grouped by depth.
- `PRD-BLAST-REQ-002` When displaying results then the system shall group by severity: depth 1 = "WILL BREAK" (direct callers/importers), depth 2 = "LIKELY AFFECTED", depth 3+ = "MAY NEED TESTING".
- `PRD-BLAST-REQ-003` When displaying results then the system shall assign a risk level: LOW (<=3 affected symbols), MEDIUM (4-10), HIGH (11-25), CRITICAL (>25).
- `PRD-BLAST-REQ-004` Where `--direction upstream` is provided (default) then the system shall traverse callers.
- `PRD-BLAST-REQ-005` Where `--direction downstream` is provided then the system shall traverse callees.
- `PRD-BLAST-REQ-006` Where `--depth N` is provided then the system shall limit traversal to N levels (default: 3, maximum: 10).
- `PRD-BLAST-REQ-007` When results span multiple files then the system shall include an affected-files summary.
- `PRD-BLAST-REQ-008` Where `--include-tests` is provided then the system shall include test file symbols; otherwise test files are excluded.
- `PRD-BLAST-REQ-009` When output format is JSON or TOON then the system shall include: target, direction, risk_level, total_affected, tiers[], affected_files[].
- `PRD-BLAST-REQ-010` The system shall expose `wonk_blast` as an MCP tool with parameters: symbol (required), direction (optional), depth (optional), include_tests (optional), format.

**Acceptance criteria**
- `wonk blast processPayment` shows callers grouped by depth with severity labels
- Risk levels correctly reflect affected symbol counts
- Test files excluded by default
- MCP tool works through Claude Code

---

### 3.25 Scoped Change Detection (PRD-CHG) [V4]

**Problem / outcome**
Developers need to understand the impact of in-progress changes before committing. The existing `wonk impact` supports `--since <commit>` but lacks ergonomic scoping for common git workflows (unstaged, staged, branch compare) and doesn't connect to blast radius or flow analysis. Scoped change detection maps git diffs to symbols and chains into blast radius/flow analysis.

**In scope**
- Git-diff scoping: unstaged, staged, all, compare (vs. branch/commit)
- Symbol-level change detection from diff hunks
- Optional chaining to blast radius (PRD-BLAST) and flow detection (PRD-FLOW)
- MCP tool exposure

**Out of scope**
- Untracked new file analysis without git diff
- Automatic commit/review workflow

**EARS Requirements**
- `PRD-CHG-REQ-001` When the user runs `wonk changes` then the system shall detect all symbols affected by unstaged git changes (default scope).
- `PRD-CHG-REQ-002` Where `--scope staged` is provided then the system shall analyze only staged changes.
- `PRD-CHG-REQ-003` Where `--scope all` is provided then the system shall analyze both unstaged and staged changes.
- `PRD-CHG-REQ-004` Where `--scope compare --base <ref>` is provided then the system shall analyze changes between working tree and the specified git ref.
- `PRD-CHG-REQ-005` When mapping diff hunks to symbols then the system shall identify which indexed symbols overlap with changed line ranges (Modified), which are absent from the re-parsed file (Removed), and which are new (Added).
- `PRD-CHG-REQ-006` Where `--blast` is provided then the system shall run blast radius analysis for each changed symbol and include aggregated impact.
- `PRD-CHG-REQ-007` Where `--flows` is provided then the system shall identify execution flows containing any changed symbols and list them as affected.
- `PRD-CHG-REQ-008` When output format is JSON or TOON then the system shall include: scope, changed_symbols[], blast_radius (optional), affected_flows (optional).
- `PRD-CHG-REQ-009` The system shall expose `wonk_changes` as an MCP tool with parameters: scope, base, blast, flows, format.

**Acceptance criteria**
- `wonk changes` shows symbols affected by unstaged changes
- `wonk changes --scope compare --base main` shows changes vs. main
- `wonk changes --blast` includes blast radius per changed symbol
- `wonk changes --flows` lists affected execution flows

---

### 3.26 Unified Symbol Context (PRD-CTX) [V4]

**Problem / outcome**
Understanding a symbol currently requires 3-4 separate commands: `wonk sym` for definition, `wonk ref` for references, `wonk callers`/`wonk callees` for call graph, `wonk deps`/`wonk rdeps` for file context. Each round-trip costs latency and budget. A unified context command aggregates all relevant information into a single response.

**In scope**
- Single command aggregating definition, categorized incoming/outgoing references, and flow participation
- Disambiguation by file path and kind
- MCP tool exposure

**Out of scope**
- Source code display (use `wonk show`)
- Semantic similarity neighbors

**EARS Requirements**
- `PRD-CTX-REQ-001` When the user runs `wonk context <name>` then the system shall display: definition (file, line, kind, signature), incoming references grouped by category (Callers, Importers, Type Users), outgoing references (Callees, Imports), and flow participation.
- `PRD-CTX-REQ-002` Where `--file <path>` is provided then the system shall restrict to symbols in that file.
- `PRD-CTX-REQ-003` Where `--kind <kind>` is provided then the system shall restrict to symbols of that kind.
- `PRD-CTX-REQ-004` When multiple symbols match then the system shall display context for all, clearly labeled.
- `PRD-CTX-REQ-005` When displaying incoming references then the system shall categorize as: Callers, Importers, Type Users.
- `PRD-CTX-REQ-006` When displaying outgoing references then the system shall categorize as: Callees, Imports.
- `PRD-CTX-REQ-007` Where execution flows are available then the system shall list which flows the symbol participates in and at which step.
- `PRD-CTX-REQ-008` When output format is JSON or TOON then the system shall include: symbol, incoming {callers[], importers[], type_users[]}, outgoing {callees[], imports[]}, flows[].
- `PRD-CTX-REQ-009` The system shall expose `wonk_context` as an MCP tool with parameters: name (required), file (optional), kind (optional), format.

**Acceptance criteria**
- `wonk context processPayment` shows definition, callers, callees, importers, and flows in one response
- `wonk context --file src/auth.ts verifyToken` narrows to that file
- Categories are clearly separated
- MCP tool works through Claude Code

---

### 3.27 Hybrid Search Fusion (PRD-RRF) [V4]

**Problem / outcome**
The current `wonk search --semantic` blending presents structural matches first, then appends semantic matches. This simple concatenation doesn't optimize for relevance — a high-relevance semantic match may appear after a low-relevance structural match. Reciprocal Rank Fusion (RRF) merges ranked lists from multiple sources into a single optimally ranked list.

**In scope**
- RRF algorithm for merging structural and semantic result lists
- Configurable fusion constant (K)
- Supersedes PRD-SEM-REQ-002 blending behavior

**Out of scope**
- BM25 indexing (wonk uses grep-based search)
- Additional ranking signals beyond structural and semantic

**EARS Requirements**
- `PRD-RRF-REQ-001` When the user provides `--semantic` on `wonk search` then the system shall merge structural and semantic result lists using Reciprocal Rank Fusion with formula: score(d) = Sum 1/(K + rank_i(d)) across all result lists. *Supersedes PRD-SEM-REQ-002.*
- `PRD-RRF-REQ-002` When computing RRF scores then the system shall use K=60 as the default fusion constant.
- `PRD-RRF-REQ-003` Where `rrf_k` is configured in the `[search]` section of config.toml then the system shall use that value instead of the default.
- `PRD-RRF-REQ-004` When displaying RRF-fused results then the system shall present them in descending RRF score order, interleaving structural and semantic matches as their fused scores dictate.

**Acceptance criteria**
- `wonk search --semantic "auth"` returns interleaved results ranked by RRF score
- A high-ranked semantic result can appear before a low-ranked structural result
- Custom K value from config.toml is respected

---

### 3.28 Edge Confidence Scoring (PRD-CONF) [V4]

**Problem / outcome**
Not all call-graph and import edges are equally reliable. An import resolved via explicit `import` statement is near-certain, while a call matched by name alone may be a false positive. Without confidence metadata, graph traversals treat all edges equally, producing noisy results. Confidence scoring lets consumers filter or weight edges by reliability.

**In scope**
- Confidence score (0.0-1.0) on reference and call-graph edges
- Scoring based on resolution method during indexing
- Filtering by minimum confidence in graph traversal commands

**Out of scope**
- Runtime/dynamic confidence adjustment
- ML-based confidence estimation

**EARS Requirements**
- `PRD-CONF-REQ-001` When indexing references and call-graph edges then the system shall assign a confidence score between 0.0 and 1.0 based on the resolution method.
- `PRD-CONF-REQ-002` When a reference is resolved via explicit import then the system shall assign confidence >= 0.9.
- `PRD-CONF-REQ-003` When a reference is resolved via same-file definition then the system shall assign confidence >= 0.8.
- `PRD-CONF-REQ-004` When a reference is resolved via fuzzy name matching (no import path) then the system shall assign confidence <= 0.5.
- `PRD-CONF-REQ-005` Where `--min-confidence <N>` is provided on graph traversal commands (blast, flows, callers, callees, callpath, context) then the system shall exclude edges with confidence below N.
- `PRD-CONF-REQ-006` When output format is JSON or TOON then reference and call-graph results shall include a `confidence` field.

**Acceptance criteria**
- Import-resolved references have confidence >= 0.9
- Fuzzy name-matched references have confidence <= 0.5
- `wonk callers foo --min-confidence 0.8` excludes fuzzy matches
- JSON output includes confidence field on all graph edges

---

### 3.29 Inheritance Tracking (PRD-HRTG) [V4]

**Problem / outcome**
The index tracks call and import relationships but not inheritance or interface implementation. When a base class method changes, subclass overrides may be affected. Without these edges, blast radius analysis and flow detection miss an entire category of dependencies.

**In scope**
- Tree-sitter extraction of extends/implements relationships
- Storage as typed edges in the index
- Integration with blast radius, flow detection, and context commands

**Out of scope**
- Mixin/composition tracking
- Generic/template specialization tracking
- Cross-language inheritance

**EARS Requirements**
- `PRD-HRTG-REQ-001` When indexing a class that extends another class then the system shall record an "extends" edge between child and parent class symbols.
- `PRD-HRTG-REQ-002` When indexing a class or struct that implements an interface or trait then the system shall record an "implements" edge between implementor and interface/trait symbol.
- `PRD-HRTG-REQ-003` When blast radius analysis (PRD-BLAST) traverses upstream from a class or interface then the system shall include child classes and implementors as depth-1 dependants.
- `PRD-HRTG-REQ-004` When `wonk context` displays incoming references then the system shall include a "Children" category listing classes that extend or implement the symbol.
- `PRD-HRTG-REQ-005` When output format is JSON or TOON then inheritance edges shall include a `relationship` field with value "extends" or "implements".

**Acceptance criteria**
- `wonk context BaseHandler` shows extending classes under "Children"
- `wonk blast IPaymentProvider` includes all implementors in depth-1 tier
- Inheritance edges extracted for all 12 supported languages where applicable

---

### 3.30 Multi-Repo MCP (PRD-MREP) [V4]

**Problem / outcome**
When an LLM agent works across related repositories, it must start a separate MCP server per repo. The current `wonk mcp serve` operates on the current repo only. Multi-repo support enables a single MCP server to serve all indexed repositories via a `repo` parameter.

**In scope**
- Global repository registry discovery
- MCP tools accept optional `repo` parameter
- Repo listing via MCP tool
- Lazy-load index connections per repo

**Out of scope**
- Cross-repo search (querying multiple repos in a single call)
- Cross-repo call graph traversal
- Remote repo serving

**EARS Requirements**
- `PRD-MREP-REQ-001` When `wonk mcp serve` is started then the system shall discover all indexed repositories from the global registry and make them available.
- `PRD-MREP-REQ-002` When an MCP tool is invoked without a `repo` parameter then the system shall default to the repository at the server's working directory.
- `PRD-MREP-REQ-003` When an MCP tool is invoked with a `repo` parameter then the system shall route the query to the specified repository's index.
- `PRD-MREP-REQ-004` When a repo is specified by name then the system shall match against the repository directory name (last path component).
- `PRD-MREP-REQ-005` The system shall expose a `wonk_repos` MCP tool that lists all available repositories with names, paths, and index statistics.
- `PRD-MREP-REQ-006` When loading a repository's index for the first time during a session then the system shall open the connection lazily and cache it.

**Acceptance criteria**
- Single MCP server can answer queries about multiple repos
- `wonk_repos` lists all indexed repos
- Default is working directory repo when no `repo` param
- Lazy-loaded connections don't block server startup

---

## 4) Traceability

| Feature | Requirement IDs | Count |
|---|---|---|
| Text Search | PRD-SRCH-REQ-001 to 005 | 5 |
| Smart Search | PRD-SSRCH-REQ-001 to 006 | 6 |
| Symbol Lookup | PRD-SYM-REQ-001 to 004 | 4 |
| Reference Finding | PRD-REF-REQ-001 to 004 | 4 |
| Signature Display | PRD-SIG-REQ-001 | 1 |
| Symbol Listing (deprecated) | PRD-LST — absorbed into PRD-SUM | 0 |
| Dependency Graph | PRD-DEP-REQ-001 to 002 | 2 |
| Index Build | PRD-IDX-REQ-001 to 015 | 15 |
| Background Daemon | PRD-DMN-REQ-001 to 015 | 15 |
| Auto-Initialization | PRD-AUT-REQ-001 to 003 | 3 |
| Query Fallback | PRD-FBK-REQ-001 to 005 | 5 |
| Configuration | PRD-CFG-REQ-001 to 010 | 10 |
| Distribution | PRD-DST-REQ-001 to 007 | 7 |
| Output Formats | PRD-OUT-REQ-001 to 004 | 4 |
| Git Worktree Support | PRD-WKT-REQ-001 to 005 | 5 |
| Semantic Search | PRD-SEM-REQ-001 to 016 | 16 |
| Semantic Dependency Analysis | PRD-SDEP-REQ-001 to 003 | 3 |
| Semantic Clustering | PRD-SCLST-REQ-001 to 003 | 3 |
| Semantic Change Impact | PRD-SIMP-REQ-001 to 004 | 4 |
| Source Display | PRD-SHOW-REQ-001 to 013 | 13 |
| Code Summary | PRD-SUM-REQ-001 to 020 | 20 |
| Call Graph Analysis | PRD-CGR-REQ-001 to 014 | 14 |
| Execution Flow Detection | PRD-FLOW-REQ-001 to 010 | 10 |
| Blast Radius Impact Analysis | PRD-BLAST-REQ-001 to 010 | 10 |
| Scoped Change Detection | PRD-CHG-REQ-001 to 009 | 9 |
| Unified Symbol Context | PRD-CTX-REQ-001 to 009 | 9 |
| Hybrid Search Fusion | PRD-RRF-REQ-001 to 004 | 4 |
| Edge Confidence Scoring | PRD-CONF-REQ-001 to 006 | 6 |
| Inheritance Tracking | PRD-HRTG-REQ-001 to 005 | 5 |
| Multi-Repo MCP | PRD-MREP-REQ-001 to 006 | 6 |
| **Total** | | **215** |

---

## 5) Open questions log

| ID | Question | Resolution | Status |
|---|---|---|---|
| OQ-001 | Grammar bundling strategy | Bundle all 11 grammars in the binary | Resolved |
| OQ-002 | Reference accuracy | Name-based only, no heuristic disambiguation for V1 | Resolved |
| OQ-003 | Auto-init threshold | No cap; always auto-init with progress indicator | Resolved |
| OQ-004 | Tool name | Renamed from `csi` to `wonk` | Resolved |
| OQ-005 | Smart search ranking weights | How should results be weighted between definitions, call sites, imports, comments, and test files? Needs validation with real Claude Code sessions to calibrate. | Open |
| OQ-006 | Similarity threshold | Should there be a minimum cosine similarity score below which results are not shown? Needs calibration with real queries. | Open |
| OQ-007 | Clustering algorithm | k-means vs. DBSCAN vs. hierarchical? Depends on typical symbol counts per directory. | Open |
| OQ-008 | Multi-daemon resource management | With daemons running indefinitely across many repos, should there be a global limit or resource budget? | Open |

---

## 6) Out of scope for V1

- **LSP server integration.** V1 uses Tree-sitter only. LSP backends (for type-aware resolution) are a V2 feature.
- ~~**Semantic / embedding search.** Natural language queries require an embedding model. Deferred to V2.~~ **Moved to V2 scope: PRD-SEM, PRD-SDEP, PRD-SCLST, PRD-SIMP.**
- ~~**Directory summaries.** LLM-generated descriptions of what each directory does. Deferred to V2.~~ **Moved to V2 scope: PRD-SUM.**
- **Cross-language call graphs.** Connecting a Python HTTP call to a Go handler. Remains out of scope through V4.
- **Editor integrations.** VS Code extension, Neovim plugin, etc. V1 is CLI-only.
- ~~**Remote / monorepo support.** V1 targets single local repos. Multi-root workspaces and remote indexing are future work.~~ **Multi-repo MCP partially addressed in V4 (PRD-MREP). Cross-repo search and remote indexing remain out of scope.**
- **Web UI.** All interaction is through the CLI.
- **Dynamic dispatch resolution.** Virtual calls, trait objects, and function pointers are not resolved by static analysis. Out of scope through V4.
- **ML-based confidence estimation.** Edge confidence uses static heuristics only (PRD-CONF). ML/runtime adjustment is out of scope.
