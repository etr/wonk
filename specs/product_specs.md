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
- **Release strategy:** V1 is CLI-only. Editor integrations, LSP backends, and cross-language call graphs are deferred to V2. V2 semantic search features (embedding-based search, clustering, impact analysis) are now specified below.

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

### 3.6 Symbol Listing (PRD-LST)

**Problem / outcome**
Users need to see all symbols defined in a file or directory for navigation.

**In scope**
- Flat and tree-view listing of symbols per file/directory

**Out of scope**
- LLM-generated directory summaries (V2)

**EARS Requirements**
- `PRD-LST-REQ-001` When the user runs `wonk ls <path>` then the system shall list all symbols defined in the specified file or directory.
- `PRD-LST-REQ-002` When the user provides `--tree` then the system shall display symbols with nesting hierarchy.

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
- Tree-sitter parsing for 10 languages
- File filtering (gitignore, wonkignore, default exclusions)
- Force re-index, status reporting, repo management

**Out of scope**
- Remote indexing, multi-root workspace support

**EARS Requirements**
- `PRD-IDX-REQ-001` When the user runs `wonk init` then the system shall build a full structural index of the current repository.
- `PRD-IDX-REQ-002` When `wonk init` is run without `--local` then the system shall store the index centrally at `~/.wonk/repos/<hash>/`.
- `PRD-IDX-REQ-003` When `wonk init --local` is run then the system shall store the index in `.wonk/` inside the repository root.
- `PRD-IDX-REQ-004` When indexing then the system shall detect and parse files using bundled Tree-sitter grammars for TypeScript/TSX, JavaScript/JSX, Python, Rust, Go, Java, C, C++, Ruby, and PHP.
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
- All 10 language families are correctly parsed
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
- `PRD-FBK-REQ-004` If `wonk ls` finds no symbols in the index for a file then the system shall perform an on-demand Tree-sitter parse.
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
- `PRD-DST-REQ-002` The system shall bundle all 10 Tree-sitter grammars within the binary.
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
- `PRD-SEM-REQ-002` When the user provides `--semantic` on `wonk search` then the system shall blend structural results with semantic results, presenting structural matches first followed by additional semantic matches not already present.
- `PRD-SEM-REQ-003` When returning semantic search results then each result shall include file path, line number, symbol name, symbol kind, and cosine similarity score.
- `PRD-SEM-REQ-004` When the user provides `--budget <n>` on `wonk ask` then the system shall limit output to approximately `n` tokens, prioritizing results with highest similarity.
- `PRD-SEM-REQ-005` When the user provides `--json` on `wonk ask` then the system shall output results as JSON objects including all fields plus the similarity score.
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
- Function-level call graph (file-level deps only)

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

---

## 4) Traceability

| Feature | Requirement IDs | Count |
|---|---|---|
| Text Search | PRD-SRCH-REQ-001 to 005 | 5 |
| Smart Search | PRD-SSRCH-REQ-001 to 006 | 6 |
| Symbol Lookup | PRD-SYM-REQ-001 to 004 | 4 |
| Reference Finding | PRD-REF-REQ-001 to 003 | 3 |
| Signature Display | PRD-SIG-REQ-001 | 1 |
| Symbol Listing | PRD-LST-REQ-001 to 002 | 2 |
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
| **Total** | | **111** |

---

## 5) Open questions log

| ID | Question | Resolution | Status |
|---|---|---|---|
| OQ-001 | Grammar bundling strategy | Bundle all 10 grammars in the binary | Resolved |
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
- **Directory summaries.** LLM-generated descriptions of what each directory does. Deferred to V2.
- **Cross-language call graphs.** Connecting a Python HTTP call to a Go handler. Deferred to V2.
- **Editor integrations.** VS Code extension, Neovim plugin, etc. V1 is CLI-only.
- **Remote / monorepo support.** V1 targets single local repos. Multi-root workspaces and remote indexing are future work.
- **Web UI.** All interaction is through the CLI.
