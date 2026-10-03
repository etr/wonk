mod common;
use rusqlite::Connection;
use std::collections::HashSet;
use wonk::{bm25, db, pipeline};

fn legacy_feedback(conn: &Connection, watermark: i64, rows: &[i64]) {
    conn.execute_batch("DROP TABLE feedback_events;
        CREATE TABLE feedback_events (id INTEGER PRIMARY KEY,result_identity TEXT NOT NULL,
        query_class TEXT,chosen_rank INTEGER NOT NULL,features TEXT NOT NULL,useful INTEGER NOT NULL,
        session TEXT,created_at INTEGER NOT NULL);
        CREATE INDEX custom_feedback_rank ON feedback_events(chosen_rank);").unwrap();
    conn.execute(
        "INSERT INTO learned_meta(key,value) VALUES ('event_watermark',?1)",
        [watermark.to_string()],
    )
    .unwrap();
    for id in rows {
        conn.execute(
            "INSERT INTO feedback_events VALUES (?1,'kept','symbol',2,'{}',1,'session',0)",
            [id],
        )
        .unwrap();
    }
}
fn next_event(conn: &Connection) -> i64 {
    conn.execute("INSERT INTO feedback_events(result_identity,chosen_rank,features,useful,created_at) VALUES ('next',2,'{}',1,0)", []).unwrap();
    conn.last_insert_rowid()
}
#[test]
fn feedback_migration_retains_rows_indexes_and_monotonic_ids_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let conn = db::open(&path).unwrap();
    legacy_feedback(&conn, 40, &[3, 51]);
    db::ensure_feedback_tables(&conn).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM feedback_events WHERE result_identity='kept'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='custom_feedback_rank'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    conn.execute("DELETE FROM feedback_events WHERE id=51", [])
        .unwrap();
    assert!(next_event(&conn) > 51, "deleted tail cannot be reused");
    conn.execute("DELETE FROM feedback_events", []).unwrap();
    drop(conn);
    let conn = db::open(&path).unwrap();
    assert!(
        next_event(&conn) > 52,
        "cleared events cannot reset sequence on reopen"
    );
}
#[test]
fn feedback_migration_cleared_legacy_events_start_after_learning_watermark() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db::open(&dir.path().join("index.db")).unwrap();
    legacy_feedback(&conn, 700, &[]);
    db::ensure_feedback_tables(&conn).unwrap();
    assert!(
        next_event(&conn) > 700,
        "new feedback must remain beyond learned cursor"
    );
}
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "fn caller() { alpha(); }\n").unwrap();
    dir
}
fn scores(conn: &Connection) -> Option<std::collections::HashMap<String, f32>> {
    bm25::file_bm25_scores(
        conn,
        &HashSet::from(["a.rs".to_owned(), "b.rs".to_owned()]),
        "alpha",
        bm25::Bm25Params { k1: 1.2, b: 0.75 },
    )
}
#[test]
fn bm25_upgrade_single_edit_keeps_fallback_until_complete_update() {
    let dir = repo();
    common::build_index(dir.path(), true).unwrap();
    let conn = db::open(&db::local_index_path(dir.path())).unwrap();
    conn.execute("DELETE FROM term_stats", []).unwrap();
    // Emulate an old index without any generation metadata.
    conn.execute_batch("DROP TABLE IF EXISTS bm25_meta")
        .unwrap();
    let conn = db::open(&db::local_index_path(dir.path())).unwrap();
    assert!(scores(&conn).is_none());
    std::fs::write(dir.path().join("a.rs"), "fn alpha() { alpha(); }\n").unwrap();
    pipeline::reindex_file(
        &conn,
        &dir.path().join("a.rs"),
        dir.path(),
        &Default::default(),
    )
    .unwrap();
    assert!(
        scores(&conn).is_none(),
        "one edited file is not a completed corpus"
    );
    common::incremental_update(dir.path(), true).unwrap();
    assert!(
        scores(&conn).unwrap()["b.rs"] > 0.0,
        "complete update includes unchanged files"
    );
}
#[test]
fn full_rebuild_term_failure_keeps_previous_complete_generation() {
    let dir = repo();
    common::build_index(dir.path(), true).unwrap();
    let conn = db::open(&db::local_index_path(dir.path())).unwrap();
    let before = scores(&conn).unwrap();
    let meta = std::fs::read(dir.path().join(".wonk/meta.json")).unwrap();
    conn.execute_batch("CREATE TRIGGER full_stats_boom BEFORE INSERT ON term_stats BEGIN SELECT RAISE(ABORT,'stats fault'); END").unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn replacement() {}\n").unwrap();
    assert!(common::build_index(dir.path(), true).is_err());
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM symbols WHERE name='alpha'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1,
        "failed full publication retains old symbols"
    );
    assert_eq!(scores(&conn), Some(before));
    assert!(
        wonk::reach::lookup_upstream(&conn, "alpha", 3)
            .unwrap()
            .is_some()
    );
    assert_eq!(
        std::fs::read(dir.path().join(".wonk/meta.json")).unwrap(),
        meta
    );
}
thread_local! { static POSTINGS_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
fn count_postings(event: rusqlite::trace::TraceEvent<'_>) {
    if let rusqlite::trace::TraceEvent::Row(stmt) = event
        && stmt.sql().starts_with("SELECT file, tf FROM term_stats")
    {
        POSTINGS_READ.with(|n| n.set(n.get() + 1));
    }
}
#[test]
fn bm25_materializes_only_candidate_postings_and_preserves_global_df() {
    let dir = repo();
    common::build_index(dir.path(), true).unwrap();
    let conn = db::open(&db::local_index_path(dir.path())).unwrap();
    for n in 0..2000 {
        let file = format!("uncandidate-{n}.rs");
        conn.execute(
            "INSERT INTO files(path,hash,last_indexed,line_count) VALUES (?1,'h',0,1)",
            [&file],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO term_stats(term,file,tf) VALUES ('alpha',?1,3)",
            [&file],
        )
        .unwrap();
    }
    conn.execute("DELETE FROM corpus_stats", []).unwrap();
    POSTINGS_READ.with(|n| n.set(0));
    conn.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_ROW,
        Some(count_postings),
    );
    let candidates = HashSet::from(["a.rs".to_owned()]);
    let actual = bm25::file_bm25_scores(
        &conn,
        &candidates,
        "alpha alpha",
        bm25::Bm25Params { k1: 1.2, b: 0.75 },
    )
    .unwrap();
    let expected = bm25::term_contribution(1, 1.0, 1.0, bm25::idf(2002, 2002), 1.2, 0.75);
    assert_eq!(
        actual["a.rs"].to_bits(),
        expected.to_bits(),
        "global DF and deduplicated query terms preserved"
    );
    assert_eq!(
        POSTINGS_READ.with(|n| n.get()),
        1,
        "noncandidate posting strings must not cross SQLite boundary"
    );
}
#[test]
fn feedback_migration_index_fault_rolls_back_table_swap_and_rows() {
    let dir = tempfile::tempdir().unwrap();
    let conn = db::open(&dir.path().join("index.db")).unwrap();
    legacy_feedback(&conn, 80, &[11, 45]);
    let padded_index = format!(
        "CREATE INDEX migration_fault_index ON feedback_events(/*{}*/ chosen_rank)",
        "x".repeat(20_000)
    );
    conn.execute_batch(&padded_index).unwrap();
    // Recreating this index fails after the copy/drop/rename under a
    // per-connection SQLite SQL-size limit, with no global failpoint.
    let previous = unsafe {
        rusqlite::ffi::sqlite3_limit(conn.handle(), rusqlite::ffi::SQLITE_LIMIT_SQL_LENGTH, 8192)
    };
    let result = db::ensure_feedback_tables(&conn);
    unsafe {
        rusqlite::ffi::sqlite3_limit(
            conn.handle(),
            rusqlite::ffi::SQLITE_LIMIT_SQL_LENGTH,
            previous,
        );
    }
    assert!(
        result.is_err(),
        "injected late index recreation fault must surface"
    );
    let ddl: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='feedback_events'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !ddl.contains("AUTOINCREMENT"),
        "rollback restores original table schema"
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM feedback_events", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='migration_fault_index'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    db::ensure_feedback_tables(&conn).unwrap();
    assert!(next_event(&conn) > 80);
}
#[test]
fn index_public_build_rejects_invalid_work_budget_without_replacing_data() {
    if std::env::var_os("WONK_INDEX_BOUNDARY_CHILD").is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env("HOME", home.path())
            .env("WONK_INDEX_BOUNDARY_CHILD", "1")
            .args([
                "--exact",
                "index_public_build_rejects_invalid_work_budget_without_replacing_data",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let dir = repo();
    common::build_index(dir.path(), true).unwrap();
    let conn = db::open(&db::local_index_path(dir.path())).unwrap();
    std::fs::write(
        dir.path().join(".wonk/config.toml"),
        "[history]\nwindow = 10001\n",
    )
    .unwrap();
    assert!(
        pipeline::build_index(dir.path(), true).is_err(),
        "invalid configured work bounds must surface at public build"
    );
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM symbols WHERE name='alpha'", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert!(
        pipeline::incremental_update(dir.path(), true).is_err(),
        "authoritative update must surface invalid work bounds"
    );
}
fn explicit_build(root: &std::path::Path) {
    let mut config = wonk::config::Config::default();
    config.history.enabled = false;
    config.topology.enabled = false;
    pipeline::build_index_with_config(root, true, &config).unwrap();
}
#[test]
fn complete_bm25_generation_handles_zero_terms_empty_corpus_and_file_mutations() {
    let dir = repo();
    std::fs::remove_file(dir.path().join("a.rs")).unwrap();
    std::fs::remove_file(dir.path().join("b.rs")).unwrap();
    std::fs::write(dir.path().join("empty.rs"), "").unwrap();
    std::fs::write(dir.path().join("zero.rs"), "////\n").unwrap();
    explicit_build(dir.path());
    let conn = db::open(&db::local_index_path(dir.path())).unwrap();
    assert!(db::bm25_generation_ready(&conn));
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM term_stats", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let params = bm25::Bm25Params { k1: 1.2, b: 0.75 };
    let files = HashSet::from(["empty.rs".to_owned(), "zero.rs".to_owned()]);
    let zero = bm25::file_bm25_scores(&conn, &files, "alpha", params).unwrap();
    assert!(
        zero.values().all(|score| *score == 0.0),
        "complete zero-term corpus is scorable"
    );
    std::fs::rename(dir.path().join("zero.rs"), dir.path().join("renamed.rs")).unwrap();
    pipeline::remove_file(&conn, &dir.path().join("zero.rs"), dir.path()).unwrap();
    pipeline::index_new_file(
        &conn,
        &dir.path().join("renamed.rs"),
        dir.path(),
        &Default::default(),
    )
    .unwrap();
    assert!(db::bm25_generation_ready(&conn));
    std::fs::write(dir.path().join("renamed.rs"), "fn alpha() {}\n").unwrap();
    pipeline::reindex_file(
        &conn,
        &dir.path().join("renamed.rs"),
        dir.path(),
        &Default::default(),
    )
    .unwrap();
    let renamed = HashSet::from(["renamed.rs".to_owned()]);
    assert!(bm25::file_bm25_scores(&conn, &renamed, "alpha", params).unwrap()["renamed.rs"] > 0.0);
    for file in ["empty.rs", "renamed.rs"] {
        std::fs::remove_file(dir.path().join(file)).unwrap();
        pipeline::remove_file(&conn, &dir.path().join(file), dir.path()).unwrap();
    }
    assert!(db::bm25_generation_ready(&conn));
    explicit_build(dir.path());
    assert!(
        db::bm25_generation_ready(&conn),
        "empty full publication completes readiness too"
    );
    assert_eq!(
        bm25::file_bm25_scores(&conn, &HashSet::new(), "alpha", params),
        Some(Default::default())
    );
}
#[test]
fn full_publication_preserves_durable_feedback_and_suppression_and_invalidates_derived_state() {
    let dir = repo();
    explicit_build(dir.path());
    let conn = db::open(&db::local_index_path(dir.path())).unwrap();
    let event = next_event(&conn);
    conn.execute(
        "INSERT INTO review_suppressions(identity,created_at) VALUES ('keep',0)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO learned_meta(key,value) VALUES ('event_watermark',?1)",
        [event.to_string()],
    )
    .unwrap();
    conn.execute("INSERT INTO summaries(path,content_hash,description,created_at) VALUES ('a.rs','old','old summary',0)",[]).unwrap();
    conn.execute(
        "INSERT INTO history_meta(key,value) VALUES ('mined_head','old')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO topology_meta(key,value) VALUES ('last_computed','old')",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO embeddings(symbol_id,file,chunk_text,vector,created_at) SELECT id,file,'old',X'00000000',0 FROM symbols LIMIT 1",[]).unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn replacement() {}\n").unwrap();
    explicit_build(dir.path());
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM feedback_events WHERE id=?1",
            [event],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM review_suppressions WHERE identity='keep'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        conn.query_row(
            "SELECT value FROM learned_meta WHERE key='event_watermark'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        event.to_string()
    );
    for table in ["embeddings", "summaries", "history_meta", "topology_meta"] {
        assert_eq!(
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "{table} cannot describe the previous symbol generation"
        );
    }
}
#[test]
fn bm25_candidate_batches_cover_sqlite_limits_and_missing_lengths() {
    let dir = repo();
    explicit_build(dir.path());
    let conn = db::open(&db::local_index_path(dir.path())).unwrap();
    let mut candidates = HashSet::new();
    for n in 0..950 {
        let file = format!("candidate-{n}.rs");
        conn.execute(
            "INSERT INTO files(path,hash,last_indexed,line_count) VALUES (?1,'h',0,1)",
            [&file],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO term_stats(term,file,tf) VALUES ('alpha',?1,1)",
            [&file],
        )
        .unwrap();
        candidates.insert(file);
    }
    conn.execute(
        "UPDATE files SET line_count=NULL WHERE path='candidate-0.rs'",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM corpus_stats", []).unwrap();
    candidates.insert("missing.rs".to_owned());
    conn.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_ROW,
        Some(count_postings),
    );
    POSTINGS_READ.with(|n| n.set(0));
    let params = bm25::Bm25Params { k1: 1.2, b: 0.75 };
    let scores = bm25::file_bm25_scores(&conn, &candidates, "alpha alpha", params).unwrap();
    let expected = bm25::term_contribution(1, 1.0, 1.0, bm25::idf(952, 952), params.k1, params.b);
    for n in 0..950 {
        assert_eq!(
            scores[&format!("candidate-{n}.rs")].to_bits(),
            expected.to_bits()
        );
    }
    assert_eq!(scores["missing.rs"], 0.0);
    assert_eq!(
        POSTINGS_READ.with(|n| n.get()),
        950,
        "TF batches materialize each matching candidate exactly once"
    );
}

#[test]
fn feedback_schema_ensure_is_read_only_when_sequence_is_already_monotonic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let writer = db::open(&path).unwrap();
    let reader = db::open_existing(&path).unwrap();
    reader.busy_timeout(std::time::Duration::ZERO).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let result = db::ensure_feedback_tables(&reader);
    writer.execute_batch("ROLLBACK").unwrap();
    assert!(
        result.is_ok(),
        "already upgraded feedback schema must not request a writer lock: {result:?}"
    );
}

#[test]
fn audit_index_fixtures_ignore_hostile_global_config() {
    for config in [
        "[reach]\nenabled=false\n[ignore]\npatterns=['*.rs']\n",
        "invalid [ config",
    ] {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".wonk")).unwrap();
        std::fs::write(home.path().join(".wonk/config.toml"), config).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env("HOME", home.path())
            .args([
                "--exact",
                "full_rebuild_term_failure_keeps_previous_complete_generation",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture must use explicit configuration: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn public_index_and_daemon_reject_invalid_global_layer_in_child_process() {
    if std::env::var_os("WONK_INVALID_GLOBAL_CHILD").is_none() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".wonk")).unwrap();
        std::fs::write(
            home.path().join(".wonk/config.toml"),
            "[history]\nwindow=10001\n",
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .env("HOME", home.path())
            .env("WONK_INVALID_GLOBAL_CHILD", "1")
            .args([
                "--exact",
                "public_index_and_daemon_reject_invalid_global_layer_in_child_process",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let dir = repo();
    for error in [
        pipeline::build_index(dir.path(), true).unwrap_err(),
        pipeline::incremental_update(dir.path(), true).unwrap_err(),
        wonk::daemon::spawn_daemon(dir.path(), true).unwrap_err(),
    ] {
        assert!(format!("{error:#}").contains("[history] window"));
    }
    assert!(!db::local_index_path(dir.path()).exists());
    assert!(!dir.path().join(".wonk/daemon.pid").exists());
}
