//! Cross-command CLI integration tests for output formatting contracts.
//!
//! Regression tests for PRD-OUT-REQ-002: structured formats (`--format json`,
//! `--format toon`) must stay machine-consumable on the piped path — one
//! serialized row per newline-terminated line — even though piped grep
//! output collapses same-file rows with ` ; `.

mod common;

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

/// Build the binary path. In test mode, cargo puts it in target/debug/.
fn wonk_bin() -> PathBuf {
    let mut path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    path.push("wonk");
    path
}

/// Fixture repo whose `src/lib.rs` mentions `needle` on two lines so a
/// same-file multi-row result is guaranteed (the case where piped grep
/// output would join rows with ` ; `).
fn fixture_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    Command::new("git")
        .args(["init"])
        .current_dir(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "fn first() -> u32 {\n    let needle = 1;\n    needle + 1\n}\n\nfn second() -> u32 {\n    let needle = 2;\n    needle + 2\n}\n",
    )
    .unwrap();
    dir
}

/// Piped `wonk search --format json` must emit one JSON object per line
/// (NDJSON), not rows joined with ` ; ` on a single line. `search` auto-index
/// is exercised implicitly because search is a query command.
#[test]
fn search_piped_json_is_newline_delimited() {
    let repo = fixture_repo();
    let out = common::command(wonk_bin(), repo.path())
        .arg("--quiet")
        .arg("search")
        .arg("needle")
        .arg("--format")
        .arg("json")
        .current_dir(repo.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "wonk search failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains(" ; "),
        "piped structured output must not join rows with ' ; ': {stdout}"
    );
    assert!(
        stdout.ends_with('\n') && !stdout.ends_with("\n\n"),
        "piped NDJSON must end with exactly one trailing newline: {stdout:?}"
    );
    let lines: Vec<&str> = stdout.trim_end().split('\n').collect();
    assert!(lines.len() >= 2, "expected both matches, got: {stdout}");
    let mut files = Vec::new();
    for line in &lines {
        let v: Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("each line must parse as JSON ({e}): {line}"));
        files.push(v["file"].as_str().unwrap().to_string());
    }
    assert!(
        files.iter().all(|f| f == "src/lib.rs"),
        "both matches come from the same file: {files:?}"
    );
}

#[test]
fn show_malformed_source_keeps_raw_payload_when_elision_requested() {
    let repo = fixture_repo();
    std::fs::write(
        repo.path().join("src/lib.rs"),
        "pub fn target() {\n    let value = ;\n    work();\n}\n",
    )
    .unwrap();
    let init = common::command(wonk_bin(), repo.path())
        .args(["init", "--local"])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let show = |elide: bool| {
        let mut command = common::command(wonk_bin(), repo.path());
        command.args(["show", "target", "--format", "json"]);
        if elide {
            command.arg("--elide=bodies");
        }
        let out = command.output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let row: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
        (row, String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let (raw, _) = show(false);
    let (fallback, warning) = show(true);
    assert_eq!(
        fallback["source"], raw["source"],
        "malformed source must be byte-identical to ordinary show"
    );
    assert!(
        fallback["source"]
            .as_str()
            .unwrap()
            .contains("let value = ;")
    );
    assert!(warning.contains("elision not applied"), "{warning}");
    assert!(warning.contains("parse failure"), "{warning}");
}
