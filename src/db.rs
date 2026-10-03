//! Database layer for SQLite storage.
//!
//! Provides connection management, schema creation (including FTS5 content-sync),
//! repo root discovery, and index path computation.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Schema SQL
// ---------------------------------------------------------------------------

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS symbols (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    file TEXT NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    end_line INTEGER,
    scope TEXT,
    signature TEXT,
    language TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS "references" (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    file TEXT NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    context TEXT,
    caller_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL,
    confidence REAL DEFAULT 0.5,
    target_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL
);

CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    language TEXT,
    hash TEXT NOT NULL,
    last_indexed INTEGER NOT NULL,
    line_count INTEGER,
    symbols_count INTEGER
);

CREATE TABLE IF NOT EXISTS daemon_status (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- O(1) corpus summary for BM25 (TASK-079 review): `n_docs` = COUNT(*),
-- `measured` = COUNT(line_count) (the non-NULL rows AVG averages over),
-- `total_lines` = SUM(line_count). Exactly one row (id = 1), maintained
-- transactionally by the pipeline's `files`-mutating helpers; AVG
-- reconstructs as total_lines/measured, bitwise-identical to the SQLite
-- aggregate (both are one IEEE f64 division of the same integers).
-- Readers fall back to the aggregate when the row is absent (pre-summary
-- indexes, hand-seeded fixtures), so a missing or stale row can only cost
-- speed, never correctness of the scores.
CREATE TABLE IF NOT EXISTS corpus_stats (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    n_docs INTEGER NOT NULL,
    measured INTEGER NOT NULL,
    total_lines INTEGER NOT NULL
);

-- Indexes
CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);
CREATE INDEX IF NOT EXISTS idx_symbols_kind ON symbols(kind);
CREATE INDEX IF NOT EXISTS idx_symbols_scope ON symbols(scope);
CREATE INDEX IF NOT EXISTS idx_references_name ON "references"(name);
CREATE INDEX IF NOT EXISTS idx_references_file ON "references"(file);

-- File-level import tracking for dependency graph
CREATE TABLE IF NOT EXISTS file_imports (
    id INTEGER PRIMARY KEY,
    source_file TEXT NOT NULL,
    import_path TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_file_imports_source ON file_imports(source_file);
CREATE INDEX IF NOT EXISTS idx_file_imports_target ON file_imports(import_path);
"#;

// Table populated by TASK-066 (inheritance extraction) and TASK-067 (pipeline wiring).
const TYPE_EDGES_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS type_edges (
    id INTEGER PRIMARY KEY,
    child_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    parent_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    relationship TEXT NOT NULL,
    UNIQUE(child_id, parent_id, relationship)
);
CREATE INDEX IF NOT EXISTS idx_type_edges_child ON type_edges(child_id);
CREATE INDEX IF NOT EXISTS idx_type_edges_parent ON type_edges(parent_id);
"#;

const EMBEDDINGS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS embeddings (
    id INTEGER PRIMARY KEY,
    symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    file TEXT NOT NULL,
    chunk_text TEXT NOT NULL,
    vector BLOB NOT NULL,
    stale INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    provider TEXT NOT NULL DEFAULT 'ollama',
    dim INTEGER NOT NULL DEFAULT 768,
    UNIQUE(symbol_id)
);
CREATE INDEX IF NOT EXISTS idx_embeddings_file ON embeddings(file);
"#;

// BM25 per-term document statistics (TASK-078). One row per distinct
// (term, file): `tf` is the term's frequency in the file's tokenized
// content. Document frequency is derived at query time
// (COUNT(*) WHERE term = ?); document length is NOT stored here —
// it reuses files.line_count (DR-033).
const TERM_STATS_SQL: &str = r"
CREATE TABLE IF NOT EXISTS term_stats (
    term TEXT NOT NULL,
    file TEXT NOT NULL,
    tf INTEGER NOT NULL,
    PRIMARY KEY (term, file)
);
CREATE INDEX IF NOT EXISTS idx_term_stats_file ON term_stats(file);
";

// Precomputed upstream reachability (TASK-080, DR-034). One row per
// (source, target) pair with the minimum BFS depth at which the target is
// reachable from the source, so blast severity tiers are preserved.
// `confidence` carries the discovering edge's confidence — a documented
// deviation from the architecture DDL's (source_id, target_id, min_depth)
// needed to reproduce BlastAffectedSymbol.confidence without changing
// V4 BFS semantics.
const REACH_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS reach (
    source_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    target_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    min_depth INTEGER NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    PRIMARY KEY (source_id, target_id)
);
CREATE INDEX IF NOT EXISTS idx_reach_source_depth ON reach(source_id, min_depth);
CREATE INDEX IF NOT EXISTS idx_reach_target ON reach(target_id);

CREATE TABLE IF NOT EXISTS reach_truncated (
    source_id INTEGER PRIMARY KEY REFERENCES symbols(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS reach_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

const SUMMARIES_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS summaries (
    path TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    description TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
"#;

// [V5] Service contracts published/consumed by this repo (DR-031). The
// columns follow the architecture DDL verbatim; there is deliberately no
// workspace column — workspace membership lives in meta.json (REQ-020,
// written by TASK-084).
const CONTRACTS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS contracts (
    id INTEGER PRIMARY KEY,
    canonical_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    role TEXT NOT NULL,
    symbol_id INTEGER REFERENCES symbols(id) ON DELETE CASCADE,
    file TEXT NOT NULL,
    line INTEGER NOT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    UNIQUE(canonical_id, role, file, line)
);
CREATE INDEX IF NOT EXISTS idx_contracts_canonical ON contracts(canonical_id, role);
CREATE INDEX IF NOT EXISTS idx_contracts_symbol ON contracts(symbol_id);
CREATE INDEX IF NOT EXISTS idx_contracts_file ON contracts(file);
"#;

const FTS_SQL: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(
    name, kind, file, content=symbols, content_rowid=id
);
"#;

// Per-repo review suppressions (TASK-089, PRD-REV-REQ-014). `rule` and
// `file` are retained display metadata: the identity is the only key a
// lookup needs, but `wonk review suppress list` and bulk `--rule` removal
// need the human-readable columns.
const REVIEW_SUPPRESSIONS_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS review_suppressions (
    identity TEXT PRIMARY KEY,
    rule TEXT NOT NULL DEFAULT '',
    file TEXT NOT NULL DEFAULT '',
    note TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_review_suppressions_rule ON review_suppressions(rule);
"#;

// Bounded history mining (TASK-096, DR-039). `file_churn` is the
// age-weighted aggregate the churn signal reads; `mined_commits` +
// `commit_files` retain the per-commit detail TASK-097's co-change needs
// and make the incremental refresh exact (weights are recomputed against
// the new window bounds without re-reading git); `history_meta.mined_head`
// records HEAD at the last successful mine (the reach_meta pattern). The
// retained-row invariant `mined_commits <= window` keeps storage O(window)
// regardless of repository age.
//
// `co_change` (TASK-097) is the age-weighted file-coupling aggregate: one
// row per DIRECTED pair, at most top-K per `file_a`, so storage stays
// linear in files rather than quadratic (the rerank loader queries by
// `file_a` through the PK's leading column; a separate index served no
// executed query — removed in the dead-code sweep).
//
// TASK-105 adds the author/recency columns: `mined_commits.author` (the
// `%an` field) and the per-file `last_ts`/`last_author`/`primary_author`
// the feedback features read — all nullable, all NULL-filled on a
// pre-TASK-105 index until the next mine rewrites them.
const HISTORY_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS file_churn (
    file TEXT PRIMARY KEY,
    score REAL NOT NULL,
    last_ts INTEGER,
    last_author TEXT,
    primary_author TEXT
);
CREATE TABLE IF NOT EXISTS mined_commits (
    commit_id TEXT PRIMARY KEY,
    commit_ts INTEGER NOT NULL,
    author TEXT
);
CREATE TABLE IF NOT EXISTS commit_files (
    commit_id TEXT NOT NULL REFERENCES mined_commits(commit_id) ON DELETE CASCADE,
    file TEXT NOT NULL,
    PRIMARY KEY (commit_id, file)
);
CREATE INDEX IF NOT EXISTS idx_commit_files_file ON commit_files(file);
CREATE TABLE IF NOT EXISTS history_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS co_change (
    file_a TEXT NOT NULL,
    file_b TEXT NOT NULL,
    weight REAL NOT NULL,
    PRIMARY KEY (file_a, file_b)
);
"#;

// Global graph topology (TASK-098, DR-040): hub and authority scores per
// symbol, recomputed as a distinct pass on a cadence and NEVER
// incrementally per file (PRD-TOPO-REQ-006) — the scores are global
// graph properties, so a file edit cannot update them locally. `community`
// and its index are created now but stay NULL-filled/unread until
// TASK-099 owns them. TASK-105 adds the CSR degrees `fan_in`/`fan_out`
// (global graph properties like hub/authority, written by the same pass;
// nullable so a pre-TASK-105 index migrates in place).
// `topology_meta.last_computed` drives the cadence
// gate and the staleness marker (PRD-TOPO-REQ-007).
const TOPOLOGY_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS symbol_topology (
    symbol_id INTEGER PRIMARY KEY REFERENCES symbols(id) ON DELETE CASCADE,
    hub REAL NOT NULL,
    authority REAL NOT NULL,
    community INTEGER,
    fan_in INTEGER,
    fan_out INTEGER
);
CREATE INDEX IF NOT EXISTS idx_topology_community ON symbol_topology(community);
CREATE TABLE IF NOT EXISTS topology_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
"#;

// Near-duplicate similarity (TASK-100, DR-041): bottom-k shingle sketches
// per symbol, written at index time (PRD-DUP-REQ-001), plus the additive
// `near_duplicates` pair store for REQ-003 — pairs are recorded by the
// query-time memo and the `wonk duplicates` sweep, never by an index-time
// all-pairs pass. Both tables cascade from the per-file symbol deletes
// that `upsert_file_data`/`batch_insert` already issue, so no new DELETE
// statements exist for them.
const DUPLICATES_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS symbol_shingles (
    symbol_id INTEGER PRIMARY KEY REFERENCES symbols(id) ON DELETE CASCADE,
    signature BLOB NOT NULL        -- bottom-k sorted u32 LE shingle hashes (TASK-100, DR-041)
);
CREATE TABLE IF NOT EXISTS near_duplicates (
    symbol_id_a INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    symbol_id_b INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    similarity REAL NOT NULL,      -- sketch Jaccard at detection time
    PRIMARY KEY (symbol_id_a, symbol_id_b)   -- canonical: symbol_id_a < symbol_id_b
);
CREATE INDEX IF NOT EXISTS idx_near_duplicates_b ON near_duplicates(symbol_id_b);
"#;

// Usage-feedback capture (TASK-101, DR-042). `feedback_events` is the
// architecture §5.2 DDL verbatim: one row per (feedback call, reported-
// useful result), the FULL slate serialized into `features` — contrastive
// credit assignment needs the alternatives the caller passed over
// (PRD-FB-REQ-002). `feedback_slates` is the TASK-101 capture mechanism
// (one addition beyond §5.2): the ranked search path persists the slate it
// is about to show — every result's identity, rank, and feature vector —
// so the feedback call references what was shown by token and the stored
// vectors are wonk's own, never caller-echoed. Both tables are written by
// query-time processes only; the indexer never touches them.
const FEEDBACK_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS feedback_events (
    id INTEGER PRIMARY KEY,
    result_identity TEXT NOT NULL,   -- content-anchored, survives re-index (PRD-FB-REQ-005)
    query_class TEXT,                -- class at query time; enables per-class learning (PRD-FB-REQ-008)
    chosen_rank INTEGER NOT NULL,    -- rank of the useful result; rank 1 yields no update (PRD-FB-REQ-009)
    features TEXT NOT NULL,          -- feature vector for chosen + alternatives (PRD-FB-REQ-002/021)
    useful INTEGER NOT NULL,         -- 1 today (events exist for reported-useful results); reserved
    session TEXT,                    -- distinct-session counting (PRD-FB-REQ-015/016)
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_feedback_identity ON feedback_events(result_identity);
CREATE INDEX IF NOT EXISTS idx_feedback_created ON feedback_events(created_at);

CREATE TABLE IF NOT EXISTS feedback_slates (
    token TEXT PRIMARY KEY,          -- 16-hex-char id echoed to the caller
    query TEXT NOT NULL,
    query_class TEXT,                -- class AT QUERY TIME (PRD-FB-REQ-008)
    members TEXT NOT NULL,           -- JSON array of SlateMember
    created_at INTEGER NOT NULL
);

-- [V5] TASK-102: Learned per-repo weights — the entire durable product of
-- feedback (DR-042/DR-043). Bounded deviation from defaults, decaying with
-- age, resettable independently of event history. `feature` is a signal
-- name (bare) or a flattened descriptive key (group-prefixed).
CREATE TABLE IF NOT EXISTS learned_weights (
    feature TEXT NOT NULL,
    query_class TEXT NOT NULL DEFAULT '',  -- '' = overall; symbol/path/signature/conceptual
    weight REAL NOT NULL,
    observations INTEGER NOT NULL,
    sessions INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (feature, query_class)
);

-- [V5] TASK-102: Exact distinct-session bookkeeping per (feature, scope):
-- one row per session that ever observed the feature. Keeps the `sessions`
-- column and the REQ-025 gate honest under interleaved sessions
-- (A,B,A counts 2).
CREATE TABLE IF NOT EXISTS learned_weight_sessions (
    feature TEXT NOT NULL,
    query_class TEXT NOT NULL DEFAULT '',
    session TEXT NOT NULL,
    PRIMARY KEY (feature, query_class, session)
);

-- [V5] TASK-102: Learning progress watermark: the highest feedback_events.id
-- processed. Missed learning runs (best-effort contract) are picked up by
-- the next successful feedback call; TASK-103's reset/recompute builds on
-- this.
CREATE TABLE IF NOT EXISTS learned_meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- [V5] TASK-104: Session-gated per-result preferences (DR-042's narrow
-- per-result layer, PRD-FB-REQ-016). One row per confirmed result identity;
-- strength grows one step per NEW distinct confirming session (a repeat
-- session grows nothing -- AR-036), decays on the learn_half_life_days rule
-- (PRD-FB-REQ-011), and is capped at half the feedback signal's full value
-- clamp -- strictly below every learned-weight channel. A materially
-- changed result yields a different identity and the row stops applying
-- (PRD-FB-REQ-006); retirement is resolved at match time, never a write.
CREATE TABLE IF NOT EXISTS result_preferences (
    result_identity TEXT PRIMARY KEY,   -- content-anchored, survives re-index (PRD-FB-REQ-005)
    strength REAL NOT NULL,             -- decaying value in [0, 0.5]; +0.1 per new distinct session
    observations INTEGER NOT NULL,      -- every qualifying confirming event (evidence)
    sessions INTEGER NOT NULL,          -- distinct confirming sessions (the gate, PRD-FB-REQ-016)
    updated_at INTEGER NOT NULL
);

-- [V5] TASK-104: Exact distinct-session bookkeeping per result -- the
-- learned_weight_sessions pattern: keeps `sessions` honest under
-- interleaved sessions (A,B,A counts 2) and makes same-session repeats
-- free (no strength, no decay refresh: one session can neither activate
-- nor sustain a preference -- AR-036).
CREATE TABLE IF NOT EXISTS result_preference_sessions (
    result_identity TEXT NOT NULL,
    session TEXT NOT NULL,
    PRIMARY KEY (result_identity, session)
);
"#;

const TRIGGERS_SQL: &str = r#"
CREATE TRIGGER IF NOT EXISTS symbols_ai AFTER INSERT ON symbols BEGIN
    INSERT INTO symbols_fts(rowid, name, kind, file)
    VALUES (new.id, new.name, new.kind, new.file);
END;

CREATE TRIGGER IF NOT EXISTS symbols_bd BEFORE DELETE ON symbols BEGIN
    INSERT INTO symbols_fts(symbols_fts, rowid, name, kind, file)
    VALUES ('delete', old.id, old.name, old.kind, old.file);
END;

CREATE TRIGGER IF NOT EXISTS symbols_bu BEFORE UPDATE ON symbols BEGIN
    INSERT INTO symbols_fts(symbols_fts, rowid, name, kind, file)
    VALUES ('delete', old.id, old.name, old.kind, old.file);
END;

CREATE TRIGGER IF NOT EXISTS symbols_au AFTER UPDATE ON symbols BEGIN
    INSERT INTO symbols_fts(rowid, name, kind, file)
    VALUES (new.id, new.name, new.kind, new.file);
END;
"#;

// ---------------------------------------------------------------------------
// Connection management
// ---------------------------------------------------------------------------

/// Open (or create) a SQLite database at `path`, apply the full schema, and
/// set pragmas suitable for concurrent access.
pub fn open(path: &Path) -> Result<Connection> {
    // Ensure parent directory exists.
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating index directory {}", parent.display()))?;
    }

    let conn =
        Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;

    apply_pragmas(&conn)?;
    apply_schema(&conn)?;

    Ok(conn)
}

/// Open an **existing** database without running schema creation.  Useful when
/// you only need to read and you know the DB already exists.
pub fn open_existing(path: &Path) -> Result<Connection> {
    if !path.exists() {
        bail!("index not found at {}", path.display());
    }
    let conn =
        Connection::open(path).with_context(|| format!("opening database {}", path.display()))?;

    apply_pragmas(&conn)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA busy_timeout = 5000;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )
    .context("setting database pragmas")?;
    Ok(())
}

/// Test-only handle to the full schema/migration pass that every
/// [`open`] runs — cross-module tests use it on hand-built connections
/// (the per-feature `ensure_*` wrappers it replaced were redundant).
#[cfg(test)]
pub(crate) fn apply_schema_for_tests(conn: &Connection) -> Result<()> {
    apply_schema(conn)
}

fn apply_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)
        .context("creating base tables and indexes")?;
    // Column migrations must run before any SQL that references these columns.
    ensure_caller_id_column(conn)?;
    ensure_confidence_column(conn)?;
    ensure_doc_comment_column(conn)?;
    ensure_target_id_column(conn)?;
    conn.execute_batch(TYPE_EDGES_SQL)
        .context("creating type_edges table")?;
    conn.execute_batch(EMBEDDINGS_SQL)
        .context("creating embeddings table")?;
    ensure_embedding_metadata_columns(conn)?;
    conn.execute_batch(TERM_STATS_SQL)
        .context("creating term_stats table")?;
    conn.execute_batch(REACH_SQL)
        .context("creating reach tables")?;
    conn.execute_batch(CONTRACTS_SQL)
        .context("creating contracts table")?;
    conn.execute_batch(REVIEW_SUPPRESSIONS_SQL)
        .context("creating review_suppressions table")?;
    conn.execute_batch(HISTORY_SQL)
        .context("creating history tables")?;
    ensure_history_columns(conn)?;
    conn.execute_batch(TOPOLOGY_SQL)
        .context("creating topology tables")?;
    ensure_topology_columns(conn)?;
    conn.execute_batch(DUPLICATES_SQL)
        .context("creating duplicate tables")?;
    conn.execute_batch(FEEDBACK_SQL)
        .context("creating feedback tables")?;
    conn.execute_batch(SUMMARIES_SQL)
        .context("creating summaries table")?;
    conn.execute_batch(FTS_SQL)
        .context("creating FTS5 virtual table")?;
    conn.execute_batch(TRIGGERS_SQL)
        .context("creating FTS5 sync triggers")?;
    Ok(())
}

/// Ensure the `embeddings` table exists, creating it if missing.
///
/// This handles schema migration for V1 indexes that were created before
/// embedding support was added.  Safe to call on databases that already
/// have the table (uses `CREATE TABLE IF NOT EXISTS`).
pub fn ensure_embeddings_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(EMBEDDINGS_SQL)
        .context("creating embeddings table (migration)")?;
    ensure_embedding_metadata_columns(conn)?;
    Ok(())
}

/// Add provider metadata to embedding tables created before V5.
fn ensure_embedding_metadata_columns(conn: &Connection) -> Result<()> {
    let columns: Vec<String> = conn
        .prepare("PRAGMA table_info(embeddings)")?
        .query_map([], |row| row.get(1))?
        .collect::<rusqlite::Result<_>>()?;

    if !columns.iter().any(|name| name == "provider") {
        conn.execute_batch(
            "ALTER TABLE embeddings
             ADD COLUMN provider TEXT NOT NULL DEFAULT 'ollama';",
        )
        .context("adding provider column to embeddings table")?;
    }
    if !columns.iter().any(|name| name == "dim") {
        conn.execute_batch(
            "ALTER TABLE embeddings
             ADD COLUMN dim INTEGER NOT NULL DEFAULT 768;",
        )
        .context("adding dimension column to embeddings table")?;
    }
    Ok(())
}

/// Ensure the `summaries` table exists, creating it if missing.
///
/// This handles schema migration for indexes created before LLM description
/// caching was added.  Safe to call on databases that already have the table
/// (uses `CREATE TABLE IF NOT EXISTS`).
pub fn ensure_summaries_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(SUMMARIES_SQL)
        .context("creating summaries table (migration)")?;
    Ok(())
}

/// Add the TASK-105 author/recency columns to history tables created
/// before them (run by `apply_schema` on every open): `mined_commits.author`, `file_churn.last_ts`,
/// `file_churn.last_author`, `file_churn.primary_author`. Existing rows
/// backfill NULL — the features stay omitted until the next mine.
fn ensure_history_columns(conn: &Connection) -> Result<()> {
    add_missing_columns(
        conn,
        "mined_commits",
        &[("author", "ALTER TABLE mined_commits ADD COLUMN author TEXT")],
    )?;
    add_missing_columns(
        conn,
        "file_churn",
        &[
            (
                "last_ts",
                "ALTER TABLE file_churn ADD COLUMN last_ts INTEGER",
            ),
            (
                "last_author",
                "ALTER TABLE file_churn ADD COLUMN last_author TEXT",
            ),
            (
                "primary_author",
                "ALTER TABLE file_churn ADD COLUMN primary_author TEXT",
            ),
        ],
    )?;
    Ok(())
}

/// Add the TASK-105 fan-degree columns to a `symbol_topology` table created
/// before them (run by `apply_schema` on every open). Existing rows backfill NULL — the graph features stay
/// omitted until the next topology recompute.
fn ensure_topology_columns(conn: &Connection) -> Result<()> {
    add_missing_columns(
        conn,
        "symbol_topology",
        &[
            (
                "fan_in",
                "ALTER TABLE symbol_topology ADD COLUMN fan_in INTEGER",
            ),
            (
                "fan_out",
                "ALTER TABLE symbol_topology ADD COLUMN fan_out INTEGER",
            ),
        ],
    )?;
    Ok(())
}

/// Add each named column to `table` when `PRAGMA table_info` shows it
/// missing — the shared body of the column migrations (the
/// `ensure_embedding_metadata_columns` precedent). `stmts` pairs a column
/// name with the ALTER statement that adds it.
fn add_missing_columns(conn: &Connection, table: &str, stmts: &[(&str, &str)]) -> Result<()> {
    let columns: Vec<String> = conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get(1))?
        .collect::<rusqlite::Result<_>>()?;
    for (column, stmt) in stmts {
        if !columns.iter().any(|name| name == column) {
            conn.execute_batch(stmt)
                .with_context(|| format!("adding {column} column to {table} table"))?;
        }
    }
    Ok(())
}

/// Ensure the TASK-100 duplicate tables exist (`symbol_shingles`,
/// `near_duplicates`).
///
/// Handles schema migration for indexes created before shingle
/// signatures: safe to call on databases that already have the tables.
pub fn ensure_duplicate_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(DUPLICATES_SQL)
        .context("creating duplicate tables (migration)")?;
    Ok(())
}

/// Ensure the TASK-101/102/104 feedback tables exist (`feedback_events`,
/// `feedback_slates`, `learned_weights`, `learned_weight_sessions`,
/// `learned_meta`, `result_preferences`, `result_preference_sessions`).
///
/// Handles schema migration for indexes created before feedback capture:
/// safe to call on databases that already have the tables.
pub fn ensure_feedback_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(FEEDBACK_SQL)
        .context("creating feedback tables (migration)")?;
    Ok(())
}

/// Ensure the `confidence` column exists on the `references` table.
///
/// Handles schema migration for pre-V4 indexes that lack the confidence
/// scoring column.  Uses `PRAGMA table_info` to check before altering.
pub fn ensure_confidence_column(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare("PRAGMA table_info(\"references\")")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == "confidence");

    if !has_column {
        conn.execute_batch("ALTER TABLE \"references\" ADD COLUMN confidence REAL DEFAULT 0.5;")
            .context("adding confidence column to references table")?;
    }

    // Always run CREATE INDEX IF NOT EXISTS for idempotent migration.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_references_name_confidence ON \"references\"(name, confidence);
         CREATE INDEX IF NOT EXISTS idx_references_caller_confidence ON \"references\"(caller_id, confidence);",
    )
    .context("creating confidence indexes")?;

    Ok(())
}

/// Ensure the `type_edges` table exists, creating it if missing.
///
/// Handles schema migration for indexes created before type hierarchy
/// support was added.  Safe to call on databases that already
/// have the table (uses `CREATE TABLE IF NOT EXISTS`).
pub fn ensure_type_edges_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(TYPE_EDGES_SQL)
        .context("creating type_edges table (migration)")?;
    Ok(())
}

/// Ensure the `term_stats` table exists, creating it if missing.
///
/// Handles schema migration for indexes created before BM25 term
/// statistics (TASK-078) were added.  Safe to call on databases that
/// already have the table (uses `CREATE TABLE IF NOT EXISTS`).
pub fn ensure_term_stats_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(TERM_STATS_SQL)
        .context("creating term_stats table (migration)")?;
    Ok(())
}

/// Ensure the `reach`, `reach_truncated`, and `reach_meta` tables exist,
/// creating them if missing.
///
/// Handles schema migration for indexes created before the precomputed
/// reach table (TASK-080) was added.  Safe to call on databases that
/// already have the tables (uses `CREATE TABLE IF NOT EXISTS`).
pub fn ensure_reach_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(REACH_SQL)
        .context("creating reach tables (migration)")?;
    Ok(())
}

/// Ensure the `review_suppressions` table exists, creating it if missing.
///
/// Handles schema migration for indexes created before durable finding
/// suppression (TASK-089) was added. Safe to call repeatedly (uses
/// `CREATE TABLE IF NOT EXISTS`).
pub fn ensure_review_suppressions_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(REVIEW_SUPPRESSIONS_SQL)
        .context("creating review_suppressions table (migration)")?;
    Ok(())
}

/// Ensure the `doc_comment` column exists on the `symbols` table.
///
/// Handles schema migration for indexes created before doc comment
/// extraction was added.
pub fn ensure_doc_comment_column(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare("PRAGMA table_info(symbols)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == "doc_comment");

    if !has_column {
        conn.execute_batch("ALTER TABLE symbols ADD COLUMN doc_comment TEXT;")
            .context("adding doc_comment column to symbols table")?;
    }

    Ok(())
}

/// Ensure the `caller_id` column exists on the `references` table.
///
/// Handles schema migration for pre-V3 indexes that lack the call graph
/// FK.  Uses `PRAGMA table_info` to check before altering.
pub fn ensure_caller_id_column(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare("PRAGMA table_info(\"references\")")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == "caller_id");

    if !has_column {
        conn.execute_batch(
            "ALTER TABLE \"references\" ADD COLUMN caller_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL;",
        )
        .context("adding caller_id column to references table")?;
    }

    // Always run CREATE INDEX IF NOT EXISTS for idempotent migration.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_references_caller ON \"references\"(caller_id);",
    )
    .context("creating caller_id index")?;

    Ok(())
}

/// Ensure the `target_id` column exists on the `references` table.
///
/// Handles schema migration for indexes built before target resolution was
/// added. Uses `PRAGMA table_info` to check before altering.
pub fn ensure_target_id_column(conn: &Connection) -> Result<()> {
    let has_column: bool = conn
        .prepare("PRAGMA table_info(\"references\")")?
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == "target_id");

    if !has_column {
        conn.execute_batch(
            "ALTER TABLE \"references\" ADD COLUMN target_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL;",
        )
        .context("adding target_id column to references table")?;
    }

    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_references_target_id ON \"references\"(target_id);",
    )
    .context("creating target_id index")?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Repo root discovery
// ---------------------------------------------------------------------------

/// Walk upwards from `start` looking for a `.git` directory or `.wonk`
/// directory.  Returns the directory that contains the marker.
pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let mut current = start.to_path_buf();
    // Canonicalize so we don't get stuck in symlink loops, but tolerate
    // failure (e.g. non-existent trailing component).
    if let Ok(canon) = fs::canonicalize(&current) {
        current = canon;
    }
    loop {
        if current.join(".git").exists() || current.join(".wonk").exists() {
            return Ok(current);
        }
        if !current.pop() {
            bail!(
                "could not find repository root (no .git or .wonk) starting from {}",
                start.display()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Index path computation
// ---------------------------------------------------------------------------

/// Compute the SHA-256-short hash (first 16 hex chars) of `repo_path`.
pub fn repo_hash(repo_path: &Path) -> String {
    let canonical = repo_path.to_string_lossy();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let digest = hasher.finalize();
    hex_encode_short(&digest)
}

/// First 16 hex characters of a byte slice.
fn hex_encode_short(bytes: &[u8]) -> String {
    // 16 hex chars = 8 bytes
    bytes.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Where the index lives when using the **central** (default) location:
/// `~/.wonk/repos/<hash>/index.db`
pub fn central_index_path(repo_path: &Path) -> Result<PathBuf> {
    let home = home_dir()?;
    let hash = repo_hash(repo_path);
    Ok(home.join(".wonk").join("repos").join(hash).join("index.db"))
}

/// Where the index lives when using the **local** location:
/// `<repo_root>/.wonk/index.db`
pub fn local_index_path(repo_root: &Path) -> PathBuf {
    repo_root.join(".wonk").join("index.db")
}

/// Resolve the index path for a given repo, respecting `local` flag.
pub fn index_path_for(repo_root: &Path, local: bool) -> Result<PathBuf> {
    if local {
        Ok(local_index_path(repo_root))
    } else {
        central_index_path(repo_root)
    }
}

/// Check whether an index exists for the given repo root.
///
/// Checks the local path first (`.wonk/index.db`), then the central path
/// (`~/.wonk/repos/<hash>/index.db`).  Returns the path if found.
pub fn find_existing_index(repo_root: &Path) -> Option<PathBuf> {
    let local = local_index_path(repo_root);
    if local.exists() {
        return Some(local);
    }
    if let Ok(central) = central_index_path(repo_root)
        && central.exists()
    {
        return Some(central);
    }
    None
}

// ---------------------------------------------------------------------------
// meta.json
// ---------------------------------------------------------------------------

/// Metadata written alongside `index.db`.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct Meta {
    pub repo_path: String,
    pub created: u64,
    pub languages: Vec<String>,
    #[serde(default)]
    pub wonk_version: Option<String>,
    /// Workspace ids this repo declared at index time (TASK-084,
    /// PRD-CTR-REQ-020). Absent in pre-TASK-084 meta.json — reads back
    /// empty, meaning "undeclared" (own-name default applies).
    #[serde(default)]
    pub workspaces: Vec<String>,
}

/// Write `meta.json` next to the given `index_db_path`.
pub fn write_meta(
    index_db_path: &Path,
    repo_path: &Path,
    languages: &[String],
    workspaces: &[String],
) -> Result<()> {
    let meta_path = index_db_path
        .parent()
        .expect("index.db must have a parent directory")
        .join("meta.json");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let meta = Meta {
        repo_path: repo_path.to_string_lossy().into_owned(),
        created: now,
        languages: languages.to_vec(),
        wonk_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        workspaces: workspaces.to_vec(),
    };

    let json = serde_json::to_string_pretty(&meta).context("serializing meta.json")?;
    fs::write(&meta_path, json).with_context(|| format!("writing {}", meta_path.display()))?;
    Ok(())
}

/// Read `meta.json` from next to the given `index_db_path`.
pub fn read_meta(index_db_path: &Path) -> Result<Meta> {
    let meta_path = index_db_path
        .parent()
        .expect("index.db must have a parent directory")
        .join("meta.json");

    let data = fs::read_to_string(&meta_path)
        .with_context(|| format!("reading {}", meta_path.display()))?;
    let meta: Meta = serde_json::from_str(&data).context("parsing meta.json")?;
    Ok(meta)
}

// ---------------------------------------------------------------------------
// Central registry (~/.wonk/repos)
// ---------------------------------------------------------------------------

/// Enumerate the central registry under `repos_dir`: every entry directory
/// holding an `index.db` with a readable `meta.json` whose claimed root
/// still carries a `.git`/`.wonk` marker. Entries failing any step are
/// skipped; the index itself is never opened (DR-030 — callers open
/// lazily). Each survivor yields `(repo_path, index_path, meta)`.
pub fn registry_entries(repos_dir: &Path) -> Vec<(PathBuf, PathBuf, Meta)> {
    let mut entries = Vec::new();

    let Ok(read_dir) = fs::read_dir(repos_dir) else {
        return entries;
    };
    for dir_entry in read_dir.flatten() {
        let index_dir = dir_entry.path();
        if !index_dir.is_dir() {
            continue;
        }
        let index_path = index_dir.join("index.db");
        if !index_path.exists() {
            continue;
        }
        let Ok(meta) = read_meta(&index_path) else {
            continue;
        };
        let repo_path = PathBuf::from(&meta.repo_path);
        if !repo_path.join(".git").exists() && !repo_path.join(".wonk").exists() {
            continue;
        }
        entries.push((repo_path, index_path, meta));
    }

    entries
}

/// Lazily-opened index connections cached by index path — the DR-030
/// lazy-open pattern shared by the MCP registry and the workspace
/// resolver: an index opens on first use and is reused thereafter;
/// indexes never asked for are never opened.
#[derive(Default)]
pub struct ConnectionCache {
    open: HashMap<PathBuf, Connection>,
}

impl ConnectionCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get or lazily open the connection for `index_path`.
    pub fn get_or_open(&mut self, index_path: &Path) -> Result<&Connection> {
        if !self.open.contains_key(index_path) {
            let conn = open_existing(index_path)?;
            self.open.insert(index_path.to_path_buf(), conn);
        }
        Ok(self.open.get(index_path).expect("just inserted"))
    }

    /// Number of currently open connections (cache introspection).
    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// Whether no connection has been opened yet (cache introspection).
    pub fn is_empty(&self) -> bool {
        self.open.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Symbol detection
// ---------------------------------------------------------------------------

/// Count symbol names in the FTS5 index matching the given pattern.
///
/// Returns 0 if the query fails (e.g. pattern contains characters that are
/// invalid in FTS5 syntax) or if no symbols match.  Used for symbol detection:
/// a non-zero result means the pattern likely refers to a code symbol and
/// ranked mode should be used.
pub fn count_matching_symbols(conn: &Connection, pattern: &str) -> u64 {
    if pattern.is_empty() {
        return 0;
    }
    // Wrap in double quotes to treat as a literal phrase in FTS5.
    // Escape any embedded double quotes by doubling them.
    let escaped = pattern.replace('"', "\"\"");
    let fts_query = format!("\"{}\"", escaped);

    conn.query_row(
        "SELECT COUNT(*) FROM symbols_fts WHERE name MATCH ?1",
        [&fts_query],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0) as u64
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check whether a file path exists in the `files` table.
pub fn file_exists_in_index(conn: &Connection, path: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files WHERE path = ?1",
        [path],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn home_dir() -> Result<PathBuf> {
    // Try $HOME first.  We avoid the `dirs` crate to keep dependencies small.
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home));
    }
    bail!("could not determine home directory ($HOME is not set)");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_open_creates_schema() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Check that all expected tables exist.
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"symbols".to_string()));
        assert!(tables.contains(&"references".to_string()));
        assert!(tables.contains(&"files".to_string()));
        assert!(tables.contains(&"daemon_status".to_string()));
        assert!(tables.contains(&"symbols_fts".to_string()));
        assert!(tables.contains(&"file_imports".to_string()));
        assert!(tables.contains(&"embeddings".to_string()));
        assert!(tables.contains(&"type_edges".to_string()));
        assert!(tables.contains(&"term_stats".to_string()));
        assert!(tables.contains(&"reach".to_string()));
        assert!(tables.contains(&"reach_truncated".to_string()));
        assert!(tables.contains(&"reach_meta".to_string()));
    }

    #[test]
    fn test_file_imports_table_insert_and_query() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/main.ts", "./utils"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO file_imports (source_file, import_path) VALUES (?1, ?2)",
            rusqlite::params!["src/main.ts", "./config"],
        )
        .unwrap();

        // Query forward deps.
        let imports: Vec<String> = conn
            .prepare("SELECT DISTINCT import_path FROM file_imports WHERE source_file = ?1")
            .unwrap()
            .query_map(rusqlite::params!["src/main.ts"], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(imports.len(), 2);

        // Query reverse deps.
        let rdeps: Vec<String> = conn
            .prepare("SELECT DISTINCT source_file FROM file_imports WHERE import_path = ?1")
            .unwrap()
            .query_map(rusqlite::params!["./utils"], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(rdeps.len(), 1);
        assert_eq!(rdeps[0], "src/main.ts");
    }

    #[test]
    fn test_busy_timeout_is_set() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let timeout: i64 = conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
    }

    #[test]
    fn test_fts5_triggers_insert() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["my_func", "function", "src/main.rs", 10, 0, "rust"],
        )
        .unwrap();

        // FTS should contain the inserted row.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'my_func'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_fts5_triggers_delete() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["del_func", "function", "src/lib.rs", 5, 0, "rust"],
        )
        .unwrap();

        // Verify it's in FTS.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'del_func'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Delete the symbol row — trigger should remove from FTS.
        conn.execute("DELETE FROM symbols WHERE name = 'del_func'", [])
            .unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'del_func'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_fts5_triggers_update() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["old_name", "function", "src/main.rs", 1, 0, "rust"],
        )
        .unwrap();

        // Update the name.
        conn.execute(
            "UPDATE symbols SET name = 'new_name' WHERE name = 'old_name'",
            [],
        )
        .unwrap();

        // Old name should be gone from FTS.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'old_name'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);

        // New name should be present.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM symbols_fts WHERE symbols_fts MATCH 'new_name'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_find_repo_root_git() {
        let dir = TempDir::new().unwrap();
        let git_dir = dir.path().join(".git");
        fs::create_dir(&git_dir).unwrap();
        let sub = dir.path().join("a").join("b").join("c");
        fs::create_dir_all(&sub).unwrap();

        let root = find_repo_root(&sub).unwrap();
        assert_eq!(root, fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn test_find_repo_root_wonk() {
        let dir = TempDir::new().unwrap();
        let wonk_dir = dir.path().join(".wonk");
        fs::create_dir(&wonk_dir).unwrap();
        let sub = dir.path().join("x");
        fs::create_dir(&sub).unwrap();

        let root = find_repo_root(&sub).unwrap();
        assert_eq!(root, fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn test_find_repo_root_fails() {
        // Use a tmpdir with no markers at all.
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("lonely");
        fs::create_dir(&sub).unwrap();

        let result = find_repo_root(&sub);
        assert!(result.is_err());
    }

    #[test]
    fn test_repo_hash_deterministic() {
        let path = Path::new("/home/user/projects/myrepo");
        let h1 = repo_hash(path);
        let h2 = repo_hash(path);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 16);
        // Ensure it's all hex characters.
        assert!(h1.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_central_index_path() {
        let repo = Path::new("/home/user/projects/myrepo");
        let path = central_index_path(repo).unwrap();
        let hash = repo_hash(repo);
        assert!(path.to_string_lossy().contains(&hash));
        assert!(path.to_string_lossy().ends_with("index.db"));
        assert!(path.to_string_lossy().contains(".wonk/repos/"));
    }

    #[test]
    fn test_local_index_path() {
        let repo = Path::new("/home/user/projects/myrepo");
        let path = local_index_path(repo);
        assert_eq!(
            path,
            PathBuf::from("/home/user/projects/myrepo/.wonk/index.db")
        );
    }

    #[test]
    fn test_write_and_read_meta() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let repo_path = Path::new("/fake/repo");
        let langs = vec!["rust".to_string(), "python".to_string()];

        write_meta(&db_path, repo_path, &langs, &[]).unwrap();

        let meta = read_meta(&db_path).unwrap();
        assert_eq!(meta.repo_path, "/fake/repo");
        assert_eq!(meta.languages, vec!["rust", "python"]);
        assert!(meta.created > 0);
    }

    #[test]
    fn meta_roundtrips_workspaces() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let workspaces = vec!["payments".to_string(), "platform".to_string()];

        write_meta(
            &db_path,
            Path::new("/fake/repo"),
            &["rust".to_string()],
            &workspaces,
        )
        .unwrap();

        let meta = read_meta(&db_path).unwrap();
        assert_eq!(meta.workspaces, workspaces);
    }

    #[test]
    fn meta_reads_missing_workspaces_as_empty() {
        let dir = TempDir::new().unwrap();
        // A meta.json written before TASK-084: no workspaces key at all.
        fs::write(
            dir.path().join("meta.json"),
            r#"{"repo_path":"/fake/repo","created":1,"languages":["rust"]}"#,
        )
        .unwrap();

        let meta = read_meta(&dir.path().join("index.db")).unwrap();
        assert!(
            meta.workspaces.is_empty(),
            "legacy meta.json must read back undeclared, not error"
        );
    }

    #[test]
    fn test_open_idempotent() {
        // Opening the same database twice should not fail.
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let _conn1 = open(&db_path).unwrap();
        drop(_conn1);
        let _conn2 = open(&db_path).unwrap();
    }

    #[test]
    fn test_schema_references_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["my_func", "src/main.rs", 20, 4, "let x = my_func();"],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM \"references\"", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_schema_files_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO files (path, language, hash, last_indexed, line_count, symbols_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["src/main.rs", "rust", "abc123", 1700000000, 100, 5],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_schema_daemon_status_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO daemon_status (key, value, updated_at) VALUES (?1, ?2, ?3)",
            rusqlite::params!["pid", "12345", 1700000000],
        )
        .unwrap();

        let val: String = conn
            .query_row(
                "SELECT value FROM daemon_status WHERE key = 'pid'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(val, "12345");
    }

    #[test]
    fn test_index_path_for_local() {
        let repo = Path::new("/home/user/repo");
        let path = index_path_for(repo, true).unwrap();
        assert_eq!(path, PathBuf::from("/home/user/repo/.wonk/index.db"));
    }

    #[test]
    fn test_index_path_for_central() {
        let repo = Path::new("/home/user/repo");
        let path = index_path_for(repo, false).unwrap();
        assert!(path.to_string_lossy().contains(".wonk/repos/"));
        assert!(path.to_string_lossy().ends_with("index.db"));
    }

    #[test]
    fn test_find_existing_index_none() {
        let dir = TempDir::new().unwrap();
        assert!(find_existing_index(dir.path()).is_none());
    }

    #[test]
    fn test_find_existing_index_local() {
        let dir = TempDir::new().unwrap();
        let wonk = dir.path().join(".wonk");
        fs::create_dir(&wonk).unwrap();
        let db_path = wonk.join("index.db");
        fs::write(&db_path, b"fake").unwrap();
        assert_eq!(find_existing_index(dir.path()), Some(db_path));
    }

    #[test]
    fn test_find_existing_index_central() {
        let dir = TempDir::new().unwrap();
        // Create a central index path and ensure it exists.
        let central = central_index_path(dir.path()).unwrap();
        fs::create_dir_all(central.parent().unwrap()).unwrap();
        fs::write(&central, b"fake").unwrap();
        assert_eq!(find_existing_index(dir.path()), Some(central));
    }

    #[test]
    fn test_find_existing_index_prefers_local() {
        let dir = TempDir::new().unwrap();
        // Create both local and central.
        let wonk = dir.path().join(".wonk");
        fs::create_dir(&wonk).unwrap();
        let local = wonk.join("index.db");
        fs::write(&local, b"local").unwrap();
        let central = central_index_path(dir.path()).unwrap();
        fs::create_dir_all(central.parent().unwrap()).unwrap();
        fs::write(&central, b"central").unwrap();
        // Local should win.
        assert_eq!(find_existing_index(dir.path()), Some(local));
    }

    // -- count_matching_symbols tests ----------------------------------------

    #[test]
    fn test_count_matching_symbols_found() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES ('processPayment', 'function', 'pay.rs', 1, 0, 'rust')",
            [],
        ).unwrap();
        assert_eq!(count_matching_symbols(&conn, "processPayment"), 1);
    }

    #[test]
    fn test_count_matching_symbols_none() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        assert_eq!(count_matching_symbols(&conn, "processPayment"), 0);
    }

    #[test]
    fn test_count_matching_symbols_multiple() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES ('process', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES ('process', 'function', 'b.rs', 5, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES ('process', 'method', 'c.rs', 10, 0, 'rust')",
            [],
        ).unwrap();
        assert_eq!(count_matching_symbols(&conn, "process"), 3);
    }

    #[test]
    fn test_count_matching_symbols_special_chars() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        // "connection refused" is not a valid symbol name; should return 0.
        assert_eq!(count_matching_symbols(&conn, "connection refused"), 0);
    }

    #[test]
    fn test_count_matching_symbols_fts_syntax_safe() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        // Patterns with FTS5-special chars should not panic or error.
        assert_eq!(count_matching_symbols(&conn, ""), 0);
        assert_eq!(count_matching_symbols(&conn, "foo OR bar"), 0);
        assert_eq!(count_matching_symbols(&conn, "foo*"), 0);
        assert_eq!(count_matching_symbols(&conn, "\"quoted\""), 0);
    }

    // -- Embeddings table tests ---------------------------------------------

    #[test]
    fn test_open_creates_embeddings_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(tables.contains(&"embeddings".to_string()));
    }

    #[test]
    fn test_embeddings_table_columns() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Insert a symbol first (FK target).
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'foo', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();

        // Insert an embedding with all columns.
        conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, stale, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![1, "a.rs", "fn foo() {}", vec![0u8; 16], 0, 1700000000i64],
        ).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_embeddings_unique_symbol_id() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'foo', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, created_at) VALUES (1, 'a.rs', 'text1', X'00', 1000)",
            [],
        ).unwrap();

        // Second insert with same symbol_id should fail (UNIQUE constraint).
        let result = conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, created_at) VALUES (1, 'a.rs', 'text2', X'00', 2000)",
            [],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_embeddings_cascade_delete() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'bar', 'function', 'b.rs', 1, 0, 'rust')",
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, created_at) VALUES (1, 'b.rs', 'fn bar()', X'AABB', 1000)",
            [],
        ).unwrap();

        // Verify embedding exists.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // Delete the symbol -- cascade should remove embedding.
        conn.execute("DELETE FROM symbols WHERE id = 1", [])
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_embeddings_stale_default() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'baz', 'function', 'c.rs', 1, 0, 'rust')",
            [],
        ).unwrap();

        // Insert without specifying stale -- should default to 0.
        conn.execute(
            "INSERT INTO embeddings (symbol_id, file, chunk_text, vector, created_at) VALUES (1, 'c.rs', 'text', X'00', 1000)",
            [],
        ).unwrap();

        let stale: i64 = conn
            .query_row(
                "SELECT stale FROM embeddings WHERE symbol_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale, 0);
    }

    #[test]
    fn test_embeddings_file_index_exists() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Check that the index exists.
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND name = 'idx_embeddings_file'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(indexes.len(), 1);
    }

    #[test]
    fn test_ensure_embeddings_table_on_v1_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate a V1 database: apply only base schema without embeddings.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute_batch(FTS_SQL).unwrap();
        conn.execute_batch(TRIGGERS_SQL).unwrap();

        // Embeddings table should NOT exist yet (V1 schema).
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='embeddings'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.is_empty());

        // Migrate: ensure_embeddings_table should create it.
        ensure_embeddings_table(&conn).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='embeddings'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tables.len(), 1);
    }

    #[test]
    fn test_ensure_embeddings_table_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Table already exists via open(). Calling ensure_embeddings_table
        // again should not fail.
        ensure_embeddings_table(&conn).unwrap();
        ensure_embeddings_table(&conn).unwrap();
    }

    #[test]
    fn test_pre_v5_embedding_rows_migrate_to_ollama_768() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute_batch(
            "CREATE TABLE embeddings (
                id INTEGER PRIMARY KEY,
                symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
                file TEXT NOT NULL,
                chunk_text TEXT NOT NULL,
                vector BLOB NOT NULL,
                stale INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                UNIQUE(symbol_id)
            );
            INSERT INTO symbols (id, name, kind, file, line, col, language)
                VALUES (1, 'main', 'function', 'src/main.rs', 1, 0, 'rust');
            INSERT INTO embeddings
                (symbol_id, file, chunk_text, vector, stale, created_at)
                VALUES (1, 'src/main.rs', 'fn main() {}', X'00000000', 0, 0);",
        )
        .unwrap();

        ensure_embeddings_table(&conn).unwrap();
        ensure_embeddings_table(&conn).unwrap();

        let metadata: (String, i64) = conn
            .query_row(
                "SELECT provider, dim FROM embeddings WHERE symbol_id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(metadata, ("ollama".to_string(), 768));
    }

    #[test]
    fn test_fresh_embeddings_schema_has_provider_metadata_defaults() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        ensure_embeddings_table(&conn).unwrap();
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language)
             VALUES (1, 'main', 'function', 'src/main.rs', 1, 0, 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO embeddings
                (symbol_id, file, chunk_text, vector, created_at)
             VALUES (1, 'src/main.rs', 'fn main() {}', X'00000000', 0)",
            [],
        )
        .unwrap();

        let metadata: (String, i64) = conn
            .query_row("SELECT provider, dim FROM embeddings", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(metadata, ("ollama".to_string(), 768));
    }

    // -- file_exists_in_index tests ------------------------------------------

    #[test]
    fn test_file_exists_in_index_found() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO files (path, language, hash, last_indexed) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["src/main.ts", "TypeScript", "abc123", 0],
        )
        .unwrap();

        assert!(file_exists_in_index(&conn, "src/main.ts").unwrap());
    }

    #[test]
    fn test_file_exists_in_index_not_found() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        assert!(!file_exists_in_index(&conn, "src/nonexistent.ts").unwrap());
    }

    // -- caller_id column tests -----------------------------------------------

    #[test]
    fn test_new_db_has_caller_id_column() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(columns.contains(&"caller_id".to_string()));
    }

    #[test]
    fn test_caller_id_index_exists() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_references_caller'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(indexes.len(), 1);
    }

    #[test]
    fn test_caller_id_nullable() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Insert without caller_id — should default to NULL.
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["foo", "a.rs", 1, 0, "foo()"],
        )
        .unwrap();

        let caller_id: Option<i64> = conn
            .query_row(
                "SELECT caller_id FROM \"references\" WHERE name = 'foo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(caller_id.is_none());
    }

    #[test]
    fn test_caller_id_with_valid_fk() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'main', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context, caller_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["foo", "a.rs", 5, 4, "foo()", 1i64],
        )
        .unwrap();

        let caller_id: Option<i64> = conn
            .query_row(
                "SELECT caller_id FROM \"references\" WHERE name = 'foo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(caller_id, Some(1));
    }

    #[test]
    fn test_caller_id_on_delete_set_null() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'main', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context, caller_id) VALUES ('foo', 'a.rs', 5, 4, 'foo()', 1)",
            [],
        )
        .unwrap();

        // Delete the symbol — FK should SET NULL.
        conn.execute("DELETE FROM symbols WHERE id = 1", [])
            .unwrap();

        let caller_id: Option<i64> = conn
            .query_row(
                "SELECT caller_id FROM \"references\" WHERE name = 'foo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(caller_id.is_none());
    }

    #[test]
    fn test_ensure_caller_id_migration_on_v2_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate a V2 database: base schema + embeddings but no caller_id.
        // Use the old schema SQL without caller_id.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS symbols (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                file TEXT NOT NULL,
                line INTEGER NOT NULL,
                col INTEGER NOT NULL,
                end_line INTEGER,
                scope TEXT,
                signature TEXT,
                language TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS "references" (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                file TEXT NOT NULL,
                line INTEGER NOT NULL,
                col INTEGER NOT NULL,
                context TEXT
            );
            "#,
        )
        .unwrap();

        // caller_id should NOT exist yet.
        let has_caller_id: bool = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .any(|name| name == "caller_id");
        assert!(!has_caller_id);

        // Run migration.
        ensure_caller_id_column(&conn).unwrap();

        let has_caller_id: bool = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .any(|name| name == "caller_id");
        assert!(has_caller_id);
    }

    #[test]
    fn test_ensure_caller_id_column_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Column already exists via open(). Calling again should not fail.
        ensure_caller_id_column(&conn).unwrap();
        ensure_caller_id_column(&conn).unwrap();
    }

    // -- Summaries table tests ------------------------------------------------

    #[test]
    fn test_open_creates_summaries_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='summaries'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tables.len(), 1);
    }

    #[test]
    fn test_summaries_insert_and_query() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO summaries (path, content_hash, description, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["src/", "abc123", "This module handles routing.", 1700000000i64],
        ).unwrap();

        let desc: String = conn
            .query_row(
                "SELECT description FROM summaries WHERE path = ?1 AND content_hash = ?2",
                rusqlite::params!["src/", "abc123"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(desc, "This module handles routing.");
    }

    #[test]
    fn test_summaries_upsert_on_path() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT OR REPLACE INTO summaries (path, content_hash, description, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["src/", "hash1", "Old description.", 1000],
        ).unwrap();

        conn.execute(
            "INSERT OR REPLACE INTO summaries (path, content_hash, description, created_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params!["src/", "hash2", "New description.", 2000],
        ).unwrap();

        // Should have only one row (path is PRIMARY KEY).
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM summaries", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let desc: String = conn
            .query_row(
                "SELECT description FROM summaries WHERE path = 'src/'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(desc, "New description.");
    }

    #[test]
    fn test_ensure_summaries_table_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        ensure_summaries_table(&conn).unwrap();
        ensure_summaries_table(&conn).unwrap();
    }

    #[test]
    fn test_ensure_summaries_table_on_old_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate old database without summaries.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();

        // No summaries table yet.
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='summaries'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.is_empty());

        // Migrate.
        ensure_summaries_table(&conn).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='summaries'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tables.len(), 1);
    }

    // -- history tables (TASK-096) -------------------------------------------

    fn history_table_names(conn: &Connection) -> Vec<String> {
        let names = "('file_churn','mined_commits','commit_files','history_meta','co_change')";
        conn.prepare(&format!(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN {names}"
        ))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    #[test]
    fn test_open_creates_history_tables() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        let mut tables = history_table_names(&conn);
        tables.sort();
        assert_eq!(
            tables,
            vec![
                "co_change",
                "commit_files",
                "file_churn",
                "history_meta",
                "mined_commits"
            ]
        );
    }

    #[test]
    fn test_ensure_history_tables_on_pre096_db_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // A pre-TASK-096 index: base schema only, no history tables.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        assert!(history_table_names(&conn).is_empty());

        // Migrate, then migrate again — CREATE IF NOT EXISTS is idempotent
        // (the wrappers this test used were redundant with apply_schema,
        // which every open() runs; dead-code sweep 2026-10-02).
        apply_schema(&conn).unwrap();
        apply_schema(&conn).unwrap();
        assert_eq!(history_table_names(&conn).len(), 5);
    }

    /// Column names of `table`, in declaration order.
    fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
        conn.prepare(&format!("PRAGMA table_info({table})"))
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect()
    }

    #[test]
    fn test_history_columns_migrate_on_pre105_index_with_null_backfill() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // A pre-TASK-105 index: the history tables in their old shape,
        // seeded so the backfill is observable.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(
            "CREATE TABLE file_churn (file TEXT PRIMARY KEY, score REAL NOT NULL);
             CREATE TABLE mined_commits (
                commit_id TEXT PRIMARY KEY, commit_ts INTEGER NOT NULL);
             INSERT INTO file_churn VALUES ('a.rs', 1.0);",
        )
        .unwrap();

        // Migrate, then migrate again — the ALTERs are guarded by PRAGMA.
        apply_schema(&conn).unwrap();
        apply_schema(&conn).unwrap();

        assert!(table_columns(&conn, "file_churn").contains(&"last_ts".to_string()));
        assert!(table_columns(&conn, "file_churn").contains(&"last_author".to_string()));
        assert!(table_columns(&conn, "file_churn").contains(&"primary_author".to_string()));
        assert!(table_columns(&conn, "mined_commits").contains(&"author".to_string()));

        let backfill: (Option<i64>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT last_ts, last_author, primary_author FROM file_churn WHERE file = 'a.rs'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(backfill, (None, None, None), "old rows stay NULL-filled");
    }

    // -- topology tables (TASK-098) -------------------------------------------

    fn topology_table_names(conn: &Connection) -> Vec<String> {
        let names = "('symbol_topology','topology_meta')";
        conn.prepare(&format!(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN {names}"
        ))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    #[test]
    fn test_open_creates_topology_tables() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        let mut tables = topology_table_names(&conn);
        tables.sort();
        assert_eq!(tables, vec!["symbol_topology", "topology_meta"]);
        // The community-membership index rides along with the table; the
        // column it covers stays NULL-filled until TASK-099 owns it.
        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_topology_community'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(indexes, vec!["idx_topology_community"]);
    }

    #[test]
    fn test_ensure_topology_tables_on_pre098_db_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // A pre-TASK-098 index: base schema only, no topology tables.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        assert!(topology_table_names(&conn).is_empty());

        // Migrate, then migrate again — CREATE IF NOT EXISTS is idempotent.
        apply_schema(&conn).unwrap();
        apply_schema(&conn).unwrap();
        assert_eq!(topology_table_names(&conn).len(), 2);
    }

    // -- duplicate tables (TASK-100) -------------------------------------------

    fn duplicate_table_names(conn: &Connection) -> Vec<String> {
        let names = "('symbol_shingles','near_duplicates')";
        conn.prepare(&format!(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN {names}"
        ))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    #[test]
    fn test_open_creates_duplicate_tables() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        let mut tables = duplicate_table_names(&conn);
        tables.sort();
        assert_eq!(tables, vec!["near_duplicates", "symbol_shingles"]);
        // The reverse-lookup index on the pair table's second column.
        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name='idx_near_duplicates_b'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(indexes, vec!["idx_near_duplicates_b"]);
    }

    #[test]
    fn test_ensure_duplicate_tables_on_pre100_db_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // A pre-TASK-100 index: base schema only, no duplicate tables.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        assert!(duplicate_table_names(&conn).is_empty());

        ensure_duplicate_tables(&conn).unwrap();
        ensure_duplicate_tables(&conn).unwrap();
        assert_eq!(duplicate_table_names(&conn).len(), 2);
    }

    // -- feedback tables (TASK-101) --------------------------------------------

    fn feedback_table_names(conn: &Connection) -> Vec<String> {
        let names = "('feedback_events','feedback_slates','learned_weights',\
                     'learned_weight_sessions','learned_meta',\
                     'result_preferences','result_preference_sessions')";
        conn.prepare(&format!(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN {names}"
        ))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    fn feedback_index_names(conn: &Connection) -> Vec<String> {
        let names = "('idx_feedback_identity','idx_feedback_created')";
        conn.prepare(&format!(
            "SELECT name FROM sqlite_master WHERE type='index' AND name IN {names}"
        ))
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
    }

    #[test]
    fn test_open_creates_feedback_tables() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        let mut tables = feedback_table_names(&conn);
        tables.sort();
        assert_eq!(
            tables,
            vec![
                "feedback_events",
                "feedback_slates",
                "learned_meta",
                "learned_weight_sessions",
                "learned_weights",
                "result_preference_sessions",
                "result_preferences"
            ]
        );
        let mut indexes = feedback_index_names(&conn);
        indexes.sort();
        assert_eq!(
            indexes,
            vec!["idx_feedback_created", "idx_feedback_identity"]
        );
    }

    #[test]
    fn test_learned_weights_columns_match_architecture_ddl() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();
        assert_eq!(
            table_columns(&conn, "learned_weights"),
            vec![
                "feature",
                "query_class",
                "weight",
                "observations",
                "sessions",
                "updated_at"
            ]
        );
        assert_eq!(
            table_columns(&conn, "learned_weight_sessions"),
            vec!["feature", "query_class", "session"]
        );
        assert_eq!(table_columns(&conn, "learned_meta"), vec!["key", "value"]);
        // The composite primary key holds: a duplicate (feature, class)
        // insert is a constraint violation, not a second row.
        conn.execute(
            "INSERT INTO learned_weights \
             (feature, query_class, weight, observations, sessions, updated_at) \
             VALUES ('path_character', '', 0.6, 1, 1, 0)",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO learned_weights \
                 (feature, query_class, weight, observations, sessions, updated_at) \
                 VALUES ('path_character', '', 0.7, 1, 1, 0)",
                [],
            )
            .is_err()
        );
        assert!(
            conn.execute(
                "INSERT INTO learned_weights \
                 (feature, query_class, weight, observations, sessions, updated_at) \
                 VALUES ('path_character', 'symbol', 0.7, 1, 1, 0)",
                [],
            )
            .is_ok()
        );
    }

    #[test]
    fn test_result_preferences_columns_match_architecture_ddl() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();
        assert_eq!(
            table_columns(&conn, "result_preferences"),
            vec![
                "result_identity",
                "strength",
                "observations",
                "sessions",
                "updated_at"
            ]
        );
        assert_eq!(
            table_columns(&conn, "result_preference_sessions"),
            vec!["result_identity", "session"]
        );
        // The identity primary key holds: a duplicate identity insert is a
        // constraint violation, not a second row; a second session for the
        // same identity is a distinct bookkeeping row.
        conn.execute(
            "INSERT INTO result_preferences \
             (result_identity, strength, observations, sessions, updated_at) \
             VALUES ('abc', 0.1, 1, 1, 0)",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO result_preferences \
                 (result_identity, strength, observations, sessions, updated_at) \
                 VALUES ('abc', 0.2, 2, 2, 0)",
                [],
            )
            .is_err()
        );
        conn.execute(
            "INSERT INTO result_preference_sessions (result_identity, session) \
             VALUES ('abc', 's1')",
            [],
        )
        .unwrap();
        assert!(
            conn.execute(
                "INSERT INTO result_preference_sessions (result_identity, session) \
                 VALUES ('abc', 's1')",
                [],
            )
            .is_err(),
            "the (identity, session) pair is the bookkeeping key"
        );
    }

    #[test]
    fn test_ensure_feedback_tables_on_pre102_db_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // A pre-TASK-102 index: the TASK-101 tables exist, the learner's
        // three do not (nor do TASK-104's preference tables) — the shape a
        // TASK-105-era index presents.
        let conn = open(&db_path).unwrap();
        conn.execute_batch(
            "DROP TABLE learned_weights;
             DROP TABLE learned_weight_sessions;
             DROP TABLE learned_meta;
             DROP TABLE result_preferences;
             DROP TABLE result_preference_sessions;",
        )
        .unwrap();
        assert_eq!(feedback_table_names(&conn).len(), 2);

        // Migrate, then migrate again — CREATE IF NOT EXISTS is idempotent.
        ensure_feedback_tables(&conn).unwrap();
        ensure_feedback_tables(&conn).unwrap();
        assert_eq!(feedback_table_names(&conn).len(), 7);
        assert_eq!(feedback_index_names(&conn).len(), 2);
    }

    #[test]
    fn test_ensure_feedback_tables_on_pre101_db_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // A pre-TASK-101 index: full schema applied, then every feedback
        // table dropped — the shape an old index presents after upgrade.
        let conn = open(&db_path).unwrap();
        conn.execute_batch(
            "DROP TABLE feedback_events;
             DROP TABLE feedback_slates;
             DROP TABLE learned_weights;
             DROP TABLE learned_weight_sessions;
             DROP TABLE learned_meta;
             DROP TABLE result_preferences;
             DROP TABLE result_preference_sessions;",
        )
        .unwrap();
        assert!(feedback_table_names(&conn).is_empty());

        // Migrate, then migrate again — CREATE IF NOT EXISTS is idempotent.
        ensure_feedback_tables(&conn).unwrap();
        ensure_feedback_tables(&conn).unwrap();
        assert_eq!(feedback_table_names(&conn).len(), 7);
        assert_eq!(feedback_index_names(&conn).len(), 2);
    }

    #[test]
    fn test_shingles_cascade_on_symbol_delete() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'f', 'function', 'a.rs', 1, 0, 'rust')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbol_shingles (symbol_id, signature) VALUES (1, x'01020304')",
            [],
        )
        .unwrap();

        // The re-index path deletes a file's symbols wholesale; the
        // signature row must follow (no new DELETE statements anywhere).
        conn.execute("DELETE FROM symbols WHERE file = 'a.rs'", [])
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbol_shingles", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_near_duplicates_cascade_both_sides() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        for id in 1..=2 {
            conn.execute(
                "INSERT INTO symbols (id, name, kind, file, line, col, language) \
                 VALUES (?1, 'f', 'function', 'a.rs', ?1, 0, 'rust')",
                rusqlite::params![id],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO near_duplicates (symbol_id_a, symbol_id_b, similarity) VALUES (1, 2, 0.9)",
            [],
        )
        .unwrap();

        // Deleting either endpoint removes the pair row.
        conn.execute("DELETE FROM symbols WHERE id = 1", [])
            .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM near_duplicates", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        conn.execute(
            "INSERT INTO near_duplicates (symbol_id_a, symbol_id_b, similarity) VALUES (2, 2, 1.0)",
            [],
        )
        .unwrap();
        conn.execute("DELETE FROM symbols WHERE id = 2", [])
            .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM near_duplicates", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    // -- confidence column tests -----------------------------------------------

    #[test]
    fn test_new_db_has_confidence_column() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            columns.contains(&"confidence".to_string()),
            "references table should have confidence column"
        );
    }

    #[test]
    fn test_confidence_default_is_0_5() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Insert without specifying confidence -- should default to 0.5.
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["foo", "a.rs", 1, 0, "foo()"],
        )
        .unwrap();

        let confidence: f64 = conn
            .query_row(
                "SELECT confidence FROM \"references\" WHERE name = 'foo'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!((confidence - 0.5).abs() < 1e-9);
    }

    #[test]
    fn test_confidence_indexes_exist() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_references_%confidence%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(indexes.contains(&"idx_references_name_confidence".to_string()));
        assert!(indexes.contains(&"idx_references_caller_confidence".to_string()));
    }

    #[test]
    fn test_ensure_confidence_column_migration() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate a pre-V4 database without confidence column.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS symbols (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                file TEXT NOT NULL,
                line INTEGER NOT NULL,
                col INTEGER NOT NULL,
                end_line INTEGER,
                scope TEXT,
                signature TEXT,
                language TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS "references" (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                file TEXT NOT NULL,
                line INTEGER NOT NULL,
                col INTEGER NOT NULL,
                context TEXT,
                caller_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL
            );
            "#,
        )
        .unwrap();

        // confidence should NOT exist yet.
        let has_confidence: bool = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .any(|name| name == "confidence");
        assert!(!has_confidence);

        // Run migration.
        ensure_confidence_column(&conn).unwrap();

        let has_confidence: bool = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .any(|name| name == "confidence");
        assert!(has_confidence);
    }

    #[test]
    fn test_ensure_confidence_column_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        ensure_confidence_column(&conn).unwrap();
        ensure_confidence_column(&conn).unwrap();
    }

    // -- type_edges table tests ------------------------------------------------

    #[test]
    fn test_new_db_has_type_edges_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='type_edges'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tables.len(), 1);
    }

    #[test]
    fn test_type_edges_insert_and_query() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Create parent and child symbols.
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'Animal', 'class', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (2, 'Dog', 'class', 'a.rs', 10, 0, 'rust')",
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO type_edges (child_id, parent_id, relationship) VALUES (?1, ?2, ?3)",
            rusqlite::params![2, 1, "extends"],
        )
        .unwrap();

        let rel: String = conn
            .query_row(
                "SELECT relationship FROM type_edges WHERE child_id = 2 AND parent_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rel, "extends");
    }

    #[test]
    fn test_type_edges_unique_constraint() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'A', 'class', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (2, 'B', 'class', 'a.rs', 10, 0, 'rust')",
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO type_edges (child_id, parent_id, relationship) VALUES (2, 1, 'extends')",
            [],
        )
        .unwrap();

        // Duplicate should fail.
        let result = conn.execute(
            "INSERT INTO type_edges (child_id, parent_id, relationship) VALUES (2, 1, 'extends')",
            [],
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_type_edges_cascade_delete() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (1, 'A', 'class', 'a.rs', 1, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, language) VALUES (2, 'B', 'class', 'a.rs', 10, 0, 'rust')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO type_edges (child_id, parent_id, relationship) VALUES (2, 1, 'extends')",
            [],
        )
        .unwrap();

        // Delete child symbol -- cascade should remove edge.
        conn.execute("DELETE FROM symbols WHERE id = 2", [])
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM type_edges", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_type_edges_bidirectional_indexes() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_type_edges_%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(indexes.contains(&"idx_type_edges_child".to_string()));
        assert!(indexes.contains(&"idx_type_edges_parent".to_string()));
    }

    #[test]
    fn test_ensure_type_edges_table_migration() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate old database without type_edges.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();

        // No type_edges table yet.
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='type_edges'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.is_empty());

        // Migrate.
        ensure_type_edges_table(&conn).unwrap();

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='type_edges'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tables.len(), 1);
    }

    #[test]
    fn test_open_creates_term_stats_table() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let _conn = open(&db_path).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='term_stats'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(tables.len(), 1, "open() should create term_stats");
    }

    #[test]
    fn open_creates_contracts_table_and_indexes() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='contracts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "open() should create the contracts table");

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_contracts%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        for expected in [
            "idx_contracts_canonical",
            "idx_contracts_symbol",
            "idx_contracts_file",
        ] {
            assert!(
                indexes.contains(&expected.to_string()),
                "{expected} should exist (got {indexes:?})"
            );
        }
    }

    #[test]
    fn ensure_contracts_table_migrates_pre_v5_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // A pre-V5 index has the base schema but no contracts table.
        {
            let conn = Connection::open(&db_path).unwrap();
            apply_pragmas(&conn).unwrap();
            conn.execute_batch(SCHEMA_SQL).unwrap();
        }

        let conn = open(&db_path).unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='contracts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            exists, 1,
            "open() should migrate the empty contracts table in"
        );

        // Idempotent: a second schema pass on a migrated DB succeeds.
        apply_schema(&conn).unwrap();
        apply_schema(&conn).unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='contracts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
    }

    #[test]
    fn ensure_review_suppressions_table_migrates_existing_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // A pre-TASK-089 index has the base schema but no suppressions
        // table.
        {
            let conn = Connection::open(&db_path).unwrap();
            apply_pragmas(&conn).unwrap();
            conn.execute_batch(SCHEMA_SQL).unwrap();
        }

        let conn = open(&db_path).unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='review_suppressions'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            exists, 1,
            "open() should migrate the empty review_suppressions table in"
        );

        // Idempotent: a second run on a migrated DB succeeds.
        ensure_review_suppressions_table(&conn).unwrap();
        ensure_review_suppressions_table(&conn).unwrap();
    }

    #[test]
    fn test_ensure_term_stats_table_migration() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate an old database created before term_stats existed.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='term_stats'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 0, "table should not exist before migration");

        ensure_term_stats_table(&conn).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='term_stats'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "ensure_term_stats_table should create it");
    }

    #[test]
    fn test_ensure_term_stats_table_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        // Table already exists via open(); calling ensure again must not fail.
        ensure_term_stats_table(&conn).unwrap();
        ensure_term_stats_table(&conn).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='term_stats'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
    }

    #[test]
    fn test_term_stats_pk_rejects_duplicate_term_file() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        conn.execute(
            "INSERT INTO term_stats (term, file, tf) VALUES (?1, ?2, ?3)",
            rusqlite::params!["hello", "src/main.rs", 2],
        )
        .unwrap();

        let dup = conn.execute(
            "INSERT INTO term_stats (term, file, tf) VALUES (?1, ?2, ?3)",
            rusqlite::params!["hello", "src/main.rs", 5],
        );
        assert!(
            dup.is_err(),
            "duplicate (term, file) pair must violate the primary key"
        );

        // A different file for the same term is fine.
        conn.execute(
            "INSERT INTO term_stats (term, file, tf) VALUES (?1, ?2, ?3)",
            rusqlite::params!["hello", "lib.rs", 1],
        )
        .unwrap();
    }

    #[test]
    fn test_term_stats_file_index_exists() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            indexes.contains(&"idx_term_stats_file".to_string()),
            "idx_term_stats_file should exist for the delete-by-file path"
        );
    }

    #[test]
    fn test_term_stats_insert_and_df_query() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        for (term, file, tf) in [
            ("hello", "a.rs", 3),
            ("hello", "b.rs", 1),
            ("world", "a.rs", 2),
        ] {
            conn.execute(
                "INSERT INTO term_stats (term, file, tf) VALUES (?1, ?2, ?3)",
                rusqlite::params![term, file, tf],
            )
            .unwrap();
        }

        // Document frequency for "hello" is the number of files containing it.
        let df_hello: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM term_stats WHERE term = ?1",
                rusqlite::params!["hello"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(df_hello, 2);

        // Postings for "hello" carry per-file tf.
        let postings: Vec<(String, i64)> = conn
            .prepare("SELECT file, tf FROM term_stats WHERE term = ?1 ORDER BY file")
            .unwrap()
            .query_map(rusqlite::params!["hello"], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(
            postings,
            vec![("a.rs".to_string(), 3), ("b.rs".to_string(), 1)]
        );
    }

    #[test]
    fn test_open_creates_reach_tables() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let _conn = open(&db_path).unwrap();

        let conn = Connection::open(&db_path).unwrap();
        for table in ["reach", "reach_truncated", "reach_meta"] {
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name = ?1",
                    rusqlite::params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "open() should create {table}");
        }
    }

    #[test]
    fn test_ensure_reach_table_migration() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Simulate an old database created before the reach tables existed.
        let conn = Connection::open(&db_path).unwrap();
        apply_pragmas(&conn).unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='reach'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 0, "table should not exist before migration");

        ensure_reach_table(&conn).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='reach'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "ensure_reach_table should create it");
    }

    #[test]
    fn test_ensure_reach_table_idempotent() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        ensure_reach_table(&conn).unwrap();
        ensure_reach_table(&conn).unwrap();

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='reach'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1);
    }

    #[test]
    fn test_reach_pk_rejects_duplicate_source_target() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["a", "function", "a.rs", 1, 1, "rust"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["b", "function", "a.rs", 5, 1, "rust"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO reach (source_id, target_id, min_depth, confidence) VALUES (1, 2, 1, 0.85)",
            [],
        )
        .unwrap();

        let dup = conn.execute(
            "INSERT INTO reach (source_id, target_id, min_depth, confidence) VALUES (1, 2, 2, 0.9)",
            [],
        );
        assert!(
            dup.is_err(),
            "duplicate (source_id, target_id) pair must violate the primary key"
        );
    }

    #[test]
    fn test_reach_indexes_exist() {
        let dir = TempDir::new().unwrap();
        let conn = open(&dir.path().join("index.db")).unwrap();

        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='index'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            indexes.contains(&"idx_reach_source_depth".to_string()),
            "idx_reach_source_depth should exist for the lookup path"
        );
        assert!(
            indexes.contains(&"idx_reach_target".to_string()),
            "idx_reach_target should exist for the reverse lookup path (TASK-081)"
        );
    }

    #[test]
    fn test_open_migrates_legacy_db_without_caller_id_or_confidence() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");

        // Create a legacy database without caller_id or confidence columns.
        {
            let conn = Connection::open(&db_path).unwrap();
            apply_pragmas(&conn).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS symbols (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    file TEXT NOT NULL,
                    line INTEGER NOT NULL,
                    col INTEGER NOT NULL,
                    end_line INTEGER,
                    scope TEXT,
                    signature TEXT,
                    language TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS "references" (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    file TEXT NOT NULL,
                    line INTEGER NOT NULL,
                    col INTEGER NOT NULL,
                    context TEXT
                );
                CREATE TABLE IF NOT EXISTS files (
                    path TEXT PRIMARY KEY,
                    language TEXT,
                    hash TEXT NOT NULL,
                    last_indexed INTEGER NOT NULL,
                    line_count INTEGER,
                    symbols_count INTEGER
                );
                CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
                CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file);
                CREATE INDEX IF NOT EXISTS idx_symbols_kind ON symbols(kind);
                CREATE INDEX IF NOT EXISTS idx_references_name ON "references"(name);
                CREATE INDEX IF NOT EXISTS idx_references_file ON "references"(file);
                "#,
            )
            .unwrap();
        }

        // open() should succeed and migrate the schema.
        let conn = open(&db_path).unwrap();

        // Verify all migrated columns exist.
        let columns: Vec<String> = conn
            .prepare("PRAGMA table_info(\"references\")")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(columns.contains(&"caller_id".to_string()));
        assert!(columns.contains(&"confidence".to_string()));
        assert!(columns.contains(&"target_id".to_string()));

        // Verify all indexes exist.
        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_references_%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(indexes.contains(&"idx_references_caller".to_string()));
        assert!(indexes.contains(&"idx_references_name_confidence".to_string()));
        assert!(indexes.contains(&"idx_references_caller_confidence".to_string()));
        assert!(indexes.contains(&"idx_references_target_id".to_string()));
    }

    #[test]
    fn test_ensure_type_edges_table_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = open(&db_path).unwrap();

        ensure_type_edges_table(&conn).unwrap();
        ensure_type_edges_table(&conn).unwrap();
    }
}
