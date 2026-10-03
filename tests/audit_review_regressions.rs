//! Real Git regressions: scoped review must use both selected snapshots.
use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Repo {
    root: TempDir,
    home: TempDir,
}
impl Repo {
    fn new(files: &[(&str, &str)]) -> Self {
        let repo = Self {
            root: tempfile::tempdir().unwrap(),
            home: tempfile::tempdir().unwrap(),
        };
        repo.git(&["init", "-b", "main"]);
        repo.git(&["config", "user.email", "test@example.test"]);
        repo.git(&["config", "user.name", "Review fixture"]);
        for (path, text) in files {
            repo.write(path, text);
        }
        repo.git(&["add", "."]);
        repo.git(&["commit", "-m", "base"]);
        let out = repo.cmd(&["--quiet", "init"]);
        assert!(
            out.status.success(),
            "init: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        repo
    }
    fn git(&self, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.root.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    fn write(&self, path: &str, text: &str) {
        let file = self.root.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    fn cmd(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_wonk"))
            .args(args)
            .env("HOME", self.home.path())
            .current_dir(self.root.path())
            .output()
            .unwrap()
    }
    fn review(&self, scope: &str) -> Value {
        let out = self.cmd(&["--quiet", "--format", "json", "review", "--scope", scope]);
        assert!(
            out.status.success(),
            "review: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rows: Vec<Value> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let mut verdict = rows.last().unwrap().clone();
        verdict["findings"] = json!(&rows[..rows.len() - 1]);
        verdict
    }
    fn mcp_review(&self, scope: &str) -> Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_wonk"))
            .args(["mcp", "serve"])
            .env("HOME", self.home.path())
            .current_dir(self.root.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"wonk_review","arguments":{"scope":scope,"format":"json"}}});
        writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "MCP: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let response: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_ne!(response["result"]["isError"], json!(true), "{response}");
        serde_json::from_str(response["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }
    fn conn(&self) -> rusqlite::Connection {
        let path = self
            .home
            .path()
            .join(".wonk/repos")
            .join(wonk::db::repo_hash(
                &self.root.path().canonicalize().unwrap(),
            ))
            .join("index.db");
        wonk::db::open_existing(&path).unwrap()
    }
}
fn has_rule(result: &Value, rule: &str) -> bool {
    result["findings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["rule"] == rule)
}
const GAP: &str = "coverage-gap/no-test-in-blast-radius";
const REMOVED: &str = "breaking-change/removed-symbol-with-callers";
const SIG: &str = "breaking-change/signature-changed-with-callers";

#[test]
fn f01_statement_deletion_is_reviewed() {
    let repo = Repo::new(&[(
        "src/lib.rs",
        "pub fn target() {\n    work();\n    audit();\n}\n",
    )]);
    repo.write("src/lib.rs", "pub fn target() {\n    work();\n}\n");
    let result = repo.review("unstaged");
    assert!(
        has_rule(&result, GAP),
        "deletion-only edit omitted: {result}"
    );
}
#[test]
fn f01_insertion_above_separate_body_edit_is_reviewed() {
    let base = format!(
        "// header\n{}pub fn target() {{\n    work();\n}}\n",
        "// gap\n".repeat(29)
    );
    let repo = Repo::new(&[("src/lib.rs", &base)]);
    let new = base
        .replacen(
            "// header\n",
            &format!(
                "// header\n{}",
                (0..10)
                    .map(|i| format!("// inserted {i}\n"))
                    .collect::<String>()
            ),
            1,
        )
        .replace("work();", "audit();");
    repo.write("src/lib.rs", &new);
    let result = repo.review("unstaged");
    assert!(
        has_rule(&result, GAP),
        "shifted body edit omitted: {result}"
    );
    assert_eq!(result["findings"][0]["line"], 41);
}
const BASE: &str = "pub fn used() {}\n\npub fn caller() { used(); }\n";
#[test]
fn f02_staged_removal_ignores_unstaged_restoration_cli_and_mcp() {
    let repo = Repo::new(&[("src/lib.rs", BASE)]);
    repo.write("src/lib.rs", "pub fn caller() { used(); }\n");
    repo.git(&["add", "src/lib.rs"]);
    repo.write("src/lib.rs", BASE);
    let cli = repo.review("staged");
    let mcp = repo.mcp_review("staged");
    assert!(has_rule(&cli, REMOVED), "CLI ignored staged removal: {cli}");
    assert!(has_rule(&mcp, REMOVED), "MCP ignored staged removal: {mcp}");
    assert_eq!(cli["findings"], mcp["findings"]);
}
#[test]
fn f02_staged_body_edit_ignores_unstaged_signature_cli_and_mcp() {
    let repo = Repo::new(&[(
        "src/lib.rs",
        "pub fn used(x: i32) {\n    work();\n}\npub fn caller() { used(1); }\n",
    )]);
    repo.write(
        "src/lib.rs",
        "pub fn used(x: i32) {\n    audit();\n}\npub fn caller() { used(1); }\n",
    );
    repo.git(&["add", "src/lib.rs"]);
    repo.write(
        "src/lib.rs",
        "pub fn used(x: i64) {\n    audit();\n}\npub fn caller() { used(1); }\n",
    );
    let cli = repo.review("staged");
    let mcp = repo.mcp_review("staged");
    assert!(
        !has_rule(&cli, SIG),
        "CLI attributed unstaged signature to index: {cli}"
    );
    assert!(
        !has_rule(&mcp, SIG),
        "MCP attributed unstaged signature to index: {mcp}"
    );
    assert_eq!(cli["findings"], mcp["findings"]);
}
#[test]
fn f03_signature_change_is_qualified_by_file() {
    let repo = Repo::new(&[
        (
            "src/a.rs",
            "pub fn shared(x: i32) {}\npub fn caller() { shared(1); }\n",
        ),
        ("src/b.rs", "pub fn shared() {\n    work();\n}\n"),
    ]);
    repo.write(
        "src/a.rs",
        "pub fn shared(x: i64) {}\npub fn caller() { shared(1); }\n",
    );
    repo.write("src/b.rs", "pub fn shared() {\n    audit();\n}\n");
    let result = repo.review("unstaged");
    let blocking: Vec<&Value> = result["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == SIG)
        .collect();
    assert_eq!(
        blocking.len(),
        1,
        "another file inherited signature flag: {result}"
    );
    assert_eq!(blocking[0]["file"], "src/a.rs");
}
const METHODS: &str = "struct A;\nimpl A {\n    pub fn work(x: i32) {}\n}\nstruct B;\nimpl B {\n    pub fn work(x: i32) {}\n}\npub fn caller() { B::work(1); }\n";
#[test]
fn f04_changed_method_anchor_and_suppression_follow_scope() {
    let repo = Repo::new(&[("src/lib.rs", METHODS)]);
    repo.write(
        "src/lib.rs",
        &METHODS.replacen(
            "impl B {\n    pub fn work(x: i32)",
            "impl B {\n    pub fn work(x: i64)",
            1,
        ),
    );
    let first = repo.review("unstaged");
    let finding = first["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["rule"] == SIG)
        .unwrap();
    assert_eq!(finding["line"], 7, "B method anchored to A: {first}");
    let identity = finding["identity"].as_str().unwrap();
    wonk::review::add_suppression(&repo.conn(), identity, SIG, "src/lib.rs", None).unwrap();
    let suppressed = repo.review("unstaged");
    assert_eq!(suppressed["drops"]["identity_suppressed"], 1);
    repo.write(
        "src/lib.rs",
        &METHODS.replacen(
            "impl B {\n    pub fn work(x: i32)",
            "impl B {\n    pub fn work(x: i128)",
            1,
        ),
    );
    let changed = repo.review("unstaged");
    assert!(
        has_rule(&changed, SIG),
        "changed B method remained suppressed: {changed}"
    );
    assert_eq!(changed["drops"]["identity_suppressed"], 0);
}
#[test]
fn f04_identity_survives_line_shift_and_whitespace() {
    let repo = Repo::new(&[("src/lib.rs", METHODS)]);
    let edited = METHODS.replacen(
        "impl B {\n    pub fn work(x: i32)",
        "impl B {\n    pub fn work(x: i64)",
        1,
    );
    repo.write("src/lib.rs", &edited);
    let first = repo.review("unstaged");
    let id = first["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["rule"] == SIG)
        .unwrap()["identity"]
        .clone();
    repo.write(
        "src/lib.rs",
        &format!(
            "// unrelated insertion\n{}",
            edited.replace("pub fn work(x: i64)", "pub  fn  work(x: i64)")
        ),
    );
    let shifted = repo.review("unstaged");
    let finding = shifted["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["rule"] == SIG)
        .unwrap();
    assert_eq!(finding["identity"], id);
    assert_eq!(finding["line"], 8);
}
#[test]
fn f02_unstaged_comparison_uses_index_as_old_snapshot() {
    let repo = Repo::new(&[(
        "src/lib.rs",
        "pub fn used(x: i32) {}\npub fn caller() { used(1); }\n",
    )]);
    repo.write(
        "src/lib.rs",
        "pub fn used(x: i64) {\n    work();\n}\npub fn caller() { used(1); }\n",
    );
    repo.git(&["add", "src/lib.rs"]);
    repo.write(
        "src/lib.rs",
        "pub fn used(x: i64) {\n    audit();\n}\npub fn caller() { used(1); }\n",
    );
    let result = repo.review("unstaged");
    assert!(
        !has_rule(&result, SIG),
        "HEAD signature incorrectly compared for unstaged scope: {result}"
    );
}

#[test]
fn f02_unmerged_index_errors_instead_of_fabricating_approve() {
    let repo = Repo::new(&[("src/lib.rs", BASE)]);
    let output = Command::new("git")
        .args(["rev-parse", "HEAD:src/lib.rs"])
        .current_dir(repo.root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let blob = String::from_utf8(output.stdout).unwrap();
    let mut git = Command::new("git")
        .args(["update-index", "--index-info"])
        .current_dir(repo.root.path())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    write!(git.stdin.take().unwrap(), "0 0000000000000000000000000000000000000000\tsrc/lib.rs\n100644 {} 1\tsrc/lib.rs\n100644 {} 2\tsrc/lib.rs\n100644 {} 3\tsrc/lib.rs\n", blob.trim(), blob.trim(), blob.trim()).unwrap();
    assert!(git.wait().unwrap().success());
    let out = repo.cmd(&["--quiet", "--format", "json", "review", "--scope", "staged"]);
    assert!(
        !out.status.success(),
        "unresolved index was approved: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("git show"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn f16_equal_basename_workspace_consumers_appear_in_review_impact() {
    let fixture = tempfile::tempdir().unwrap();
    let provider_root = fixture.path().join("org_a/service");
    let consumer_root = fixture.path().join("org_b/service");
    std::fs::create_dir_all(provider_root.join("src")).unwrap();
    std::fs::create_dir_all(consumer_root.join(".git")).unwrap();
    std::fs::write(
        provider_root.join("src/server.rs"),
        "pub fn provide() {\n    work();\n}\n",
    )
    .unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&provider_root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "test@example.test"]);
    git(&["config", "user.name", "Workspace fixture"]);
    git(&["add", "."]);
    git(&["commit", "-m", "base"]);
    let registry = fixture.path().join("registry");
    let provider_db = registry
        .join(wonk::db::repo_hash(&provider_root))
        .join("index.db");
    let consumer_db = registry
        .join(wonk::db::repo_hash(&consumer_root))
        .join("index.db");
    let provider = wonk::db::open(&provider_db).unwrap();
    let consumer = wonk::db::open(&consumer_db).unwrap();
    wonk::db::write_meta(&provider_db, &provider_root, &[], &["shared".into()]).unwrap();
    wonk::db::write_meta(&consumer_db, &consumer_root, &[], &["shared".into()]).unwrap();
    provider.execute("INSERT INTO symbols (id,name,kind,file,line,col,end_line,signature,language) VALUES (1,'provide','function','src/server.rs',1,0,3,'pub fn provide()','rust')", []).unwrap();
    provider.execute("INSERT INTO contracts (canonical_id,kind,role,symbol_id,file,line,confidence) VALUES ('grpc::users.v1.UserService::GetUser','grpc','provider',1,'src/server.rs',1,1.0)", []).unwrap();
    consumer.execute("INSERT INTO contracts (canonical_id,kind,role,file,line,confidence) VALUES ('grpc::UserService::get_user','grpc','consumer','src/client.rs',2,1.0)", []).unwrap();
    std::fs::write(
        provider_root.join("src/server.rs"),
        "pub fn provide() {\n    audit();\n}\n",
    )
    .unwrap();
    let report = wonk::review::run_review(
        &provider,
        &wonk::types::ChangeScope::Unstaged,
        &provider_root,
        &wonk::review::ReviewOptions {
            breaking_change: false,
            coverage_gap: false,
            cross_repo: true,
            ..wonk::review::ReviewOptions::default()
        },
        Some(&wonk::review::CrossRepoInputs {
            declared: vec!["shared".into()],
            repos_dir: registry,
        }),
    )
    .unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(
        report.findings.len(),
        1,
        "same-basename consumer missing: {report:?}"
    );
    assert_eq!(
        report.findings[0].rule,
        "cross-repo/changed-provider-with-external-consumers"
    );
    assert_eq!(report.findings[0].related.len(), 1);
    assert_eq!(
        report.findings[0].related[0].file,
        format!(
            "{}:src/client.rs",
            consumer_root.canonicalize().unwrap().display()
        )
    );
}

#[test]
fn f02_add_delete_rename_have_selected_missing_endpoints() {
    let repo = Repo::new(&[
        ("src/old.rs", "pub fn moved() {}\n"),
        ("src/deleted.rs", "pub fn deleted() {}\n"),
    ]);
    repo.git(&["mv", "src/old.rs", "src/renamed.rs"]);
    repo.write("src/added.rs", "pub fn added() {}\n");
    std::fs::remove_file(repo.root.path().join("src/deleted.rs")).unwrap();
    repo.git(&["add", "src/added.rs", "src/deleted.rs"]);
    let detail = wonk::impact::detect_changes_detail(
        &repo.conn(),
        &wonk::types::ChangeScope::Staged,
        repo.root.path(),
    )
    .unwrap();
    assert!(detail.snapshots["src/old.rs"].new.is_none());
    assert!(detail.snapshots["src/renamed.rs"].old.is_none());
    assert!(detail.snapshots["src/added.rs"].old.is_none());
    assert!(detail.snapshots["src/deleted.rs"].new.is_none());
    let changes = &detail.analysis.changed_symbols;
    assert_eq!(changes.len(), 4, "{changes:?}");
    assert!(changes.iter().any(|c| c.name == "moved"
        && c.file == "src/old.rs"
        && c.change_type == wonk::types::ChangeType::Removed));
    assert!(changes.iter().any(|c| c.name == "moved"
        && c.file == "src/renamed.rs"
        && c.change_type == wonk::types::ChangeType::Added));
}
#[test]
fn f02_all_and_since_compare_head_to_disk() {
    let base = "pub fn used(x: i32) {\n    work();\n}\npub fn caller() { used(1); }\n";
    let repo = Repo::new(&[("src/lib.rs", base)]);
    repo.write("src/lib.rs", &base.replace("i32", "i64"));
    repo.git(&["add", "src/lib.rs"]);
    repo.write("src/lib.rs", &base.replace("work();", "audit();"));
    let all = repo.review("all");
    assert!(has_rule(&all, GAP), "body edit missing: {all}");
    assert!(
        !has_rule(&all, SIG),
        "all used index instead of disk: {all}"
    );
    let since = repo.cmd(&["--quiet", "--format", "json", "review", "--since", "HEAD"]);
    assert!(
        since.status.success(),
        "{}",
        String::from_utf8_lossy(&since.stderr)
    );
    let rows: Vec<Value> = String::from_utf8(since.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let finding_rows = &rows[..rows.len() - 1];
    assert_eq!(json!(finding_rows), all["findings"]);
}
#[test]
fn f02_staged_anchor_identity_hashes_index_blob_cli_and_mcp() {
    let base = "pub fn used(x: i32) {\n    work();\n}\npub fn caller() { used(1); }\n";
    let repo = Repo::new(&[("src/lib.rs", base)]);
    repo.write("src/lib.rs", &base.replace("i32", "i64"));
    repo.git(&["add", "src/lib.rs"]);
    repo.write("src/lib.rs", &base.replace("i32", "i128"));
    let cli = repo.review("staged");
    let mcp = repo.mcp_review("staged");
    let finding = cli["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["rule"] == SIG)
        .unwrap();
    let expected = wonk::review::finding_identity(
        SIG,
        "breaking-change",
        "src/lib.rs",
        "used",
        Some("pub fn used(x: i64) {"),
    );
    assert_eq!(
        finding["identity"], expected,
        "anchor was hashed from disk: {cli}"
    );
    assert_eq!(cli["findings"], mcp["findings"]);
}
#[test]
fn f04_two_changed_scopes_keep_distinct_findings_and_identities() {
    let base = format!("{METHODS}pub fn caller_a() {{ A::work(1); }}\n");
    let repo = Repo::new(&[("src/lib.rs", &base)]);
    repo.write("src/lib.rs", &base.replace("i32", "i64"));
    let result = repo.review("unstaged");
    let findings: Vec<&Value> = result["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["rule"] == SIG)
        .collect();
    assert_eq!(findings.len(), 2, "changed scopes merged: {result}");
    assert_eq!(findings[0]["line"], 3);
    assert_eq!(findings[1]["line"], 7);
    assert_ne!(
        findings[0]["identity"], findings[1]["identity"],
        "equal method headers in different scopes must not share suppression"
    );
}
