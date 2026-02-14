# Project Memory

## Project: wonk (code search and indexing tool)
- **Language**: Rust (edition 2024)
- **Package manager**: cargo
- **Build**: `cargo build`
- **Test**: `cargo test` (binary crate, not lib -- use `cargo test` not `cargo test --lib`)
- **Typecheck**: `cargo check`
- **Lint**: `cargo clippy -- -W clippy::all`
- **PATH**: Need `export PATH="$HOME/.cargo/bin:$PATH"` before cargo commands

## Key File Locations
- Source: `/home/etr/progs/csi/src/`
- CLI args: `src/cli.rs` (clap derive)
- Command dispatch: `src/router.rs` (dispatch function + QueryRouter)
- Output formatting: `src/output.rs` (Formatter<W>, output structs)
- Types: `src/types.rs` (Symbol, Reference, SymbolKind, etc.)
- File walker: `src/walker.rs` (Walker with gitignore support)
- DB: `src/db.rs` (SQLite via rusqlite)
- Indexer: `src/indexer.rs` (tree-sitter parsing)
- Pipeline: `src/pipeline.rs` (index building)
- Errors: `src/errors.rs`

## Test Patterns
- Tests are in `#[cfg(test)] mod tests {}` inside each source file
- Use `tempfile::TempDir` for temp directories
- Use `db::open()` to create in-memory test databases
- `QueryRouter::with_conn()` and `QueryRouter::grep_only()` for test-only construction
- Formatter tests use helper: `fn render<F>(json: bool, f: F) -> String`
- Pattern: create DB, insert test data, query via router, format output, assert

## Architecture Notes
- `Symbol` has `scope: Option<String>` for parent symbol name
- Output structs use `#[serde(skip_serializing_if = "Option::is_none")]` for optional fields
- Grep-style format: `file:line:  content`
- JSON format: NDJSON (one JSON object per line)
- `print_hint(msg, json)` prints to stderr, suppressed in JSON mode

## Worktree Setup
- Worktrees go in `.worktrees/` (need to add to .gitignore if not already)
- `.worktrees/` was NOT in .gitignore originally; added manually
- `head`/`grep` shell commands not available in sandbox; use tool alternatives

## Gotchas
- No `head`, `grep`, `cat` in sandbox shell -- use Read/Grep/Write tools instead
- cargo not on PATH by default -- always prefix with `export PATH="$HOME/.cargo/bin:$PATH"`
- Many pre-existing dead_code warnings (daemon, config, walker) -- ignore these
- Pre-existing clippy warnings (io_other_error pattern, ptr_arg) -- match existing style
