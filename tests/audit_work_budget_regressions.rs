use std::fs;

use wonk::config::Config;
use wonk::elide::{self, Mode, NotElided};
use wonk::indexer::Lang;
use wonk::show::{ShowOptions, show_symbol};

#[test]
fn explicit_config_paths_merge_layers_without_process_home() {
    let global = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    fs::create_dir(repo.path().join(".wonk")).unwrap();
    fs::write(
        global.path().join("config.toml"),
        "[history]\nwindow=900\nmax_commit_files=80\n[topology]\niterations=40\n",
    )
    .unwrap();
    fs::write(
        repo.path().join(".wonk/config.toml"),
        "[history]\nwindow=700\n",
    )
    .unwrap();
    let config = Config::load_with_paths(Some(global.path()), Some(repo.path())).unwrap();
    assert_eq!(config.history.window, 700);
    assert_eq!(config.history.max_commit_files, 80);
    assert_eq!(config.topology.iterations, 40);
    let isolated = Config::load_with_paths(None, Some(repo.path())).unwrap();
    assert_eq!(isolated.history.window, 700);
    assert_eq!(
        isolated.history.max_commit_files,
        Config::default().history.max_commit_files
    );
    assert_eq!(isolated.topology, Config::default().topology);
    fs::write(
        global.path().join("config.toml"),
        "[history]\nwindow=10001\n",
    )
    .unwrap();
    assert!(
        Config::load_with_paths(Some(global.path()), Some(repo.path())).is_err(),
        "invalid global layer must not be masked by valid repo override"
    );
}

#[test]
fn malformed_show_returns_raw_source_through_public_api() {
    let dir = tempfile::tempdir().unwrap();
    let source = "pub fn target() {\n    let value = ;\n    work();\n}\n";
    fs::write(dir.path().join("target.rs"), source).unwrap();
    let conn = wonk::db::open(&dir.path().join("index.db")).unwrap();
    conn.execute("INSERT INTO symbols(name,kind,file,line,col,end_line,signature,language) VALUES ('target','function','target.rs',1,1,4,'pub fn target()','rust')", []).unwrap();
    let options = ShowOptions {
        file: None,
        kind: None,
        exact: true,
        suppress: true,
        shallow: false,
        scope: None,
        signatures_only: false,
        elide: Some(Mode::Bodies),
    };
    let rows = show_symbol(&conn, "target", dir.path(), &options).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, source.trim_end_matches('\n'));
    assert_eq!(
        elide::elide(source, Some(Lang::Rust), Mode::Bodies),
        Err(NotElided::ParseFailure)
    );
}

#[test]
fn valid_elision_retains_bytes_and_exact_line_counts() {
    let source = "// kept\npub fn target() {\n    let value = 1;\n    work();\n}\n";
    assert_eq!(
        elide::elide(source, Some(Lang::Rust), Mode::Bodies).unwrap(),
        "// kept\npub fn target() { /* 4 lines elided */ }\n"
    );
}
