use super::*;

#[test]
fn method_uppercases_raw_verb() {
    assert_eq!(normalize_method("get"), "GET");
    assert_eq!(normalize_method("GET"), "GET");
    assert_eq!(normalize_method("Delete"), "DELETE");
    assert_eq!(normalize_method("patch"), "PATCH");
}

#[test]
fn method_catchalls_map_to_any() {
    assert_eq!(normalize_method("any"), "ANY");
    assert_eq!(normalize_method("ALL"), "ANY");
    assert_eq!(normalize_method("match"), "ANY");
    assert_eq!(normalize_method("Any"), "ANY");
}

#[test]
fn method_empty_maps_to_any() {
    assert_eq!(normalize_method(""), "ANY");
}

#[test]
fn method_nonverb_still_uppercases() {
    assert_eq!(normalize_method("NewRequest"), "NEWREQUEST");
}

#[test]
fn canonical_http() {
    assert_eq!(
        canonical_contract_id(ContractKind::Http, "GET", "/v1/users/{p1}"),
        "http::GET::/v1/users/{p1}"
    );
}

#[test]
fn canonical_env_empty_qualifier() {
    assert_eq!(
        canonical_contract_id(ContractKind::Env, "", "DATABASE_URL"),
        "env::::DATABASE_URL"
    );
}

#[test]
#[should_panic(expected = "contract identifier must be non-empty")]
fn canonical_rejects_empty_identifier() {
    canonical_contract_id(ContractKind::Http, "GET", "  ");
}

// -- topic normalization (TASK-087, PRD-CTR-REQ-002) ----------------------

/// Matrix: raw topic literal -> normalized identifier (`None` = rejected).
const NORMALIZE_TOPIC_CASES: &[(&str, Option<&str>)] = &[
    (" orders.created ", Some("orders.created")),
    ("'orders.created'", Some("orders.created")),
    ("orders:created", Some("orders.created")),
    ("orders/created", Some("orders.created")),
    ("orders..created", Some("orders.created")),
    ("orders.created.v2", Some("orders.created.v2")),
    ("/topic/orders", Some("topic.orders")),
    ("topic:orders", Some("topic.orders")),
    (".:orders.:created:.", Some("orders.created")),
    ("EmailWorker", Some("EmailWorker")),
    ("orders.created", Some("orders.created")),
    ("orders.*", Some("orders.*")),
    ("orders.>", Some("orders.>")),
    ("order-created_v2", Some("order-created_v2")),
    ("hello world", None),
    ("", None),
    ("   ", None),
    ("f\"{env}-orders\"", None),
    ("${prefix}.orders", None),
    ("orders.#fragment", None),
    ("$topic", None),
    ("orders.{env}.created", None),
];

#[test]
fn topic_matrix_normalizes_and_rejects() {
    for (raw, want) in NORMALIZE_TOPIC_CASES {
        assert_eq!(
            &normalize_topic(raw),
            &want.map(|w| w.to_string()),
            "normalize_topic({raw:?})"
        );
    }
}

#[test]
fn topic_trims_quotes_and_whitespace() {
    assert_eq!(
        normalize_topic("  'orders.created'  "),
        Some("orders.created".into())
    );
    assert_eq!(
        normalize_topic("\"orders.created\""),
        Some("orders.created".into())
    );
}

#[test]
fn topic_preserves_case_and_segment_order() {
    assert_eq!(
        normalize_topic("Orders.Created"),
        Some("Orders.Created".into())
    );
    assert_eq!(normalize_topic("a.b.c"), Some("a.b.c".into()));
}

#[test]
fn topic_wildcards_stay_literal() {
    // NATS wildcards never exact-match a concrete topic — by design.
    assert_eq!(normalize_topic("orders.*"), Some("orders.*".into()));
    assert_eq!(normalize_topic("orders.>"), Some("orders.>".into()));
}

#[test]
fn canonical_queue_with_broker_qualifier() {
    assert_eq!(
        canonical_contract_id(ContractKind::Queue, "kafka", "orders.created"),
        "queue::kafka::orders.created"
    );
}

#[test]
fn canonical_queue_unknown_broker_has_empty_qualifier() {
    assert_eq!(
        canonical_contract_id(ContractKind::Queue, "", "orders.created"),
        "queue::::orders.created"
    );
}

#[test]
fn canonical_websocket_and_job_empty_qualifier() {
    assert_eq!(
        canonical_contract_id(ContractKind::WebSocket, "", "chat.message"),
        "websocket::::chat.message"
    );
    assert_eq!(
        canonical_contract_id(ContractKind::Job, "", "email-send"),
        "job::::email-send"
    );
}

// -- stage matrices (PRD-CTR-REQ-003, §9.1) -------------------------------

/// (input, expected) pairs for one pipeline stage.
const STAGE_TRIM_CASES: &[(&str, &str)] = &[
    ("/users", "/users"),
    ("  /users  ", "/users"),
    ("\"/users\"", "/users"),
    ("'/users'", "/users"),
    ("`/users`", "/users"),
    (" \" /users ' `", "/users"),
    // Unicode whitespace (ideographic space U+3000) trims like ASCII.
    ("/users\u{3000}", "/users"),
    ("", ""),
    ("\"\"", ""),
];

#[test]
fn stage1_trim_variants() {
    for (raw, want) in STAGE_TRIM_CASES {
        assert_eq!(&stage_trim(raw), want, "stage_trim({raw:?})");
    }
}

const STAGE_SCHEME_CASES: &[(&str, &str)] = &[
    ("http://api.example.com/v1/users", "/v1/users"),
    ("https://x.io/a/b", "/a/b"),
    // Protocol-relative URL.
    ("//h/x", "/x"),
    // No scheme — untouched.
    ("h/x", "h/x"),
    ("users", "users"),
    // Query and fragment stripped wherever they appear.
    ("/users?id=1", "/users"),
    ("/users#frag", "/users"),
    (
        "http://api.example.com/v1/users?fields=all#top",
        "/v1/users",
    ),
    ("users?v=2", "users"),
    ("http://api.example.com/v1/users#only", "/v1/users"),
    // Bare authority resolves to the root path.
    ("http://example.com", "/"),
    // Ruby interpolation is not a fragment.
    ("/api/#{id}", "/api/#{id}"),
];

#[test]
fn stage2_scheme_authority_query_fragment() {
    for (raw, want) in STAGE_SCHEME_CASES {
        assert_eq!(
            &stage_strip_scheme_authority(raw),
            want,
            "stage_strip_scheme_authority({raw:?})"
        );
    }
}

const STAGE_BASE_CASES: &[(&str, &str)] = &[
    ("${API_URL}/users", "users"),
    ("/${BASE}/users", "users"),
    ("$BASE/users", "users"),
    ("{BASE_URL}/users", "{BASE_URL}/users"),
    // Interpolation that is not the leading segment stays.
    ("/v1/${BASE}/users", "/v1/${BASE}/users"),
    // Lone token (no following path) stays — it is the route itself.
    ("/{id}", "/{id}"),
    ("${API_URL}", "${API_URL}"),
    // Every literal brace token stays for positional parameter rewriting.
    ("/{tenant}/users", "/{tenant}/users"),
    ("/{org}/{repo}", "/{org}/{repo}"),
    ("{BASE_URL}/x", "{BASE_URL}/x"),
    ("/{BASE}/x", "/{BASE}/x"),
    ("${A}/x", "x"),
    ("$A/x", "x"),
];

#[test]
fn stage3_leading_base_interpolation() {
    for (raw, want) in STAGE_BASE_CASES {
        assert_eq!(
            &stage_strip_base_interpolation(raw),
            want,
            "stage_strip_base_interpolation({raw:?})"
        );
    }
}

const STAGE_PLACEHOLDER_CASES: &[(&str, &str, &[&str])] = &[
    ("/users/:id", "/users/{p1}", &["id"]),
    ("/users/${id}", "/users/{p1}", &["id"]),
    ("/users/$id", "/users/{p1}", &["id"]),
    ("/$id/posts", "/{p1}/posts", &["id"]),
    ("/users/{id}", "/users/{p1}", &["id"]),
    ("/users/<id>", "/users/{p1}", &["id"]),
    // Flask converter: type stripped, name kept.
    ("/users/<int:id>", "/users/{p1}", &["id"]),
    ("/files/<path:sub>", "/files/{p1}", &["sub"]),
    // Ruby interpolation.
    ("/users/#{id}", "/users/{p1}", &["id"]),
    // Colon is a placeholder only at the start of a segment.
    ("/a:b", "/a:b", &[]),
    ("/a/:b", "/a/{p1}", &["b"]),
    (":id", "{p1}", &["id"]),
    // Rails optional-format group dropped.
    ("/users(.:format)", "/users", &[]),
    ("/users(.:format)/:id", "/users/{p1}", &["id"]),
    // No placeholders.
    ("/v1/users", "/v1/users", &[]),
    // Member-expression names keep their dots.
    ("/users/${user.id}", "/users/{p1}", &["user.id"]),
    // `$name` grabs the whole identifier.
    ("/u/$user_id/x", "/u/{p1}/x", &["user_id"]),
];

#[test]
fn stage4_placeholder_rewrites() {
    for (raw, want_path, want_names) in STAGE_PLACEHOLDER_CASES {
        let (marked, names) = stage_rewrite_placeholders(raw);
        let positional = stage_positional_markers(&marked);
        assert_eq!(
            &positional, want_path,
            "placeholder pipeline for {raw:?} (marked {marked:?})"
        );
        assert_eq!(&names, want_names, "names for {raw:?}");
    }
}

#[test]
fn stage5_repeated_names_keep_both_positions() {
    let (marked, names) = stage_rewrite_placeholders("/{id}/docs/{id}");
    assert_eq!(stage_positional_markers(&marked), "/{p1}/docs/{p2}");
    assert_eq!(names, vec!["id", "id"]);
}

const STAGE_SHAPE_CASES: &[(&str, Option<&str>)] = &[
    ("users", Some("/users")),
    ("/users", Some("/users")),
    ("/a//b", Some("/a/b")),
    ("//a//b//", Some("/a/b")),
    ("/a/b/", Some("/a/b")),
    ("/", Some("/")),
    ("", None),
];

#[test]
fn stage6_shape_edges() {
    for (raw, want) in STAGE_SHAPE_CASES {
        assert_eq!(
            stage_ensure_shape(raw),
            want.map(str::to_string),
            "stage_ensure_shape({raw:?})"
        );
    }
}

// -- end-to-end ordering (stages compose in fixed order) ------------------

fn norm(raw: &str) -> Option<NormalizedPath> {
    normalize_http_path(raw)
}

#[test]
fn pipeline_absolute_url_equals_relative() {
    assert_eq!(
        norm("http://api.example.com/v1/users"),
        Some(NormalizedPath {
            path: "/v1/users".into(),
            params: vec![]
        })
    );
}

#[test]
fn pipeline_base_interpolation_matches_colon_param() {
    let a = norm("${API_URL}/v1/tags/${id}");
    let b = norm("/v1/tags/:id");
    assert_eq!(a, b);
    assert_eq!(
        a,
        Some(NormalizedPath {
            path: "/v1/tags/{p1}".into(),
            params: vec!["id".into()]
        })
    );
}

#[test]
fn pipeline_positional_ids_equal_while_names_differ() {
    let a = norm("/workspaces/{wid}/tags/{id}");
    let b = norm("/workspaces/{workspaceId}/tags/{id}");
    assert_eq!(
        a.as_ref().map(|n| n.path.clone()),
        b.as_ref().map(|n| n.path.clone())
    );
    assert_eq!(
        a.map(|n| n.params),
        Some(vec!["wid".to_string(), "id".to_string()])
    );
    assert_eq!(
        b.map(|n| n.params),
        Some(vec!["workspaceId".to_string(), "id".to_string()])
    );
}

#[test]
fn pipeline_query_interpolation_is_not_a_param() {
    // Stage 2 strips the query before stage 4 could see `${q}`.
    assert_eq!(
        norm("/users?${q}"),
        Some(NormalizedPath {
            path: "/users".into(),
            params: vec![]
        })
    );
}

#[test]
fn pipeline_quoted_padded_url() {
    assert_eq!(
        norm("  \"https://api.io/v1/users?x=1\"  "),
        Some(NormalizedPath {
            path: "/v1/users".into(),
            params: vec![]
        })
    );
}

#[test]
fn pipeline_lone_base_token_becomes_single_param_route() {
    assert_eq!(
        norm("${API_URL}"),
        Some(NormalizedPath {
            path: "/{p1}".into(),
            params: vec!["API_URL".into()]
        })
    );
}

#[test]
fn pipeline_leading_curly_param_is_param_not_base() {
    // A leading `{name}` path parameter must survive stage 3 and be
    // rewritten positionally by stage 4 with its name retained.
    assert_eq!(
        norm("/{tenant}/users"),
        Some(NormalizedPath {
            path: "/{p1}/users".into(),
            params: vec!["tenant".into()]
        })
    );
    assert_eq!(
        norm("/{org}/{repo}"),
        Some(NormalizedPath {
            path: "/{p1}/{p2}".into(),
            params: vec!["org".into(), "repo".into()]
        })
    );
    // Explicit dollar interpolation strips; bare braces remain route parameters.
    assert_eq!(norm("${A}/x").map(|n| n.path), Some("/x".into()));
    assert_eq!(norm("$A/x").map(|n| n.path), Some("/x".into()));
    assert_eq!(norm("{BASE_URL}/x").map(|n| n.path), Some("/{p1}/x".into()));
}
