use std::path::Path;
use wonk::contracts::{
    self, ConsumerStatus, ContractOptions, ContractQuery, ContractRow, DocumentKind,
};
use wonk::indexer::{Lang, get_parser};
use wonk::types::{ContractKind, ContractRole};

fn extracted(lang: Lang, source: &str) -> Vec<wonk::types::ContractCandidate> {
    let tree = get_parser(lang).parse(source, None).unwrap();
    contracts::extract_contracts(&tree, source, lang, &ContractOptions::default())
}

#[test]
fn literal_brace_route_parameters_survive_while_real_consumer_bases_strip() {
    let providers = extracted(
        Lang::Python,
        "@app.get(\"/{user_id}/orders\")\ndef orders(): pass\n@app.get(\"/orders\")\ndef all_orders(): pass\n@app.get(\"/{USER_ID}/orders\")\ndef upper(): pass\n",
    );
    let ids: Vec<_> = providers.iter().map(|c| c.canonical_id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "http::GET::/{p1}/orders",
            "http::GET::/orders",
            "http::GET::/{p1}/orders"
        ]
    );
    assert_eq!(providers[0].params[0].name, "user_id");
    let consumers = extracted(Lang::JavaScript, "fetch(`${BASE_URL}/${userId}/orders`);");
    assert_eq!(consumers[0].canonical_id, providers[0].canonical_id);
    let python = extracted(Lang::Python, "requests.get(f'{BASE_URL}/{user_id}/orders')");
    assert_eq!(python[0].canonical_id, providers[0].canonical_id);
    assert_eq!(
        contracts::normalize_http_path("/{USER_ID}/orders")
            .unwrap()
            .path,
        "/{p1}/orders"
    );
    let dirs = tempfile::tempdir().unwrap();
    let registry = dirs.path().join("registry");
    let provider_root = dirs.path().join("route-provider");
    let consumer_root = dirs.path().join("route-consumer");
    let mut parameterized = row(&providers[0].canonical_id, ContractRole::Provider, 1);
    parameterized.kind = ContractKind::Http;
    let mut unparameterized = row(&providers[1].canonical_id, ContractRole::Provider, 2);
    unparameterized.kind = ContractKind::Http;
    let own = [parameterized, unparameterized];
    let mut call = row(&consumers[0].canonical_id, ContractRole::Consumer, 3);
    call.kind = ContractKind::Http;
    let _provider_conn = registered(&provider_root, &registry, &own);
    let _consumer_conn = registered(&consumer_root, &registry, &[call]);
    let resolution =
        contracts::resolve_workspace(&provider_root, &own, &["shared".into()], &registry).unwrap();
    assert_eq!(resolution.links.len(), 1);
    assert_eq!(
        resolution.links[0].provider.canonical_id,
        "http::GET::/{p1}/orders"
    );
    assert_eq!(resolution.unused_providers.len(), 1);
    assert_eq!(
        resolution.unused_providers[0].canonical_id,
        "http::GET::/orders"
    );
}

#[test]
fn document_and_websocket_literal_braces_are_parameters() {
    let source =
        "openapi: 3.0.0\npaths:\n  /{USER_ID}/orders:\n    get:\n      operationId: orders\n";
    let rows = contracts::extract_document_contracts(
        DocumentKind::OpenApi,
        source,
        &ContractOptions::default(),
    );
    assert_eq!(rows[0].identifier, "/{p1}/orders");
    let sockets = extracted(Lang::JavaScript, "app.ws('/{USER_ID}/orders', handler);");
    assert_eq!(sockets[0].identifier, "/{p1}/orders");
}

fn row(id: &str, role: ContractRole, line: usize) -> ContractRow {
    ContractRow {
        canonical_id: id.into(),
        kind: ContractKind::Grpc,
        role,
        symbol: None,
        file: "src/service.rs".into(),
        line,
        confidence: 1.0,
    }
}

fn registered(root: &Path, registry: &Path, rows: &[ContractRow]) -> rusqlite::Connection {
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let dir = registry.join(wonk::db::repo_hash(root));
    std::fs::create_dir_all(&dir).unwrap();
    let index = dir.join("index.db");
    let conn = wonk::db::open(&index).unwrap();
    for row in rows {
        conn.execute("INSERT INTO contracts(canonical_id,kind,role,file,line,confidence) VALUES(?1,?2,?3,?4,?5,?6)",
            rusqlite::params![row.canonical_id,row.kind.as_str(),row.role.as_str(),row.file,row.line as i64,row.confidence]).unwrap();
    }
    wonk::db::write_meta(&index, root, &[], &["shared".into(), "SHARED".into()]).unwrap();
    conn
}

#[test]
fn equal_basename_roots_keep_rpc_links_consumed_status_and_blast_consumers() {
    let dir = tempfile::tempdir().unwrap();
    let registry = dir.path().join("registry");
    let own = dir.path().join("org_a/service");
    let sibling = dir.path().join("org_b/service");
    let provider_id = "grpc::users.v1.UserService::GetUser";
    let providers = [row(provider_id, ContractRole::Provider, 1)];
    let consumers = [row(
        "grpc::UserService::get_user",
        ContractRole::Consumer,
        2,
    )];
    let own_conn = registered(&own, &registry, &providers);
    let _sibling_conn = registered(&sibling, &registry, &consumers);
    let declared = vec!["shared".into(), "SHARED".into()];
    let resolution = contracts::resolve_workspace(&own, &providers, &declared, &registry).unwrap();
    assert_eq!(resolution.siblings.len(), 1);
    assert_eq!(
        resolution.links.len(),
        1,
        "different roots with equal basenames must link"
    );
    assert_eq!(
        resolution.links[0].provider.repo,
        std::fs::canonicalize(&own).unwrap().to_string_lossy()
    );
    assert_eq!(
        resolution.links[0].consumer.repo,
        std::fs::canonicalize(&sibling).unwrap().to_string_lossy()
    );
    assert!(resolution.unused_providers.is_empty());
    let reversed =
        contracts::resolve_workspace(&sibling, &consumers, &declared, &registry).unwrap();
    assert_eq!(
        reversed.status.values().copied().collect::<Vec<_>>(),
        [ConsumerStatus::Linked]
    );
    let impacted = wonk::blast::resolve_cross_repo_consumers(
        &own,
        &own_conn,
        &declared,
        &registry,
        &[provider_id.into()],
    )
    .unwrap();
    assert_eq!(impacted.len(), 1);
    assert_eq!(impacted[0].repo, resolution.links[0].consumer.repo);
    let stored = contracts::list_contracts(&own_conn, &ContractQuery::default()).unwrap();
    assert_eq!(stored.len(), 1);
}

#[test]
fn distant_angle_closers_preserve_source_document_and_websocket_paths() {
    for fragment in ["/<", "/<a", "/<é"] {
        let raw = format!("{}>", fragment.repeat(256));
        let expected = contracts::normalize_http_path(&raw).unwrap();
        let http = extracted(Lang::JavaScript, &format!("app.get('{raw}', handler);"));
        let websocket = extracted(Lang::JavaScript, &format!("app.ws('{raw}', handler);"));
        let document = contracts::extract_document_contracts(
            DocumentKind::OpenApi,
            &format!("openapi: 3.0.0\npaths:\n  '{raw}':\n    get:\n      operationId: route\n"),
            &ContractOptions::default(),
        );
        assert_eq!(http[0].identifier, expected.path);
        assert_eq!(websocket[0].identifier, expected.path);
        assert_eq!(document[0].identifier, expected.path);
    }
}

#[test]
fn angle_converters_keep_final_name_and_reject_only_name_slashes() {
    for (raw, path, names) in [
        ("/<int:id>/<uuid:item>", "/{p1}/{p2}", vec!["id", "item"]),
        ("/<path:a/b:é>", "/{p1}", vec!["é"]),
        ("/<a:b:c>", "/{p1}", vec!["c"]),
        ("/<path:a/b>", "/<path:a/b>", vec![]),
        ("/<int:>", "/<int:>", vec![]),
    ] {
        let result = contracts::normalize_http_path(raw).unwrap();
        assert_eq!(result.path, path);
        assert_eq!(
            result.params.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
            names
        );
    }
}
