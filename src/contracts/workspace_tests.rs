use super::*;

// -- RPC canonical join (TASK-088, PRD-CTR-REQ-024) -------------------------

/// Shorthand test candidate for the join.
fn join_grpc(service: &str, method: &str, role: ContractRole) -> ContractCandidate {
    grpc_candidate(service, method, role, None, 1)
}

fn join_scope<'a>(workspace: &str, cands: &'a [ContractCandidate]) -> RpcJoinScope<'a> {
    RpcJoinScope {
        workspace: workspace.to_string(),
        candidates: cands,
    }
}

#[test]
fn rpc_join_case_folded_method() {
    let provider = join_grpc("UserService", "GetUser", ContractRole::Provider);
    let consumer = join_grpc("UserService", "get_user", ContractRole::Consumer);
    let cands = [provider, consumer];
    let scopes = [join_scope("alpha", &cands)];
    let joins = canonical_rpc_join(&scopes);
    assert_eq!(joins.len(), 1, "got {joins:?}");
    assert_eq!(joins[0].basis, RpcMatchBasis::CaseFoldedMethod);
    assert_eq!(joins[0].provider.canonical_id, "grpc::UserService::GetUser");
    assert_eq!(
        joins[0].consumer.canonical_id,
        "grpc::UserService::get_user"
    );
    assert_eq!(joins[0].provider.workspace, "alpha");
    assert_eq!(joins[0].consumer.role, ContractRole::Consumer);
}

#[test]
fn rpc_join_package_qualified_service() {
    let provider = join_grpc("users.v1.UserService", "GetUser", ContractRole::Provider);
    let consumer = join_grpc("UserService", "GetUser", ContractRole::Consumer);
    let cands = [provider, consumer];
    let scopes = [join_scope("alpha", &cands)];
    let joins = canonical_rpc_join(&scopes);
    assert_eq!(joins.len(), 1, "got {joins:?}");
    assert_eq!(joins[0].basis, RpcMatchBasis::PackageQualifiedService);
}

#[test]
fn rpc_join_service_level_star_pairs_with_any_method() {
    let provider = join_grpc("UserService", "*", ContractRole::Provider);
    let consumer = join_grpc("UserService", "DeleteUser", ContractRole::Consumer);
    let cands = [provider, consumer];
    let scopes = [join_scope("alpha", &cands)];
    let joins = canonical_rpc_join(&scopes);
    assert_eq!(joins.len(), 1, "got {joins:?}");
    assert_eq!(joins[0].basis, RpcMatchBasis::ServiceLevelProvider);
}

#[test]
fn rpc_join_method_level_beats_service_level() {
    let star = join_grpc("UserService", "*", ContractRole::Provider);
    let method = join_grpc("users.v1.UserService", "GetUser", ContractRole::Provider);
    let consumer = join_grpc("UserService", "getUser", ContractRole::Consumer);
    // The `*` provider comes first; the method-level one must still win.
    let cands = [star, method.clone(), consumer];
    let scopes = [join_scope("alpha", &cands)];
    let joins = canonical_rpc_join(&scopes);
    assert_eq!(joins.len(), 1, "got {joins:?}");
    assert_eq!(joins[0].provider.canonical_id, method.canonical_id);
    assert_eq!(joins[0].basis, RpcMatchBasis::PackageQualifiedService);
}

#[test]
fn rpc_join_equal_rank_tie_break_prefers_first_provider() {
    // Two method-level providers of the same service+method carry equal
    // rank: the documented rule keeps the LOWEST (scope, candidate)
    // index — the first provider in order wins, never the last.
    let first = join_grpc("users.v1.UserService", "GetUser", ContractRole::Provider);
    let second = join_grpc("UserService", "GetUser", ContractRole::Provider);
    let consumer = join_grpc("UserService", "getUser", ContractRole::Consumer);
    let cands = [first.clone(), second, consumer];
    let scopes = [join_scope("alpha", &cands)];
    let joins = canonical_rpc_join(&scopes);
    assert_eq!(joins.len(), 1, "got {joins:?}");
    assert_eq!(
        joins[0].provider.canonical_id, first.canonical_id,
        "equal-rank tie must keep the first (lowest-index) provider"
    );
}

#[test]
fn rpc_join_exact_id_matches_are_excluded() {
    // The exact pair belongs to the first pass; the join must not emit a
    // second link for that consumer (nor consume the exact provider).
    let exact_provider = join_grpc("UserService", "getUser", ContractRole::Provider);
    let exact_consumer = join_grpc("UserService", "getUser", ContractRole::Consumer);
    let relaxed_provider = join_grpc("users.v1.UserService", "getUser", ContractRole::Provider);
    let cands = [exact_provider, relaxed_provider, exact_consumer];
    let scopes = [join_scope("alpha", &cands)];
    let joins = canonical_rpc_join(&scopes);
    assert!(
        joins.is_empty(),
        "exact-ID pair must never be overridden, got {joins:?}"
    );
}

#[test]
fn rpc_join_exact_counterpart_in_other_workspace_does_not_exclude() {
    // Exclusion is per workspace: an exact provider behind a different
    // workspace boundary never pairs, so the consumer stays joinable
    // within its own workspace (PRD-CTR-REQ-014).
    let far_provider = join_grpc("UserService", "getUser", ContractRole::Provider);
    let near_provider = join_grpc("users.v1.UserService", "getUser", ContractRole::Provider);
    let consumer = join_grpc("UserService", "getUser", ContractRole::Consumer);
    let cands_a = [far_provider];
    let cands_b = [near_provider, consumer];
    let scopes = [join_scope("beta", &cands_a), join_scope("alpha", &cands_b)];
    let joins = canonical_rpc_join(&scopes);
    assert_eq!(joins.len(), 1, "got {joins:?}");
    assert_eq!(joins[0].provider.workspace, "alpha");
}

#[test]
fn rpc_join_respects_workspace_boundaries() {
    let provider = join_grpc("users.v1.UserService", "getUser", ContractRole::Provider);
    let consumer = join_grpc("UserService", "getUser", ContractRole::Consumer);
    let cands_a = [consumer];
    let cands_b = [provider];
    let scopes = [join_scope("alpha", &cands_a), join_scope("beta", &cands_b)];
    assert!(
        canonical_rpc_join(&scopes).is_empty(),
        "the join relaxes name matching, never workspace scope"
    );
}

#[test]
fn rpc_join_workspace_comparison_trims_and_case_folds() {
    let provider = join_grpc("users.v1.UserService", "getUser", ContractRole::Provider);
    let consumer = join_grpc("UserService", "getUser", ContractRole::Consumer);
    let cands_a = [consumer];
    let cands_b = [provider];
    let scopes = [
        join_scope("alpha", &cands_a),
        join_scope("  ALPHA ", &cands_b),
    ];
    assert_eq!(canonical_rpc_join(&scopes).len(), 1);
}

#[test]
fn rpc_join_ignores_non_rpc_kinds() {
    // GraphQL exact ID shape must not pair with a grpc provider here.
    let provider = join_grpc("UserService", "getUser", ContractRole::Provider);
    let consumer = graphql_candidate("UserService", "getUser", ContractRole::Consumer, None, 1);
    let cands = [provider, consumer];
    let scopes = [join_scope("alpha", &cands)];
    assert!(canonical_rpc_join(&scopes).is_empty());
}

#[test]
fn rpc_join_within_repo_single_scope() {
    // REQ-016: workspace bounds pairing, never extraction — one scope
    // with both roles links (a repo calling its own service).
    let provider = join_grpc("UserService", "get_user", ContractRole::Provider);
    let consumer = join_grpc("UserService", "GetUser", ContractRole::Consumer);
    let cands = [provider, consumer];
    let scopes = [join_scope("solo", &cands)];
    assert_eq!(canonical_rpc_join(&scopes).len(), 1);
}

#[test]
fn rpc_join_deterministic_consumer_order() {
    let provider = join_grpc("UserService", "*", ContractRole::Provider);
    let first = join_grpc("UserService", "GetUser", ContractRole::Consumer);
    let second = join_grpc("users.v1.UserService", "DeleteUser", ContractRole::Consumer);
    let cands = [provider, first.clone(), second.clone()];
    let scopes = [join_scope("alpha", &cands)];
    let joins = canonical_rpc_join(&scopes);
    assert_eq!(joins.len(), 2, "got {joins:?}");
    assert_eq!(joins[0].consumer.canonical_id, first.canonical_id);
    assert_eq!(joins[1].consumer.canonical_id, second.canonical_id);
}

#[test]
fn rpc_join_unpaired_consumer_emits_nothing() {
    let consumer = join_grpc("OrderService", "PlaceOrder", ContractRole::Consumer);
    let provider = join_grpc("UserService", "*", ContractRole::Provider);
    let cands = [provider, consumer];
    let scopes = [join_scope("alpha", &cands)];
    assert!(canonical_rpc_join(&scopes).is_empty());
}

#[test]
fn rpc_family_today_is_grpc_only() {
    assert!(is_rpc_family(ContractKind::Grpc));
    assert!(!is_rpc_family(ContractKind::Graphql));
    assert!(!is_rpc_family(ContractKind::Http));
}

#[test]
fn workspace_id_trims_and_case_folds() {
    assert_eq!(normalize_workspace_id("  Payments "), "payments");
    assert_eq!(normalize_workspace_id("PAYMENTS"), "payments");
}

// -- storage query API (TASK-083) -----------------------------------------

/// Contract row shape used to seed [`list_contracts`] test databases.
struct SeedRow {
    canonical_id: &'static str,
    kind: &'static str,
    role: &'static str,
    symbol_id: Option<i64>,
    file: &'static str,
    line: i64,
    confidence: f64,
}

/// Open a temp index and seed it with one symbol (`load` in src/a.js)
/// plus the given contract rows.
fn seeded_query_db(rows: &[SeedRow]) -> rusqlite::Connection {
    let dir = tempfile::tempdir().unwrap();
    let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
    // Leak the TempDir: the SQLite file must outlive this helper for the
    // duration of the test (tempfiles delete on drop; the OS cleans up).
    std::mem::forget(dir);
    conn.execute(
        "INSERT INTO symbols (name, kind, file, line, col, language, signature) \
             VALUES ('load', 'function', 'src/a.js', 10, 0, 'JavaScript', 'async function load()')",
        [],
    )
    .unwrap();
    for r in rows {
        conn.execute(
            "INSERT INTO contracts (canonical_id, kind, role, symbol_id, file, line, confidence) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                r.canonical_id,
                r.kind,
                r.role,
                r.symbol_id,
                r.file,
                r.line,
                r.confidence
            ],
        )
        .unwrap();
    }
    conn
}

fn query(kind: Option<ContractKind>, role: Option<ContractRole>, orphans: bool) -> ContractQuery {
    ContractQuery {
        kind,
        role,
        orphans,
    }
}

#[test]
fn list_contracts_filters_kind_and_role() {
    let conn = seeded_query_db(&[
        SeedRow {
            canonical_id: "http::GET::/users",
            kind: "http",
            role: "provider",
            symbol_id: None,
            file: "src/a.js",
            line: 1,
            confidence: 1.0,
        },
        SeedRow {
            canonical_id: "http::GET::/users",
            kind: "http",
            role: "consumer",
            symbol_id: Some(1),
            file: "src/b.js",
            line: 5,
            confidence: 1.0,
        },
        SeedRow {
            canonical_id: "env::::API_KEY",
            kind: "env",
            role: "consumer",
            symbol_id: Some(1),
            file: "src/b.js",
            line: 7,
            confidence: 0.5,
        },
    ]);

    let all = list_contracts(&conn, &query(None, None, false)).unwrap();
    assert_eq!(all.len(), 3);
    // ORDER BY kind, canonical_id, file, line: env sorts before http.
    assert_eq!(all[0].canonical_id, "env::::API_KEY");
    assert_eq!(all[1].canonical_id, "http::GET::/users");
    assert_eq!(all[1].file, "src/a.js");

    let http = list_contracts(&conn, &query(Some(ContractKind::Http), None, false)).unwrap();
    assert_eq!(http.len(), 2);

    let consumers =
        list_contracts(&conn, &query(None, Some(ContractRole::Consumer), false)).unwrap();
    assert_eq!(consumers.len(), 2);

    let env_consumers = list_contracts(
        &conn,
        &query(Some(ContractKind::Env), Some(ContractRole::Consumer), false),
    )
    .unwrap();
    assert_eq!(env_consumers.len(), 1);
    assert_eq!(env_consumers[0].canonical_id, "env::::API_KEY");
}

#[test]
fn list_contracts_orphans_within_repo() {
    let conn = seeded_query_db(&[
        SeedRow {
            canonical_id: "http::GET::/users",
            kind: "http",
            role: "provider",
            symbol_id: None,
            file: "src/a.js",
            line: 1,
            confidence: 1.0,
        },
        SeedRow {
            canonical_id: "http::GET::/users",
            kind: "http",
            role: "consumer",
            symbol_id: Some(1),
            file: "src/b.js",
            line: 5,
            confidence: 1.0,
        },
        SeedRow {
            canonical_id: "env::::DATABASE_URL",
            kind: "env",
            role: "consumer",
            symbol_id: Some(1),
            file: "src/b.js",
            line: 9,
            confidence: 1.0,
        },
        SeedRow {
            canonical_id: "env::::FEATURE_X",
            kind: "env",
            role: "provider",
            symbol_id: None,
            file: "src/a.js",
            line: 3,
            confidence: 1.0,
        },
    ]);

    // The matched http pair is excluded; only the env consumer lacks an
    // in-repo provider; providers are never orphans.
    let orphans = list_contracts(&conn, &query(None, None, true)).unwrap();
    assert_eq!(
        orphans
            .iter()
            .map(|r| r.canonical_id.as_str())
            .collect::<Vec<_>>(),
        vec!["env::::DATABASE_URL"]
    );

    // Without the flag every row is listed (REQ-008 wording: orphans
    // appear only when asked for).
    let all = list_contracts(&conn, &query(None, None, false)).unwrap();
    assert_eq!(all.len(), 4);

    // --orphans hard-constrains role=consumer: the combination with
    // role=provider is deterministically empty.
    let none = list_contracts(&conn, &query(None, Some(ContractRole::Provider), true)).unwrap();
    assert!(none.is_empty());
}

#[test]
fn list_contracts_resolves_symbol_name() {
    let conn = seeded_query_db(&[
        SeedRow {
            canonical_id: "http::GET::/users",
            kind: "http",
            role: "consumer",
            symbol_id: Some(1),
            file: "src/b.js",
            line: 5,
            confidence: 1.0,
        },
        SeedRow {
            canonical_id: "env::::DATABASE_URL",
            kind: "env",
            role: "consumer",
            symbol_id: None,
            file: "doc/policy.yaml",
            line: 2,
            confidence: 1.0,
        },
    ]);

    let rows = list_contracts(&conn, &query(None, None, false)).unwrap();
    let named = rows
        .iter()
        .find(|r| r.canonical_id == "http::GET::/users")
        .unwrap();
    assert_eq!(named.symbol.as_deref(), Some("load"));
    assert_eq!(named.kind, ContractKind::Http);
    assert_eq!(named.role, ContractRole::Consumer);
    assert_eq!(named.line, 5);
    assert_eq!(named.confidence, 1.0);

    let document = rows
        .iter()
        .find(|r| r.canonical_id == "env::::DATABASE_URL")
        .unwrap();
    assert_eq!(document.symbol, None, "document contracts have no symbol");
}

/// TASK-083 acceptance: a built single-repo index answers the `wonk
/// contracts` queries end to end — routes with role/file/line/confidence
/// (AC1), within-repo orphans with no sibling repos indexed and no error
/// (AC3), NDJSON rows that parse without post-processing (AC4).
#[test]
fn contracts_end_to_end_single_repo() {
    use crate::output::{ContractOutput, Formatter, OutputFormat};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
            root.join("src/app.js"),
            "const app = express();\n\nfunction setupRoutes() {\n  app.get('/v1/users/:id', getUser);\n  app.post('/orders', createOrder);\n}\n\nasync function load() {\n  const db = process.env.DATABASE_URL;\n  process.env.FEATURE_X = '1';\n}\n",
        )
        .unwrap();

    let config = crate::config::Config::load_with_paths(None, Some(root)).unwrap();
    crate::pipeline::build_index_with_config(root, true, &config).unwrap();
    let conn = crate::db::open_existing(&crate::db::local_index_path(root)).unwrap();

    // AC1: kind=http lists this repo's routes with role, file, line,
    // and confidence.
    let routes = list_contracts(&conn, &query(Some(ContractKind::Http), None, false)).unwrap();
    assert_eq!(routes.len(), 2, "got {routes:?}");
    for r in &routes {
        assert_eq!(r.role, ContractRole::Provider);
        assert_eq!(r.file, "src/app.js");
        assert_eq!(r.confidence, 1.0);
        assert_eq!(r.symbol.as_deref(), Some("setupRoutes"));
    }

    // AC3: no sibling repos indexed — the query still succeeds and
    // answers within-repo orphans (DATABASE_URL read has no writer;
    // FEATURE_X is written, so a provider, and routes are providers).
    let orphans = list_contracts(&conn, &query(None, None, true)).unwrap();
    assert_eq!(
        orphans
            .iter()
            .map(|r| r.canonical_id.as_str())
            .collect::<Vec<_>>(),
        vec!["env::::DATABASE_URL"]
    );

    // AC4: NDJSON rows parse straight off the formatter.
    let mut buf = Vec::new();
    {
        let mut fmt = Formatter::new(&mut buf, OutputFormat::Json, false);
        for row in &orphans {
            fmt.format_contract(&ContractOutput::from(row)).unwrap();
        }
    }
    let text = String::from_utf8(buf).unwrap();
    let v: serde_json::Value = serde_json::from_str(text.trim_end()).unwrap();
    assert_eq!(v["canonical_id"], "env::::DATABASE_URL");
    assert_eq!(v["kind"], "env");
    assert_eq!(v["role"], "consumer");
    assert_eq!(v["symbol"], "load", "the read sits inside load()");
}

// -- cross-repo resolution (TASK-084) --------------------------------------

/// Minimal JS source with one HTTP provider so registry fixtures have
/// at least one indexed contract row.
const REGISTRY_SRC: &str =
    "const app = express();\nfunction routes() {\n  app.get('/v1/users/:id', getUser);\n}\n";

/// Build a real indexed repo named `name` declaring `workspaces`, then
/// place its `index.db` + `meta.json` under `repos_dir/<hash>/` exactly
/// as `wonk init`'s central location looks. Returns `(TempDir, root)`;
/// keep the TempDir alive while the registry is scanned.
fn registry_repo(
    repos_dir: &std::path::Path,
    name: &str,
    workspaces: &[&str],
) -> (tempfile::TempDir, std::path::PathBuf) {
    registry_repo_files(repos_dir, name, workspaces, &[("src/app.js", REGISTRY_SRC)])
}

/// [`registry_repo`] with explicit source files.
fn registry_repo_files(
    repos_dir: &std::path::Path,
    name: &str,
    workspaces: &[&str],
    files: &[(&str, &str)],
) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(name);
    std::fs::create_dir_all(root.join(".git")).unwrap();
    for (path, content) in files {
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(root.join(parent)).unwrap();
        }
        std::fs::write(root.join(path), content).unwrap();
    }
    if !workspaces.is_empty() {
        std::fs::create_dir_all(root.join(".wonk")).unwrap();
        let list = workspaces
            .iter()
            .map(|w| format!("{w:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            root.join(".wonk/config.toml"),
            format!("[contracts]\nworkspace = [{list}]\n"),
        )
        .unwrap();
    }
    let config = crate::config::Config::load_with_paths(None, Some(&root)).unwrap();
    crate::pipeline::build_index_with_config(&root, true, &config).unwrap();
    let dest = repos_dir.join(crate::db::repo_hash(&root));
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::copy(root.join(".wonk/index.db"), dest.join("index.db")).unwrap();
    std::fs::copy(root.join(".wonk/meta.json"), dest.join("meta.json")).unwrap();
    (dir, root)
}

/// Build and register the querying repo, opening its local index the
/// way the CLI does. Returns `(TempDir, root, connection)`.
fn own_indexed_repo_files(
    repos_dir: &std::path::Path,
    name: &str,
    workspaces: &[&str],
    files: &[(&str, &str)],
) -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
    let (dir, root) = registry_repo_files(repos_dir, name, workspaces, files);
    let conn = crate::db::open_existing(&root.join(".wonk").join("index.db")).unwrap();
    (dir, root, conn)
}

/// Resolve the own repo's workspace the way dispatch will.
fn resolve_around(
    own_root: &std::path::Path,
    own_conn: &rusqlite::Connection,
    declared: &[String],
    repos_dir: &std::path::Path,
) -> WorkspaceResolution {
    let rows = list_contracts(own_conn, &ContractQuery::default()).unwrap();
    resolve_workspace(own_root, &rows, declared, repos_dir).unwrap()
}

#[test]
fn workspace_scope_defaults_to_repo_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("payments-api");

    let undeclared = workspace_scope(&[], &root);
    assert!(undeclared.declared.is_empty());
    assert_eq!(undeclared.repo_name, "payments-api");
    assert_eq!(undeclared.effective, vec!["payments-api".to_string()]);

    let declared = workspace_scope(&[" Payments ".to_string(), "Platform".to_string()], &root);
    assert_eq!(
        declared.effective,
        vec!["payments".to_string(), "platform".to_string()],
        "effective workspaces are trimmed and case-folded"
    );
}

#[test]
fn scan_registry_filters_on_workspace_intersection() {
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root) = registry_repo(repos_dir.path(), "own-repo", &["payments"]);
    let (_in_dir, _in_root) =
        registry_repo(repos_dir.path(), "sibling-in", &["payments", "platform"]);
    let (_out_dir, _out_root) = registry_repo(repos_dir.path(), "sibling-out", &["billing"]);

    let scope = workspace_scope(&["payments".to_string()], &own_root);
    let members = scan_registry(repos_dir.path(), &own_root, &scope.effective);
    let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, vec!["sibling-in"], "only workspace members survive");
}

#[test]
fn skips_self_and_unindexed_dirs() {
    let repos_dir = tempfile::tempdir().unwrap();
    // Leftover registry directory with no index.db.
    std::fs::create_dir_all(repos_dir.path().join("deadbeef")).unwrap();
    let (_own_dir, own_root) = registry_repo(repos_dir.path(), "own-repo", &["payments"]);
    let (_sib_dir, _sib_root) = registry_repo(repos_dir.path(), "team-mate", &["payments"]);

    let scope = workspace_scope(&["payments".to_string()], &own_root);
    let members = scan_registry(repos_dir.path(), &own_root, &scope.effective);
    let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["team-mate"],
        "own repo and index-less dirs are skipped"
    );
}

#[test]
fn undeclared_sibling_defaults_to_own_name() {
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root) = registry_repo(repos_dir.path(), "payments-api", &["payments"]);
    let (_sib_dir, _sib_root) = registry_repo(repos_dir.path(), "payments", &[]);

    let scope = workspace_scope(&["payments".to_string()], &own_root);
    let members = scan_registry(repos_dir.path(), &own_root, &scope.effective);
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].name, "payments");
    assert_eq!(
        members[0].workspaces,
        vec!["payments".to_string()],
        "undeclared sibling pairs through its own repo name"
    );
}

#[test]
fn sibling_connections_open_lazily_and_read_member_rows() {
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root) = registry_repo(repos_dir.path(), "own-repo", &["payments"]);
    let (_sib_dir, _sib_root) = registry_repo(repos_dir.path(), "sibling-in", &["payments"]);

    let scope = workspace_scope(&["payments".to_string()], &own_root);
    let members = scan_registry(repos_dir.path(), &own_root, &scope.effective);
    assert_eq!(members.len(), 1);
    let mut conns = SiblingConnections::new();
    // First use opens the member index; repeat calls reuse the cache.
    for _ in 0..2 {
        let conn = conns.connection(&members[0]).expect("member connection");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM contracts", [], |r| r.get(0))
            .unwrap();
        assert!(n >= 1, "member connection reads the sibling's contracts");
    }
}

#[test]
fn default_repos_dir_points_under_home() {
    if let Some(dir) = default_repos_dir() {
        assert!(
            dir.ends_with(std::path::Path::new(".wonk").join("repos")),
            "registry lives at $HOME/.wonk/repos, got {}",
            dir.display()
        );
    }
}

#[test]
fn never_opens_non_member_index() {
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root) = registry_repo(repos_dir.path(), "own-repo", &["payments"]);
    let (_in_dir, _in_root) = registry_repo(repos_dir.path(), "sibling-in", &["payments"]);
    let (_out_dir, out_root) = registry_repo(repos_dir.path(), "sibling-out", &["billing"]);
    // Corrupt the non-member's index: a scan that touched indexes
    // eagerly (or opened non-members) would fail here.
    std::fs::write(
        repos_dir
            .path()
            .join(crate::db::repo_hash(&out_root))
            .join("index.db"),
        b"garbage bytes, not sqlite",
    )
    .unwrap();

    let scope = workspace_scope(&["payments".to_string()], &own_root);
    let members = scan_registry(repos_dir.path(), &own_root, &scope.effective);
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].name, "sibling-in");
}

// -- resolution engine (TASK-084 phase 4) ----------------------------------

const HTTP_CONSUMER_USERS: &str =
    "async function load() { await fetch('https://api.io/v1/users'); }";
const HTTP_PROVIDER_USERS: &str = "const app = express();\napp.get('/v1/users', h);\n";
const HTTP_PROVIDER_ORDERS: &str = "const app = express();\napp.get('/v1/orders', h);\n";
const HTTP_PROVIDER_HEALTH: &str = "const app = express();\napp.get('/health', h);\n";
const GRPC_JS_CONSUMER: &str = "const grpc = require('@grpc/grpc-js');\nconst client = new user.UserServiceClient(host, creds);\nclient.getUser(arg, cb);\n";
const GRPC_PROTO_PROVIDER: &str = "syntax = \"proto3\";\n\nservice UserService {\n  rpc GetUser(GetUserRequest) returns (User);\n}\n";

#[test]
fn row_to_candidate_splits_canonical_id() {
    let grpc_row = ContractRow {
        canonical_id: "grpc::users.v1.UserService::GetUser".to_string(),
        kind: ContractKind::Grpc,
        role: ContractRole::Provider,
        symbol: Some("impl".to_string()),
        file: "src/server.rs".to_string(),
        line: 3,
        confidence: 1.0,
    };
    let cand = row_to_candidate(&grpc_row).expect("grpc id must split");
    assert_eq!(cand.kind, ContractKind::Grpc);
    assert_eq!(cand.qualifier, "users.v1.UserService");
    assert_eq!(cand.identifier, "GetUser");
    assert_eq!(cand.canonical_id, grpc_row.canonical_id);
    assert_eq!(cand.role, ContractRole::Provider);

    let env_row = ContractRow {
        canonical_id: "env::::DATABASE_URL".to_string(),
        kind: ContractKind::Env,
        role: ContractRole::Consumer,
        symbol: None,
        file: "src/config.js".to_string(),
        line: 1,
        confidence: 1.0,
    };
    let env_cand = row_to_candidate(&env_row).expect("env id must split");
    assert_eq!(env_cand.qualifier, "");
    assert_eq!(env_cand.identifier, "DATABASE_URL");
}

#[test]
fn exact_link_one_pair_same_workspace() {
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root, own_conn) = own_indexed_repo_files(
        repos_dir.path(),
        "own-api",
        &["payments"],
        &[("src/client.js", HTTP_CONSUMER_USERS)],
    );
    let (_sib_dir, _sib_root) = registry_repo_files(
        repos_dir.path(),
        "users-svc",
        &["payments"],
        &[("src/app.js", HTTP_PROVIDER_USERS)],
    );

    let r = resolve_around(
        &own_root,
        &own_conn,
        &["payments".to_string()],
        repos_dir.path(),
    );
    assert_eq!(r.links.len(), 1, "got {:?}", r.links);
    let link = &r.links[0];
    assert_eq!(link.basis, LinkBasis::ExactId);
    assert_eq!(link.provider.repo, "users-svc");
    assert_eq!(link.consumer.repo, "own-api");
    assert_eq!(link.provider.canonical_id, "http::GET::/v1/users");
    assert_eq!(link.consumer.canonical_id, "http::GET::/v1/users");
    assert_eq!(
        r.status
            .get(&(
                link.consumer.canonical_id.clone(),
                link.consumer.file.clone(),
                link.consumer.line
            ))
            .copied(),
        Some(ConsumerStatus::Linked),
        "matched consumers are never orphans"
    );
}

#[test]
fn no_link_across_workspaces() {
    let repos_dir = tempfile::tempdir().unwrap();
    // Both repos expose GET /health, but in disjoint workspaces.
    let (_own_dir, own_root, own_conn) = own_indexed_repo_files(
        repos_dir.path(),
        "own-api",
        &["payments"],
        &[("src/app.js", HTTP_PROVIDER_HEALTH)],
    );
    let (_sib_dir, _sib_root) = registry_repo_files(
        repos_dir.path(),
        "billing-svc",
        &["billing"],
        &[("src/app.js", HTTP_PROVIDER_HEALTH)],
    );

    let r = resolve_around(
        &own_root,
        &own_conn,
        &["payments".to_string()],
        repos_dir.path(),
    );
    assert!(r.links.is_empty(), "got {:?}", r.links);
    assert!(r.siblings.is_empty());
}

#[test]
fn link_directions_both_ways() {
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root, own_conn) = own_indexed_repo_files(
        repos_dir.path(),
        "own-api",
        &["payments"],
        &[
            ("src/a.js", HTTP_PROVIDER_ORDERS),
            ("src/b.js", HTTP_CONSUMER_USERS),
        ],
    );
    let (_sib_dir, _sib_root) = registry_repo_files(
        repos_dir.path(),
        "sib-svc",
        &["payments"],
        &[
            ("src/a.js", "const d = axios.get('/v1/orders');"),
            ("src/b.js", HTTP_PROVIDER_USERS),
        ],
    );

    let r = resolve_around(
        &own_root,
        &own_conn,
        &["payments".to_string()],
        repos_dir.path(),
    );
    assert_eq!(r.links.len(), 2, "got {:?}", r.links);
    let own_as_provider = r
        .links
        .iter()
        .any(|l| l.provider.repo == "own-api" && l.consumer.repo == "sib-svc");
    let own_as_consumer = r
        .links
        .iter()
        .any(|l| l.consumer.repo == "own-api" && l.provider.repo == "sib-svc");
    assert!(own_as_provider, "own provider consumed by sibling");
    assert!(own_as_consumer, "own consumer served by sibling");
}

#[test]
fn multi_workspace_repo_links_both_and_dedupes_overlap() {
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root, own_conn) = own_indexed_repo_files(
        repos_dir.path(),
        "own-api",
        &["payments", "platform"],
        &[
            ("src/a.js", HTTP_CONSUMER_USERS),
            (
                "src/b.js",
                "async function load() { await fetch('https://api.io/v1/orders'); }",
            ),
        ],
    );
    // Same two workspaces: the shared overlap must not double-link.
    let (_both_dir, _both_root) = registry_repo_files(
        repos_dir.path(),
        "both-svc",
        &["payments", "platform"],
        &[("src/app.js", HTTP_PROVIDER_USERS)],
    );
    let (_plat_dir, _plat_root) = registry_repo_files(
        repos_dir.path(),
        "plat-svc",
        &["platform"],
        &[("src/app.js", HTTP_PROVIDER_ORDERS)],
    );

    let r = resolve_around(
        &own_root,
        &own_conn,
        &["payments".to_string(), "platform".to_string()],
        repos_dir.path(),
    );
    assert_eq!(r.links.len(), 2, "got {:?}", r.links);
    let to_both = r
        .links
        .iter()
        .filter(|l| l.provider.repo == "both-svc")
        .count();
    assert_eq!(to_both, 1, "two shared workspaces still yield one link");
    assert!(r.links.iter().any(|l| l.provider.repo == "plat-svc"));
}

#[test]
fn rpc_relaxed_cross_repo_link() {
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root, own_conn) = own_indexed_repo_files(
        repos_dir.path(),
        "own-api",
        &["payments"],
        &[("src/client.js", GRPC_JS_CONSUMER)],
    );
    let (_sib_dir, _sib_root) = registry_repo_files(
        repos_dir.path(),
        "proto-svc",
        &["payments"],
        &[("proto/user.proto", GRPC_PROTO_PROVIDER)],
    );

    let r = resolve_around(
        &own_root,
        &own_conn,
        &["payments".to_string()],
        repos_dir.path(),
    );
    assert_eq!(r.links.len(), 1, "got {:?}", r.links);
    let link = &r.links[0];
    assert_eq!(
        link.basis,
        LinkBasis::Rpc(RpcMatchBasis::PackageQualifiedService)
    );
    assert_eq!(link.provider.canonical_id, "grpc::UserService::GetUser");
    assert_eq!(
        link.consumer.canonical_id,
        "grpc::user.UserService::getUser"
    );
    assert_eq!(
        r.status
            .get(&(
                link.consumer.canonical_id.clone(),
                link.consumer.file.clone(),
                link.consumer.line
            ))
            .copied(),
        Some(ConsumerStatus::Linked),
        "a relaxed match still satisfies the consumer"
    );
}

#[test]
fn rpc_pass_collision_identical_grpc_rows_two_repos() {
    // Two repos hold IDENTICAL (grpc id, file, line) provider rows —
    // the same proto copied into two services of one workspace — and
    // the sibling additionally holds the folded client call. The join
    // side expansion covers both repos' rows, so this pins that the
    // own-status sets key only own rows: the sibling's member row can
    // never satisfy an own row, and the own provider's consumed status
    // comes from its OWN row joining the sibling's consumer.
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root, own_conn) = own_indexed_repo_files(
        repos_dir.path(),
        "own-api",
        &["payments"],
        &[("proto/user.proto", GRPC_PROTO_PROVIDER)],
    );
    let (_sib_dir, _sib_root) = registry_repo_files(
        repos_dir.path(),
        "proto-svc",
        &["payments"],
        &[
            ("proto/user.proto", GRPC_PROTO_PROVIDER),
            ("src/client.js", GRPC_JS_CONSUMER),
        ],
    );

    let r = resolve_around(
        &own_root,
        &own_conn,
        &["payments".to_string()],
        repos_dir.path(),
    );
    // Own's identical copy genuinely joins the sibling's folded
    // consumer: exactly one link, own as the serving side.
    assert_eq!(r.links.len(), 1, "got {:?}", r.links);
    assert_eq!(r.links[0].provider.repo, "own-api");
    assert_eq!(r.links[0].consumer.repo, "proto-svc");
    assert_eq!(r.links[0].provider.file, "proto/user.proto");
    // The consumed status is owned by own's row, not the sibling's
    // identical-key member row.
    assert!(
        r.unused_providers.is_empty(),
        "the joined provider is consumed: {:?}",
        r.unused_providers
    );

    // Control: with the sibling absent (fresh registry), the same own
    // row has no consumer anywhere and must report as unused — the
    // collision case above must not blur this boundary.
    let repos2 = tempfile::tempdir().unwrap();
    let (_own2_dir, own2_root, own2_conn) = own_indexed_repo_files(
        repos2.path(),
        "own-api",
        &["payments"],
        &[("proto/user.proto", GRPC_PROTO_PROVIDER)],
    );
    let r2 = resolve_around(
        &own2_root,
        &own2_conn,
        &["payments".to_string()],
        repos2.path(),
    );
    assert_eq!(
        r2.unused_providers.len(),
        1,
        "got {:?}",
        r2.unused_providers
    );
    assert_eq!(
        r2.unused_providers[0].canonical_id,
        "grpc::UserService::GetUser"
    );
}

#[test]
fn consumer_status_orphan_vs_unscoped_vs_linked() {
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root, own_conn) = own_indexed_repo_files(
        repos_dir.path(),
        "lone-api",
        &[],
        &[
            (
                "src/client.js",
                "async function a() { await fetch('https://api.io/v1/users'); }\nasync function b() { await fetch('https://api.io/v1/orders'); }",
            ),
            ("src/app.js", HTTP_PROVIDER_ORDERS),
        ],
    );

    // Undeclared: no match AND nothing declared -> Unscoped (AR-025).
    let undeclared = resolve_around(&own_root, &own_conn, &[], repos_dir.path());
    assert_eq!(
        undeclared
            .status
            .get(&(
                "http::GET::/v1/users".to_string(),
                "src/client.js".to_string(),
                1
            ))
            .copied(),
        Some(ConsumerStatus::Unscoped)
    );
    assert_eq!(
        undeclared
            .status
            .get(&(
                "http::GET::/v1/orders".to_string(),
                "src/client.js".to_string(),
                2
            ))
            .copied(),
        Some(ConsumerStatus::Linked)
    );

    // Declared with no matching sibling: same row is now an Orphan.
    let declared = resolve_around(
        &own_root,
        &own_conn,
        &["payments".to_string()],
        repos_dir.path(),
    );
    assert_eq!(
        declared
            .status
            .get(&(
                "http::GET::/v1/users".to_string(),
                "src/client.js".to_string(),
                1
            ))
            .copied(),
        Some(ConsumerStatus::Orphan)
    );
}

/// ~40-contract fixture: 20 routes + 20 outbound calls. `prov` and
/// `cons` steer which repo's routes the calls target.
fn bulk_contract_source(prov: usize, cons: usize) -> String {
    let mut src = String::from("const app = express();\nfunction routes() {\n");
    for i in 0..20 {
        src.push_str(&format!("  app.get('/v{prov}/p{i}', h);\n"));
    }
    src.push_str("}\nasync function callers() {\n");
    for i in 0..20 {
        src.push_str(&format!("  await fetch('https://api.io/v{cons}/p{i}');\n"));
    }
    src.push_str("}\n");
    src
}

#[test]
fn link_resolution_typical_eleven_repo_set_has_exact_links() {
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root, own_conn) = own_indexed_repo_files(
        repos_dir.path(),
        "own-api",
        &["payments"],
        &[("src/app.js", &bulk_contract_source(98, 0))],
    );
    // Keep every sibling TempDir alive through the scan below: a repo
    // deleted mid-test loses its registry entry (meta.json guards fail).
    let mut keepalive = Vec::new();
    for i in 0..10 {
        keepalive.push(registry_repo_files(
            repos_dir.path(),
            &format!("sibling-{i}"),
            &["payments"],
            &[("src/app.js", &bulk_contract_source(i, 90 + i))],
        ));
    }

    let rows = list_contracts(&own_conn, &ContractQuery::default()).unwrap();
    assert!(
        rows.len() >= 40,
        "fixture must hold a typical set: {}",
        rows.len()
    );

    let resolution =
        resolve_workspace(&own_root, &rows, &["payments".into()], repos_dir.path()).unwrap();
    assert_eq!(resolution.siblings.len(), 10);
    assert_eq!(resolution.links.len(), 40);
    assert_eq!(resolution.status.len(), 20);
    assert!(
        resolution
            .status
            .values()
            .all(|s| *s == ConsumerStatus::Linked)
    );
    assert!(resolution.unused_providers.is_empty());
    let identities: std::collections::HashSet<_> = resolution
        .links
        .iter()
        .map(|link| {
            (
                link.provider.repo.clone(),
                link.provider.canonical_id.clone(),
                link.consumer.repo.clone(),
                link.consumer.canonical_id.clone(),
            )
        })
        .collect();
    assert_eq!(identities.len(), 40);
    for i in 0..20 {
        assert!(identities.contains(&(
            "sibling-0".into(),
            format!("http::GET::/v0/p{i}"),
            "own-api".into(),
            format!("http::GET::/v0/p{i}")
        )));
        assert!(identities.contains(&(
            "own-api".into(),
            format!("http::GET::/v98/p{i}"),
            "sibling-8".into(),
            format!("http::GET::/v98/p{i}")
        )));
    }
}

#[test]
fn unused_providers_computed() {
    let repos_dir = tempfile::tempdir().unwrap();
    let (_own_dir, own_root, own_conn) = own_indexed_repo_files(
        repos_dir.path(),
        "own-api",
        &["payments"],
        &[
            (
                "src/app.js",
                "const app = express();\napp.get('/v1/orders', h);\napp.get('/v1/health', h);\napp.get('/v1/metrics', h);\n",
            ),
            ("src/client.js", "const d = axios.get('/v1/orders');"),
        ],
    );
    // /v1/health is consumed by the sibling; /v1/metrics by nobody.
    let (_sib_dir, _sib_root) = registry_repo_files(
        repos_dir.path(),
        "sib-svc",
        &["payments"],
        &[("src/client.js", "const h = axios.get('/v1/health');")],
    );

    let r = resolve_around(
        &own_root,
        &own_conn,
        &["payments".to_string()],
        repos_dir.path(),
    );
    let unused: Vec<&str> = r
        .unused_providers
        .iter()
        .map(|p| p.canonical_id.as_str())
        .collect();
    assert_eq!(unused, vec!["http::GET::/v1/metrics"], "got {unused:?}");
}
