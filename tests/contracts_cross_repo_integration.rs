//! Cross-repo contract resolution integration tests (TASK-084).
//!
//! Spawns the built binary against real fixture repos indexed into an
//! isolated `$HOME/.wonk/repos` registry, then verifies the acceptance
//! criteria end to end: same-workspace links, workspace scoping and
//! normalization, undeclared defaults, orphan widening, cross-repo blast
//! impact, unused-provider gating, meta-based (never working-tree)
//! resolution, and no persisted link rows. The perf criterion (AC 13) is
//! covered at the library boundary in `contracts.rs`.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// One indexed fixture repo plus the TempDir keeping it on disk.
struct RepoFixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

/// The isolated home holding the central registry all repos index into.
struct RegistryHome {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl RegistryHome {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        Self { _dir: dir, path }
    }

    /// Write a global config (for the REQ-017 ignored-layer test).
    fn write_global_config(&self, toml: &str) {
        std::fs::create_dir_all(self.path.join(".wonk")).unwrap();
        std::fs::write(self.path.join(".wonk/config.toml"), toml).unwrap();
    }
}

/// Create a git repo with the given files and optional workspace
/// declaration, then index it into the shared registry.
fn indexed_repo(
    home: &RegistryHome,
    name: &str,
    workspace: Option<&str>,
    files: &[(&str, &str)],
) -> RepoFixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(name);
    std::fs::create_dir_all(root.join(".git")).unwrap();
    for (path, content) in files {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(root.join(parent)).unwrap();
        }
        std::fs::write(root.join(path), content).unwrap();
    }
    if let Some(ws) = workspace {
        std::fs::create_dir_all(root.join(".wonk")).unwrap();
        std::fs::write(
            root.join(".wonk/config.toml"),
            format!("[contracts]\nworkspace = {ws}\n"),
        )
        .unwrap();
    }
    let out = Command::new(wonk_bin())
        .env("HOME", &home.path)
        .current_dir(&root)
        .args(["init"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "wonk init failed for {name}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    RepoFixture { _dir: dir, root }
}

/// Run the binary in `repo` under the isolated `home`.
fn run_wonk<I, S>(home: &RegistryHome, repo: &Path, args: I) -> (i32, String, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let out = Command::new(wonk_bin())
        .env("HOME", &home.path)
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `wonk contracts [...]` with `--quiet` (hints suppressed).
fn contracts_quiet(home: &RegistryHome, repo: &Path, extra: &[&str]) -> (i32, String, String) {
    let mut args: Vec<String> = vec!["--quiet".to_string(), "contracts".to_string()];
    args.extend(extra.iter().map(|s| s.to_string()));
    run_wonk(home, repo, &args)
}

/// `wonk contracts [...]` without `--quiet`, so stderr hints are visible.
fn contracts_verbose(home: &RegistryHome, repo: &Path, extra: &[&str]) -> (i32, String, String) {
    let mut args: Vec<String> = vec!["contracts".to_string()];
    args.extend(extra.iter().map(|s| s.to_string()));
    run_wonk(home, repo, &args)
}

const PROVIDER_USERS: &str = "const app = express();\napp.get('/v1/users', h);\n";
const CONSUMER_USERS: &str =
    "async function loadUsers() {\n  await fetch('https://api.io/v1/users');\n}\n";
const CONSUMER_ORDERS: &str =
    "async function loadOrders() {\n  await fetch('https://api.io/v1/orders');\n}\n";

// -- AC 1 + AC 2: link scoping ------------------------------------------------

#[test]
fn links_same_workspace_exactly_one_link() {
    let home = RegistryHome::new();
    let _provider = indexed_repo(
        &home,
        "users-svc",
        Some(r#""payments""#),
        &[("src/app.js", PROVIDER_USERS)],
    );
    let consumer = indexed_repo(
        &home,
        "own-api",
        Some(r#""payments""#),
        &[("src/client.js", CONSUMER_USERS)],
    );

    let (code, stdout, _stderr) = contracts_quiet(&home, &consumer.root, &["--links"]);
    assert_eq!(code, 0, "stdout: {stdout}");
    let lines: Vec<&str> = stdout.trim_end().lines().collect();
    assert_eq!(lines.len(), 1, "exactly one link: {stdout}");
    assert!(
        lines[0].starts_with("users-svc:src/app.js:2 http::GET::/v1/users role=provider"),
        "{lines:?}"
    );
    assert!(
        lines[0].contains("<-> own-api:src/client.js:2 role=consumer basis=exact"),
        "{lines:?}"
    );
}

#[test]
fn links_different_workspaces_health_no_link() {
    let home = RegistryHome::new();
    let health = "const app = express();\napp.get('/health', h);\n";
    let health_client = "async function ping() {\n  await fetch('https://api.io/health');\n}\n";
    let a = indexed_repo(
        &home,
        "payments-api",
        Some(r#""payments""#),
        &[("src/app.js", health), ("src/client.js", health_client)],
    );
    let _billing = indexed_repo(
        &home,
        "billing-api",
        Some(r#""billing""#),
        &[("src/app.js", health), ("src/client.js", health_client)],
    );

    let (code, stdout, _stderr) = contracts_quiet(&home, &a.root, &["--links"]);
    assert_eq!(code, 0);
    assert!(
        stdout.trim().is_empty(),
        "no links across workspaces: {stdout}"
    );
    let (_code, _out, stderr) = contracts_verbose(&home, &a.root, &["--links"]);
    assert!(stderr.contains("no cross-repo links"), "stderr: {stderr}");
}

// -- AC 3 + AC 4: undeclared default -------------------------------------------

#[test]
fn undeclared_effective_workspace_reported() {
    let home = RegistryHome::new();
    // Undeclared: matches only itself via the repo-name default.
    let lone = indexed_repo(
        &home,
        "lone-api",
        None,
        &[("src/client.js", CONSUMER_USERS)],
    );

    let (code, stdout, stderr) = contracts_verbose(&home, &lone.root, &[]);
    assert_eq!(code, 0);
    // "no provider" is distinguishable from "no sibling indexed": the
    // effective workspace and the exact line to add are both surfaced.
    assert!(
        stderr.contains(
            "workspace: lone-api (undeclared — add 'workspace = \"lone-api\"' under [contracts] in .wonk/config.toml to link sibling repos)"
        ),
        "stderr: {stderr}"
    );
    assert!(
        stdout.contains("http::GET::/v1/users role=consumer"),
        "contracts still list: {stdout}"
    );
    assert!(
        stdout.contains("status=unscoped"),
        "a configuration gap is never labeled a defect: {stdout}"
    );
}

#[test]
fn undeclared_contract_output_identical_with_and_without_workspace_key() {
    let run = |workspace: Option<&str>| -> String {
        let home = RegistryHome::new();
        let repo = indexed_repo(
            &home,
            "solo-api",
            workspace,
            &[
                (
                    "src/app.js",
                    "const app = express();\nfunction routes() {\n  app.get('/v1/users', getUser);\n}\n",
                ),
                ("src/client.js", CONSUMER_ORDERS),
            ],
        );
        // Kind filtering and own provider<->consumer pairing still work;
        // no sibling repos are indexed in this home either way.
        let (code, stdout, stderr) = contracts_quiet(&home, &repo.root, &["--kind", "http"]);
        assert_eq!(code, 0, "stderr: {stderr}");
        stdout
    };
    let normalize = |out: String| {
        out.replace("status=unscoped", "status=X")
            .replace("status=orphan", "status=X")
    };
    let without = run(None);
    let with_key = run(Some(r#""payments""#));
    assert_eq!(
        normalize(without.clone()),
        normalize(with_key),
        "workspace key alone changes nothing beyond the status classification"
    );
    assert!(
        without.lines().count() >= 2,
        "provider route and own pair both list: {without}"
    );
    assert!(
        without.contains("status=unscoped") && !without.contains("status=orphan"),
        "undeclared rows are unscoped: {without}"
    );
}

// -- AC 5: global layer ignored -------------------------------------------------

#[test]
fn global_workspace_does_not_group_repos() {
    let home = RegistryHome::new();
    home.write_global_config("[contracts]\nworkspace = \"payments\"\n");
    // Neither repo declares a workspace locally.
    let a = indexed_repo(
        &home,
        "left-api",
        None,
        &[("src/client.js", CONSUMER_USERS)],
    );
    let _right = indexed_repo(&home, "right-api", None, &[("src/app.js", PROVIDER_USERS)]);

    let (code, stdout, stderr) = contracts_verbose(&home, &a.root, &["--links"]);
    assert_eq!(code, 0);
    assert!(
        stdout.trim().is_empty(),
        "global workspace must not group: {stdout}"
    );
    assert!(
        stderr.contains("workspace is repo-local only"),
        "warning fires on every command: {stderr}"
    );
}

// -- AC 6: one repo in two workspaces -------------------------------------------

#[test]
fn repo_in_two_workspaces_pairs_with_both() {
    let home = RegistryHome::new();
    let own = indexed_repo(
        &home,
        "own-api",
        Some(r#"["payments", "platform"]"#),
        &[("src/a.js", CONSUMER_USERS), ("src/b.js", CONSUMER_ORDERS)],
    );
    let _pay = indexed_repo(
        &home,
        "pay-svc",
        Some(r#""payments""#),
        &[("src/app.js", PROVIDER_USERS)],
    );
    let _plat = indexed_repo(
        &home,
        "plat-svc",
        Some(r#""platform""#),
        &[(
            "src/app.js",
            "const app = express();\napp.get('/v1/orders', h);\n",
        )],
    );

    let (code, stdout, _stderr) = contracts_quiet(&home, &own.root, &["--links"]);
    assert_eq!(code, 0);
    assert_eq!(
        stdout.trim_end().lines().count(),
        2,
        "links to both: {stdout}"
    );
    assert!(stdout.contains("pay-svc:src/app.js:2"), "{stdout}");
    assert!(stdout.contains("plat-svc:src/app.js:2"), "{stdout}");
}

// -- AC 7: trim + case-fold ------------------------------------------------------

#[test]
fn workspace_ids_trim_and_case_fold_match() {
    let home = RegistryHome::new();
    // " Payments " with padding, "payments" plain, and "PAYMENTS" loud all
    // compare equal after normalization.
    let a = indexed_repo(
        &home,
        "case-a",
        Some(r#"" Payments ""#),
        &[("src/client.js", CONSUMER_USERS)],
    );
    let _b = indexed_repo(
        &home,
        "case-b",
        Some(r#""PAYMENTS""#),
        &[("src/app.js", PROVIDER_USERS)],
    );

    let (code, stdout, _stderr) = contracts_quiet(&home, &a.root, &["--links"]);
    assert_eq!(code, 0);
    assert_eq!(
        stdout.trim_end().lines().count(),
        1,
        "folded ids match: {stdout}"
    );
}

// -- AC 8: status surfaces membership --------------------------------------------

#[test]
fn status_lists_workspaces_and_comembers() {
    let home = RegistryHome::new();
    let own = indexed_repo(
        &home,
        "own-api",
        Some(r#""payments""#),
        &[("src/app.js", PROVIDER_USERS)],
    );
    let _mate = indexed_repo(
        &home,
        "team-mate",
        Some(r#""payments""#),
        &[("src/client.js", CONSUMER_USERS)],
    );

    let (code, _stdout, stderr) = run_wonk(&home, &own.root, ["--quiet", "status"]);
    assert_eq!(code, 0);
    assert!(
        stderr.contains("Workspaces: payments (co-members: team-mate)"),
        "stderr: {stderr}"
    );
}

#[test]
fn status_mistyped_workspace_is_singleton() {
    let home = RegistryHome::new();
    let own = indexed_repo(
        &home,
        "own-api",
        Some(r#""paymenst""#),
        &[("src/app.js", PROVIDER_USERS)],
    );
    let _mate = indexed_repo(
        &home,
        "team-mate",
        Some(r#""payments""#),
        &[("src/client.js", CONSUMER_USERS)],
    );

    let (code, _stdout, stderr) = run_wonk(&home, &own.root, ["--quiet", "status"]);
    assert_eq!(code, 0);
    let ws_line = stderr
        .lines()
        .find(|l| l.starts_with("Workspaces:"))
        .unwrap_or_else(|| panic!("no Workspaces line in {stderr}"));
    assert_eq!(
        ws_line, "Workspaces: paymenst",
        "mistyped = singleton: {ws_line}"
    );
}

// -- AC 9: resolution reads stored meta, never sibling working-tree config --------

#[test]
fn resolution_uses_sibling_meta_not_working_tree() {
    let home = RegistryHome::new();
    let consumer = indexed_repo(
        &home,
        "own-api",
        Some(r#""payments""#),
        &[("src/client.js", CONSUMER_USERS)],
    );
    let provider = indexed_repo(
        &home,
        "users-svc",
        Some(r#""payments""#),
        &[("src/app.js", PROVIDER_USERS)],
    );

    let before = contracts_quiet(&home, &consumer.root, &["--links"]).1;
    assert_eq!(before.trim_end().lines().count(), 1);

    // Simulate a branch switch in the sibling: rewrite and then delete its
    // working-tree config entirely. The registry decision was published to
    // meta.json at index time and must not move.
    let sibling_config = provider.root.join(".wonk/config.toml");
    std::fs::write(&sibling_config, "[contracts]\nworkspace = \"billing\"\n").unwrap();
    let after_rewrite = contracts_quiet(&home, &consumer.root, &["--links"]).1;
    assert_eq!(
        before, after_rewrite,
        "working-tree rewrite must not matter"
    );

    std::fs::remove_file(&sibling_config).unwrap();
    let after_delete = contracts_quiet(&home, &consumer.root, &["--links"]).1;
    assert_eq!(
        before, after_delete,
        "working-tree deletion must not matter"
    );
}

// -- AC 10: orphans widen to the workspace ----------------------------------------

#[test]
fn orphans_widen_to_workspace() {
    let home = RegistryHome::new();
    let own = indexed_repo(
        &home,
        "own-api",
        Some(r#""payments""#),
        &[
            ("src/a.js", CONSUMER_USERS),  // served by sibling
            ("src/b.js", CONSUMER_ORDERS), // served nowhere
        ],
    );
    // The sibling serves /v1/users in-workspace...
    let _sibling = indexed_repo(
        &home,
        "users-svc",
        Some(r#""payments""#),
        &[("src/app.js", PROVIDER_USERS)],
    );
    // ...and a non-member serves /v1/orders from another workspace.
    let _nonmember = indexed_repo(
        &home,
        "orders-svc",
        Some(r#""billing""#),
        &[(
            "src/app.js",
            "const app = express();\napp.get('/v1/orders', h);\n",
        )],
    );

    let (code, stdout, _stderr) = contracts_quiet(&home, &own.root, &["--orphans"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = stdout.trim_end().lines().collect();
    assert_eq!(lines.len(), 1, "workspace-wide orphans only: {stdout}");
    assert!(lines[0].contains("http::GET::/v1/orders"), "{lines:?}");
    assert!(lines[0].contains("status=orphan"), "{lines:?}");
}

// -- AC 11: blast cross-repo tier ---------------------------------------------------

#[test]
fn blast_reports_cross_repo_consumer_tier() {
    let home = RegistryHome::new();
    let own = indexed_repo(
        &home,
        "own-api",
        Some(r#""payments""#),
        &[(
            "src/routes.js",
            "const app = express();\nfunction registerUserRoutes() {\n  app.get('/v1/users', getUser);\n}\n",
        )],
    );
    let _client = indexed_repo(
        &home,
        "web-client",
        Some(r#""payments""#),
        &[("src/client.js", CONSUMER_USERS)],
    );

    let (code, stdout, stderr) =
        run_wonk(&home, &own.root, ["--quiet", "blast", "registerUserRoutes"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("[CROSS-REPO IMPACT]"),
        "tier present: {stdout}"
    );
    assert!(
        stdout.contains("web-client:src/client.js"),
        "consuming repo folded into the file field: {stdout}"
    );
}

// -- AC 12: unused providers gated ----------------------------------------------------

#[test]
fn unused_providers_flag_gated() {
    let home = RegistryHome::new();
    let own = indexed_repo(
        &home,
        "own-api",
        Some(r#""payments""#),
        &[
            (
                "src/app.js",
                "const app = express();\napp.get('/v1/users', h);\napp.get('/v1/metrics', h);\n",
            ),
            ("src/client.js", CONSUMER_USERS),
        ],
    );
    let _client = indexed_repo(
        &home,
        "web-client",
        Some(r#""payments""#),
        &[("src/client.js", CONSUMER_USERS)],
    );

    // Default listing: no unused-provider report — provider rows appear
    // without status tokens (083 shape).
    let (code, default_out, _stderr) = contracts_quiet(&home, &own.root, &[]);
    assert_eq!(code, 0);
    assert!(
        default_out.contains("http::GET::/v1/metrics role=provider"),
        "{default_out}"
    );
    assert!(
        !default_out.contains("status="),
        "no status tokens on providers or matched consumers: {default_out}"
    );

    // Behind the flag: only the unconsumed provider survives.
    let (code, unused_out, _stderr) = contracts_quiet(&home, &own.root, &["--unused-providers"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = unused_out.trim_end().lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "gated report lists only the unused row: {unused_out}"
    );
    assert!(lines[0].contains("http::GET::/v1/metrics"), "{lines:?}");

    // --kind/--role apply on the unused-providers mode too (TASK-084
    // review debt): a kind filter selects within the unused rows, and
    // --role consumer — definitionally empty for providers — yields the
    // empty hint instead of the unfiltered rows.
    let (code, kind_out, _stderr) = contracts_quiet(
        &home,
        &own.root,
        &["--unused-providers", "--kind", "queue"],
    );
    assert_eq!(code, 0);
    assert!(
        !kind_out.contains("http::GET::/v1/metrics"),
        "kind filter must apply: {kind_out}"
    );
    let (code, role_out, _stderr) = contracts_quiet(
        &home,
        &own.root,
        &["--unused-providers", "--role", "consumer"],
    );
    assert_eq!(code, 0);
    assert!(
        !role_out.contains("http::GET::"),
        "role=consumer selects no providers: {role_out}"
    );
}

// -- AC 14: nothing persisted -----------------------------------------------------------

#[test]
fn no_link_rows_persisted() {
    let home = RegistryHome::new();
    let consumer = indexed_repo(
        &home,
        "own-api",
        Some(r#""payments""#),
        &[("src/client.js", CONSUMER_USERS)],
    );
    let provider = indexed_repo(
        &home,
        "users-svc",
        Some(r#""payments""#),
        &[("src/app.js", PROVIDER_USERS)],
    );

    // The fixtures index centrally under the isolated HOME; open both
    // registry indexes by their stored repo_path.
    let registry_dbs: Vec<PathBuf> = std::fs::read_dir(home.path.join(".wonk/repos"))
        .unwrap()
        .flatten()
        .map(|e| e.path().join("index.db"))
        .filter(|p| p.exists())
        .collect();
    assert_eq!(registry_dbs.len(), 2, "both repos registered");
    let db_for = |root: &Path| -> PathBuf {
        registry_dbs
            .iter()
            .find(|db| {
                let meta: serde_json::Value = serde_json::from_str(
                    &std::fs::read_to_string(db.parent().unwrap().join("meta.json")).unwrap(),
                )
                .unwrap();
                std::fs::canonicalize(std::path::Path::new(meta["repo_path"].as_str().unwrap()))
                    .unwrap()
                    == std::fs::canonicalize(root).unwrap()
            })
            .unwrap()
            .clone()
    };
    let own_conn = rusqlite::Connection::open(db_for(&provider.root)).unwrap();
    let sibling_conn = rusqlite::Connection::open(db_for(&consumer.root)).unwrap();
    let tables = |conn: &rusqlite::Connection| -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let own_tables_before = tables(&own_conn);
    let sibling_tables_before = tables(&sibling_conn);
    let count = |conn: &rusqlite::Connection| -> i64 {
        conn.query_row("SELECT COUNT(*) FROM contracts", [], |r| r.get(0))
            .unwrap()
    };
    let own_count_before = count(&own_conn);
    let sibling_count_before = count(&sibling_conn);
    drop(own_conn);
    drop(sibling_conn);

    // Resolve repeatedly — links are computed live, never written.
    for _ in 0..2 {
        let (code, stdout, _stderr) = contracts_quiet(&home, &consumer.root, &["--links"]);
        assert_eq!(code, 0);
        assert_eq!(stdout.trim_end().lines().count(), 1);
    }

    let own_conn = rusqlite::Connection::open(db_for(&provider.root)).unwrap();
    let sibling_conn = rusqlite::Connection::open(db_for(&consumer.root)).unwrap();
    assert_eq!(tables(&own_conn), own_tables_before, "no new tables");
    assert_eq!(
        tables(&sibling_conn),
        sibling_tables_before,
        "no new tables"
    );
    assert!(!own_tables_before.iter().any(|t| t.contains("link")));
    assert_eq!(count(&own_conn), own_count_before, "row count unchanged");
    assert_eq!(
        count(&sibling_conn),
        sibling_count_before,
        "row count unchanged"
    );
}

// -- MCP exposure (TASK-084 §9) -----------------------------------------------------------

/// Minimal MCP stdio client for the two cross-repo tool checks.
fn mcp_call(home: &RegistryHome, repo: &Path, method_args: serde_json::Value) -> serde_json::Value {
    let mut child = Command::new(wonk_bin())
        .env("HOME", &home.path)
        .current_dir(repo)
        .args(["mcp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn wonk mcp serve");
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let mut send = |req: serde_json::Value| -> serde_json::Value {
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
        let mut buf = String::new();
        reader.read_line(&mut buf).unwrap();
        serde_json::from_str(buf.trim()).unwrap()
    };

    send(serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}));
    let response = send(serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": method_args
    }));
    drop(stdin);
    let _ = child.wait();
    response
}

#[test]
fn tool_contracts_links_cross_repo() {
    let home = RegistryHome::new();
    let _provider = indexed_repo(
        &home,
        "users-svc",
        Some(r#""payments""#),
        &[("src/app.js", PROVIDER_USERS)],
    );
    let consumer = indexed_repo(
        &home,
        "own-api",
        Some(r#""payments""#),
        &[("src/client.js", CONSUMER_USERS)],
    );

    let response = mcp_call(
        &home,
        &consumer.root,
        serde_json::json!({
            "name": "wonk_contracts",
            "arguments": {"repo": "own-api", "links": true}
        }),
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text in {response}"));
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    let links = parsed["links"].as_array().unwrap();
    assert_eq!(links.len(), 1, "got {links:?}");
    assert_eq!(links[0]["provider"]["repo"], "users-svc");
    assert_eq!(links[0]["consumer"]["repo"], "own-api");
    assert_eq!(links[0]["basis"], "exact");
    assert_eq!(parsed["workspace"]["effective"][0], "payments");
    // Links mode mirrors the CLI: no row listing.
    assert!(parsed["contracts"].as_array().unwrap().is_empty());
}

#[test]
fn tool_blast_appends_cross_repo_tier() {
    let home = RegistryHome::new();
    let own = indexed_repo(
        &home,
        "own-api",
        Some(r#""payments""#),
        &[(
            "src/routes.js",
            "const app = express();\nfunction registerUserRoutes() {\n  app.get('/v1/users', getUser);\n}\n",
        )],
    );
    let _client = indexed_repo(
        &home,
        "web-client",
        Some(r#""payments""#),
        &[("src/client.js", CONSUMER_USERS)],
    );

    let response = mcp_call(
        &home,
        &own.root,
        serde_json::json!({
            "name": "wonk_blast",
            "arguments": {"symbol": "registerUserRoutes"}
        }),
    );
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    let tiers = parsed["tiers"].as_array().unwrap();
    let cross = tiers
        .iter()
        .find(|t| t["severity"] == "CROSS-REPO IMPACT")
        .unwrap_or_else(|| panic!("no cross-repo tier in {tiers:?}"));
    assert!(
        cross["symbols"][0]["file"]
            .as_str()
            .is_some_and(|f| f.starts_with("web-client:")),
        "consuming repo named: {cross:?}"
    );
}
