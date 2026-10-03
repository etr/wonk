use super::*;
use crate::contracts::source::extract_test_helpers::*;

// -- document kinds (TASK-088, DQ1) ----------------------------------------

#[test]
fn document_kind_extension_gate() {
    use crate::contracts::{DocumentKind, document_kind};
    use std::path::Path;
    assert_eq!(
        document_kind(Path::new("proto/users.proto")),
        Some(DocumentKind::Proto)
    );
    assert_eq!(
        document_kind(Path::new("schema.graphql")),
        Some(DocumentKind::Graphql)
    );
    assert_eq!(
        document_kind(Path::new("queries.gql")),
        Some(DocumentKind::Graphql)
    );
    // .yaml/.yml/.json pass the extension gate; the OpenAPI content
    // sniff inside extract_document_contracts decides their fate.
    assert_eq!(
        document_kind(Path::new("api.yaml")),
        Some(DocumentKind::OpenApi)
    );
    assert_eq!(
        document_kind(Path::new("api.yml")),
        Some(DocumentKind::OpenApi)
    );
    assert_eq!(
        document_kind(Path::new("openapi.json")),
        Some(DocumentKind::OpenApi)
    );
    assert_eq!(document_kind(Path::new("README.md")), None);
    assert_eq!(document_kind(Path::new("main.rs")), None);
    assert_eq!(document_kind(Path::new("plain")), None);
}

#[test]
fn document_kind_language_names() {
    use crate::contracts::DocumentKind;
    assert_eq!(DocumentKind::Proto.as_str(), "Proto");
    assert_eq!(DocumentKind::Graphql.as_str(), "GraphQL");
    assert_eq!(DocumentKind::OpenApi.as_str(), "OpenApi");
}

#[test]
fn scannable_document_kind_bounds_the_negative_case() {
    use crate::contracts::{DocumentKind, scannable_document_kind};
    use std::path::Path;
    // Lock/data file names never carry contracts — skipped by name,
    // whatever their extension or content.
    for name in [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "composer.lock",
        "Cargo.lock",
        "poetry.lock",
    ] {
        assert_eq!(scannable_document_kind(Path::new(name)), None, "{name}");
    }
    // Ordinary documents still classify (these paths do not exist, so
    // no size is known — the pipeline's read happens afterwards and
    // leaves missing files un-indexed).
    assert_eq!(
        scannable_document_kind(Path::new("api.yaml")),
        Some(DocumentKind::OpenApi)
    );
    assert_eq!(
        scannable_document_kind(Path::new("schema.graphql")),
        Some(DocumentKind::Graphql)
    );
    assert_eq!(scannable_document_kind(Path::new("README.md")), None);
}

// -- proto document scanner (TASK-088, plan 5.2) ---------------------------

#[test]
fn proto_document_services_and_methods() {
    let src = "\
syntax = \"proto3\";
package users.v1;

message User { string id = 1; }

service UserService {
  rpc GetUser(GetUserRequest) returns (User);
  rpc ListUsers(ListUsersRequest) returns (stream User);
}
";
    let cands = extract_document_contracts(DocumentKind::Proto, src, &ContractOptions::default());
    assert_eq!(cands.len(), 2, "got {cands:?}");
    let get = find(&cands, "grpc::UserService::GetUser").expect("GetUser missing");
    assert_eq!(get.kind, ContractKind::Grpc);
    assert_eq!(get.role, ContractRole::Provider);
    assert_eq!(get.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(get.owning_symbol, None);
    assert_eq!(get.params, Vec::<PathParam>::new());
    assert_eq!(get.qualifier, "UserService");
    // Bare service name — the package declaration is NOT composed.
    assert_eq!(get.line, 7);
    assert!(find(&cands, "grpc::UserService::ListUsers").is_some());
}

#[test]
fn proto_document_comment_wrapped_rpc_ignored() {
    let src = "\
service UserService {
  // rpc Commented(In) returns (Out);
  /* rpc Blocked(In) returns (Out); */
  rpc Real(In) returns (Out);
}
";
    let cands = extract_document_contracts(DocumentKind::Proto, src, &ContractOptions::default());
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let real = find(&cands, "grpc::UserService::Real").expect("Real missing");
    assert_eq!(real.line, 4);
}

#[test]
fn proto_document_rpc_with_option_block() {
    let src = "\
service UserService {
  rpc ListUsers(In) returns (stream Out) {
    option deprecated = true;
  }
  rpc GetUser(In) returns (Out);
}
";
    let cands = extract_document_contracts(DocumentKind::Proto, src, &ContractOptions::default());
    assert_eq!(cands.len(), 2, "got {cands:?}");
    let list = find(&cands, "grpc::UserService::ListUsers").expect("ListUsers missing");
    assert_eq!(list.line, 2);
    // The option block's closing brace must not end the service early.
    let get = find(&cands, "grpc::UserService::GetUser").expect("GetUser missing");
    assert_eq!(get.line, 5);
}

#[test]
fn proto_document_extend_is_not_service() {
    let src = "\
extend google.protobuf.MethodOptions {
  string opt = 50001;
}
service UserService {
  rpc GetUser(In) returns (Out);
}
";
    let cands = extract_document_contracts(DocumentKind::Proto, src, &ContractOptions::default());
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert!(find(&cands, "grpc::UserService::GetUser").is_some());
}

#[test]
fn proto_document_disabled_by_option() {
    let opts = ContractOptions {
        grpc: false,
        ..ContractOptions::default()
    };
    let cands = extract_document_contracts(
        DocumentKind::Proto,
        "service UserService {\n  rpc GetUser(In) returns (Out);\n}\n",
        &opts,
    );
    assert!(cands.is_empty(), "got {cands:?}");
}

// -- graphql (TASK-088, plan 5.3) -------------------------------------------

#[test]
fn graphql_parse_named_query_multiple_fields() {
    let ops = parse_graphql_operation(
        "query GetUser($id: ID!) { user(id: $id) { name } posts { title } }",
    );
    assert_eq!(
        ops.as_deref(),
        Some(
            &[
                ("Query".to_string(), "user".to_string()),
                ("Query".to_string(), "posts".to_string())
            ][..]
        )
    );
}

#[test]
fn graphql_parse_anonymous_mutation() {
    let ops = parse_graphql_operation("mutation { deleteUser(id: 1) }");
    assert_eq!(
        ops.as_deref(),
        Some(&[("Mutation".to_string(), "deleteUser".to_string())][..])
    );
}

#[test]
fn graphql_parse_shorthand_query() {
    let ops = parse_graphql_operation("{ user posts }");
    assert_eq!(
        ops.as_deref(),
        Some(
            &[
                ("Query".to_string(), "user".to_string()),
                ("Query".to_string(), "posts".to_string())
            ][..]
        )
    );
}

#[test]
fn graphql_parse_subscription() {
    let ops = parse_graphql_operation("subscription Sub { userAdded }");
    assert_eq!(
        ops.as_deref(),
        Some(&[("Subscription".to_string(), "userAdded".to_string())][..])
    );
}

#[test]
fn graphql_parse_nested_braces_do_not_leak() {
    let ops = parse_graphql_operation("query Q { a { b { c } } d }");
    assert_eq!(
        ops.as_deref(),
        Some(
            &[
                ("Query".to_string(), "a".to_string()),
                ("Query".to_string(), "d".to_string())
            ][..]
        )
    );
}

#[test]
fn graphql_parse_alias_reports_the_field_not_the_alias() {
    // TASK-088 review debt: the alias skip had zero coverage —
    // deleting it used to pass the suite silently.
    let ops = parse_graphql_operation("query { u: user posts }");
    assert_eq!(
        ops.as_deref(),
        Some(
            &[
                ("Query".to_string(), "user".to_string()),
                ("Query".to_string(), "posts".to_string())
            ][..]
        )
    );
}

#[test]
fn graphql_parse_tight_and_spaced_spreads_are_skipped() {
    let tight = parse_graphql_operation("query Q { ...UserFields posts }");
    assert_eq!(
        tight.as_deref(),
        Some(&[("Query".to_string(), "posts".to_string())][..])
    );
    let spaced = parse_graphql_operation("query Q { ... UserFields posts }");
    assert_eq!(
        spaced.as_deref(),
        Some(&[("Query".to_string(), "posts".to_string())][..])
    );
}

#[test]
fn graphql_parse_inline_fragment_and_directives_emit_no_phantoms() {
    // `... on User` and `@include` used to leak `on`, `User`, and
    // `include` as phantom depth-0 fields (TASK-088 review debt).
    let ops = parse_graphql_operation("query Q { ... on User { id } user @include(if: $x) posts }");
    assert_eq!(
        ops.as_deref(),
        Some(
            &[
                ("Query".to_string(), "user".to_string()),
                ("Query".to_string(), "posts".to_string())
            ][..]
        )
    );
}

#[test]
fn graphql_parse_non_operation_rejected() {
    assert!(parse_graphql_operation("SELECT * FROM users").is_none());
    assert!(parse_graphql_operation("").is_none());
    // `queryx` is an identifier, not the keyword.
    assert!(parse_graphql_operation("queryx { a }").is_none());
    // No selection set.
    assert!(parse_graphql_operation("query GetUser").is_none());
}

#[test]
fn graphql_document_sdl_resolvers() {
    let src = "\
type Query {
  user(id: ID!): User
  posts: [Post]
}

type Mutation {
  deleteUser(id: ID!): Boolean
}

type User {
  id: ID
}
";
    let cands = extract_document_contracts(DocumentKind::Graphql, src, &ContractOptions::default());
    assert_eq!(cands.len(), 3, "got {cands:?}");
    let user = find(&cands, "graphql::Query::user").expect("user missing");
    assert_eq!(user.role, ContractRole::Provider);
    assert_eq!(user.kind, ContractKind::Graphql);
    assert_eq!(user.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(user.line, 2);
    assert!(find(&cands, "graphql::Query::posts").is_some());
    assert!(find(&cands, "graphql::Mutation::deleteUser").is_some());
}

#[test]
fn graphql_document_extend_type() {
    let src = "\
type Query { base: String }

extend type Query {
  extra: Int
}
";
    let cands = extract_document_contracts(DocumentKind::Graphql, src, &ContractOptions::default());
    assert_eq!(cands.len(), 2, "got {cands:?}");
    assert!(find(&cands, "graphql::Query::base").is_some());
    let extra = find(&cands, "graphql::Query::extra").expect("extra missing");
    assert_eq!(extra.line, 4);
}

#[test]
fn graphql_document_operation_document_consumers() {
    let src = "\
query GetUser {
  user(id: 1) {
    name
  }
}
";
    let cands = extract_document_contracts(DocumentKind::Graphql, src, &ContractOptions::default());
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "graphql::Query::user");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.line, 1);
}

#[test]
fn graphql_js_resolver_map_providers() {
    let src = "\
const resolvers = {
  Query: {
    user: (parent, args) => db.user(),
    posts: () => [],
  },
  Mutation: {
    deleteUser: (parent, { id }) => true,
  },
};
";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 3, "got {cands:?}");
    let user = find(&cands, "graphql::Query::user").expect("user missing");
    assert_eq!(user.role, ContractRole::Provider);
    assert_eq!(user.kind, ContractKind::Graphql);
    assert_eq!(user.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(user.line, 3);
    assert!(find(&cands, "graphql::Query::posts").is_some());
    assert!(find(&cands, "graphql::Mutation::deleteUser").is_some());
}

#[test]
fn graphql_js_gql_tagged_template_consumers() {
    let src = "import { gql } from '@apollo/client';\nconst USER = gql`query { user }`;\nconst DEL = graphql`mutation { deleteUser(id: 1) }`;\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    let user = find(&cands, "graphql::Query::user").expect("user missing");
    assert_eq!(user.role, ContractRole::Consumer);
    assert_eq!(user.line, 2);
    assert!(find(&cands, "graphql::Mutation::deleteUser").is_some());
}

#[test]
fn graphql_js_gql_fragment_alias_directive_no_phantoms() {
    // TASK-088 review debt, through extract(): aliases report the
    // field, spreads and inline fragments and directives emit
    // nothing.
    let src = "const Q = gql`query { u: user ...UserFields ... on User { id } posts @include(if: $x) }`;\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    assert!(find(&cands, "graphql::Query::user").is_some());
    assert!(find(&cands, "graphql::Query::posts").is_some());
}

#[test]
fn graphql_js_apollo_client_call_consumers() {
    let src = "client.query({ query: 'query { user }' });\nclient.mutate({ mutation: 'mutation { deleteUser(id: 1) }' });\n";
    let cands = extract(Lang::JavaScript, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    assert!(find(&cands, "graphql::Query::user").is_some());
    assert!(find(&cands, "graphql::Mutation::deleteUser").is_some());
}

#[test]
fn graphql_js_plain_strings_ignored() {
    // A plain string that merely looks like a query is not a contract
    // site; only tagged templates and client .query/.mutate calls are.
    let cands = extract(Lang::JavaScript, "const q = 'query { user }';\n");
    assert!(cands.is_empty(), "got {cands:?}");
}

#[test]
fn graphql_python_gql_consumer() {
    let src = "\
from gql import gql

def fetch(client):
    return client.execute(gql('query { user }'))
";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "graphql::Query::user");
    assert_eq!(c.role, ContractRole::Consumer);
    assert_eq!(c.owning_symbol.as_deref(), Some("fetch"));
}

#[test]
fn graphql_python_strawberry_providers() {
    let src = "\
import strawberry

class Query:
    @strawberry.field
    def user(self) -> User:
        return db.user()

    @strawberry.mutation
    def deleteUser(self) -> bool:
        return True
";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 2, "got {cands:?}");
    let user = find(&cands, "graphql::Query::user").expect("user missing");
    assert_eq!(user.role, ContractRole::Provider);
    assert_eq!(user.owning_symbol.as_deref(), Some("user"));
    assert!(find(&cands, "graphql::Mutation::deleteUser").is_some());
}

#[test]
fn graphql_python_ariadne_provider() {
    let src = "\
from ariadne import QueryType
Query = QueryType()

@Query.field('get_user')
def resolve_get_user(obj, info):
    return db.user()
";
    let cands = extract(Lang::Python, src);
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "graphql::Query::get_user");
    assert_eq!(c.role, ContractRole::Provider);
    assert_eq!(c.owning_symbol.as_deref(), Some("resolve_get_user"));
}

#[test]
fn graphql_disabled_by_option() {
    let opts = ContractOptions {
        graphql: false,
        ..ContractOptions::default()
    };
    let src = "const resolvers = {\n  Query: {\n    user: () => db.user(),\n  },\n};\n";
    assert!(extract_with(Lang::JavaScript, src, &opts).is_empty());
}

// -- openapi scanner (TASK-088, plan 5.4) ------------------------------------

#[test]
fn openapi_yaml_two_paths() {
    let src = "\
openapi: 3.0.0
info:
  title: Users API
  version: 1.0.0
paths:
  /v1/users:
    get:
      summary: List users
    post:
      summary: Create user
  /v1/users/{id}:
    get:
      summary: Fetch one user
    delete:
      summary: Delete a user
components: {}
";
    let cands = extract_document_contracts(DocumentKind::OpenApi, src, &ContractOptions::default());
    assert_eq!(cands.len(), 4, "got {cands:?}");
    let get = find(&cands, "openapi::GET::/v1/users").expect("GET missing");
    assert_eq!(get.kind, ContractKind::Openapi);
    assert_eq!(get.role, ContractRole::Provider);
    assert_eq!(get.confidence, CONFIDENCE_FRAMEWORK);
    assert_eq!(get.owning_symbol, None);
    assert_eq!(get.line, 7);
    assert!(find(&cands, "openapi::POST::/v1/users").is_some());
    assert!(find(&cands, "openapi::GET::/v1/users/{p1}").is_some());
    assert!(find(&cands, "openapi::DELETE::/v1/users/{p1}").is_some());
}

#[test]
fn openapi_swagger_2() {
    let src = "\
swagger: \"2.0\"
info:
  title: Pets
paths:
  /pets:
    get:
      summary: List pets
    post:
      summary: Add pet
";
    let cands = extract_document_contracts(DocumentKind::OpenApi, src, &ContractOptions::default());
    assert_eq!(cands.len(), 2, "got {cands:?}");
    assert!(find(&cands, "openapi::GET::/pets").is_some());
    assert!(find(&cands, "openapi::POST::/pets").is_some());
}

#[test]
fn openapi_placeholder_becomes_positional_with_params() {
    let src = "\
openapi: 3.0.0
paths:
  /workspaces/{wid}/tags/{id}:
    get:
      summary: Fetch tag
";
    let cands = extract_document_contracts(DocumentKind::OpenApi, src, &ContractOptions::default());
    assert_eq!(cands.len(), 1, "got {cands:?}");
    let c = &cands[0];
    assert_eq!(c.canonical_id, "openapi::GET::/workspaces/{p1}/tags/{p2}");
    // Original names retained as metadata (PRD-CTR-REQ-022 symmetry).
    assert_eq!(
        c.params,
        vec![
            PathParam {
                position: 1,
                name: "wid".to_string(),
            },
            PathParam {
                position: 2,
                name: "id".to_string(),
            },
        ]
    );
}

#[test]
fn openapi_json_flavor() {
    let src = "\
{
  \"openapi\": \"3.0.0\",
  \"info\": {
    \"title\": \"Users\"
  },
  \"paths\": {
    \"/v1/users\": {
      \"get\": {
        \"summary\": \"List\"
      }
    }
  }
}
";
    let cands = extract_document_contracts(DocumentKind::OpenApi, src, &ContractOptions::default());
    assert_eq!(cands.len(), 1, "got {cands:?}");
    assert!(find(&cands, "openapi::GET::/v1/users").is_some());
}

#[test]
fn openapi_sniff_negatives_stay_unindexed() {
    let compose = "services:\n  app:\n    image: busybox\n";
    assert!(
        extract_document_contracts(DocumentKind::OpenApi, compose, &ContractOptions::default())
            .is_empty()
    );
    let pkg = "{\n  \"name\": \"x\",\n  \"version\": \"1.0.0\"\n}\n";
    assert!(
        extract_document_contracts(DocumentKind::OpenApi, pkg, &ContractOptions::default())
            .is_empty()
    );
    // Multi-document YAML is skipped (extraction, not validation).
    let multi = "---\nopenapi: 3.0.0\npaths:\n  /x:\n    get: {}\n---\nopenapi: 3.0.1\n";
    assert!(
        extract_document_contracts(DocumentKind::OpenApi, multi, &ContractOptions::default())
            .is_empty()
    );
}

#[test]
fn openapi_disabled_by_option() {
    let opts = ContractOptions {
        openapi: false,
        ..ContractOptions::default()
    };
    let src = "openapi: 3.0.0\npaths:\n  /x:\n    get: {}\n";
    assert!(extract_document_contracts(DocumentKind::OpenApi, src, &opts).is_empty());
}

// -- E2E acceptance (TASK-088 acceptance criterion 1) ------------------------

/// IDL definition + generated-stub call site pair despite package
/// qualification and casing, through both pipeline paths: the document
/// path for the `.proto` and the grammar path for the `.java`.
#[test]
fn acceptance_proto_idl_pairs_with_java_stub() {
    use crate::indexer::get_parser;
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("proto")).unwrap();
    fs::create_dir_all(root.join("src/main/java")).unwrap();
    fs::write(
            root.join("proto/users.proto"),
            "syntax = \"proto3\";\npackage users.v1;\n\nservice UserService {\n  rpc GetUser(GetUserRequest) returns (User);\n}\n",
        )
        .unwrap();
    fs::write(
            root.join("src/main/java/Client.java"),
            "import io.grpc.ManagedChannel;\n\nclass Client {\n    void call(ManagedChannel channel) {\n        UserServiceGrpc.UserServiceBlockingStub stub = UserServiceGrpc.newBlockingStub(channel);\n        stub.getUser(request);\n    }\n}\n",
        )
        .unwrap();

    let opts = ContractOptions::default();
    let mut cands = Vec::new();

    // Document path (pipeline's parse_one_file fallback).
    let proto = root.join("proto/users.proto");
    let content = fs::read_to_string(&proto).unwrap();
    let kind = document_kind(&proto).expect("proto is a document kind");
    cands.extend(extract_document_contracts(kind, &content, &opts));

    // Grammar path.
    let java = root.join("src/main/java/Client.java");
    let src = fs::read_to_string(&java).unwrap();
    let lang = crate::indexer::detect_language(&java).expect("java detected");
    let mut parser = get_parser(lang);
    let tree = parser.parse(&src, None).expect("parse failed");
    cands.extend(extract_contracts(&tree, &src, lang, &opts));

    assert_eq!(cands.len(), 2, "got {cands:?}");
    assert!(
        find(&cands, "grpc::UserService::GetUser").is_some(),
        "proto provider (package NOT composed): {cands:?}"
    );
    assert!(
        find(&cands, "grpc::UserService::getUser").is_some(),
        "java stub consumer: {cands:?}"
    );

    let scopes = [RpcJoinScope {
        workspace: "e2e".to_string(),
        candidates: &cands,
    }];
    let joins = canonical_rpc_join(&scopes);
    assert_eq!(joins.len(), 1, "got {joins:?}");
    assert_eq!(joins[0].provider.canonical_id, "grpc::UserService::GetUser");
    assert_eq!(joins[0].consumer.canonical_id, "grpc::UserService::getUser");
}
