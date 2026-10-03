use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

fn run(root: &Path, home: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_wonk"))
        .env("HOME", home)
        .current_dir(root)
        .arg("--quiet")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn fixture() -> (TempDir, PathBuf, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("repo");
    let home = dir.path().join("home");
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(root.join(".wonk")).unwrap();
    for n in 0..20 {
        fs::write(
            root.join(format!("file{n:02}.rs")),
            format!("pub fn gamma_{n:02}() {{}}\n"),
        )
        .unwrap();
    }
    configure(&root, true);
    run(&root, &home, &["init", "--local"]);
    (dir, root, home)
}

fn configure(root: &Path, enabled: bool) {
    fs::write(
        root.join(".wonk/config.toml"),
        format!("[feedback]\nenabled={enabled}\nauthor_features=false\n"),
    )
    .unwrap();
}

fn members(conn: &Connection, token: &str) -> Vec<Value> {
    let stored: String = conn
        .query_row(
            "SELECT members FROM feedback_slates WHERE token=?1",
            [token],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&stored).unwrap()
}

#[test]
fn audit_g4_cli_delivered_page_is_the_feedback_slate() {
    let (_dir, root, home) = fixture();
    let conn = Connection::open(root.join(".wonk/index.db")).unwrap();
    for page in ["1", "2"] {
        let args = [
            "--format", "json", "--budget", "180", "--page", page, "search", "gamma", "--smart",
        ];
        let stdout = run(&root, &home, &args);
        let rows: Vec<Value> = stdout
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|row| row.get("file").is_some())
            .collect();
        assert!(!rows.is_empty(), "{stdout}");
        let stored = members(&conn, rows[0]["slate"].as_str().unwrap());
        assert_eq!(
            stored.len(),
            rows.len(),
            "slate contains only delivered rows for page {page}"
        );
        for (row, member) in rows.iter().zip(&stored) {
            assert_eq!(row["file"], member["file"]);
            assert_eq!(row["line"], member["line"]);
            assert_eq!(row["identity"], member["identity"]);
        }
        if page == "2" {
            assert!(stored[0]["rank"].as_u64().unwrap() > 1);
        }
        let rendered_cost: usize = rows
            .iter()
            .map(|row| wonk::budget::estimate_tokens(&(serde_json::to_string(row).unwrap() + "\n")))
            .sum();
        assert!(
            rendered_cost <= 180,
            "actual rendered row bytes exceed budget: {rendered_cost}"
        );
    }
}

fn centralize(root: &Path, home: &Path) {
    let root = fs::canonicalize(root).unwrap();
    let index = home
        .join(".wonk/repos")
        .join(wonk::db::repo_hash(&root))
        .join("index.db");
    fs::create_dir_all(index.parent().unwrap()).unwrap();
    fs::copy(root.join(".wonk/index.db"), &index).unwrap();
    fs::copy(
        root.join(".wonk/meta.json"),
        index.with_file_name("meta.json"),
    )
    .unwrap();
}

fn mcp_tool(root: &Path, home: &Path, name: &str, args: Value) -> Value {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_wonk"))
        .env("HOME", home)
        .current_dir(root)
        .args(["mcp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(stdin, "{}", serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}})).unwrap();
    stdin.flush().unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    let result = &response["result"];
    assert!(!result["isError"].as_bool().unwrap_or(false), "{response}");
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
}

#[test]
fn audit_g4_weighted_order_and_absolute_path_identity_match_cli_mcp() {
    let (_dir, root, home) = fixture();
    let relocated = root.parent().unwrap().join("tests");
    fs::rename(&root, &relocated).unwrap();
    let root = relocated;
    for n in 0..20 {
        fs::remove_file(root.join(format!("file{n:02}.rs"))).unwrap();
    }
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/gamma.d.ts"),
        "declare function gamma(): void;\n",
    )
    .unwrap();
    fs::write(root.join("src/call.ts"), "function invoke() { gamma(); }\n").unwrap();
    let mut config =
        "[feedback]\nenabled=true\nauthor_features=false\n[rank.weights]\n".to_string();
    for name in wonk::rerank::known_signal_names() {
        let weight = match name {
            "kind" => 0.1,
            "path_character" => 1.0,
            _ => 0.0,
        };
        config.push_str(&format!("{name}={weight}\n"));
    }
    fs::write(root.join(".wonk/config.toml"), config).unwrap();
    run(&root, &home, &["update", "--force", "--skip-embed"]);
    centralize(&root, &home);
    let text = run(
        &root,
        &home,
        &[
            "--format",
            "json",
            "search",
            "gamma",
            "--smart",
            "--why",
            "--query-class",
            "conceptual",
        ],
    );
    let cli: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(cli.len(), 2, "{text}");
    assert_eq!(
        cli[0]["file"], "src/call.ts",
        "high score callsite leads weak definition"
    );
    assert!(cli[0]["why"]["total"].as_f64().unwrap() > cli[1]["why"]["total"].as_f64().unwrap());
    let absolute = fs::canonicalize(&root).unwrap();
    let absolute_text = run(
        &root,
        &home,
        &[
            "--format",
            "json",
            "search",
            "gamma",
            "--smart",
            "--why",
            "--query-class",
            "conceptual",
            "--",
            absolute.to_str().unwrap(),
        ],
    );
    let absolute_rows: Vec<Value> = absolute_text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        absolute_rows.len(),
        cli.len(),
        "CLI absolute owned paths retain ordinary source under an ancestor named tests"
    );
    for (relative, absolute) in cli.iter().zip(&absolute_rows) {
        assert_eq!(relative["identity"], absolute["identity"]);
    }
    let mcp = mcp_tool(
        &root,
        &home,
        "wonk_search",
        serde_json::json!({"query": "gamma", "query_class": "conceptual", "format": "json"}),
    );
    let mcp = mcp.as_array().unwrap();
    assert_eq!(mcp.len(), cli.len());
    for (cli, mcp) in cli.iter().zip(mcp) {
        assert!(
            mcp["file"]
                .as_str()
                .unwrap()
                .ends_with(cli["file"].as_str().unwrap())
        );
        assert_eq!(
            cli["identity"], mcp["identity"],
            "relative CLI and root-owned absolute MCP anchor the same indexed result"
        );
    }
}

#[test]
fn audit_g4_cross_repo_blast_helper_matches_both_surfaces_with_duplicate_names() {
    let dir = TempDir::new().unwrap();
    let home = dir.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let own = dir.path().join("left/service");
    let sibling = dir.path().join("right/service");
    for (root, source) in [
        (
            &own,
            "const app = express();\nfunction gammaRoutes() {\n app.get('/gamma', handler);\n app.get('/spare', handler);\n}\n",
        ),
        (
            &sibling,
            "async function gammaClient() {\n return fetch('/gamma');\n}\n",
        ),
    ] {
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join(".wonk")).unwrap();
        fs::write(root.join("app.js"), source).unwrap();
        fs::write(
            root.join(".wonk/config.toml"),
            "[contracts]\nworkspace='demo'\n",
        )
        .unwrap();
        run(root, &home, &["init", "--local"]);
        centralize(root, &home);
    }
    let cli: Value = serde_json::from_str(&run(
        &own,
        &home,
        &["--format", "json", "blast", "gammaRoutes"],
    ))
    .unwrap();
    let mcp = mcp_tool(
        &own,
        &home,
        "wonk_blast",
        serde_json::json!({"symbol": "gammaRoutes", "format": "json"}),
    );
    assert_eq!(
        cli, mcp,
        "both boundaries use identical resolution-and-append sequence"
    );
    let cross = cli["tiers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tier| tier["severity"] == "CROSS-REPO IMPACT")
        .expect("cross-repo tier");
    assert_eq!(cross["symbols"].as_array().unwrap().len(), 1);
    assert!(
        cross["symbols"][0]["file"]
            .as_str()
            .unwrap()
            .contains("right/service")
    );
    for filter in [vec!["--kind", "env"], vec!["--role", "consumer"]] {
        let mut args = vec!["--format", "json", "contracts", "--unused-providers"];
        args.extend(filter.iter().copied());
        assert!(run(&own, &home, &args).trim().is_empty());
        let key = filter[0].trim_start_matches("--");
        let mut arguments = serde_json::json!({"unused_providers": true});
        arguments[key] = serde_json::json!(filter[1]);
        let mcp = mcp_tool(&own, &home, "wonk_contracts", arguments);
        assert!(mcp["contracts"].as_array().unwrap().is_empty(), "{mcp}");
    }
}

#[test]
fn audit_g4_cli_grep_page_slate_contains_only_emitted_rows() {
    let (_dir, root, home) = fixture();
    let output = run(
        &root,
        &home,
        &[
            "--format", "grep", "--budget", "40", "--page", "2", "search", "gamma", "--smart",
        ],
    );
    let rows: Vec<&str> = output
        .lines()
        .filter(|line| line.starts_with("file"))
        .collect();
    let token = output
        .lines()
        .find_map(|line| line.strip_prefix("slate: "))
        .unwrap();
    let conn = Connection::open(root.join(".wonk/index.db")).unwrap();
    let stored = members(&conn, token);
    assert_eq!(stored.len(), rows.len(), "{output}");
    assert!(!stored.is_empty());
    for (row, member) in rows.iter().zip(&stored) {
        assert!(row.starts_with(&format!(
            "{}:{}:",
            member["file"].as_str().unwrap(),
            member["line"]
        )));
    }
    assert!(stored[0]["rank"].as_u64().unwrap() > 1);
}
