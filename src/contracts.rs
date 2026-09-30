//! Contract extraction: canonical ID normalization, the `http`, `env`,
//! `queue`, `websocket`, and `job` contract kinds (TASK-082, TASK-087), the
//! RPC-family and schema kinds `grpc`, `graphql`, and `openapi` plus the
//! canonical join they require (TASK-088, PRD-CTR-REQ-001..004, 024).
//!
//! Contracts are detected by walking the tree-sitter tree that the symbol
//! indexer already parsed — no second parse or file read (PRD-CTR-REQ-011).
//! Storage lands in TASK-083; this module only produces
//! [`ContractCandidate`] values.
//!
//! Normalization core (AR-017):
//! - [`canonical_contract_id`] is the only place contract IDs are built.
//! - [`normalize_http_path`] runs a fixed 6-stage pipeline (PRD-CTR-REQ-003).
//! - [`normalize_method`] upper-cases verbs and maps router catch-alls to
//!   `ANY`.
//! - [`normalize_topic`] normalizes queue/websocket/job names; unlike the
//!   HTTP pipeline it REJECTS computed topics outright (`None`), because
//!   exact-ID matching is the only pairing mechanism for the message kinds.
//!
//! Role mappings are PER SPEC and deliberately inverted between kinds —
//! do not "fix" one to match the other:
//! - **queue (DR-031):** provider = the construct that registers a handler
//!   (subscriber, `@KafkaListener`, `ch.consume`), consumer = the code that
//!   initiates by publishing (`producer.send`, `ch.publish`). The reader of
//!   a topic serves it; the writer calls on it.
//! - **websocket:** provider = emit sites (`io.emit`, `@SendTo`), consumer =
//!   handler registrations (`socket.on`, `@MessageMapping`, `app.ws`). Here
//!   the writer serves and the reader subscribes — the mirror image of the
//!   queue rule, per the TASK-087 specification.
//! - **grpc:** provider = the serving side (proto `rpc` declarations,
//!   `XGrpc.XImplBase`/Servicer method impls, `Register<S>Server`,
//!   `addService`, `impl …::S for T`); consumer = generated-stub call sites
//!   (`stub.getUser`, `client.GetUser`, `pb.New<S>Client(conn).M(…)`). A
//!   service-level registration uses identifier `*` (`grpc::S::*`) and pairs
//!   with any method-level consumer of that service.
//! - **graphql:** provider = resolvers (SDL root-type fields, JS resolver
//!   maps, `@strawberry.*`, Ariadne `@Query.field`); consumer = operation
//!   call sites (`gql` tagged templates, Apollo `.query`/`.mutate` strings,
//!   Python `gql(…)`, operation documents).
//! - **openapi:** provider only — specification documents are file-level
//!   contracts with a NULL owning symbol by design (§4.24); there is no
//!   consumer side to detect.
//!
//! Document files (TASK-088, no-new-crates constraint): `.proto`,
//! `.graphql`/`.gql`, and `.yaml`/`.yml`/`.json` carry no grammar, so tiny
//! line-oriented scanners ([`extract_document_contracts`]) read them
//! instead. Only a document that yields candidates gets a `files` row
//! (empty symbols, language = [`DocumentKind::as_str`]) — that row is
//! TASK-083's hash/re-index anchor; everything else stays un-indexed
//! exactly as before. OpenAPI additionally sniffs content (a top-level
//! `openapi:`/`swagger:` key plus `paths:`), so CI/compose/package files
//! never index.
//!
//! RPC canonical join ([`canonical_rpc_join`], PRD-CTR-REQ-024): exact ID
//! equality is the first pass and is never overridden — candidates with an
//! opposite-role exact counterpart in their own workspace are excluded
//! entirely. The join is the SECOND pass, pure and in-memory over
//! workspace-scoped slices, tolerating package qualification (service
//! compared on the last dot-segment, case-folded), method casing and
//! snake/camel separators (`get_user` = `GetUser` = `getUser`), and
//! service-level `*` registration (method-level providers win). Workspace
//! equality on normalized identifiers ([`normalize_workspace_id`],
//! PRD-CTR-REQ-019) is the REQ-014 guard — the join relaxes names, never
//! scope. IDs keep the developer's spelling; only the comparison relaxes.
//!
//! RabbitMQ note: producers address exchange+routing-key while consumers
//! address queue names; the binding between them is broker config and
//! invisible to static analysis. Pairing therefore relies on aligned names
//! (topic-exchange convention); the Ruby `channel.queue` binding pre-pass
//! closes part of the gap where the queue name is declared inline.
//!
//! Verb collisions (`send`/`emit`) resolve by guard order: websocket
//! receivers first, then the queue generic arms, and the ambiguous HTTP
//! arm last behind its `is_path_like` gate.
//!
//! Out of scope by design (extraction, not validation): GraphQL servers in
//! Java/Go/C#/PHP/Rust; untracked gRPC receiver variables; JS grpc arms
//! without a file-level "grpc" marker; plain look-alike strings (only
//! `gql`/`graphql` tags, Apollo option objects, and Python `gql(…)` parse);
//! flow-style YAML maps and multi-document YAML; a GraphQL document's
//! second operation.

use std::collections::HashMap;

use tree_sitter::{Node, Tree};

use crate::indexer::Lang;
use crate::types::{ContractCandidate, ContractKind, ContractRole, PathParam};

/// Confidence for framework-recognized constructs (DR-028 / AR-018).
pub const CONFIDENCE_FRAMEWORK: f64 = 1.0;
/// Confidence for string-literal heuristics and role-ambiguous constructs.
pub const CONFIDENCE_HEURISTIC: f64 = 0.5;

/// Per-kind extraction switches threaded through the pipeline (TASK-087).
///
/// `Copy` so the `par_iter` in `build_index_with_progress` can carry it per
/// file; mirrors [`crate::config::ContractsConfig`] one flag per kind so a
/// noisy detector can be disabled without degrading the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractOptions {
    /// HTTP route/outbound-call detection.
    pub http: bool,
    /// Environment-variable read/write detection.
    pub env: bool,
    /// Message-queue producer/consumer detection.
    pub queue: bool,
    /// WebSocket emit/handler-registration detection.
    pub websocket: bool,
    /// Scheduled and background job detection.
    pub job: bool,
    /// gRPC IDL + generated-stub detection (RPC family, TASK-088).
    pub grpc: bool,
    /// GraphQL resolver + operation detection (TASK-088).
    pub graphql: bool,
    /// OpenAPI specification-document detection (TASK-088).
    pub openapi: bool,
}

impl Default for ContractOptions {
    fn default() -> Self {
        Self {
            http: true,
            env: true,
            queue: true,
            websocket: true,
            job: true,
            grpc: true,
            graphql: true,
            openapi: true,
        }
    }
}

impl ContractOptions {
    /// Whether detections of `kind` should be emitted.
    pub fn enabled(&self, kind: ContractKind) -> bool {
        match kind {
            ContractKind::Http => self.http,
            ContractKind::Env => self.env,
            ContractKind::Queue => self.queue,
            ContractKind::WebSocket => self.websocket,
            ContractKind::Job => self.job,
            ContractKind::Grpc => self.grpc,
            ContractKind::Graphql => self.graphql,
            ContractKind::Openapi => self.openapi,
        }
    }
}

impl From<&crate::config::ContractsConfig> for ContractOptions {
    fn from(cfg: &crate::config::ContractsConfig) -> Self {
        Self {
            http: cfg.http,
            env: cfg.env,
            queue: cfg.queue,
            websocket: cfg.websocket,
            job: cfg.job,
            grpc: cfg.grpc,
            graphql: cfg.graphql,
            openapi: cfg.openapi,
        }
    }
}

/// Extract contract candidates from an already-parsed tree.
///
/// `source` must be the exact byte string the tree was parsed from.
/// One binding pre-pass collects router context (REQ-023), then a single
/// iterative DFS walks the tree and dispatches per-language matchers.
/// `opts` gates each kind at its emit choke point, so a noisy detector can
/// be disabled without degrading the others.
pub fn extract_contracts(
    tree: &Tree,
    source: &str,
    lang: Lang,
    opts: &ContractOptions,
) -> Vec<ContractCandidate> {
    if !opts.http
        && !opts.env
        && !opts.queue
        && !opts.websocket
        && !opts.job
        && !opts.grpc
        && !opts.graphql
        && !opts.openapi
    {
        return Vec::new();
    }
    let src = source.as_bytes();
    // The router pre-pass serves http prefixes and Ruby queue bindings.
    let ctx = if opts.http || (opts.queue && matches!(lang, Lang::Ruby)) {
        collect_router_context(tree.root_node(), src, lang)
    } else {
        RouterContext::default()
    };
    // The RPC pre-pass binds generated stubs to their services (TASK-088);
    // it runs only for the languages with gRPC facts — C/C++/Ruby/PHP/C#
    // files would pay a full tree walk for a provably empty context.
    // JS/TS additionally require a file-level "grpc" marker before any
    // generated-code detection fires — `new XClient(...)` alone is not
    // evidence of gRPC.
    let rpc = if opts.grpc && lang_collects_rpc_facts(lang) {
        collect_rpc_context(tree.root_node(), src, lang)
    } else {
        RpcContext::default()
    };
    let grpc_hint = opts.grpc
        && matches!(lang, Lang::JavaScript | Lang::TypeScript | Lang::Tsx)
        && source.to_lowercase().contains("grpc");
    let mut ex = Extractor {
        src,
        lang,
        ctx,
        rpc,
        grpc_hint,
        opts: *opts,
        out: Vec::new(),
    };
    let mut stack = vec![(tree.root_node(), String::new())];
    while let Some((node, prefix)) = stack.pop() {
        let child_prefix = ex.visit(node, &prefix);
        for i in (0..node.child_count()).rev() {
            if let Some(child) = node.child(i as u32) {
                stack.push((child, child_prefix.clone()));
            }
        }
    }
    ex.out
}

// ---------------------------------------------------------------------------
// Router context (PRD-CTR-REQ-023)
// ---------------------------------------------------------------------------

/// Variable-to-prefix knowledge gathered before the route walk.
///
/// `bindings` maps a router variable to its absolute prefix (e.g. a gin
/// group resolved through its chain); `mounts` maps a router variable to
/// the paths it is mounted under (e.g. `app.use('/v1', router)`).
#[derive(Default)]
struct RouterContext {
    bindings: HashMap<String, String>,
    mounts: HashMap<String, Vec<String>>,
    /// `let VAR = <initializer>` byte ranges: Rust chains attribute their
    /// `.route` literals to the variable the chain initializes.
    initializer_ranges: Vec<(std::ops::Range<usize>, String)>,
    /// Ruby `q = channel.queue("NAME")` / `channel.direct|topic|fanout`
    /// declarations: variable -> queue/exchange name (TASK-087). A bound
    /// variable's `.subscribe` is a RabbitMQ provider on that name.
    queue_bindings: HashMap<String, String>,
}

/// Maximum chain depth when resolving group prefixes (cycles, deep chains).
const PREFIX_DEPTH_CAP: usize = 8;

impl RouterContext {
    fn is_router_var(&self, name: &str) -> bool {
        self.bindings.contains_key(name) || self.mounts.contains_key(name)
    }

    /// Variable whose initializer contains `byte`, if any.
    fn var_for_range(&self, byte: usize) -> Option<&str> {
        self.initializer_ranges
            .iter()
            .find(|(range, _)| range.contains(&byte))
            .map(|(_, var)| var.as_str())
    }

    /// Concatenated prefix for a router variable: mount paths first, then
    /// the variable's own binding.
    fn effective_prefix(&self, var: &str) -> String {
        let mut prefix = String::new();
        if let Some(mount_paths) = self.mounts.get(var) {
            for m in mount_paths {
                prefix.push_str(m);
            }
        }
        if let Some(b) = self.bindings.get(var) {
            prefix.push_str(b);
        }
        prefix
    }
}

fn collect_router_context(root: Node, src: &[u8], lang: Lang) -> RouterContext {
    let mut ctx = RouterContext::default();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match lang {
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
                collect_js_router_facts(node, src, &mut ctx);
            }
            Lang::Python => {
                collect_py_router_facts(node, src, &mut ctx);
            }
            Lang::Go => {
                collect_go_router_facts(node, src, &mut ctx);
            }
            Lang::Rust => {
                collect_rust_router_facts(node, src, &mut ctx);
            }
            Lang::Ruby => {
                collect_ruby_queue_facts(node, src, &mut ctx);
            }
            _ => {}
        }
        for i in (0..node.child_count()).rev() {
            if let Some(child) = node.child(i as u32) {
                stack.push(child);
            }
        }
    }
    ctx
}

/// JS: `const r = express.Router() | new Router() | new Hono() | express()`
/// binds a router variable; `app.use('/v1', r)` mounts one.
fn collect_js_router_facts(node: Node, src: &[u8], ctx: &mut RouterContext) {
    match node.kind() {
        "variable_declarator" => {
            let name = node_text(node.child_by_field_name("name"), src);
            if name.is_empty() {
                return;
            }
            let value = match node.child_by_field_name("value") {
                Some(v) => v,
                None => return,
            };
            let callee = match value.kind() {
                "new_expression" => value.child_by_field_name("constructor"),
                "call_expression" => value.child_by_field_name("function"),
                _ => None,
            };
            let is_router_ctor = match callee {
                Some(c) => matches!(
                    node_text(Some(c), src),
                    "express" | "express.Router" | "Router" | "Hono" | "Bun.serve"
                ),
                None => false,
            };
            if is_router_ctor {
                ctx.bindings.insert(name.to_string(), String::new());
            }
        }
        "call_expression" => {
            let func = node.child_by_field_name("function");
            let args = node.child_by_field_name("arguments");
            let (Some(func), Some(args)) = (func, args) else {
                return;
            };
            if func.kind() != "member_expression" {
                return;
            }
            if node_text(func.child_by_field_name("property"), src) != "use" {
                return;
            }
            // app.use('/v1', router): mount string -> identifier.
            let mut iter = (0..args.named_child_count()).filter_map(|i| args.named_child(i as u32));
            let first = iter.next();
            let second = iter.next();
            if let (Some(path_node), Some(var_node)) = (first, second)
                && path_node.kind() == "string"
                && var_node.kind() == "identifier"
            {
                let path = string_content(path_node, src);
                let var = node_text(Some(var_node), src);
                if !var.is_empty() {
                    ctx.mounts.entry(var.to_string()).or_default().push(path);
                }
            }
        }
        _ => {}
    }
}

/// Python: `bp = Blueprint(..., url_prefix='/v1')`,
/// `router = APIRouter(prefix='/users')`, `app = Flask(__name__)` bind
/// router variables; `register_blueprint(bp, url_prefix=…)` and
/// `include_router(router, prefix=…)` mount them.
fn collect_py_router_facts(node: Node, src: &[u8], ctx: &mut RouterContext) {
    match node.kind() {
        "assignment" => {
            let Some(left) = node.child_by_field_name("left") else {
                return;
            };
            if left.kind() != "identifier" {
                return;
            }
            let var = node_text(Some(left), src);
            if var.is_empty() {
                return;
            }
            let Some(right) = node.child_by_field_name("right") else {
                return;
            };
            if right.kind() != "call" {
                return;
            }
            let Some(func) = right.child_by_field_name("function") else {
                return;
            };
            if func.kind() != "identifier" {
                return;
            }
            let Some(args) = right.child_by_field_name("arguments") else {
                return;
            };
            let prefix = match node_text(Some(func), src) {
                "Blueprint" => kwarg_string(args, "url_prefix", src),
                "APIRouter" => kwarg_string(args, "prefix", src),
                "Flask" | "FastAPI" | "Falcon" => Some(String::new()),
                _ => None,
            };
            if let Some(prefix) = prefix {
                ctx.bindings.insert(var.to_string(), prefix);
            }
        }
        "call" => {
            let func = node.child_by_field_name("function");
            let args = node.child_by_field_name("arguments");
            let (Some(func), Some(args)) = (func, args) else {
                return;
            };
            if func.kind() != "attribute" {
                return;
            }
            let attr = node_text(func.child_by_field_name("attribute"), src);
            let mount = match attr {
                "register_blueprint" => kwarg_string(args, "url_prefix", src),
                "include_router" => kwarg_string(args, "prefix", src),
                _ => None,
            };
            if let Some(mount) = mount
                && let Some(var_node) = positional_arg(args, 0)
                && var_node.kind() == "identifier"
            {
                let var = node_text(Some(var_node), src);
                if !var.is_empty() {
                    ctx.mounts.entry(var.to_string()).or_default().push(mount);
                }
            }
        }
        _ => {}
    }
}

/// Go: `v1 := r.Group("/v1")` binds a group variable to an absolute prefix;
/// nested groups resolve through earlier bindings (depth-capped).
fn collect_go_router_facts(node: Node, src: &[u8], ctx: &mut RouterContext) {
    if node.kind() != "short_var_declaration" {
        return;
    }
    let Some(var) = node
        .child_by_field_name("left")
        .and_then(|l| l.named_child(0))
        .filter(|n| n.kind() == "identifier")
    else {
        return;
    };
    let var = node_text(Some(var), src);
    if var.is_empty() {
        return;
    }
    let Some(call) = node
        .child_by_field_name("right")
        .and_then(|r| r.named_child(0))
        .filter(|n| n.kind() == "call_expression")
    else {
        return;
    };
    let Some(func) = call.child_by_field_name("function") else {
        return;
    };
    if func.kind() != "selector_expression" {
        return;
    }
    if node_text(func.child_by_field_name("field"), src) != "Group" {
        return;
    }
    let Some(args) = call.child_by_field_name("arguments") else {
        return;
    };
    let Some(path_node) = positional_arg(args, 0) else {
        return;
    };
    let literal = render_string_node(path_node, src, Lang::Go);
    let mut prefix = ctx
        .bindings
        .get(node_text(func.child_by_field_name("operand"), src))
        .cloned()
        .unwrap_or_default();
    // Cap resolution depth to keep pathological chains bounded.
    if prefix.split('/').count() <= PREFIX_DEPTH_CAP {
        append_segment(&mut prefix, &literal);
        ctx.bindings.insert(var.to_string(), prefix);
    }
}

/// Rust: `let s = web::scope("/p")…` binds scope prefixes, `.nest("/p", r)`
/// mounts routers, and every `let VAR = …` records its initializer range.
fn collect_rust_router_facts(node: Node, src: &[u8], ctx: &mut RouterContext) {
    match node.kind() {
        "let_declaration" => {
            let Some(var_node) = node
                .child_by_field_name("pattern")
                .filter(|n| n.kind() == "identifier")
            else {
                return;
            };
            let var = node_text(Some(var_node), src);
            if var.is_empty() {
                return;
            }
            let Some(value) = node.child_by_field_name("value") else {
                return;
            };
            ctx.initializer_ranges
                .push((value.start_byte()..value.end_byte(), var.to_string()));
            // Compose inline web::scope("p") prefixes along the chain; the
            // callee may be scoped (`web::scope`) or a field (`x.scope`).
            let mut prefix = String::new();
            let mut current = Some(value);
            let mut depth = 0;
            while let Some(c) = current
                && c.kind() == "call_expression"
                && depth < PREFIX_DEPTH_CAP
            {
                depth += 1;
                let Some(func) = c.child_by_field_name("function") else {
                    break;
                };
                let (callee, next) = match func.kind() {
                    "field_expression" => (
                        node_text(func.child_by_field_name("field"), src),
                        func.child_by_field_name("value"),
                    ),
                    "scoped_identifier" => (node_text(func.child_by_field_name("name"), src), None),
                    _ => break,
                };
                if callee == "scope"
                    && let Some(args) = c.child_by_field_name("arguments")
                    && let Some(path_node) = positional_arg(args, 0)
                    && path_node.kind() == "string_literal"
                {
                    append_segment(&mut prefix, &render_string_node(path_node, src, Lang::Rust));
                }
                current = next;
            }
            if !prefix.is_empty() {
                ctx.bindings.insert(var.to_string(), prefix);
            }
        }
        "call_expression" => {
            let Some(func) = node.child_by_field_name("function") else {
                return;
            };
            if func.kind() != "field_expression" {
                return;
            }
            if node_text(func.child_by_field_name("field"), src) != "nest" {
                return;
            }
            let Some(args) = node.child_by_field_name("arguments") else {
                return;
            };
            let (Some(path_node), Some(var_node)) =
                (positional_arg(args, 0), positional_arg(args, 1))
            else {
                return;
            };
            if path_node.kind() == "string_literal" && var_node.kind() == "identifier" {
                let var = node_text(Some(var_node), src);
                if !var.is_empty() {
                    let mut mount = String::new();
                    append_segment(&mut mount, &render_string_node(path_node, src, Lang::Rust));
                    ctx.mounts.entry(var.to_string()).or_default().push(mount);
                }
            }
        }
        _ => {}
    }
}

/// Ruby: `q = channel.queue("NAME")` and `x = channel.direct|topic|fanout("NAME")`
/// bind a variable to the queue/exchange name it addresses (TASK-087).
fn collect_ruby_queue_facts(node: Node, src: &[u8], ctx: &mut RouterContext) {
    if node.kind() != "assignment" {
        return;
    }
    let Some(left) = node
        .child_by_field_name("left")
        .filter(|n| n.kind() == "identifier")
    else {
        return;
    };
    let var = node_text(Some(left), src);
    if var.is_empty() {
        return;
    }
    let Some(right) = node
        .child_by_field_name("right")
        .filter(|n| n.kind() == "call")
    else {
        return;
    };
    if !matches!(
        node_text(right.child_by_field_name("method"), src),
        "queue" | "direct" | "topic" | "fanout"
    ) {
        return;
    }
    let Some(name_node) = right
        .child_by_field_name("arguments")
        .and_then(|args| positional_arg(args, 0))
        .filter(|n| n.kind() == "string")
    else {
        return;
    };
    let name = ruby_string_content(name_node, src);
    if !name.is_empty() {
        ctx.queue_bindings.insert(var.to_string(), name);
    }
}

// ---------------------------------------------------------------------------
// RPC context (TASK-088, plan 5.1): generated-stub bindings
// ---------------------------------------------------------------------------

/// Variable-to-service knowledge for generated gRPC stubs/clients, gathered
/// before the contract walk so `stub.GetUser(req)` call sites can resolve
/// their service (mirrors [`RouterContext`]).
#[derive(Default)]
struct RpcContext {
    /// Client variable -> service qualifier as written by the developer
    /// (`stub` -> `UserService`, JS `client` -> `user.UserService`).
    stubs: HashMap<String, String>,
}

/// Languages whose generated-stub bindings the RPC pre-pass resolves.
/// C/C++/Ruby/PHP/C# have no gRPC arms, so their files never pay the walk.
fn lang_collects_rpc_facts(lang: Lang) -> bool {
    matches!(
        lang,
        Lang::JavaScript
            | Lang::TypeScript
            | Lang::Tsx
            | Lang::Python
            | Lang::Go
            | Lang::Rust
            | Lang::Java
    )
}

fn collect_rpc_context(root: Node, src: &[u8], lang: Lang) -> RpcContext {
    let mut ctx = RpcContext::default();
    // The per-language collector is selected once, not per node; a language
    // with no facts returns before the walk allocates anything.
    let collect: fn(Node, &[u8], &mut RpcContext) = match lang {
        Lang::JavaScript | Lang::TypeScript | Lang::Tsx => collect_js_rpc_facts,
        Lang::Python => collect_py_rpc_facts,
        Lang::Go => collect_go_rpc_facts,
        Lang::Rust => collect_rust_rpc_facts,
        Lang::Java => collect_java_rpc_facts,
        _ => return ctx,
    };
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        collect(node, src, &mut ctx);
        for i in (0..node.child_count()).rev() {
            if let Some(child) = node.child(i as u32) {
                stack.push(child);
            }
        }
    }
    ctx
}

/// `XGrpc` receiver of a stub constructor -> service `X` (dotted
/// qualification before `Grpc` survives).
fn java_grpc_service_from_object(object: Option<Node>, src: &[u8]) -> Option<String> {
    let rest = node_text(object, src).strip_suffix("Grpc")?;
    if rest.rsplit('.').next().unwrap_or("").is_empty() {
        return None;
    }
    Some(rest.to_string())
}

/// `New<S>Client` generated constructor -> `S` (Go: always the bare service).
fn go_service_from_new_client(func: Node, src: &[u8]) -> Option<String> {
    let last = node_text(Some(func), src).rsplit('.').next().unwrap_or("");
    let service = last.strip_prefix("New")?.strip_suffix("Client")?;
    if service.is_empty() {
        return None;
    }
    Some(service.to_string())
}

/// `Register<S>Server(...)` registration function -> `S`.
fn go_service_from_register(name: &str) -> Option<String> {
    let service = name.strip_prefix("Register")?.strip_suffix("Server")?;
    if service.is_empty() {
        return None;
    }
    Some(service.to_string())
}

/// `UserServiceGrpc.newBlockingStub(channel)` initializer binding (Java).
fn collect_java_rpc_facts(node: Node, src: &[u8], ctx: &mut RpcContext) {
    if node.kind() != "variable_declarator" {
        return;
    }
    let var = node_text(node.child_by_field_name("name"), src);
    if var.is_empty() {
        return;
    }
    let Some(value) = node.child_by_field_name("value") else {
        return;
    };
    if value.kind() != "method_invocation"
        || !matches!(
            node_text(value.child_by_field_name("name"), src),
            "newBlockingStub" | "newStub" | "newFutureStub"
        )
    {
        return;
    }
    if let Some(service) = java_grpc_service_from_object(value.child_by_field_name("object"), src) {
        ctx.stubs.insert(var.to_string(), service);
    }
}

/// `client := pb.NewUserServiceClient(conn)` binding (Go).
fn collect_go_rpc_facts(node: Node, src: &[u8], ctx: &mut RpcContext) {
    if node.kind() != "short_var_declaration" {
        return;
    }
    let Some(var) = node
        .child_by_field_name("left")
        .and_then(|l| l.named_child(0))
        .filter(|n| n.kind() == "identifier")
    else {
        return;
    };
    let var = node_text(Some(var), src);
    if var.is_empty() {
        return;
    }
    let Some(func) = node
        .child_by_field_name("right")
        .and_then(|r| r.named_child(0))
        .filter(|n| n.kind() == "call_expression")
        .and_then(|call| call.child_by_field_name("function"))
    else {
        return;
    };
    if let Some(service) = go_service_from_new_client(func, src) {
        ctx.stubs.insert(var.to_string(), service);
    }
}

/// `let client = UserServiceClient::new(channel)` binding (Rust/tonic).
fn collect_rust_rpc_facts(node: Node, src: &[u8], ctx: &mut RpcContext) {
    if node.kind() != "let_declaration" {
        return;
    }
    // tonic convention is `let mut client`; the pattern then wraps the
    // identifier in a `mut_pattern`.
    let var_node = match node.child_by_field_name("pattern") {
        Some(p) if p.kind() == "identifier" => Some(p),
        Some(p) if p.kind() == "mut_pattern" => {
            p.named_child(0).filter(|c| c.kind() == "identifier")
        }
        _ => None,
    };
    let Some(var) = var_node else {
        return;
    };
    let var = node_text(Some(var), src);
    if var.is_empty() {
        return;
    }
    let Some(value) = node.child_by_field_name("value") else {
        return;
    };
    if value.kind() != "call_expression" {
        return;
    }
    let Some(func) = value.child_by_field_name("function") else {
        return;
    };
    // `<path>::new` where the constructor path ends in `<S>Client`.
    let text = node_text(Some(func), src);
    let Some(ctor) = text.strip_suffix("::new") else {
        return;
    };
    let Some(last) = ctor.rsplit("::").next() else {
        return;
    };
    if let Some(service) = last.strip_suffix("Client").filter(|s| !s.is_empty()) {
        ctx.stubs.insert(var.to_string(), service.to_string());
    }
}

/// `stub = user_service_pb2.UserServiceStub(channel)` binding (Python).
fn collect_py_rpc_facts(node: Node, src: &[u8], ctx: &mut RpcContext) {
    if node.kind() != "assignment" {
        return;
    }
    let Some(var) = node
        .child_by_field_name("left")
        .filter(|n| n.kind() == "identifier")
    else {
        return;
    };
    let var = node_text(Some(var), src);
    if var.is_empty() {
        return;
    }
    let Some(func) = node
        .child_by_field_name("right")
        .filter(|n| n.kind() == "call")
        .and_then(|call| call.child_by_field_name("function"))
    else {
        return;
    };
    // `<module>.<S>Stub` — the pb2 module prefix is a Python import
    // artifact, so the bare service name is kept.
    let last = node_text(Some(func), src).rsplit('.').next().unwrap_or("");
    if let Some(service) = last.strip_suffix("Stub").filter(|s| !s.is_empty()) {
        ctx.stubs.insert(var.to_string(), service.to_string());
    }
}

/// `const c = new user.UserServiceClient(host, creds)` binding (JS/TS) —
/// the package-qualification acceptance case: the dotted prefix stays in the
/// service qualifier, and the canonical join relaxes it at match time.
fn collect_js_rpc_facts(node: Node, src: &[u8], ctx: &mut RpcContext) {
    if node.kind() != "variable_declarator" {
        return;
    }
    let var = node_text(node.child_by_field_name("name"), src);
    if var.is_empty() {
        return;
    }
    let Some(ctor) = node
        .child_by_field_name("value")
        .filter(|n| n.kind() == "new_expression")
        .and_then(|new| new.child_by_field_name("constructor"))
    else {
        return;
    };
    let text = node_text(Some(ctor), src);
    if let Some(service) = text.strip_suffix("Client").filter(|s| !s.is_empty()) {
        ctx.stubs.insert(var.to_string(), service.to_string());
    }
}

/// The i-th positional argument of an argument list (skipping keywords).
///
/// Grammars that wrap each argument in an `argument` node (PHP, C#) are
/// unwrapped to the underlying value expression; the wrapper's last named
/// child is the value for both positional and named forms.
fn positional_arg<'t>(args: Node<'t>, i: usize) -> Option<Node<'t>> {
    let mut seen = 0;
    for j in 0..args.named_child_count() {
        if let Some(child) = args.named_child(j as u32)
            && child.kind() != "keyword_argument"
        {
            if seen == i {
                return Some(unwrap_argument(child));
            }
            seen += 1;
        }
    }
    None
}

/// Depth-first search for the first descendant of the given kind.
fn first_descendant_of_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    let mut stack = vec![node];
    while let Some(current) = stack.pop() {
        for i in 0..current.named_child_count() {
            if let Some(child) = current.named_child(i as u32) {
                if child.kind() == kind {
                    return Some(child);
                }
                stack.push(child);
            }
        }
    }
    None
}

/// Unwrap `argument` / `attribute_argument` wrapper nodes to their value.
fn unwrap_argument(node: Node) -> Node {
    if matches!(node.kind(), "argument" | "attribute_argument") {
        let last = (0..node.named_child_count())
            .rev()
            .find_map(|i| node.named_child(i as u32));
        if let Some(inner) = last {
            return inner;
        }
    }
    node
}

/// Celery task decorator: bare `@app.task` (attribute form, no parens)
/// or call form `@app.task(...)` / `@shared_task(...)`. Returns the job
/// name and the node that carries it — the `name=` literal when given,
/// the decorated function's name otherwise.
fn py_job_decorator<'a>(
    dec: Node<'a>,
    def_name: Option<Node<'a>>,
    src: &[u8],
) -> Option<(String, Node<'a>)> {
    let target = dec.named_child(0)?;
    let is_task = match target.kind() {
        // Bare @app.task (attribute) and bare @shared_task (identifier).
        "attribute" => node_text(target.child_by_field_name("attribute"), src) == "task",
        "identifier" => node_text(Some(target), src) == "shared_task",
        "call" => {
            let func = target.child_by_field_name("function")?;
            match func.kind() {
                "attribute" => node_text(func.child_by_field_name("attribute"), src) == "task",
                "identifier" => node_text(Some(func), src) == "shared_task",
                _ => false,
            }
        }
        _ => false,
    };
    if !is_task {
        return None;
    }
    if let Some(args) = target.child_by_field_name("arguments")
        && let Some(lit) = kwarg_string_node(args, "name", src)
        && let Some(name) = py_string_content(lit, src)
    {
        return Some((name, lit));
    }
    let def_name = def_name?;
    Some((node_text(Some(def_name), src).to_string(), def_name))
}

/// String value of a keyword argument, if it is a string literal.
fn kwarg_string(args: Node, name: &str, src: &[u8]) -> Option<String> {
    for j in 0..args.named_child_count() {
        if let Some(kw) = args.named_child(j as u32)
            && kw.kind() == "keyword_argument"
            && node_text(kw.child_by_field_name("name"), src) == name
            && let Some(value) = kw.child_by_field_name("value")
        {
            return py_string_content(value, src);
        }
    }
    None
}

/// Value node of a keyword argument (kind-checked by the caller's
/// topic-argument renderer).
fn kwarg_string_node<'t>(args: Node<'t>, name: &str, src: &[u8]) -> Option<Node<'t>> {
    for j in 0..args.named_child_count() {
        if let Some(kw) = args.named_child(j as u32)
            && kw.kind() == "keyword_argument"
            && node_text(kw.child_by_field_name("name"), src) == name
            && let Some(value) = kw.child_by_field_name("value")
        {
            return Some(value);
        }
    }
    None
}

/// Content of a Python string node (f-string interpolations rendered as
/// `{expr}`).
fn py_string_content(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut out = String::new();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            match child.kind() {
                "string_content" => out.push_str(node_text(Some(child), src)),
                "interpolation" => {
                    out.push('{');
                    out.push_str(node_text(child.named_child(0), src));
                    out.push('}');
                }
                _ => {}
            }
        }
    }
    Some(out)
}

/// Verb for `@app.route(...)`: first entry of `methods=[...]`, else GET.
fn py_route_verb(args: Node, src: &[u8]) -> &'static str {
    for j in 0..args.named_child_count() {
        if let Some(kw) = args.named_child(j as u32)
            && kw.kind() == "keyword_argument"
            && node_text(kw.child_by_field_name("name"), src) == "methods"
            && let Some(value) = kw.child_by_field_name("value")
            && value.kind() == "list"
            && let Some(first) = value.named_child(0)
            && let Some(method) = py_string_content(first, src)
        {
            return canonical_verb(&method.to_lowercase()).unwrap_or("get");
        }
    }
    "get"
}

// ---------------------------------------------------------------------------
// Walker
// ---------------------------------------------------------------------------

/// Shared state for the single route walk.
struct Extractor<'a> {
    src: &'a [u8],
    lang: Lang,
    ctx: RouterContext,
    rpc: RpcContext,
    /// JS/TS only: the file mentions "grpc" somewhere (TASK-088 gate).
    grpc_hint: bool,
    opts: ContractOptions,
    out: Vec<ContractCandidate>,
}

/// Receiver names treated as routers even without a tracked binding.
const JS_ROUTER_VARS: &[&str] = &["app", "router", "api", "server", "r"];
/// Verb-named methods that register routes on a router receiver.
const JS_PROVIDER_VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "all"];
/// Verb-named methods on HTTP client receivers (provider verbs minus the
/// `all` catch-all, which has no consumer meaning).
const JS_CONSUMER_VERBS: &[&str] = &["get", "post", "put", "patch", "delete"];
/// HTTP client receivers whose verb-named methods are outbound calls.
const JS_CONSUMER_RECEIVERS: &[&str] = &["axios", "got", "http", "https"];
/// Receiver/chain-root names treated as websocket endpoints (TASK-087).
/// Checked before the queue generic arms so `socket.send` is websocket,
/// never queue.
///
/// Deliberately narrow: `server` and `conn` are NOT whitelisted because
/// Node's `http`/`net` idioms use them for plain event streams
/// (`server.on('listening')`, `conn.on('data')`), which would flood the
/// websocket kind with false lifecycle/stream contracts. Those calls fall
/// through to the generic `.on` skip (EventEmitter gate) in the queue
/// arms instead.
const WS_RECEIVERS: &[&str] = &["io", "socket", "ws", "wss", "websocket"];
/// Verb-like callee names used by the 0.5 heuristic on unknown receivers.
const AMBIGUOUS_VERBS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "any", "all", "request",
];
/// Receiver names treated as Flask/FastAPI routers without a tracked binding.
const PY_ROUTER_VARS: &[&str] = &["app", "bp", "router", "api"];
/// HTTP client receivers for Python outbound calls.
const PY_CONSUMER_RECEIVERS: &[&str] = &["requests", "httpx", "session", "client"];
/// Verb-named methods on those receivers.
const PY_CONSUMER_VERBS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "request",
];

/// Text of a node, or empty string.
fn node_text<'a>(node: Option<Node<'a>>, src: &'a [u8]) -> &'a str {
    node.and_then(|n| n.utf8_text(src).ok())
        .filter(|t| !t.is_empty())
        .unwrap_or("")
}

/// Extracted textual content of a call's path argument.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PathArg {
    /// Sole string literal or rendered template — keeps caller confidence.
    Direct(String),
    /// Concatenation carrying exactly one path-like literal — 0.5 confidence.
    Concat(String),
}

/// Heuristic gate deciding whether a string could be an HTTP path:
/// it starts with `/` (including protocol-relative `//`) or carries a scheme.
fn is_path_like(s: &str) -> bool {
    let t = s.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`'));
    t.starts_with('/') || t.contains("://")
}

/// Append a raw segment to a prefix with exactly one separating slash.
/// Never produces a leading `//` — stage 2 reads that as a protocol-relative
/// URL and would strip the first segment as an authority.
fn append_segment(prefix: &mut String, seg: &str) {
    let seg = seg.trim_start_matches('/');
    if prefix.is_empty() || !prefix.ends_with('/') {
        prefix.push('/');
    }
    prefix.push_str(seg);
}

/// Concatenate a raw prefix and a route literal; stage 6 collapses slashes.
fn join_raw(prefix: &str, literal: &str) -> String {
    if prefix.is_empty() {
        literal.to_string()
    } else {
        format!("{prefix}/{literal}")
    }
}

impl<'a> Extractor<'a> {
    /// Visit one node; returns the prefix its children should inherit.
    fn visit(&mut self, node: Node, prefix: &str) -> String {
        match self.lang {
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => self.visit_js(node, prefix),
            Lang::Python => self.visit_python(node, prefix),
            Lang::Ruby => self.visit_ruby(node, prefix),
            Lang::Go => self.visit_go(node, prefix),
            Lang::Rust => self.visit_rust(node, prefix),
            Lang::Java => self.visit_java(node, prefix),
            Lang::Php => self.visit_php(node, prefix),
            Lang::CSharp => self.visit_csharp(node, prefix),
            Lang::C | Lang::Cpp => self.visit_c(node, prefix),
        }
    }

    fn visit_js(&mut self, node: Node, prefix: &str) -> String {
        match node.kind() {
            "call_expression" => self.js_call(node, prefix),
            "pair" => self.js_graphql_pair(node),
            "member_expression" => {
                self.js_env_member(node);
            }
            "subscript_expression" => {
                self.js_env_subscript(node);
            }
            "assignment_expression" => {
                self.js_env_assign(node);
            }
            _ => {}
        }
        prefix.to_string()
    }

    fn js_call(&mut self, node: Node, prefix: &str) {
        if self.grpc_hint {
            self.js_grpc_call(node);
        }
        self.js_graphql_call(node);
        let func = node.child_by_field_name("function");
        let args = node.child_by_field_name("arguments");
        let (Some(func), Some(args)) = (func, args) else {
            return;
        };
        let first_arg = args.named_child(0);
        match func.kind() {
            "identifier" => {
                let name = node_text(Some(func), self.src);
                if name == "fetch"
                    && let Some(arg) = first_arg
                {
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Consumer,
                        "GET",
                        prefix,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            "member_expression" => {
                let recv = node_text(func.child_by_field_name("object"), self.src);
                let prop = node_text(func.child_by_field_name("property"), self.src);
                if JS_PROVIDER_VERBS.contains(&prop)
                    && (self.ctx.is_router_var(recv) || JS_ROUTER_VARS.contains(&recv))
                    && let Some(arg) = first_arg
                {
                    let mount_prefix = self.ctx.effective_prefix(recv);
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Provider,
                        prop,
                        &join_raw(prefix, &mount_prefix),
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                } else if JS_CONSUMER_RECEIVERS.contains(&recv)
                    && JS_CONSUMER_VERBS.contains(&prop)
                    && let Some(arg) = first_arg
                {
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Consumer,
                        prop,
                        prefix,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                } else {
                    // Message-kind matchers run between the HTTP client arm
                    // and the ambiguous HTTP arm (TASK-087 guard order:
                    // websocket receivers, then queue, then HTTP 0.5).
                    self.js_message_call(node, func, recv, prop, args);
                    self.js_job_call(node, recv, prop, args);
                    if AMBIGUOUS_VERBS.contains(&prop)
                        && !self.ctx.is_router_var(recv)
                        && !JS_ROUTER_VARS.contains(&recv)
                        && !JS_CONSUMER_RECEIVERS.contains(&recv)
                        && let Some(arg) = first_arg
                        && matches!(self.path_arg(arg), Some(PathArg::Direct(ref s)) if is_path_like(s))
                    {
                        self.emit_http(
                            node,
                            arg,
                            ContractRole::Provider,
                            prop,
                            prefix,
                            CONFIDENCE_HEURISTIC,
                            None,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// Queue (TASK-087) and websocket call matchers for JS/TS. Runs after
    /// the HTTP arms so verb collisions resolve by guard order.
    /// Websocket (TASK-087 step 6) and queue call matchers for JS/TS.
    /// Websocket receivers win the send/emit collisions; queue generic
    /// arms see only non-ws receivers; a generic `.emit` on an unknown
    /// receiver is a 0.5 websocket provider; generic `.on` never fires
    /// (EventEmitter flood).
    fn js_message_call(&mut self, node: Node, func: Node, recv: &str, prop: &str, args: Node) {
        let ws_receiver = self.is_ws_receiver(func);
        let first = positional_arg(args, 0);
        match prop {
            _ if ws_receiver && matches!(prop, "emit" | "send") => {
                if let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_ws(
                        node,
                        t,
                        &raw,
                        ContractRole::Provider,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            _ if ws_receiver && prop == "on" => {
                // A registration carries an event name and a handler.
                if args.named_child_count() >= 2
                    && let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_ws(
                        node,
                        t,
                        &raw,
                        ContractRole::Consumer,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            // express-ws: app.ws('/path', handler) — path-identified
            // consumer; pair-inert (its provider is io connections).
            "ws" if self.ctx.is_router_var(recv) || JS_ROUTER_VARS.contains(&recv) => {
                if let Some(t) = first {
                    self.emit_ws_path(node, t, ContractRole::Consumer);
                }
            }
            "emit" => {
                if let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_ws(
                        node,
                        t,
                        &raw,
                        ContractRole::Provider,
                        CONFIDENCE_HEURISTIC,
                        None,
                    );
                }
            }
            _ => {
                if !ws_receiver {
                    self.js_queue_call(node, recv, prop, args);
                }
            }
        }
    }

    /// Job matchers (TASK-087 step 7): cron/agenda registrations are
    /// providers — named by the callback when it is an identifier, else by
    /// the enclosing function; BullMQ-style queue adds are 0.5 consumers
    /// (generic `add` verb).
    fn js_job_call(&mut self, node: Node, recv: &str, prop: &str, args: Node) {
        let recv_lower = recv.to_lowercase();
        let first = positional_arg(args, 0);
        match prop {
            "schedule" if recv == "cron" || recv_lower.contains("cron") => {
                let (name, name_node) =
                    match positional_arg(args, 1).filter(|cb| cb.kind() == "identifier") {
                        Some(cb) => (node_text(Some(cb), self.src).to_string(), cb),
                        None => {
                            let Some(own) =
                                crate::indexer::find_enclosing_function(node, self.src, self.lang)
                            else {
                                return;
                            };
                            (own, node)
                        }
                    };
                self.emit_job(
                    node,
                    name_node,
                    &name,
                    ContractRole::Provider,
                    CONFIDENCE_FRAMEWORK,
                    None,
                );
            }
            "define" if recv_lower.contains("agenda") => {
                if let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_job(
                        node,
                        t,
                        &raw,
                        ContractRole::Provider,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            "add" if recv == "q" || recv_lower.contains("queue") || recv_lower.contains("bull") => {
                if let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_job(
                        node,
                        t,
                        &raw,
                        ContractRole::Consumer,
                        CONFIDENCE_HEURISTIC,
                        None,
                    );
                }
            }
            _ => {}
        }
    }

    /// Queue producers/consumers (DR-031: the code that publishes initiates,
    /// so it is the consumer; the subscriber registers the handler).
    fn js_queue_call(&mut self, node: Node, recv: &str, prop: &str, args: Node) {
        let argc = args.named_child_count();
        let first = positional_arg(args, 0);
        let cons = ContractRole::Consumer;
        let prov = ContractRole::Provider;
        match prop {
            "sendToQueue" => {
                // amqplib: sendToQueue(queue, content)
                if let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_queue(node, t, &raw, cons, "rabbitmq", CONFIDENCE_FRAMEWORK, None);
                }
            }
            "publish" if argc >= 3 => {
                // amqplib: publish(exchange, routingKey, content)
                if let Some(t) = positional_arg(args, 1)
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_queue(node, t, &raw, cons, "rabbitmq", CONFIDENCE_FRAMEWORK, None);
                }
            }
            "consume" => {
                // amqplib: consume(queue, callback) — the only idiomatic
                // `.consume` in JS clients.
                if let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_queue(node, t, &raw, prov, "rabbitmq", CONFIDENCE_FRAMEWORK, None);
                }
            }
            "send" | "subscribe" => {
                let role = if prop == "send" { cons } else { prov };
                if let Some(arg) = first {
                    // kafkajs object shape: send({topic: 'x'}) / subscribe({topic})
                    if let Some(value_node) = js_prop_string_node(arg, "topic", self.src) {
                        let raw = string_content(value_node, self.src);
                        self.emit_queue(
                            node,
                            value_node,
                            &raw,
                            role,
                            "kafka",
                            CONFIDENCE_FRAMEWORK,
                            None,
                        );
                    } else if matches!(recv, "nc" | "nats")
                        && let Some(raw) = self.topic_arg(arg)
                    {
                        // nats.js: nc.publish(subj, payload) / nc.subscribe(subj)
                        self.emit_queue(node, arg, &raw, role, "nats", CONFIDENCE_FRAMEWORK, None);
                    } else if let Some(raw) = self.topic_arg(arg) {
                        // Generic tier: broker token from the receiver name.
                        self.emit_queue(
                            node,
                            arg,
                            &raw,
                            role,
                            broker_token(recv),
                            CONFIDENCE_HEURISTIC,
                            None,
                        );
                    }
                }
            }
            "publish" => {
                if let Some(arg) = first {
                    if matches!(recv, "nc" | "nats")
                        && let Some(raw) = self.topic_arg(arg)
                    {
                        self.emit_queue(node, arg, &raw, cons, "nats", CONFIDENCE_FRAMEWORK, None);
                    } else if let Some(raw) = self.topic_arg(arg) {
                        self.emit_queue(
                            node,
                            arg,
                            &raw,
                            cons,
                            broker_token(recv),
                            CONFIDENCE_HEURISTIC,
                            None,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// `process.env.NAME` / `import.meta.env.NAME` reads.
    fn js_env_member(&mut self, node: Node) {
        if is_assignment_target(node, "assignment_expression") {
            return; // the assignment handler owns this site
        }
        let obj = node_text(node.child_by_field_name("object"), self.src);
        let prop = node_text(node.child_by_field_name("property"), self.src);
        if matches!(obj, "process.env" | "import.meta.env") && is_env_name(prop) {
            self.emit_env(node, prop, ContractRole::Consumer, CONFIDENCE_FRAMEWORK);
        }
    }

    /// `process.env['NAME']` reads.
    fn js_env_subscript(&mut self, node: Node) {
        if is_assignment_target(node, "assignment_expression") {
            return;
        }
        let obj = node_text(node.child_by_field_name("object"), self.src);
        let name = node
            .child_by_field_name("index")
            .filter(|n| n.kind() == "string")
            .map(|n| string_content(n, self.src))
            .unwrap_or_default();
        if obj == "process.env" && is_env_name(&name) {
            self.emit_env(node, &name, ContractRole::Consumer, CONFIDENCE_FRAMEWORK);
        }
    }

    /// `process.env.NAME = …` / `process.env['NAME'] = …` writes (0.5).
    fn js_env_assign(&mut self, node: Node) {
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        let name = match left.kind() {
            "member_expression" => {
                let obj = node_text(left.child_by_field_name("object"), self.src);
                let prop = node_text(left.child_by_field_name("property"), self.src);
                if matches!(obj, "process.env" | "import.meta.env") {
                    prop.to_string()
                } else {
                    return;
                }
            }
            "subscript_expression" => {
                let obj = node_text(left.child_by_field_name("object"), self.src);
                let name = left
                    .child_by_field_name("index")
                    .filter(|n| n.kind() == "string")
                    .map(|n| string_content(n, self.src))
                    .unwrap_or_default();
                if obj == "process.env" {
                    name
                } else {
                    return;
                }
            }
            _ => return,
        };
        if is_env_name(&name) {
            self.emit_env(node, &name, ContractRole::Provider, CONFIDENCE_HEURISTIC);
        }
    }

    // -- Python ------------------------------------------------------------------

    fn visit_python(&mut self, node: Node, prefix: &str) -> String {
        match node.kind() {
            "decorated_definition" => return self.py_decorated(node, prefix),
            "call" => {
                if self.opts.grpc {
                    self.python_grpc_call(node);
                }
                self.python_graphql_call(node);
                self.py_call(node, prefix);
            }
            "subscript" => self.py_env_subscript(node),
            "assignment" => self.py_assignment(node, prefix),
            "class_definition" if self.opts.grpc => {
                self.python_grpc_class(node);
            }
            _ => {}
        }
        prefix.to_string()
    }

    /// `@app.get('/x')` / `@app.route('/x', methods=['POST'])` decorators.
    fn py_decorated(&mut self, node: Node, prefix: &str) -> String {
        let def_name = node
            .child_by_field_name("definition")
            .and_then(|def| def.child_by_field_name("name"));
        let owning = def_name.map(|n| node_text(Some(n), self.src).to_string());
        for i in 0..node.child_count() {
            let Some(dec) = node.child(i as u32) else {
                continue;
            };
            if dec.kind() != "decorator" {
                continue;
            }
            // TASK-088 graphql: @strawberry.field/@strawberry.mutation and
            // Ariadne @Query.field("name")/@Mutation.mutation declare
            // resolvers (providers) — gated at the emit_graphql choke point.
            if let Some((root, field)) = py_graphql_decorator(dec, def_name, self.src) {
                let anchor = def_name.unwrap_or(dec);
                self.emit_graphql(
                    anchor,
                    &root,
                    &field,
                    ContractRole::Provider,
                    owning.as_deref(),
                );
                continue;
            }
            // TASK-087 job: @app.task / @app.task(name=…) / @shared_task.
            // `name=` wins; otherwise the decorated function names the job.
            if let Some((job_name, name_node)) = py_job_decorator(dec, def_name, self.src) {
                self.emit_job(
                    dec,
                    name_node,
                    &job_name,
                    ContractRole::Provider,
                    CONFIDENCE_FRAMEWORK,
                    owning.as_deref(),
                );
                continue;
            }
            let Some(call) = dec.named_child(0) else {
                continue;
            };
            if call.kind() != "call" {
                continue;
            }
            let Some(func) = call.child_by_field_name("function") else {
                continue;
            };
            if func.kind() != "attribute" {
                continue;
            }
            let args = call.child_by_field_name("arguments");
            let Some(args) = args else { continue };
            let recv = node_text(func.child_by_field_name("object"), self.src);
            let attr = node_text(func.child_by_field_name("attribute"), self.src);
            let is_router = self.ctx.is_router_var(recv) || PY_ROUTER_VARS.contains(&recv);
            let Some(path_node) = positional_arg(args, 0) else {
                continue;
            };
            let verb = match attr {
                "route" => py_route_verb(args, self.src),
                "get" | "post" | "put" | "patch" | "delete" if is_router => attr,
                _ => continue,
            };
            let mount_prefix = self.ctx.effective_prefix(recv);
            self.emit_http(
                call,
                path_node,
                ContractRole::Provider,
                verb,
                &join_raw(prefix, &mount_prefix),
                CONFIDENCE_FRAMEWORK,
                owning.as_deref(),
            );
        }
        prefix.to_string()
    }

    /// Celery task decorator: bare `@app.task` (attribute form, no parens)
    /// or call form `@app.task(...)` / `@shared_task(...)`. Returns the job
    /// name and the node that carries it — the `name=` literal when given,
    /// the decorated function's name otherwise.
    /// Consumer calls, Falcon `add_route`, and env accessors.
    fn py_call(&mut self, node: Node, prefix: &str) {
        let func = node.child_by_field_name("function");
        let args = node.child_by_field_name("arguments");
        let (Some(func), Some(args)) = (func, args) else {
            return;
        };
        match func.kind() {
            "attribute" => {
                let recv = node_text(func.child_by_field_name("object"), self.src);
                let attr = node_text(func.child_by_field_name("attribute"), self.src);
                let first = positional_arg(args, 0);
                match (recv, attr) {
                    ("os.environ", "get") | ("os", "getenv") => {
                        if let Some(arg) = first
                            && let Some(name) = py_string_content(arg, self.src)
                        {
                            self.emit_env(
                                node,
                                &name,
                                ContractRole::Consumer,
                                CONFIDENCE_FRAMEWORK,
                            );
                        }
                    }
                    ("os.environ", "setdefault") => {
                        if let Some(arg) = first
                            && let Some(name) = py_string_content(arg, self.src)
                        {
                            self.emit_env(
                                node,
                                &name,
                                ContractRole::Provider,
                                CONFIDENCE_HEURISTIC,
                            );
                        }
                    }
                    (_, "add_route")
                        if self.ctx.is_router_var(recv) || PY_ROUTER_VARS.contains(&recv) =>
                    {
                        if let Some(arg) = first {
                            self.emit_http(
                                node,
                                arg,
                                ContractRole::Provider,
                                "ANY",
                                prefix,
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        }
                    }
                    // -- queue (TASK-087, DR-031) --------------------------------
                    // confluent-kafka: producer.produce('topic', value=…).
                    (_, "produce") => {
                        if let Some(t) = first
                            && let Some(raw) = self.topic_arg(t)
                        {
                            self.emit_queue(
                                node,
                                t,
                                &raw,
                                ContractRole::Consumer,
                                "kafka",
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        }
                    }
                    // pika: basic_publish(…, routing_key=…) else positional 2.
                    (_, "basic_publish") => {
                        let t = kwarg_string_node(args, "routing_key", self.src)
                            .or_else(|| positional_arg(args, 1));
                        if let Some(t) = t
                            && let Some(raw) = self.topic_arg(t)
                        {
                            self.emit_queue(
                                node,
                                t,
                                &raw,
                                ContractRole::Consumer,
                                "rabbitmq",
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        }
                    }
                    // pika: basic_consume(queue=…) else positional 1.
                    (_, "basic_consume") => {
                        let t = kwarg_string_node(args, "queue", self.src).or(first);
                        if let Some(t) = t
                            && let Some(raw) = self.topic_arg(t)
                        {
                            self.emit_queue(
                                node,
                                t,
                                &raw,
                                ContractRole::Provider,
                                "rabbitmq",
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        }
                    }
                    // nats-py: nc.publish(subj, payload) / nc.subscribe(subj).
                    (_, "publish") if matches!(recv, "nc" | "nats") => {
                        if let Some(t) = first
                            && let Some(raw) = self.topic_arg(t)
                        {
                            self.emit_queue(
                                node,
                                t,
                                &raw,
                                ContractRole::Consumer,
                                "nats",
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        }
                    }
                    (_, "subscribe") if matches!(recv, "nc" | "nats") => {
                        if let Some(t) = first
                            && let Some(raw) = self.topic_arg(t)
                        {
                            self.emit_queue(
                                node,
                                t,
                                &raw,
                                ContractRole::Provider,
                                "nats",
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        }
                    }
                    // -- job (TASK-087 step 7) ---------------------------------
                    // task_fn.delay()/apply_async(): the receiver is the
                    // task, so a simple-identifier receiver is required.
                    (_, "delay") | (_, "apply_async") => {
                        if let Some(recv_node) = func
                            .child_by_field_name("object")
                            .filter(|n| n.kind() == "identifier")
                        {
                            let name = node_text(Some(recv_node), self.src).to_string();
                            self.emit_job(
                                node,
                                recv_node,
                                &name,
                                ContractRole::Consumer,
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        }
                    }
                    // celery_app.send_task('orders.sync', …) — dispatch by
                    // name.
                    (_, "send_task") => {
                        if let Some(t) = first
                            && let Some(raw) = self.topic_arg(t)
                        {
                            self.emit_job(
                                node,
                                t,
                                &raw,
                                ContractRole::Consumer,
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        }
                    }
                    // scheduler.add_job(fn, …): the function (or the
                    // enclosing one) names the schedule.
                    (_, "add_job") if recv.to_lowercase().contains("sched") => {
                        let target = positional_arg(args, 0).filter(|n| n.kind() == "identifier");
                        let (name, name_node) = match target {
                            Some(id) => (node_text(Some(id), self.src).to_string(), id),
                            None => {
                                let Some(own) = crate::indexer::find_enclosing_function(
                                    node, self.src, self.lang,
                                ) else {
                                    return;
                                };
                                (own, node)
                            }
                        };
                        self.emit_job(
                            node,
                            name_node,
                            &name,
                            ContractRole::Provider,
                            CONFIDENCE_FRAMEWORK,
                            None,
                        );
                    }
                    // Generic tier: send/subscribe with a string literal at
                    // 0.5, broker token from the receiver name. The
                    // list-of-one subscribe shape is kafka-python's and
                    // carries 1.0.
                    (_, "send") | (_, "subscribe") => {
                        if let Some(t) = first {
                            let role = if attr == "send" {
                                ContractRole::Consumer
                            } else {
                                ContractRole::Provider
                            };
                            let (broker, confidence) =
                                if attr == "subscribe" && matches!(t.kind(), "list" | "tuple") {
                                    ("kafka", CONFIDENCE_FRAMEWORK)
                                } else {
                                    (broker_token(recv), CONFIDENCE_HEURISTIC)
                                };
                            if let Some(raw) = self.topic_arg(t) {
                                self.emit_queue(node, t, &raw, role, broker, confidence, None);
                            }
                        }
                    }
                    _ => {
                        if let Some(arg) = first {
                            if PY_CONSUMER_RECEIVERS.contains(&recv)
                                && PY_CONSUMER_VERBS.contains(&attr)
                            {
                                self.emit_http(
                                    node,
                                    arg,
                                    ContractRole::Consumer,
                                    attr,
                                    prefix,
                                    CONFIDENCE_FRAMEWORK,
                                    None,
                                );
                            } else if !PY_CONSUMER_RECEIVERS.contains(&recv)
                                && !self.ctx.is_router_var(recv)
                                && !PY_ROUTER_VARS.contains(&recv)
                                && AMBIGUOUS_VERBS.contains(&attr)
                            {
                                self.ambiguous_http(node, arg, attr, prefix);
                            }
                        }
                    }
                }
            }
            "identifier" => {
                if node_text(Some(func), self.src) == "urlopen"
                    && let Some(arg) = positional_arg(args, 0)
                {
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Consumer,
                        "GET",
                        prefix,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            _ => {}
        }
    }

    /// `os.environ['X']` reads.
    fn py_env_subscript(&mut self, node: Node) {
        if is_assignment_target(node, "assignment") {
            return; // the assignment handler owns this site
        }
        let obj = node_text(node.child_by_field_name("value"), self.src);
        let name = node
            .child_by_field_name("subscript")
            .filter(|n| n.kind() == "string")
            .and_then(|n| py_string_content(n, self.src))
            .unwrap_or_default();
        if obj == "os.environ" && is_env_name(&name) {
            self.emit_env(node, &name, ContractRole::Consumer, CONFIDENCE_FRAMEWORK);
        }
    }

    /// `os.environ['X'] = …` writes and Django `urlpatterns` lists.
    fn py_assignment(&mut self, node: Node, prefix: &str) {
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        match left.kind() {
            "subscript" => {
                let obj = node_text(left.child_by_field_name("value"), self.src);
                let name = left
                    .child_by_field_name("subscript")
                    .filter(|n| n.kind() == "string")
                    .and_then(|n| py_string_content(n, self.src))
                    .unwrap_or_default();
                if obj == "os.environ" && is_env_name(&name) {
                    self.emit_env(node, &name, ContractRole::Provider, CONFIDENCE_HEURISTIC);
                }
            }
            "identifier" if node_text(Some(left), self.src) == "urlpatterns" => {
                // Django URLconf: each path()/re_path() call is a route (ANY).
                let Some(right) = node.child_by_field_name("right") else {
                    return;
                };
                if right.kind() == "list" {
                    for i in 0..right.named_child_count() {
                        let Some(call) = right.named_child(i as u32) else {
                            continue;
                        };
                        if call.kind() != "call" {
                            continue;
                        }
                        let Some(func) = call.child_by_field_name("function") else {
                            continue;
                        };
                        if func.kind() != "identifier" {
                            continue;
                        }
                        if !matches!(node_text(Some(func), self.src), "path" | "re_path") {
                            continue;
                        }
                        let Some(args) = call.child_by_field_name("arguments") else {
                            continue;
                        };
                        let Some(arg) = positional_arg(args, 0) else {
                            continue;
                        };
                        self.emit_http(
                            call,
                            arg,
                            ContractRole::Provider,
                            "ANY",
                            prefix,
                            CONFIDENCE_FRAMEWORK,
                            None,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    // -- Ruby --------------------------------------------------------------------

    fn visit_ruby(&mut self, node: Node, prefix: &str) -> String {
        match node.kind() {
            "call" => return self.ruby_call(node, prefix),
            "class" => self.ruby_class_job(node),
            "element_reference" => self.ruby_env_ref(node),
            "assignment" => self.ruby_env_assign(node),
            _ => {}
        }
        prefix.to_string()
    }

    /// TASK-087 job: a class that includes Sidekiq::Job/Sidekiq::Worker or
    /// subclasses ApplicationJob defines a background job named after the
    /// class.
    fn ruby_class_job(&mut self, node: Node) {
        let Some(name_node) = node.named_child(0).filter(|n| n.kind() == "constant") else {
            return;
        };
        let is_active_job = (0..node.named_child_count())
            .filter_map(|i| node.named_child(i as u32))
            .find(|n| n.kind() == "superclass")
            .and_then(|s| s.named_child(0))
            .is_some_and(|c| node_text(Some(c), self.src) == "ApplicationJob");
        let is_sidekiq = (0..node.named_child_count())
            .filter_map(|i| node.named_child(i as u32))
            .find(|n| n.kind() == "body_statement")
            .is_some_and(|body| ruby_includes_sidekiq(body, self.src));
        if is_active_job || is_sidekiq {
            let name = node_text(Some(name_node), self.src).to_string();
            self.emit_job(
                node,
                name_node,
                &name,
                ContractRole::Provider,
                CONFIDENCE_FRAMEWORK,
                None,
            );
        }
    }

    /// Sinatra `get '/x' do`, Rails `get '/x', to: …` / `match`, client
    /// libraries with constant receivers, and `ENV.fetch`. `namespace`/`scope`
    /// blocks return the prefix their children inherit.
    fn ruby_call(&mut self, node: Node, prefix: &str) -> String {
        let method = node_text(node.child_by_field_name("method"), self.src);
        let receiver = node.child_by_field_name("receiver");
        let args = node.child_by_field_name("arguments");
        // TASK-087: Bunny `q.subscribe do … end` carries no argument list —
        // the topic comes from the channel.queue binding pre-pass.
        if method == "subscribe"
            && let Some(recv) = receiver
            && recv.kind() == "identifier"
            && let Some(name) = self
                .ctx
                .queue_bindings
                .get(node_text(Some(recv), self.src))
                .cloned()
        {
            self.emit_queue(
                node,
                recv,
                &name,
                ContractRole::Provider,
                "rabbitmq",
                CONFIDENCE_FRAMEWORK,
                None,
            );
            return prefix.to_string();
        }
        let Some(args) = args else {
            return prefix.to_string();
        };
        let first = positional_arg(args, 0);
        match receiver {
            None => {
                if matches!(method, "namespace" | "scope")
                    && let Some(arg) = positional_arg(args, 0)
                {
                    let seg = match arg.kind() {
                        "simple_symbol" => node_text(Some(arg), self.src)
                            .trim_start_matches(':')
                            .to_string(),
                        "string" => ruby_string_content(arg, self.src),
                        _ => String::new(),
                    };
                    if !seg.is_empty() {
                        return join_raw(prefix, &seg);
                    }
                }
                let verb = match method {
                    "get" | "post" | "put" | "patch" | "delete" | "match" => method,
                    _ => return prefix.to_string(),
                };
                if let Some(arg) = first {
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Provider,
                        verb,
                        prefix,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            Some(recv) => {
                let recv_text = node_text(Some(recv), self.src);
                match recv_text {
                    "ENV" if method == "fetch" => {
                        if let Some(arg) = first {
                            let name = ruby_string_content(arg, self.src);
                            self.emit_env(
                                node,
                                &name,
                                ContractRole::Consumer,
                                CONFIDENCE_FRAMEWORK,
                            );
                        }
                    }
                    // TASK-087 job: Worker.perform_async / perform_in /
                    // perform_at (Sidekiq) and Job.perform_later (ActiveJob)
                    // enqueue — constant receiver names the job class.
                    _ if matches!(
                        method,
                        "perform_async" | "perform_in" | "perform_at" | "perform_later"
                    ) && recv.kind() == "constant" =>
                    {
                        let name = node_text(Some(recv), self.src).to_string();
                        self.emit_job(
                            node,
                            recv,
                            &name,
                            ContractRole::Consumer,
                            CONFIDENCE_FRAMEWORK,
                            None,
                        );
                    }
                    // TASK-087 queue: Bunny publish(payload, routing_key: …)
                    // is a 1.0 consumer; a plain string first argument falls
                    // to the generic tier with the receiver's broker token.
                    _ if method == "publish" => {
                        if let Some((t, raw)) = ruby_kwarg_string(args, "routing_key", self.src) {
                            self.emit_queue(
                                node,
                                t,
                                &raw,
                                ContractRole::Consumer,
                                "rabbitmq",
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        } else if let Some(t) = first
                            && let Some(raw) = self.topic_arg(t)
                        {
                            self.emit_queue(
                                node,
                                t,
                                &raw,
                                ContractRole::Consumer,
                                broker_token(recv_text),
                                CONFIDENCE_HEURISTIC,
                                None,
                            );
                        }
                    }
                    _ if RUBY_CONSUMER_RECEIVERS.contains(&recv_text) => {
                        if let (Some(arg), Some(verb)) = (first, canonical_verb(method)) {
                            self.emit_http(
                                node,
                                arg,
                                ContractRole::Consumer,
                                verb,
                                prefix,
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        }
                    }
                    _ if recv_text != "ENV" && AMBIGUOUS_VERBS.contains(&method) => {
                        if let Some(arg) = first {
                            self.ambiguous_http(node, arg, method, prefix);
                        }
                    }
                    _ => {}
                }
            }
        }
        prefix.to_string()
    }

    /// `ENV['X']` reads.
    fn ruby_env_ref(&mut self, node: Node) {
        if is_assignment_target(node, "assignment") {
            return; // the assignment handler owns this site
        }
        let obj = node_text(node.named_child(0), self.src);
        let name = node
            .named_child(1)
            .filter(|n| n.kind() == "string")
            .map(|n| ruby_string_content(n, self.src))
            .unwrap_or_default();
        if obj == "ENV" && is_env_name(&name) {
            self.emit_env(node, &name, ContractRole::Consumer, CONFIDENCE_FRAMEWORK);
        }
    }

    /// `ENV['X'] = …` writes (0.5).
    fn ruby_env_assign(&mut self, node: Node) {
        let Some(left) = node.child_by_field_name("left") else {
            return;
        };
        if left.kind() != "element_reference" {
            return;
        }
        let obj = node_text(left.named_child(0), self.src);
        let name = left
            .named_child(1)
            .filter(|n| n.kind() == "string")
            .map(|n| ruby_string_content(n, self.src))
            .unwrap_or_default();
        if obj == "ENV" && is_env_name(&name) {
            self.emit_env(node, &name, ContractRole::Provider, CONFIDENCE_HEURISTIC);
        }
    }

    // -- Go ----------------------------------------------------------------------

    fn visit_go(&mut self, node: Node, prefix: &str) -> String {
        if node.kind() == "call_expression" {
            if self.opts.grpc {
                self.go_grpc(node);
            }
            self.go_call(node, prefix);
        }
        prefix.to_string()
    }

    fn go_call(&mut self, node: Node, prefix: &str) {
        let func = node.child_by_field_name("function");
        let args = node.child_by_field_name("arguments");
        let (Some(func), Some(args)) = (func, args) else {
            return;
        };
        if func.kind() != "selector_expression" {
            return;
        }
        let recv = node_text(func.child_by_field_name("operand"), self.src);
        let meth = node_text(func.child_by_field_name("field"), self.src);
        let first = positional_arg(args, 0);
        let second = positional_arg(args, 1);
        let argc = args.named_child_count();
        match (recv, meth) {
            // gin/chi-style registration: uppercase verb + handler arg.
            _ if GO_PROVIDER_VERBS.contains(&meth) && argc >= 2 => {
                if let Some(arg) = first {
                    let pfx = join_raw(prefix, &self.ctx.effective_prefix(recv));
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Provider,
                        meth,
                        &pfx,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            (_, "HandleFunc") | (_, "Handle") if argc >= 2 => {
                if let Some(arg) = first {
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Provider,
                        "ANY",
                        prefix,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            ("http", "NewRequest") => {
                if let (Some(verb), Some(path)) = (first, second) {
                    let verb = render_string_node(verb, self.src, self.lang);
                    self.emit_http(
                        node,
                        path,
                        ContractRole::Consumer,
                        &verb,
                        prefix,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            ("os", "Getenv") | ("os", "LookupEnv") => {
                if let Some(arg) = first {
                    let name = render_string_node(arg, self.src, self.lang);
                    self.emit_env(node, &name, ContractRole::Consumer, CONFIDENCE_FRAMEWORK);
                }
            }
            ("os", "Setenv") => {
                if let Some(arg) = first {
                    let name = render_string_node(arg, self.src, self.lang);
                    self.emit_env(node, &name, ContractRole::Provider, CONFIDENCE_HEURISTIC);
                }
            }
            // -- queue (TASK-087, DR-031) --------------------------------
            // Publish arity disambiguates the broker: nats.Publish(subj, data)
            // takes 2 positional args; amqp Publish*/PublishWithContext carry
            // exchange+key before the message. PublishWithContext carries a
            // leading ctx: the idiomatic nats.go shape
            // PublishWithContext(ctx, subj, data) has exactly 3 args with the
            // subject at position 1, while amqp091's
            // PublishWithContext(ctx, exchange, key, msg, ...) has >= 4 args
            // with the routing key at position 2.
            (_, "PublishWithContext") if argc >= 4 => {
                if let Some(t) = positional_arg(args, 2)
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_queue(
                        node,
                        t,
                        &raw,
                        ContractRole::Consumer,
                        "rabbitmq",
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            (_, "PublishWithContext") if argc == 3 => {
                if let Some(t) = positional_arg(args, 1)
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_queue(
                        node,
                        t,
                        &raw,
                        ContractRole::Consumer,
                        "nats",
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            (_, m) if m.starts_with("Publish") && m != "PublishWithContext" && argc >= 3 => {
                if let Some(t) = positional_arg(args, 1)
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_queue(
                        node,
                        t,
                        &raw,
                        ContractRole::Consumer,
                        "rabbitmq",
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            (_, "Publish") if argc == 2 => {
                if let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_queue(
                        node,
                        t,
                        &raw,
                        ContractRole::Consumer,
                        "nats",
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            (_, "Subscribe") | (_, "QueueSubscribe") => {
                if let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_queue(
                        node,
                        t,
                        &raw,
                        ContractRole::Provider,
                        "nats",
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            (_, "Consume") => {
                if let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_queue(
                        node,
                        t,
                        &raw,
                        ContractRole::Provider,
                        "rabbitmq",
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            // sarama: ConsumePartition(topic, partition, offset).
            (_, "ConsumePartition") => {
                if let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_queue(
                        node,
                        t,
                        &raw,
                        ContractRole::Provider,
                        "kafka",
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            // sarama: producer.SendMessage(&ProducerMessage{Topic: "…"}).
            (_, "SendMessage") => {
                if let Some(t) = first
                    && let Some(raw) = self.topic_arg(t)
                {
                    self.emit_queue(
                        node,
                        t,
                        &raw,
                        ContractRole::Consumer,
                        "kafka",
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            // TASK-087 job: robfig/cron c.AddFunc(spec, fn)/AddJob — the
            // scheduled function names the job, else the enclosing one.
            (_, "AddFunc") | (_, "AddJob") => {
                let (name, name_node) =
                    match positional_arg(args, 1).filter(|n| n.kind() == "identifier") {
                        Some(id) => (node_text(Some(id), self.src).to_string(), id),
                        None => {
                            let Some(own) =
                                crate::indexer::find_enclosing_function(node, self.src, self.lang)
                            else {
                                return;
                            };
                            (own, node)
                        }
                    };
                self.emit_job(
                    node,
                    name_node,
                    &name,
                    ContractRole::Provider,
                    CONFIDENCE_FRAMEWORK,
                    None,
                );
            }
            _ => {
                if let Some(verb) = go_client_verb(recv, meth)
                    && let Some(arg) = first
                {
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Consumer,
                        verb,
                        prefix,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                } else if argc == 1
                    && GO_AMBIGUOUS_VERBS.contains(&meth)
                    && let Some(arg) = first
                {
                    self.ambiguous_http(node, arg, meth, prefix);
                }
            }
        }
    }

    // -- Rust --------------------------------------------------------------------

    fn visit_rust(&mut self, node: Node, prefix: &str) -> String {
        match node.kind() {
            "attribute_item" => {
                self.rust_attribute(node);
            }
            "call_expression" => {
                if self.opts.grpc {
                    self.rust_grpc_call(node);
                }
                self.rust_call(node, prefix);
            }
            "impl_item" => {
                if self.opts.grpc {
                    self.rust_grpc_impl(node);
                }
            }
            "macro_invocation" => {
                self.rust_macro(node);
            }
            _ => {}
        }
        prefix.to_string()
    }

    /// `#[get("/x")]` / `#[route("/x", method = "GET")]` attribute macros
    /// (Actix, Rocket).
    fn rust_attribute(&mut self, node: Node) {
        let Some(attr) = node.named_child(0) else {
            return;
        };
        if attr.kind() != "attribute" {
            return;
        }
        // tree-sitter-rust gives the attribute name no field slot; it is the
        // leading identifier child.
        let name = node_text(attr.named_child(0), self.src).to_string();
        let Some(tree) = attr.child_by_field_name("arguments") else {
            return;
        };
        let Some(path_node) = tree_strings(tree).into_iter().next() else {
            return;
        };
        let verb = match name.as_str() {
            "get" | "post" | "put" | "delete" | "patch" | "head" => name.clone(),
            "route" => {
                // method = "GET" — the literal after the `method` token.
                let toks: Vec<_> = (0..tree.named_child_count())
                    .filter_map(|i| tree.named_child(i as u32))
                    .collect();
                let idx = toks.iter().position(|n| {
                    n.kind() == "identifier" && node_text(Some(*n), self.src) == "method"
                });
                match idx.and_then(|i| toks.get(i + 1)) {
                    Some(v) => render_string_node(*v, self.src, self.lang),
                    None => "ANY".to_string(),
                }
            }
            _ => return,
        };
        // The handler is the item this attribute decorates.
        let owning = node
            .next_named_sibling()
            .and_then(|item| item.child_by_field_name("name"))
            .map(|n| node_text(Some(n), self.src).to_string());
        self.emit_http(
            node,
            path_node,
            ContractRole::Provider,
            &verb,
            "",
            CONFIDENCE_FRAMEWORK,
            owning.as_deref(),
        );
    }

    /// `.route("/x", get(handler))` providers, `reqwest`/`client` consumers,
    /// and `std::env::var` / `set_var`.
    fn rust_call(&mut self, node: Node, prefix: &str) {
        let func = node.child_by_field_name("function");
        let args = node.child_by_field_name("arguments");
        let (Some(func), Some(args)) = (func, args) else {
            return;
        };
        let first = positional_arg(args, 0);
        match func.kind() {
            "field_expression" => {
                let field = node_text(func.child_by_field_name("field"), self.src);
                if field == "route"
                    && let Some(arg) = first
                {
                    let verb = positional_arg(args, 1)
                        .and_then(|handler| rust_handler_verb(handler, self.src))
                        .unwrap_or("ANY");
                    let route_prefix = self.rust_receiver_prefix(func.child_by_field_name("value"));
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Provider,
                        verb,
                        &join_raw(prefix, &route_prefix),
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                } else if let Some(verb) = canonical_verb(field) {
                    let value = func.child_by_field_name("value");
                    let root = rust_chain_root(value);
                    let root_text = node_text(root, self.src);
                    let is_client = matches!(root_text, "client" | "reqwest_client")
                        || root_text.starts_with("Client::new");
                    if is_client && let Some(arg) = first {
                        self.emit_http(
                            node,
                            arg,
                            ContractRole::Consumer,
                            verb,
                            prefix,
                            CONFIDENCE_FRAMEWORK,
                            None,
                        );
                    }
                } else if matches!(field, "subscribe" | "publish")
                    && let Some(arg) = first
                    && let Some(raw) = self.topic_arg(arg)
                {
                    // TASK-087 queue: async_nats clients pass "subject".into();
                    // rdkafka consumers pass a single-string slice &[ "topic" ].
                    // No generic Rust tier — only these two pinned shapes.
                    let root_text =
                        node_text(rust_chain_root(func.child_by_field_name("value")), self.src);
                    let is_nats = matches!(root_text, "client" | "nats" | "nc")
                        && arg.kind() == "call_expression";
                    let is_kafka_slice =
                        field == "subscribe" && arg.kind() == "reference_expression";
                    if is_nats || is_kafka_slice {
                        self.emit_queue(
                            node,
                            arg,
                            &raw,
                            if field == "subscribe" {
                                ContractRole::Provider
                            } else {
                                ContractRole::Consumer
                            },
                            if is_nats { "nats" } else { "kafka" },
                            CONFIDENCE_FRAMEWORK,
                            None,
                        );
                    }
                }
            }
            "scoped_identifier" => {
                let text = node_text(Some(func), self.src);
                match text {
                    // rdkafka: FutureRecord::to / BaseRecord::to — publishing
                    // a record initiates, so the site is the consumer (DR-031).
                    t if t.ends_with("Record::to") => {
                        if let Some(arg) = first
                            && let Some(raw) = self.topic_arg(arg)
                        {
                            self.emit_queue(
                                node,
                                arg,
                                &raw,
                                ContractRole::Consumer,
                                "kafka",
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        }
                    }
                    "std::env::var" | "env::var" => {
                        if let Some(arg) = first {
                            let name = render_string_node(arg, self.src, self.lang);
                            self.emit_env(
                                node,
                                &name,
                                ContractRole::Consumer,
                                CONFIDENCE_FRAMEWORK,
                            );
                        }
                    }
                    "std::env::set_var" | "env::set_var" => {
                        if let Some(arg) = first {
                            let name = render_string_node(arg, self.src, self.lang);
                            self.emit_env(
                                node,
                                &name,
                                ContractRole::Provider,
                                CONFIDENCE_HEURISTIC,
                            );
                        }
                    }
                    _ => {
                        if text.starts_with("reqwest::")
                            && let Some(verb) =
                                canonical_verb(text.rsplit("::").next().unwrap_or(""))
                            && let Some(arg) = first
                        {
                            self.emit_http(
                                node,
                                arg,
                                ContractRole::Consumer,
                                verb,
                                prefix,
                                CONFIDENCE_FRAMEWORK,
                                None,
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Prefix carried by a `.route` receiver: inline `web::scope("/p")`
    /// chain segments, plus the prefix of the variable whose initializer
    /// the chain belongs to (Axum `let user_routes = Router::new()…`).
    fn rust_receiver_prefix(&self, recv: Option<Node>) -> String {
        // A bound variable (`api.route(…)`) carries its own prefix.
        if let Some(r) = recv
            && r.kind() == "identifier"
        {
            return self.ctx.effective_prefix(node_text(Some(r), self.src));
        }
        let mut prefix = String::new();
        let mut current = recv;
        let mut last = recv;
        let mut depth = 0;
        while let Some(c) = current
            && c.kind() == "call_expression"
            && depth < PREFIX_DEPTH_CAP
        {
            last = Some(c);
            depth += 1;
            let Some(func) = c.child_by_field_name("function") else {
                break;
            };
            // `web::scope("/p")` parses with a scoped callee; `x.scope("/p")`
            // with a field callee. Either way the receiver continues left.
            let (callee, next) = match func.kind() {
                "field_expression" => (
                    node_text(func.child_by_field_name("field"), self.src),
                    func.child_by_field_name("value"),
                ),
                "scoped_identifier" => {
                    (node_text(func.child_by_field_name("name"), self.src), None)
                }
                _ => break,
            };
            if callee == "scope"
                && let Some(args) = c.child_by_field_name("arguments")
                && let Some(path_node) = positional_arg(args, 0)
                && path_node.kind() == "string_literal"
            {
                append_segment(
                    &mut prefix,
                    &render_string_node(path_node, self.src, self.lang),
                );
            }
            current = next;
        }
        // Attribute the chain's root to the `let` variable it initializes.
        if let Some(root) = current.or(last)
            && let Some(var) = self.ctx.var_for_range(root.start_byte())
        {
            prefix.push_str(&self.ctx.effective_prefix(var));
        }
        prefix
    }

    /// `env!("X")` — macro_invocation children carry no field names.
    fn rust_macro(&mut self, node: Node) {
        let name = node_text(node.named_child(0), self.src);
        if name != "env" {
            return;
        }
        let Some(tree) = node.named_child(1) else {
            return;
        };
        let Some(arg) = tree_strings(tree).into_iter().next() else {
            return;
        };
        let value = render_string_node(arg, self.src, self.lang);
        self.emit_env(node, &value, ContractRole::Consumer, CONFIDENCE_FRAMEWORK);
    }

    // -- Java --------------------------------------------------------------------

    fn visit_java(&mut self, node: Node, prefix: &str) -> String {
        match node.kind() {
            "class_declaration" => {
                if self.opts.grpc {
                    self.java_grpc_impl_base(node);
                }
                return self.java_class(node, prefix);
            }
            "method_declaration" => {
                self.java_method(node, prefix);
            }
            "method_invocation" => {
                if self.opts.grpc {
                    self.java_grpc_call(node);
                }
                self.java_call(node, prefix);
            }
            _ => {}
        }
        prefix.to_string()
    }

    /// Class-level `@RequestMapping("/v1")` (Spring) and `@Path("/items")`
    /// (JAX-RS) prefix every member route.
    fn java_class(&mut self, node: Node, prefix: &str) -> String {
        let Some(modifiers) = node.named_child(0).filter(|n| n.kind() == "modifiers") else {
            return prefix.to_string();
        };
        for i in 0..modifiers.named_child_count() {
            let Some(annot) = modifiers.named_child(i as u32) else {
                continue;
            };
            if !matches!(
                node_text(annot.child_by_field_name("name"), self.src),
                "RequestMapping" | "Path"
            ) {
                continue;
            }
            if let Some(args) = annot.child_by_field_name("arguments")
                && let Some(path) = java_annotation_path(args, self.src)
            {
                return join_raw(prefix, &render_string_node(path, self.src, self.lang));
            }
        }
        prefix.to_string()
    }

    /// Spring mapping annotations and JAX-RS `@Path` + verb markers.
    fn java_method(&mut self, node: Node, prefix: &str) {
        // tree-sitter-java exposes modifiers as a positional child.
        let Some(modifiers) = node.named_child(0).filter(|n| n.kind() == "modifiers") else {
            return;
        };
        let owning = node
            .child_by_field_name("name")
            .map(|n| node_text(Some(n), self.src).to_string());
        let mut verb: Option<String> = None;
        let mut path: Option<Node> = None;
        for i in 0..modifiers.named_child_count() {
            let Some(annot) = modifiers.named_child(i as u32) else {
                continue;
            };
            let name = node_text(annot.child_by_field_name("name"), self.src);
            let args = annot.child_by_field_name("arguments");
            // Queue listener registrations (TASK-087, DR-031: registering
            // the handler is the provider side) and websocket annotations
            // (emit = provider, handler registration = consumer).
            if let Some(args) = args {
                match name {
                    "KafkaListener" => {
                        if let Some(topics) = java_annotation_kwarg_node(args, "topics", self.src) {
                            for lit in java_string_literals(topics) {
                                if let Some(raw) = self.topic_arg(lit) {
                                    self.emit_queue(
                                        annot,
                                        lit,
                                        &raw,
                                        ContractRole::Provider,
                                        "kafka",
                                        CONFIDENCE_FRAMEWORK,
                                        owning.as_deref(),
                                    );
                                }
                            }
                        }
                        continue;
                    }
                    "RabbitListener" => {
                        if let Some(queues) = java_annotation_kwarg_node(args, "queues", self.src)
                            && let Some(lit) = java_string_literals(queues).into_iter().next()
                            && let Some(raw) = self.topic_arg(lit)
                        {
                            self.emit_queue(
                                annot,
                                lit,
                                &raw,
                                ContractRole::Provider,
                                "rabbitmq",
                                CONFIDENCE_FRAMEWORK,
                                owning.as_deref(),
                            );
                        }
                        continue;
                    }
                    "MessageMapping" => {
                        if let Some(lit) = positional_arg(args, 0)
                            && let Some(raw) = self.topic_arg(lit)
                        {
                            self.emit_ws(
                                annot,
                                lit,
                                &raw,
                                ContractRole::Consumer,
                                CONFIDENCE_FRAMEWORK,
                                owning.as_deref(),
                            );
                        }
                        continue;
                    }
                    "SendTo" => {
                        if let Some(lit) = positional_arg(args, 0)
                            && let Some(raw) = self.topic_arg(lit)
                        {
                            self.emit_ws(
                                annot,
                                lit,
                                &raw,
                                ContractRole::Provider,
                                CONFIDENCE_FRAMEWORK,
                                owning.as_deref(),
                            );
                        }
                        continue;
                    }
                    // TASK-087 job: @Scheduled — the method is the job;
                    // the cron expression is a schedule, not an identity.
                    "Scheduled" => {
                        if let Some(name_node) = node.child_by_field_name("name") {
                            let name = node_text(Some(name_node), self.src).to_string();
                            self.emit_job(
                                annot,
                                name_node,
                                &name,
                                ContractRole::Provider,
                                CONFIDENCE_FRAMEWORK,
                                owning.as_deref(),
                            );
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            match name {
                "GetMapping" | "PostMapping" | "PutMapping" | "DeleteMapping" | "PatchMapping" => {
                    verb = Some(
                        match name {
                            "GetMapping" => "get",
                            "PostMapping" => "post",
                            "PutMapping" => "put",
                            "DeleteMapping" => "delete",
                            _ => "patch",
                        }
                        .to_string(),
                    );
                    path = args.and_then(|a| java_annotation_path(a, self.src));
                }
                "RequestMapping" => {
                    // method = RequestMethod.POST — take the segment after
                    // the last dot; absence means ANY.
                    verb = Some(
                        match args.and_then(|a| java_annotation_kwarg_text(a, "method", self.src)) {
                            Some(m) => m.rsplit('.').next().unwrap_or("ANY").to_string(),
                            None => "ANY".to_string(),
                        },
                    );
                    path = args.and_then(|a| java_annotation_path(a, self.src));
                }
                "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "HEAD" => {
                    verb = Some(
                        match name {
                            "GET" => "get",
                            "POST" => "post",
                            "PUT" => "put",
                            "DELETE" => "delete",
                            "PATCH" => "patch",
                            _ => "head",
                        }
                        .to_string(),
                    );
                }
                "Path" => {
                    path = args.and_then(|a| java_annotation_path(a, self.src));
                }
                _ => {}
            }
        }
        if let (Some(verb), Some(path)) = (verb, path) {
            self.emit_http(
                node,
                path,
                ContractRole::Provider,
                &verb,
                prefix,
                CONFIDENCE_FRAMEWORK,
                owning.as_deref(),
            );
        }
    }

    /// `restTemplate.getForObject(…)`, `System.getenv(…)`, and the queue
    /// template producers (TASK-087).
    fn java_call(&mut self, node: Node, prefix: &str) {
        let name = node_text(node.child_by_field_name("name"), self.src);
        let object = node_text(node.child_by_field_name("object"), self.src);
        let args = node.child_by_field_name("arguments");
        let Some(args) = args else { return };
        let first = positional_arg(args, 0);
        if object == "System" && name == "getenv" {
            if let Some(arg) = first {
                let value = render_string_node(arg, self.src, self.lang);
                self.emit_env(node, &value, ContractRole::Consumer, CONFIDENCE_FRAMEWORK);
            }
            return;
        }
        // Queue template producers (DR-031: publishing initiates, so these
        // are the consumer side).
        let object_lower = object.to_lowercase();
        if object_lower.contains("kafka") && name == "send" {
            if let Some(t) = first
                && let Some(raw) = self.topic_arg(t)
            {
                self.emit_queue(
                    node,
                    t,
                    &raw,
                    ContractRole::Consumer,
                    "kafka",
                    CONFIDENCE_FRAMEWORK,
                    None,
                );
            }
            return;
        }
        if object_lower.contains("rabbit") && matches!(name, "send" | "convertAndSend") {
            // The routing key is the last leading string literal — the
            // argument just before the non-literal payload.
            if let Some(t) = java_last_leading_string(args)
                && let Some(raw) = self.topic_arg(t)
            {
                self.emit_queue(
                    node,
                    t,
                    &raw,
                    ContractRole::Consumer,
                    "rabbitmq",
                    CONFIDENCE_FRAMEWORK,
                    None,
                );
            }
            return;
        }
        // STOMP/Simp messaging templates emit to websocket destinations
        // (checked after the rabbit arm — `rabbitTemplate` also contains
        // "Template").
        if object.contains("Template") && name == "convertAndSend" {
            if let Some(t) = first
                && let Some(raw) = self.topic_arg(t)
            {
                self.emit_ws(
                    node,
                    t,
                    &raw,
                    ContractRole::Provider,
                    CONFIDENCE_FRAMEWORK,
                    None,
                );
            }
            return;
        }
        if let Some(verb) = java_client_verb(name)
            && let Some(arg) = first
        {
            self.emit_http(
                node,
                arg,
                ContractRole::Consumer,
                verb,
                prefix,
                CONFIDENCE_FRAMEWORK,
                None,
            );
        }
    }

    // -- PHP ---------------------------------------------------------------------

    fn visit_php(&mut self, node: Node, prefix: &str) -> String {
        match node.kind() {
            "scoped_call_expression" => {
                self.php_scoped_call(node, prefix);
            }
            "member_call_expression" => {
                self.php_member_call(node, prefix);
            }
            "method_declaration" => {
                self.php_method_attribute(node);
            }
            "subscript_expression" => {
                self.php_env_subscript(node);
            }
            "function_call_expression" => {
                self.php_function_call(node);
            }
            _ => {}
        }
        prefix.to_string()
    }

    /// `Route::get('/x', …)` (Laravel) providers, `Http::get(…)` consumers.
    fn php_scoped_call(&mut self, node: Node, prefix: &str) {
        let scope = node_text(node.child_by_field_name("scope"), self.src);
        let name = node_text(node.child_by_field_name("name"), self.src);
        let args = node.child_by_field_name("arguments");
        let Some(args) = args else { return };
        let first = positional_arg(args, 0);
        match scope {
            "Route" => {
                if matches!(name, "get" | "post" | "put" | "patch" | "delete" | "any")
                    && let Some(arg) = first
                {
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Provider,
                        name,
                        prefix,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            "Http" => {
                if let (Some(verb), Some(arg)) = (canonical_verb(name), first) {
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Consumer,
                        verb,
                        prefix,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            _ => {}
        }
    }

    /// `$app->get('/x', …)` (Slim) providers, `$client->get(…)` consumers.
    fn php_member_call(&mut self, node: Node, prefix: &str) {
        // PHP variable_name children are positional: `$client` -> name "client".
        let object = node
            .child_by_field_name("object")
            .and_then(|o| o.named_child(0))
            .map(|o| node_text(Some(o), self.src))
            .unwrap_or_default();
        let name = node_text(node.child_by_field_name("name"), self.src);
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };
        let Some(first) = positional_arg(args, 0) else {
            return;
        };
        let Some(verb) = canonical_verb(name) else {
            return;
        };
        if matches!(object, "app" | "group" | "router") {
            self.emit_http(
                node,
                first,
                ContractRole::Provider,
                verb,
                prefix,
                CONFIDENCE_FRAMEWORK,
                None,
            );
        } else if matches!(object, "client" | "http") {
            self.emit_http(
                node,
                first,
                ContractRole::Consumer,
                verb,
                prefix,
                CONFIDENCE_FRAMEWORK,
                None,
            );
        } else if AMBIGUOUS_VERBS.contains(&name) {
            self.ambiguous_http(node, first, verb, prefix);
        }
    }

    /// Symfony `#[Route('/x', methods: ['GET'])]` attributes.
    fn php_method_attribute(&mut self, node: Node) {
        let Some(attrs) = node.child_by_field_name("attributes") else {
            return;
        };
        for i in 0..attrs.named_child_count() {
            let Some(group) = attrs.named_child(i as u32) else {
                continue;
            };
            for j in 0..group.named_child_count() {
                let Some(attr) = group.named_child(j as u32) else {
                    continue;
                };
                if node_text(attr.named_child(0), self.src) != "Route" {
                    continue;
                }
                let Some(params) = attr.child_by_field_name("parameters") else {
                    continue;
                };
                let Some(path) = positional_arg(params, 0) else {
                    continue;
                };
                let verb = php_attribute_kwarg_verb(params, self.src).unwrap_or("ANY");
                let owning = node
                    .child_by_field_name("name")
                    .map(|n| node_text(Some(n), self.src).to_string());
                self.emit_http(
                    node,
                    path,
                    ContractRole::Provider,
                    verb,
                    "",
                    CONFIDENCE_FRAMEWORK,
                    owning.as_deref(),
                );
            }
        }
    }

    /// `$_ENV['X']` reads.
    fn php_env_subscript(&mut self, node: Node) {
        if is_assignment_target(node, "assignment_expression") {
            return;
        }
        // PHP subscript children are positional: [variable_name, string].
        let object = node
            .named_child(0)
            .and_then(|o| o.named_child(0))
            .map(|o| node_text(Some(o), self.src))
            .unwrap_or_default();
        let name = node
            .named_child(1)
            .filter(|n| n.kind() == "string")
            .map(|n| render_string_node(n, self.src, self.lang))
            .unwrap_or_default();
        if object == "_ENV" && is_env_name(&name) {
            self.emit_env(node, &name, ContractRole::Consumer, CONFIDENCE_FRAMEWORK);
        }
    }

    /// `putenv('X=x')` writes — the name is the part before `=`.
    fn php_function_call(&mut self, node: Node) {
        let name = node_text(node.child_by_field_name("function"), self.src);
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };
        let Some(first) = positional_arg(args, 0) else {
            return;
        };
        if name != "putenv" {
            return;
        }
        let value = render_string_node(first, self.src, self.lang);
        let env_name = value.split('=').next().unwrap_or("").trim();
        if is_env_name(env_name) {
            self.emit_env(node, env_name, ContractRole::Provider, CONFIDENCE_HEURISTIC);
        }
    }

    // -- C# ----------------------------------------------------------------------

    fn visit_csharp(&mut self, node: Node, prefix: &str) -> String {
        match node.kind() {
            "class_declaration" => {
                return self.csharp_class(node, prefix);
            }
            "method_declaration" => {
                self.csharp_method(node, prefix);
            }
            "invocation_expression" => {
                self.csharp_invocation(node, prefix);
            }
            "object_creation_expression" => {
                self.csharp_object_creation(node, prefix);
            }
            _ => {}
        }
        prefix.to_string()
    }

    /// Class-level `[Route("api/[controller]")]` prefixes member routes;
    /// `[controller]`/`[action]` tokens become placeholder parameters.
    fn csharp_class(&mut self, node: Node, prefix: &str) -> String {
        for i in 0..node.named_child_count() {
            let Some(attrs) = node.named_child(i as u32) else {
                continue;
            };
            if attrs.kind() != "attribute_list" {
                continue;
            }
            for j in 0..attrs.named_child_count() {
                let Some(attr) = attrs.named_child(j as u32) else {
                    continue;
                };
                if attr.kind() != "attribute" || node_text(attr.named_child(0), self.src) != "Route"
                {
                    continue;
                }
                let Some(arg_list) = attr.named_child(1) else {
                    continue;
                };
                let Some(arg) = positional_arg(arg_list, 0) else {
                    continue;
                };
                let raw = render_string_node(arg, self.src, self.lang);
                let rewritten = rewrite_aspnet_tokens(raw);
                if !rewritten.is_empty() {
                    return join_raw(prefix, &rewritten);
                }
            }
        }
        prefix.to_string()
    }

    /// `[HttpGet("/x")]` / `[Route("x")]` attributes. tree-sitter-c-sharp
    /// exposes attribute lists and attribute parts positionally.
    fn csharp_method(&mut self, node: Node, prefix: &str) {
        for i in 0..node.named_child_count() {
            let Some(attrs) = node.named_child(i as u32) else {
                continue;
            };
            if attrs.kind() != "attribute_list" {
                continue;
            }
            for j in 0..attrs.named_child_count() {
                let Some(attr) = attrs.named_child(j as u32) else {
                    continue;
                };
                if attr.kind() != "attribute" {
                    continue;
                }
                let name = node_text(attr.named_child(0), self.src);
                let verb = match name {
                    "HttpGet" => "get",
                    "HttpPost" => "post",
                    "HttpPut" => "put",
                    "HttpDelete" => "delete",
                    "HttpPatch" => "patch",
                    "Route" => "ANY",
                    _ => continue,
                };
                let Some(arg_list) = attr.named_child(1) else {
                    continue;
                };
                let Some(arg) = positional_arg(arg_list, 0) else {
                    continue;
                };
                let owning = node
                    .child_by_field_name("name")
                    .map(|n| node_text(Some(n), self.src).to_string());
                self.emit_http(
                    node,
                    arg,
                    ContractRole::Provider,
                    verb,
                    prefix,
                    CONFIDENCE_FRAMEWORK,
                    owning.as_deref(),
                );
            }
        }
    }

    /// `app.MapGet(…)` providers, `httpClient.GetAsync(…)`,
    /// `Environment.GetEnvironmentVariable(…)`.
    fn csharp_invocation(&mut self, node: Node, prefix: &str) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        if func.kind() != "member_access_expression" {
            return;
        }
        let recv = node_text(func.child_by_field_name("expression"), self.src);
        let name = csharp_name_text(func.child_by_field_name("name"), self.src);
        let args = node.child_by_field_name("arguments");
        let Some(args) = args else { return };
        let Some(first) = positional_arg(args, 0) else {
            return;
        };
        if name.starts_with("Map")
            && let Some(verb) =
                canonical_verb(name.trim_start_matches("Map").to_lowercase().as_str())
        {
            self.emit_http(
                node,
                first,
                ContractRole::Provider,
                verb,
                prefix,
                CONFIDENCE_FRAMEWORK,
                None,
            );
        } else if recv == "Environment" && name == "GetEnvironmentVariable" {
            let value = render_string_node(first, self.src, self.lang);
            self.emit_env(node, &value, ContractRole::Consumer, CONFIDENCE_FRAMEWORK);
        } else if recv == "Environment" && name == "SetEnvironmentVariable" {
            let value = render_string_node(first, self.src, self.lang);
            self.emit_env(node, &value, ContractRole::Provider, CONFIDENCE_HEURISTIC);
        } else if CSHARP_CONSUMER_RECEIVERS.contains(&recv)
            && let Some(verb) = csharp_client_verb(&name)
        {
            self.emit_http(
                node,
                first,
                ContractRole::Consumer,
                verb,
                prefix,
                CONFIDENCE_FRAMEWORK,
                None,
            );
        }
    }

    /// `new HttpRequestMessage(HttpMethod.Get, "/x")`.
    fn csharp_object_creation(&mut self, node: Node, prefix: &str) {
        let type_name = node_text(node.child_by_field_name("type"), self.src);
        if type_name != "HttpRequestMessage" {
            return;
        }
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };
        let (Some(method), Some(path)) = (positional_arg(args, 0), positional_arg(args, 1)) else {
            return;
        };
        if method.kind() != "member_access_expression" {
            return;
        }
        let verb = csharp_name_text(method.child_by_field_name("name"), self.src);
        self.emit_http(
            node,
            path,
            ContractRole::Consumer,
            &verb,
            prefix,
            CONFIDENCE_FRAMEWORK,
            None,
        );
    }

    // -- C / C++ -----------------------------------------------------------------

    fn visit_c(&mut self, node: Node, prefix: &str) -> String {
        if node.kind() == "call_expression" {
            self.c_call(node, prefix);
        }
        prefix.to_string()
    }

    fn c_call(&mut self, node: Node, prefix: &str) {
        let name = node_text(node.child_by_field_name("function"), self.src);
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };
        match name {
            "getenv" => {
                if let Some(arg) = positional_arg(args, 0) {
                    let value = render_string_node(arg, self.src, self.lang);
                    self.emit_env(node, &value, ContractRole::Consumer, CONFIDENCE_FRAMEWORK);
                }
            }
            "putenv" => {
                if let Some(arg) = positional_arg(args, 0) {
                    let value = render_string_node(arg, self.src, self.lang);
                    let env_name = value.split('=').next().unwrap_or("").trim();
                    if is_env_name(env_name) {
                        self.emit_env(node, env_name, ContractRole::Provider, CONFIDENCE_HEURISTIC);
                    }
                }
            }
            "setenv" => {
                if let Some(arg) = positional_arg(args, 0) {
                    let value = render_string_node(arg, self.src, self.lang);
                    self.emit_env(node, &value, ContractRole::Provider, CONFIDENCE_HEURISTIC);
                }
            }
            "curl_easy_setopt" => {
                // curl_easy_setopt(h, CURLOPT_URL, "https://…") — GET only
                // (documented approximation: curl defaults to GET).
                let opt = positional_arg(args, 1);
                let url = positional_arg(args, 2);
                if let (Some(opt), Some(url)) = (opt, url)
                    && node_text(Some(opt), self.src) == "CURLOPT_URL"
                {
                    self.emit_http(
                        node,
                        url,
                        ContractRole::Consumer,
                        "GET",
                        prefix,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                }
            }
            _ => {}
        }
    }

    /// 0.5 heuristic: verb-named call on an unrecognized receiver whose
    /// first argument is a path-like string literal (PRD-CTR-REQ-004).
    /// Non-path-like literals (`cache.get("user:1")`) stay out.
    fn ambiguous_http(&mut self, node: Node, arg: Node, verb: &str, prefix: &str) {
        if matches!(self.path_arg(arg), Some(PathArg::Direct(ref s)) if is_path_like(s)) {
            self.emit_http(
                node,
                arg,
                ContractRole::Provider,
                verb,
                prefix,
                CONFIDENCE_HEURISTIC,
                None,
            );
        }
    }

    /// Extract the textual path of a call argument, per language.
    fn path_arg(&self, arg: Node) -> Option<PathArg> {
        let src = self.src;
        let lang = self.lang;
        let concat = |node: Node, leaf_kinds: &[&str]| concat_literal(node, src, lang, leaf_kinds);
        match self.lang {
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => match arg.kind() {
                "string" => Some(PathArg::Direct(string_content(arg, src))),
                "template_string" => Some(PathArg::Direct(template_content(arg, src))),
                "binary_expression" if node_text(arg.child(1), src) == "+" => {
                    concat(arg, &["string", "template_string"])
                }
                _ => None,
            },
            Lang::Python => match arg.kind() {
                "string" => py_string_content(arg, src).map(PathArg::Direct),
                "binary_operator" if node_text(arg.child(1), src) == "+" => {
                    concat(arg, &["string"])
                }
                _ => None,
            },
            Lang::Ruby => match arg.kind() {
                "string" => Some(PathArg::Direct(ruby_string_content(arg, src))),
                "binary" if node_text(arg.child(1), src) == "+" => concat(arg, &["string"]),
                _ => None,
            },
            Lang::Go => match arg.kind() {
                "interpreted_string_literal" => {
                    Some(PathArg::Direct(render_string_node(arg, src, lang)))
                }
                "binary_expression" if node_text(arg.child(1), src) == "+" => {
                    concat(arg, &["interpreted_string_literal"])
                }
                _ => None,
            },
            Lang::Rust => match arg.kind() {
                "string_literal" => Some(PathArg::Direct(render_string_node(arg, src, lang))),
                "binary_expression" if node_text(arg.child(1), src) == "+" => {
                    concat(arg, &["string_literal"])
                }
                _ => None,
            },
            Lang::Java => match arg.kind() {
                "string_literal" => Some(PathArg::Direct(render_string_node(arg, src, lang))),
                "binary_expression" if node_text(arg.child(1), src) == "+" => {
                    concat(arg, &["string_literal"])
                }
                _ => None,
            },
            Lang::Php => match arg.kind() {
                "string" => Some(PathArg::Direct(render_string_node(arg, src, lang))),
                "binary_expression" if node_text(arg.child(1), src) == "." => {
                    concat(arg, &["string"])
                }
                _ => None,
            },
            Lang::CSharp => match arg.kind() {
                "string_literal" => Some(PathArg::Direct(rewrite_aspnet_tokens(
                    render_string_node(arg, src, lang),
                ))),
                "binary_expression" if node_text(arg.child(1), src) == "+" => {
                    concat(arg, &["string_literal"])
                }
                _ => None,
            },
            Lang::C | Lang::Cpp => match arg.kind() {
                "string_literal" => Some(PathArg::Direct(render_string_node(arg, src, lang))),
                "binary_expression" if node_text(arg.child(1), src) == "+" => {
                    concat(arg, &["string_literal"])
                }
                _ => None,
            },
        }
    }

    /// Record an HTTP contract from a call site.
    ///
    /// `owning` overrides the enclosing-function lookup (decorators report
    /// the decorated function instead).
    #[allow(clippy::too_many_arguments)]
    fn emit_http(
        &mut self,
        node: Node,
        arg: Node,
        role: ContractRole,
        verb: &str,
        prefix: &str,
        confidence: f64,
        owning: Option<&str>,
    ) {
        if !self.opts.http {
            return;
        }
        let (raw, confidence) = match self.path_arg(arg) {
            Some(PathArg::Direct(s)) => (s, confidence),
            Some(PathArg::Concat(s)) => (s, CONFIDENCE_HEURISTIC),
            None => return,
        };
        let joined = join_raw(prefix, &raw);
        let Some(norm) = normalize_http_path(&joined) else {
            return;
        };
        let qualifier = normalize_method(verb);
        self.out.push(ContractCandidate {
            kind: ContractKind::Http,
            role,
            qualifier: qualifier.clone(),
            identifier: norm.path.clone(),
            canonical_id: canonical_contract_id(ContractKind::Http, &qualifier, &norm.path),
            params: norm
                .params
                .into_iter()
                .enumerate()
                .map(|(i, name)| PathParam {
                    position: i + 1,
                    name,
                })
                .collect(),
            owning_symbol: owning
                .map(str::to_string)
                .or_else(|| crate::indexer::find_enclosing_function(node, self.src, self.lang)),
            line: arg.start_position().row + 1,
            confidence,
        });
    }

    /// Record an env contract.
    fn emit_env(&mut self, node: Node, name: &str, role: ContractRole, confidence: f64) {
        if !self.opts.env || !is_env_name(name) {
            return;
        }
        self.out.push(ContractCandidate {
            kind: ContractKind::Env,
            role,
            qualifier: String::new(),
            identifier: name.to_string(),
            canonical_id: canonical_contract_id(ContractKind::Env, "", name),
            params: Vec::new(),
            owning_symbol: crate::indexer::find_enclosing_function(node, self.src, self.lang),
            line: node.start_position().row + 1,
            confidence,
        });
    }

    /// Record a queue contract from a call site.
    ///
    /// `raw` is the already-rendered topic literal; `name_node` is the
    /// argument that carries it (line attribution).
    #[allow(clippy::too_many_arguments)]
    fn emit_queue(
        &mut self,
        node: Node,
        name_node: Node,
        raw: &str,
        role: ContractRole,
        broker: &str,
        confidence: f64,
        owning: Option<&str>,
    ) {
        if !self.opts.queue {
            return;
        }
        self.push_message(
            ContractKind::Queue,
            broker,
            node,
            name_node,
            raw,
            role,
            confidence,
            owning,
        );
    }

    /// Record a websocket contract from a call site (TASK-087 step 6).
    ///
    /// `raw` is the rendered event-name literal; `name_node` is the
    /// argument that carries it (line attribution).
    #[allow(clippy::too_many_arguments)]
    fn emit_ws(
        &mut self,
        node: Node,
        name_node: Node,
        raw: &str,
        role: ContractRole,
        confidence: f64,
        owning: Option<&str>,
    ) {
        if !self.opts.websocket {
            return;
        }
        self.push_message(
            ContractKind::WebSocket,
            "",
            node,
            name_node,
            raw,
            role,
            confidence,
            owning,
        );
    }

    /// Record a job contract (TASK-087 step 7). Definitions and schedule
    /// registrations are providers; enqueue/dispatch sites are consumers.
    #[allow(clippy::too_many_arguments)]
    fn emit_job(
        &mut self,
        node: Node,
        name_node: Node,
        name: &str,
        role: ContractRole,
        confidence: f64,
        owning: Option<&str>,
    ) {
        if !self.opts.job {
            return;
        }
        self.push_message(
            ContractKind::Job,
            "",
            node,
            name_node,
            name,
            role,
            confidence,
            owning,
        );
    }

    /// Record a websocket contract identified by an HTTP path
    /// (express-ws `app.ws('/chat', …)`): the identifier is a normalized
    /// path, not a topic.
    fn emit_ws_path(&mut self, node: Node, path_node: Node, role: ContractRole) {
        if !self.opts.websocket {
            return;
        }
        let Some(PathArg::Direct(raw)) = self.path_arg(path_node) else {
            return;
        };
        let Some(norm) = normalize_http_path(&raw) else {
            return;
        };
        self.out.push(ContractCandidate {
            kind: ContractKind::WebSocket,
            role,
            qualifier: String::new(),
            identifier: norm.path.clone(),
            canonical_id: canonical_contract_id(ContractKind::WebSocket, "", &norm.path),
            params: Vec::new(),
            owning_symbol: crate::indexer::find_enclosing_function(node, self.src, self.lang),
            line: path_node.start_position().row + 1,
            confidence: CONFIDENCE_FRAMEWORK,
        });
    }

    /// Shared tail of the message-kind emitters: normalize the name, build
    /// the canonical ID, attribute the site.
    #[allow(clippy::too_many_arguments)]
    fn push_message(
        &mut self,
        kind: ContractKind,
        qualifier: &str,
        node: Node,
        name_node: Node,
        raw: &str,
        role: ContractRole,
        confidence: f64,
        owning: Option<&str>,
    ) {
        let Some(norm) = normalize_topic(raw) else {
            return;
        };
        self.out.push(ContractCandidate {
            kind,
            role,
            qualifier: qualifier.to_string(),
            identifier: norm.clone(),
            canonical_id: canonical_contract_id(kind, qualifier, &norm),
            params: Vec::new(),
            owning_symbol: owning
                .map(str::to_string)
                .or_else(|| crate::indexer::find_enclosing_function(node, self.src, self.lang)),
            line: name_node.start_position().row + 1,
            confidence,
        });
    }

    /// Render the topic-bearing literal of a call argument, per language.
    ///
    /// Returns `None` for non-literals, multi-literal containers, and
    /// computed topics (`None` from `normalize_topic` then rejects
    /// interpolations — callers skip the site entirely).
    fn topic_arg(&self, arg: Node) -> Option<String> {
        let src = self.src;
        let lang = self.lang;
        match lang {
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => match arg.kind() {
                "string" => Some(string_content(arg, src)),
                "template_string" => Some(template_content(arg, src)),
                // `"topic".toString()` wrapping.
                "call_expression" => js_string_wrap(arg, src),
                _ => None,
            },
            Lang::Python => match arg.kind() {
                "string" => py_string_content(arg, src),
                // kafka-python: subscribe(['orders']) — exactly one string.
                "list" | "tuple" => {
                    let only = (0..arg.named_child_count())
                        .filter_map(|i| arg.named_child(i as u32))
                        .collect::<Vec<_>>();
                    if only.len() == 1 && only[0].kind() == "string" {
                        py_string_content(only[0], src)
                    } else {
                        None
                    }
                }
                _ => None,
            },
            Lang::Ruby => match arg.kind() {
                "string" => Some(ruby_string_content(arg, src)),
                _ => None,
            },
            Lang::Go => match arg.kind() {
                "interpreted_string_literal" => Some(render_string_node(arg, src, lang)),
                // sarama: SendMessage(&ProducerMessage{Topic: "orders"}).
                _ => go_keyed_topic_literal(arg, src),
            },
            Lang::Rust => match arg.kind() {
                "string_literal" => Some(render_string_node(arg, src, lang)),
                // rdkafka: subscribe(&["orders"]).
                "reference_expression" => {
                    let inner = arg.named_child(0)?;
                    if inner.kind() == "array_expression"
                        && inner.named_child_count() == 1
                        && let Some(only) = inner.named_child(0)
                        && only.kind() == "string_literal"
                    {
                        Some(render_string_node(only, src, lang))
                    } else {
                        None
                    }
                }
                // async_nats: publish("orders".into(), payload).
                "call_expression" => rust_string_wrap(arg, src),
                _ => None,
            },
            Lang::Java => match arg.kind() {
                "string_literal" => Some(render_string_node(arg, src, lang)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether a JS callee chain is rooted at a websocket receiver
    /// (`io.to(room).emit` counts — the root decides).
    fn is_ws_receiver(&self, func: Node) -> bool {
        WS_RECEIVERS.contains(&node_text(Some(js_chain_root(func)), self.src))
    }

    /// Emit one grpc-family contract at `node`'s line (TASK-088 emit choke
    /// point — the single place the kind is gated).
    fn emit_grpc(
        &mut self,
        node: Node,
        service: &str,
        method: &str,
        role: ContractRole,
        owning: Option<&str>,
    ) {
        if !self.opts.grpc || service.is_empty() || method.is_empty() {
            return;
        }
        let owning = owning
            .map(str::to_string)
            .or_else(|| crate::indexer::find_enclosing_function(node, self.src, self.lang));
        let line = node.start_position().row + 1;
        self.out.push(grpc_candidate(
            service,
            method,
            role,
            owning.as_deref(),
            line,
        ));
    }

    /// Emit one provider per member method of a generated service body
    /// (Java `method_declaration`, Rust `function_item`, Python
    /// `function_definition` — the loops differ only in the child kind).
    fn emit_grpc_body_methods(&mut self, body: Node, service: &str, method_kind: &str) {
        for i in 0..body.child_count() {
            let Some(m) = body.child(i as u32) else {
                continue;
            };
            if m.kind() != method_kind {
                continue;
            }
            let name_node = m.child_by_field_name("name");
            let name = node_text(name_node, self.src);
            if let Some(anchor) = name_node.filter(|_| !name.is_empty()) {
                self.emit_grpc(anchor, service, name, ContractRole::Provider, Some(name));
            }
        }
    }

    /// Emit one graphql-family contract at `node`'s line (TASK-088 emit
    /// choke point — the single place the kind is gated). Unlike
    /// [`Self::emit_grpc`], `owning` is not back-filled from the enclosing
    /// function: resolver-map properties and decorated definitions carry
    /// their own ownership semantics, and only operation call sites resolve
    /// through [`crate::indexer::find_enclosing_function`] before calling
    /// this.
    fn emit_graphql(
        &mut self,
        node: Node,
        root: &str,
        field: &str,
        role: ContractRole,
        owning: Option<&str>,
    ) {
        if !self.opts.graphql || root.is_empty() || field.is_empty() {
            return;
        }
        let line = node.start_position().row + 1;
        self.out
            .push(graphql_candidate(root, field, role, owning, line));
    }

    // -- grpc generated/server code arms (TASK-088, plan 5.1) ------------------

    /// Java provider: `class X extends <…><S>Grpc.<S>ImplBase` — every member
    /// method serves one rpc.
    fn java_grpc_impl_base(&mut self, node: Node) {
        let sup = node_text(node.child_by_field_name("superclass"), self.src);
        let Some(service) = sup
            .split('.')
            .next_back()
            .and_then(|last| last.strip_suffix("ImplBase"))
            .filter(|s| !s.is_empty())
        else {
            return;
        };
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        self.emit_grpc_body_methods(body, service, "method_declaration");
    }

    /// Java call sites: `addService(XGrpc.bindService(...))` registers the
    /// whole service (`*`); bound `stub.M(req)` and inline
    /// `XGrpc.newBlockingStub(ch).M(req)` consume one method.
    fn java_grpc_call(&mut self, node: Node) {
        let name = node_text(node.child_by_field_name("name"), self.src);
        if name == "addService" {
            let bind = node
                .child_by_field_name("arguments")
                .and_then(|args| positional_arg(args, 0))
                .filter(|arg| arg.kind() == "method_invocation")
                .filter(|arg| {
                    node_text(arg.child_by_field_name("name"), self.src) == "bindService"
                });
            if let Some(bind) = bind
                && let Some(service) =
                    java_grpc_service_from_object(bind.child_by_field_name("object"), self.src)
            {
                self.emit_grpc(node, &service, "*", ContractRole::Provider, None);
            }
            return;
        }
        let Some(object) = node.child_by_field_name("object") else {
            return;
        };
        // Inline chain: XGrpc.newBlockingStub(ch).M(req)
        if object.kind() == "method_invocation"
            && matches!(
                node_text(object.child_by_field_name("name"), self.src),
                "newBlockingStub" | "newStub" | "newFutureStub"
            )
            && let Some(service) =
                java_grpc_service_from_object(object.child_by_field_name("object"), self.src)
        {
            self.emit_grpc(node, &service, name, ContractRole::Consumer, None);
            return;
        }
        // Bound stub: stub.M(req)
        let recv = node_text(Some(object), self.src);
        if let Some(service) = self.rpc.stubs.get(recv).cloned() {
            self.emit_grpc(node, &service, name, ContractRole::Consumer, None);
        }
    }

    /// Go sites: `Register<S>Server(...)` registers the service (`*`);
    /// bound `client.M(...)` and inline `pb.New<S>Client(conn).M(...)`
    /// consume one method.
    fn go_grpc(&mut self, node: Node) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        match func.kind() {
            "identifier" => {
                if let Some(service) = go_service_from_register(node_text(Some(func), self.src)) {
                    self.emit_grpc(node, &service, "*", ContractRole::Provider, None);
                }
            }
            "selector_expression" => {
                let meth = node_text(func.child_by_field_name("field"), self.src);
                if meth.is_empty() {
                    return;
                }
                // pb.Register<S>Server(...) — qualified registration call.
                if let Some(service) = go_service_from_register(meth) {
                    self.emit_grpc(node, &service, "*", ContractRole::Provider, None);
                    return;
                }
                let operand = func.child_by_field_name("operand");
                if let Some(op) = operand.filter(|op| op.kind() == "call_expression")
                    && let Some(inner) = op.child_by_field_name("function")
                    && let Some(service) = go_service_from_new_client(inner, self.src)
                {
                    self.emit_grpc(node, &service, meth, ContractRole::Consumer, None);
                    return;
                }
                let recv = node_text(operand, self.src);
                if let Some(service) = self.rpc.stubs.get(recv).cloned() {
                    self.emit_grpc(node, &service, meth, ContractRole::Consumer, None);
                }
            }
            _ => {}
        }
    }

    /// Rust provider: `impl <path>::<S> for T` — every impl method serves one
    /// rpc (tonic generates snake_case method names; the join folds casing).
    /// The trait must be a qualified path: `impl UserService for T` without a
    /// module path is indistinguishable from any std trait impl.
    fn rust_grpc_impl(&mut self, node: Node) {
        let trait_text = node_text(node.child_by_field_name("trait"), self.src);
        let Some(service) = trait_text
            .rsplit("::")
            .next()
            .filter(|s| trait_text.contains("::") && !s.is_empty())
        else {
            return;
        };
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        self.emit_grpc_body_methods(body, service, "function_item");
    }

    /// Rust consumer: bound `client.m(req)` and inline
    /// `<S>Client::new(ch).m(req)` (with or without `.await`).
    fn rust_grpc_call(&mut self, node: Node) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        if func.kind() != "field_expression" {
            return;
        }
        let meth = node_text(func.child_by_field_name("field"), self.src);
        if meth.is_empty() {
            return;
        }
        // This grammar names the receiver field "value" (not "object").
        let object = func.child_by_field_name("value");
        // Inline: UserServiceClient::new(ch).get_user(req)
        if let Some(op) = object.filter(|op| op.kind() == "call_expression")
            && let Some(inner) = op.child_by_field_name("function")
            && let Some(ctor) = node_text(Some(inner), self.src).strip_suffix("::new")
            && let Some(last) = ctor.rsplit("::").next()
            && let Some(service) = last.strip_suffix("Client").filter(|s| !s.is_empty())
        {
            self.emit_grpc(node, service, meth, ContractRole::Consumer, None);
            return;
        }
        let recv = node_text(object, self.src);
        if let Some(service) = self.rpc.stubs.get(recv).cloned() {
            self.emit_grpc(node, &service, meth, ContractRole::Consumer, None);
        }
    }

    /// Python provider: `class S(<pkg>.<S>Servicer)` — each member def serves
    /// one rpc; free `add_<S>Servicer_to_server` registers the service (`*`).
    fn python_grpc_class(&mut self, node: Node) {
        let bases = node.child_by_field_name("superclasses");
        let Some(bases) = bases else { return };
        let mut service: Option<&str> = None;
        for i in 0..bases.named_child_count() {
            let Some(base) = bases.named_child(i as u32) else {
                continue;
            };
            let last = node_text(Some(base), self.src)
                .rsplit('.')
                .next()
                .unwrap_or("");
            if let Some(s) = last.strip_suffix("Servicer").filter(|s| !s.is_empty()) {
                service = Some(s);
                break;
            }
        }
        let Some(service) = service else { return };
        let Some(body) = node.child_by_field_name("body") else {
            return;
        };
        self.emit_grpc_body_methods(body, service, "function_definition");
    }

    /// Python sites: the free call `add_<S>Servicer_to_server(...)` (the
    /// generated registration function, imported — never defined here)
    /// registers the service (`*`); bound `stub.M(req)` consumes a method.
    fn python_grpc_call(&mut self, node: Node) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        if func.kind() == "identifier" {
            let name = node_text(Some(func), self.src);
            if let Some(service) = name
                .strip_prefix("add_")
                .and_then(|m| m.strip_suffix("_to_server"))
                .and_then(|m| m.strip_suffix("Servicer"))
                .filter(|s| !s.is_empty())
            {
                self.emit_grpc(node, service, "*", ContractRole::Provider, None);
            }
            return;
        }
        if func.kind() != "attribute" {
            return;
        }
        let meth = node_text(func.child_by_field_name("attribute"), self.src);
        if meth.is_empty() {
            return;
        }
        let recv = node_text(func.child_by_field_name("object"), self.src);
        if let Some(service) = self.rpc.stubs.get(recv).cloned() {
            self.emit_grpc(node, &service, meth, ContractRole::Consumer, None);
        }
    }

    /// JS/TS sites (gated by the file-level grpc marker):
    /// `addService(<x>.service, …)` registers the service (`*`); bound
    /// `client.m(arg, cb)` consumes one method.
    fn js_grpc_call(&mut self, node: Node) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        if func.kind() != "member_expression" {
            return;
        }
        let prop = node_text(func.child_by_field_name("property"), self.src);
        if prop.is_empty() {
            return;
        }
        if prop == "addService" {
            let svc = node
                .child_by_field_name("arguments")
                .and_then(|args| positional_arg(args, 0))
                .map(|arg| node_text(Some(arg), self.src).to_string())
                .and_then(|text| text.strip_suffix(".service").map(str::to_string))
                .filter(|s| !s.is_empty());
            if let Some(service) = svc {
                self.emit_grpc(node, &service, "*", ContractRole::Provider, None);
            }
            return;
        }
        let recv = node_text(func.child_by_field_name("object"), self.src);
        if let Some(service) = self.rpc.stubs.get(recv).cloned() {
            self.emit_grpc(node, &service, prop, ContractRole::Consumer, None);
        }
    }

    // -- graphql arms (TASK-088, plan 5.3) --------------------------------------

    /// JS/TS resolver map: `Query: { user: (…) => … }` — each inner property
    /// of a Query/Mutation/Subscription key declares one resolver.
    fn js_graphql_pair(&mut self, node: Node) {
        let key = node_text(node.child_by_field_name("key"), self.src);
        if !matches!(key, "Query" | "Mutation" | "Subscription") {
            return;
        }
        let Some(value) = node
            .child_by_field_name("value")
            .filter(|v| v.kind() == "object")
        else {
            return;
        };
        for i in 0..value.child_count() {
            let Some(p) = value.child(i as u32) else {
                continue;
            };
            if p.kind() != "pair" {
                continue;
            }
            let field = node_text(p.child_by_field_name("key"), self.src);
            self.emit_graphql(p, key, field, ContractRole::Provider, None);
        }
    }

    /// JS/TS operation call sites: `gql`…`` / `graphql`…`` tagged templates
    /// (rendered as calls with a template argument) and Apollo-style
    /// `client.query({query: '…'})` / `.mutate(…)` / `.subscribe(…)` with
    /// an operation string. The mini-parser is the gate — plain look-alike
    /// strings parse to nothing and are ignored.
    fn js_graphql_call(&mut self, node: Node) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };
        let Some(args) = node.child_by_field_name("arguments") else {
            return;
        };
        match func.kind() {
            "identifier" if matches!(node_text(Some(func), self.src), "gql" | "graphql") => {
                let text = if args.kind() == "template_string" {
                    Some(template_content(args, self.src))
                } else {
                    args.named_child(0)
                        .filter(|a| a.kind() == "template_string")
                        .map(|a| template_content(a, self.src))
                };
                if let Some(text) = text {
                    self.emit_graphql_ops(node, &text);
                }
            }
            "member_expression" => {
                let prop = node_text(func.child_by_field_name("property"), self.src);
                if !matches!(prop, "query" | "mutate" | "subscribe") {
                    return;
                }
                for i in 0..args.named_child_count() {
                    let Some(arg) = args.named_child(i as u32) else {
                        continue;
                    };
                    let text = match arg.kind() {
                        "string" => Some(string_content(arg, self.src)),
                        "template_string" => Some(template_content(arg, self.src)),
                        "object" => js_object_operation_string(arg, self.src),
                        _ => None,
                    };
                    if let Some(text) = text {
                        self.emit_graphql_ops(node, &text);
                    }
                }
            }
            _ => {}
        }
    }

    /// Emit one consumer per top-level field of a parsed GraphQL operation.
    /// Gated at the [`Self::emit_graphql`] choke point.
    fn emit_graphql_ops(&mut self, node: Node, text: &str) {
        let Some(ops) = parse_graphql_operation(text) else {
            return;
        };
        let owning = crate::indexer::find_enclosing_function(node, self.src, self.lang);
        for (root, field) in ops {
            self.emit_graphql(
                node,
                &root,
                &field,
                ContractRole::Consumer,
                owning.as_deref(),
            );
        }
    }

    /// Python `gql("query { user }")` operation call site.
    fn python_graphql_call(&mut self, node: Node) {
        let is_gql = node
            .child_by_field_name("function")
            .filter(|f| f.kind() == "identifier")
            .is_some_and(|f| node_text(Some(f), self.src) == "gql");
        if !is_gql {
            return;
        }
        let text = node
            .child_by_field_name("arguments")
            .and_then(|args| positional_arg(args, 0))
            .and_then(|arg| py_string_content(arg, self.src));
        if let Some(text) = text {
            self.emit_graphql_ops(node, &text);
        }
    }
}

/// Whether `node` is the left-hand side of an assignment of kind
/// `assign_kind` (`assignment_expression` for JS/PHP, `assignment` for
/// Python/Ruby). Read matchers skip such sites — the assignment handler
/// owns them.
fn is_assignment_target(node: Node, assign_kind: &str) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == assign_kind
            && parent.child_by_field_name("left").map(|n| n.id()) == Some(node.id())
    })
}

/// Env-var names: non-empty, single token, no whitespace.
fn is_env_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(char::is_whitespace)
}

/// Uppercase gin/chi verbs accepted as route registrations.
const GO_PROVIDER_VERBS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "Any",
];
/// Ruby HTTP client libraries (constant receivers).
const RUBY_CONSUMER_RECEIVERS: &[&str] = &["HTTParty", "RestClient", "Faraday"];
/// Capitalized Go verbs for the single-argument 0.5 heuristic.
const GO_AMBIGUOUS_VERBS: &[&str] = &[
    "Get", "Post", "Put", "Patch", "Delete", "Head", "Options", "Any", "Request",
];
/// C# HTTP client receiver names.
const CSHARP_CONSUMER_RECEIVERS: &[&str] = &["httpClient", "client", "http"];

/// Map a lowercase verb name to its canonical form; `None` if not a
/// verb. Shared across language walkers (Ruby, Rust, PHP, C#).
fn canonical_verb(name: &str) -> Option<&'static str> {
    match name {
        "get" => Some("get"),
        "post" => Some("post"),
        "put" => Some("put"),
        "patch" => Some("patch"),
        "delete" => Some("delete"),
        "head" => Some("head"),
        "options" => Some("options"),
        _ => None,
    }
}

/// Go client verbs: `http.Get`, `client.Get`, `http.Post`…
fn go_client_verb(recv: &str, meth: &str) -> Option<&'static str> {
    match (recv, meth) {
        ("http", "Get") | ("client", "Get") => Some("get"),
        ("http", "Post") | ("http", "PostForm") | ("client", "Post") => Some("post"),
        ("http", "Head") => Some("head"),
        ("client", "Do") => Some("ANY"),
        _ => None,
    }
}

/// Verb for a `.route` handler argument: the callee's own name, or the
/// first verb-named method along its receiver chain (`web::get().to(h)`).
fn rust_handler_verb<'t>(handler: Node<'t>, src: &'t [u8]) -> Option<&'t str> {
    if let Some(name) = rust_callee_name(handler, src)
        && let Some(verb) = canonical_verb(name)
    {
        return Some(verb);
    }
    let mut current = if handler.kind() == "call_expression" {
        handler.child_by_field_name("function")
    } else {
        Some(handler)
    };
    let mut depth = 0;
    while let Some(node) = current
        && depth < PREFIX_DEPTH_CAP
    {
        depth += 1;
        match node.kind() {
            "call_expression" => current = node.child_by_field_name("function"),
            "field_expression" => {
                if let Some(verb) =
                    canonical_verb(node_text(node.child_by_field_name("field"), src))
                {
                    return Some(verb);
                }
                current = node.child_by_field_name("value");
            }
            // `web::get()` parses with a scoped callee — the name is the verb.
            "scoped_identifier" => {
                if let Some(verb) = canonical_verb(node_text(node.child_by_field_name("name"), src))
                {
                    return Some(verb);
                }
                break;
            }
            _ => break,
        }
    }
    None
}

/// Final callee name of a Rust call node (identifier / scoped / field).
fn rust_callee_name<'t>(call: Node<'t>, src: &'t [u8]) -> Option<&'t str> {
    let func = call.child_by_field_name("function")?;
    match func.kind() {
        "identifier" => Some(node_text(Some(func), src)),
        "scoped_identifier" => Some(node_text(func.child_by_field_name("name"), src)),
        "field_expression" => Some(node_text(func.child_by_field_name("field"), src)),
        _ => None,
    }
}

/// Leftmost node of a Rust method chain, or the node itself.
fn rust_chain_root<'t>(mut node: Option<Node<'t>>) -> Option<Node<'t>> {
    while let Some(current) = node
        && current.kind() == "field_expression"
    {
        node = current.child_by_field_name("value");
    }
    node
}

/// String literals inside a Rust token tree (attributes / macros).
fn tree_strings(tree: Node) -> Vec<Node> {
    (0..tree.named_child_count())
        .filter_map(|i| tree.named_child(i as u32))
        .filter(|n| n.kind() == "string_literal")
        .collect()
}

/// Path argument of a Java annotation: direct string or `value=`/`path=`.
fn java_annotation_path<'t>(args: Node<'t>, src: &'t [u8]) -> Option<Node<'t>> {
    if let Some(direct) = positional_arg(args, 0)
        && direct.kind() == "string_literal"
    {
        return Some(direct);
    }
    for i in 0..args.named_child_count() {
        if let Some(pair) = args.named_child(i as u32)
            && pair.kind() == "element_value_pair"
            && matches!(
                node_text(pair.child_by_field_name("key"), src),
                "value" | "path"
            )
            && let Some(value) = pair.child_by_field_name("value")
            && value.kind() == "string_literal"
        {
            return Some(value);
        }
    }
    None
}

/// Text of a Java annotation keyword argument (non-string values included).
fn java_annotation_kwarg_text(args: Node, name: &str, src: &[u8]) -> Option<String> {
    for i in 0..args.named_child_count() {
        if let Some(pair) = args.named_child(i as u32)
            && pair.kind() == "element_value_pair"
            && node_text(pair.child_by_field_name("key"), src) == name
            && let Some(value) = pair.child_by_field_name("value")
        {
            return Some(node_text(Some(value), src).to_string());
        }
    }
    None
}

/// Value node of a Java annotation keyword argument.
fn java_annotation_kwarg_node<'t>(args: Node<'t>, name: &str, src: &[u8]) -> Option<Node<'t>> {
    for i in 0..args.named_child_count() {
        if let Some(pair) = args.named_child(i as u32)
            && pair.kind() == "element_value_pair"
            && node_text(pair.child_by_field_name("key"), src) == name
            && let Some(value) = pair.child_by_field_name("value")
        {
            return Some(value);
        }
    }
    None
}

/// String literals carried by an annotation value node: the node itself
/// when it is a literal, or every element when it is an array initializer.
fn java_string_literals(value: Node) -> Vec<Node> {
    match value.kind() {
        "string_literal" => vec![value],
        "element_value_array_initializer" => (0..value.named_child_count())
            .filter_map(|i| value.named_child(i as u32))
            .filter(|n| n.kind() == "string_literal")
            .collect(),
        _ => Vec::new(),
    }
}

/// Last string literal among the call's leading arguments — the routing
/// key sits directly before the non-literal payload
/// (`convertAndSend(exchange, routingKey, payload)`).
fn java_last_leading_string(args: Node) -> Option<Node> {
    let mut last = None;
    for j in 0..args.named_child_count() {
        let Some(arg) = args.named_child(j as u32).map(unwrap_argument) else {
            break;
        };
        if arg.kind() == "string_literal" {
            last = Some(arg);
        } else {
            break;
        }
    }
    last
}

/// Java RestTemplate-style client method verbs.
fn java_client_verb(name: &str) -> Option<&'static str> {
    match name {
        "getForObject" | "getForEntity" => Some("get"),
        "postForObject" | "postForEntity" => Some("post"),
        "put" => Some("put"),
        "delete" => Some("delete"),
        "exchange" | "execute" => Some("ANY"),
        _ => None,
    }
}

/// First entry of the `methods: ['GET']` named argument of a PHP attribute.
fn php_attribute_kwarg_verb(params: Node, src: &[u8]) -> Option<&'static str> {
    for i in 0..params.named_child_count() {
        let Some(arg) = params.named_child(i as u32) else {
            continue;
        };
        if arg.kind() != "argument" {
            continue;
        }
        let arg_name = arg
            .child_by_field_name("name")
            .map(|n| node_text(Some(n), src))
            .unwrap_or_default();
        if arg_name != "methods" {
            continue;
        }
        // The value follows the name positionally inside the argument node;
        // array elements arrive wrapped in array_element_initializer nodes.
        let value = (0..arg.named_child_count())
            .filter_map(|k| arg.named_child(k as u32))
            .find(|c| c.kind() == "array_creation_expression");
        if let Some(value) = value
            && let Some(method_node) = first_descendant_of_kind(value, "string")
        {
            let method = render_string_node(method_node, src, Lang::Php);
            return canonical_verb(&method.to_lowercase());
        }
    }
    None
}

/// Name text of a C# node that may be an identifier or generic name.
fn csharp_name_text(node: Option<Node>, src: &[u8]) -> String {
    let Some(node) = node else {
        return String::new();
    };
    if node.kind() == "generic_name" {
        // (generic_name (identifier) (type_argument_list …)) — positional.
        return node_text(node.named_child(0), src).to_string();
    }
    node_text(Some(node), src).to_string()
}

/// ASP.NET route tokens become placeholder parameters:
/// `[controller]` -> `{controller}`, `[action]` -> `{action}`.
fn rewrite_aspnet_tokens(raw: String) -> String {
    raw.replace("[controller]", "{controller}")
        .replace("[action]", "{action}")
}

/// C# HttpClient method verbs.
fn csharp_client_verb(name: &str) -> Option<&'static str> {
    match name {
        "GetAsync" | "GetFromJsonAsync" | "GetStringAsync" | "GetStreamAsync"
        | "GetByteArrayAsync" => Some("get"),
        "PostAsync" | "PostAsJsonAsync" => Some("post"),
        "PutAsync" | "PutAsJsonAsync" => Some("put"),
        "DeleteAsync" => Some("delete"),
        "PatchAsync" => Some("patch"),
        "SendAsync" => Some("ANY"),
        _ => None,
    }
}

/// Content of a string node (quote-stripped, fragments concatenated).
fn string_content(node: Node, src: &[u8]) -> String {
    let mut out = String::new();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32)
            && child.kind() == "string_fragment"
        {
            out.push_str(node_text(Some(child), src));
        }
    }
    out
}

/// Rendered content of a JS template string: substitutions reinserted as
/// `${expr}` so stage 3/4 of the pipeline can process them.
fn template_content(node: Node, src: &[u8]) -> String {
    let mut out = String::new();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            match child.kind() {
                "string_fragment" => out.push_str(node_text(Some(child), src)),
                "template_substitution" => {
                    out.push_str("${");
                    out.push_str(node_text(child.named_child(0), src));
                    out.push('}');
                }
                _ => {}
            }
        }
    }
    out
}

/// `+` concatenation: keep going only when the tree holds exactly one
/// path-like string literal (PRD-CTR-REQ-004 skip rules). `leaf_kinds` names
/// the language's string-node kinds; concat operator nodes are matched by
/// the caller.
fn concat_literal(node: Node, src: &[u8], lang: Lang, leaf_kinds: &[&str]) -> Option<PathArg> {
    let mut literals = Vec::new();
    collect_string_leaves(node, src, lang, leaf_kinds, &mut literals);
    if literals.len() == 1 && is_path_like(&literals[0]) {
        Some(PathArg::Concat(literals.into_iter().next()?))
    } else {
        None
    }
}

fn collect_string_leaves(
    node: Node,
    src: &[u8],
    lang: Lang,
    leaf_kinds: &[&str],
    out: &mut Vec<String>,
) {
    if leaf_kinds.contains(&node.kind()) {
        out.push(render_string_node(node, src, lang));
        return;
    }
    if node.kind().starts_with("binary") {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32) {
                collect_string_leaves(child, src, lang, leaf_kinds, out);
            }
        }
    }
}

/// Render any language's string node to its content. Content children are
/// matched by kind suffix (`*_content` / `*_fragment`), which covers every
/// bundled grammar; interpolations render per-language (`{x}` for Python,
/// `#{x}` for Ruby).
fn render_string_node(node: Node, src: &[u8], lang: Lang) -> String {
    let mut out = String::new();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            let kind = child.kind();
            if kind.ends_with("_content") || kind.ends_with("_fragment") {
                out.push_str(node_text(Some(child), src));
            } else if kind == "interpolation" {
                let expr = node_text(child.named_child(0), src);
                match lang {
                    Lang::Ruby => {
                        out.push_str("#{");
                        out.push_str(expr);
                        out.push('}');
                    }
                    _ => {
                        out.push('{');
                        out.push_str(expr);
                        out.push('}');
                    }
                }
            }
        }
    }
    out
}

/// Ruby string content with `#{x}` interpolations preserved.
fn ruby_string_content(node: Node, src: &[u8]) -> String {
    render_string_node(node, src, Lang::Ruby)
}

/// Whether a class body directly includes Sidekiq::Job / Sidekiq::Worker.
fn ruby_includes_sidekiq(body: Node, src: &[u8]) -> bool {
    (0..body.named_child_count())
        .filter_map(|i| body.named_child(i as u32))
        .any(|n| {
            n.kind() == "call"
                && node_text(n.child_by_field_name("method"), src) == "include"
                && n.child_by_field_name("arguments").is_some_and(|args| {
                    (0..args.named_child_count()).any(|i| {
                        args.named_child(i as u32).is_some_and(|a| {
                            matches!(node_text(Some(a), src), "Sidekiq::Job" | "Sidekiq::Worker")
                        })
                    })
                })
        })
}

/// `key: 'value'` keyword argument of a Ruby call: the pair's value node
/// and its string content (`pair` children are key symbol then value).
fn ruby_kwarg_string<'t>(args: Node<'t>, name: &str, src: &[u8]) -> Option<(Node<'t>, String)> {
    for j in 0..args.named_child_count() {
        let Some(pair) = args.named_child(j as u32) else {
            continue;
        };
        if pair.kind() != "pair" {
            continue;
        }
        let (Some(key), Some(value)) = (pair.named_child(0), pair.named_child(1)) else {
            continue;
        };
        if node_text(Some(key), src).trim_start_matches(':') == name && value.kind() == "string" {
            return Some((value, ruby_string_content(value, src)));
        }
    }
    None
}

/// Broker family implied by a receiver/variable name (`kafkaProducer`,
/// `nc`, `rabbitChan`, `bunny`, `amqpConn`); empty when unknown. Used by
/// the 0.5 generic tier — the qualifier still pairs cross-repo when both
/// sides agree on the broker.
fn broker_token(recv: &str) -> &'static str {
    let lower = recv.to_lowercase();
    if lower.contains("kafka") {
        "kafka"
    } else if lower.contains("nats") {
        "nats"
    } else if lower.contains("rabbit") || lower.contains("bunny") || lower.contains("amqp") {
        "rabbitmq"
    } else {
        ""
    }
}

/// Leftmost node of a JS member/call chain (`io.to(room).emit` -> `io`).
fn js_chain_root<'t>(mut node: Node<'t>) -> Node<'t> {
    loop {
        match node.kind() {
            "member_expression" => {
                let Some(next) = node.child_by_field_name("object") else {
                    return node;
                };
                node = next;
            }
            "call_expression" => {
                let Some(next) = node.child_by_field_name("function") else {
                    return node;
                };
                node = next;
            }
            _ => return node,
        }
    }
}

/// String value of an object-literal property (`{topic: 'orders'}`), and
/// the value node that carries it.
fn js_prop_string_node<'t>(arg: Node<'t>, prop: &str, src: &[u8]) -> Option<Node<'t>> {
    if arg.kind() != "object" {
        return None;
    }
    (0..arg.named_child_count())
        .filter_map(|i| arg.named_child(i as u32))
        .find(|pair| {
            pair.kind() == "pair"
                && node_text(pair.child_by_field_name("key"), src) == prop
                && pair
                    .child_by_field_name("value")
                    .is_some_and(|v| v.kind() == "string")
        })
        .and_then(|pair| pair.child_by_field_name("value"))
}

/// `"topic".toString()` — a call wrapping a plain string literal.
fn js_string_wrap(arg: Node, src: &[u8]) -> Option<String> {
    let func = arg.child_by_field_name("function")?;
    if func.kind() != "member_expression"
        || node_text(func.child_by_field_name("property"), src) != "toString"
    {
        return None;
    }
    let recv = func.child_by_field_name("object")?;
    if recv.kind() == "string" {
        Some(string_content(recv, src))
    } else {
        None
    }
}

/// `"orders".into()` / `.to_string()` / `.to_owned()` — a Rust call
/// wrapping a plain string literal.
fn rust_string_wrap(arg: Node, src: &[u8]) -> Option<String> {
    let func = arg.child_by_field_name("function")?;
    if func.kind() != "field_expression"
        || !matches!(
            node_text(func.child_by_field_name("field"), src),
            "into" | "to_string" | "to_owned"
        )
    {
        return None;
    }
    let recv = func.child_by_field_name("value")?;
    if recv.kind() == "string_literal" {
        Some(render_string_node(recv, src, Lang::Rust))
    } else {
        None
    }
}

/// sarama `&ProducerMessage{Topic: "orders"}` — a keyed composite literal
/// (possibly behind `&`) whose `Topic` key holds the string.
fn go_keyed_topic_literal(arg: Node, src: &[u8]) -> Option<String> {
    let mut node = arg;
    if node.kind() == "unary_expression"
        && let Some(inner) = node.named_child(0)
    {
        node = inner;
    }
    if node.kind() != "composite_literal" {
        return None;
    }
    // keyed elements live inside the literal_value wrapper; keys and values
    // are each wrapped in a literal_element node.
    let literal_value = (0..node.named_child_count())
        .filter_map(|i| node.named_child(i as u32))
        .find(|n| n.kind() == "literal_value")?;
    for i in 0..literal_value.named_child_count() {
        let Some(keyed) = literal_value.named_child(i as u32) else {
            continue;
        };
        if keyed.kind() != "keyed_element" {
            continue;
        }
        let key = keyed.named_child(0)?.named_child(0)?;
        let value = keyed.named_child(1)?.named_child(0)?;
        if node_text(Some(key), src) == "Topic" && value.kind() == "interpreted_string_literal" {
            return Some(render_string_node(value, src, Lang::Go));
        }
    }
    None
}

/// Build the canonical contract ID `<kind>::<qualifier>::<identifier>`.
///
/// This is the only place contract IDs are constructed (PRD-CTR-REQ-002);
/// every extractor funnels through it so the format can never drift.
pub fn canonical_contract_id(kind: ContractKind, qualifier: &str, identifier: &str) -> String {
    assert!(
        !identifier.trim().is_empty(),
        "contract identifier must be non-empty"
    );
    format!("{}::{}::{}", kind.as_str(), qualifier, identifier)
}

/// Normalize an HTTP method token: upper-case, with router catch-alls
/// (`any`, `all`, `match`) and unknown/empty verbs mapped to `ANY`.
///
/// Django `path()` registrations and bare `HandleFunc` mounts pass an empty
/// verb and normalize to `ANY` here.
pub fn normalize_method(raw: &str) -> String {
    let upper = raw.trim().to_uppercase();
    match upper.as_str() {
        "" | "ANY" | "ALL" | "MATCH" => "ANY".to_string(),
        other => other.to_string(),
    }
}

/// Characters that separate topic segments across brokers (Kafka `.`,
/// NATS `.`, RabbitMQ routing keys `.`, STOMP `/topic/x`, Redis `:`).
const TOPIC_SEPARATORS: &[char] = &['.', ':', '/'];

/// Normalize a queue/websocket/job topic name (TASK-087, PRD-CTR-REQ-002).
///
/// Unlike [`normalize_http_path`] this pipeline REJECTS computed topics
/// (`None` rather than a partial-confidence tier): exact-ID matching is the
/// only pairing mechanism for message kinds, and partial knowledge is
/// useless there.
///
/// 1. trim whitespace and quote characters (stage 1 of the HTTP pipeline),
/// 2. reject empty, internal whitespace, or `{`/`}`/`$`/`#` (interpolation),
/// 3. trim leading/trailing separator runs,
/// 4. collapse maximal separator runs (single or mixed `.`/`:`/`/`) to one
///    dot — the canonical grammar (`queue::kafka::orders.created`),
/// 5. preserve case, segment order, and all other characters — NATS
///    wildcards `*` and `>` stay literal and never exact-match a concrete
///    topic.
pub fn normalize_topic(raw: &str) -> Option<String> {
    let trimmed = stage_trim(raw);
    if trimmed.is_empty()
        || trimmed.chars().any(char::is_whitespace)
        || trimmed.chars().any(|c| matches!(c, '{' | '}' | '$' | '#'))
    {
        return None;
    }
    let inner = trimmed.trim_matches(|c: char| TOPIC_SEPARATORS.contains(&c));
    if inner.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(inner.len());
    let mut in_separators = false;
    for c in inner.chars() {
        if TOPIC_SEPARATORS.contains(&c) {
            if !in_separators {
                out.push('.');
                in_separators = true;
            }
        } else {
            out.push(c);
            in_separators = false;
        }
    }
    Some(out)
}

/// Result of normalizing a raw HTTP path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedPath {
    /// Canonical path with positional `{p1}`, `{p2}` parameter markers.
    pub path: String,
    /// Original parameter names in declaration order (PRD-CTR-REQ-022).
    pub params: Vec<String>,
}

/// Sentinel marking a rewritten placeholder before positional renumbering.
/// `\u{1}` cannot occur in real source strings.
const PARAM_SENTINEL: char = '\u{1}';

/// Normalize a raw HTTP path through the fixed 6-stage pipeline
/// (PRD-CTR-REQ-003):
///
/// 1. trim whitespace and quote characters,
/// 2. strip scheme + authority, query, and fragment,
/// 3. strip a leading base-URL interpolation token,
/// 4. rewrite placeholder syntaxes, recording original names,
/// 5. renumber placeholders positionally as `{p1}`, `{p2}`, …,
/// 6. ensure leading slash, collapse `//`, drop trailing slash.
///
/// Returns `None` when nothing path-like remains — callers skip the site.
pub fn normalize_http_path(raw: &str) -> Option<NormalizedPath> {
    let trimmed = stage_trim(raw);
    let de_schemed = stage_strip_scheme_authority(&trimmed);
    let de_based = stage_strip_base_interpolation(&de_schemed);
    let (marked, names) = stage_rewrite_placeholders(&de_based);
    let positional = stage_positional_markers(&marked);
    let path = stage_ensure_shape(&positional)?;
    Some(NormalizedPath {
        path,
        params: names,
    })
}

/// Stage 1: trim whitespace and quote characters from both ends.
fn stage_trim(raw: &str) -> String {
    raw.trim_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`'))
        .to_string()
}

/// Stage 2: strip `scheme://authority`, protocol-relative `//authority`,
/// query (`?…`), and fragment (`#…`).
///
/// A `#` immediately followed by `{` is a Ruby interpolation, not a
/// fragment, and is kept for stage 4.
fn stage_strip_scheme_authority(path: &str) -> String {
    let mut s = path;
    if let Some(rest) = strip_scheme(s) {
        s = rest;
    } else if let Some(rest) = s.strip_prefix("//") {
        // Protocol-relative: everything up to the next '/' is authority.
        match rest.find('/') {
            Some(i) => s = &rest[i..],
            None => return "/".to_string(),
        }
    }
    // Query and fragment (but not Ruby "#{...}" interpolation).
    let mut end = s.len();
    for (i, c) in s.char_indices() {
        if c == '?' {
            end = i;
            break;
        }
        if c == '#' && !s[i + 1..].starts_with('{') {
            end = i;
            break;
        }
    }
    s[..end].to_string()
}

/// Strip `scheme://` plus the authority that follows; returns the path part.
fn strip_scheme(s: &str) -> Option<&str> {
    let idx = s.find("://")?;
    let scheme = &s[..idx];
    if scheme.is_empty()
        || !scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        || !scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    let rest = &s[idx + 3..];
    match rest.find('/') {
        Some(i) => Some(&rest[i..]),
        // Authority with no path component addresses the root.
        None => Some("/"),
    }
}

/// Stage 3: strip a single leading base-URL interpolation token — only
/// when a `/` follows it, so a lone `/{id}` route remains a parameter
/// route. Dollar-sigil forms (`${name}`, `$name`) are always
/// interpolations; the curly `{name}` form doubles as a path-parameter
/// syntax (stage 4 rewrites it), so it is stripped only when the name is
/// base-URL-like — contains an underscore or is ALL_CAPS (`API_URL`,
/// `BASE`) — while parameter-like names (`tenant`, `org`) survive as
/// the route's leading parameter.
fn stage_strip_base_interpolation(path: &str) -> String {
    let body = path.strip_prefix('/').unwrap_or(path);
    let Some(len) = interpolation_token_len(body) else {
        return path.to_string();
    };
    if !body[len..].starts_with('/') {
        return path.to_string();
    }
    if body.starts_with('{') && !is_base_url_name(&body[1..len - 1]) {
        return path.to_string();
    }
    body[len + 1..].to_string()
}

/// Whether a curly `{NAME}` token names a base-URL variable rather than
/// a route parameter: underscored (`API_URL`) or ALL_CAPS (`BASE`).
fn is_base_url_name(name: &str) -> bool {
    name.contains('_')
        || (name.chars().any(|c| c.is_ascii_uppercase())
            && !name.chars().any(|c| c.is_ascii_lowercase()))
}

/// Length of an interpolation token (`${…}`, `{…}`, `$name`) at the start of
/// `s`, if any.
fn interpolation_token_len(s: &str) -> Option<usize> {
    if let Some(rest) = s.strip_prefix("${") {
        let end = rest.find('}')?;
        let name = &rest[..end];
        if is_interpolation_name(name) {
            return Some(2 + end + 1);
        }
        return None;
    }
    if let Some(rest) = s.strip_prefix('{') {
        let end = rest.find('}')?;
        let name = &rest[..end];
        if is_interpolation_name(name) {
            return Some(1 + end + 1);
        }
        return None;
    }
    if let Some(rest) = s.strip_prefix('$') {
        let len = identifier_len(rest);
        if len > 0 {
            return Some(1 + len);
        }
    }
    None
}

/// Placeholder names may contain dots (member expressions reinserted from
/// JS/Ruby templates) but no path separators.
fn is_interpolation_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/')
}

/// Length of a maximal `[A-Za-z_][A-Za-z0-9_]*` run at the start of `s`.
fn identifier_len(s: &str) -> usize {
    let mut len = 0;
    for c in s.chars() {
        if (len == 0 && (c.is_ascii_alphabetic() || c == '_'))
            || (len > 0 && (c.is_ascii_alphanumeric() || c == '_'))
        {
            len += c.len_utf8();
        } else {
            break;
        }
    }
    len
}

/// Stage 4: rewrite every placeholder syntax to a sentinel, recording the
/// original names in declaration order. Recognized syntaxes:
/// `${name}`, `{name}`, `$name`, `:name`, `<name>`, `<type:name>`, Ruby
/// `#{name}` — each only at the start of a path segment — plus the Rails
/// optional-format group `(.:format)`, which is dropped.
fn stage_rewrite_placeholders(path: &str) -> (String, Vec<String>) {
    let bytes = path.as_bytes();
    let mut out = String::with_capacity(path.len());
    let mut names = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &path[i..];
        let at_segment_start = i == 0 || bytes[i - 1] == b'/';
        // Rails optional-format group: drop "(.:format)" entirely.
        if rest.starts_with("(.:")
            && let Some(close) = rest.find(')')
        {
            i += close + 1;
            continue;
        }
        let mut consumed = 0usize;
        if at_segment_start && let Some((name, len)) = placeholder_at(rest) {
            names.push(name);
            out.push(PARAM_SENTINEL);
            consumed = len;
        }
        if consumed == 0 {
            let c = rest.chars().next().expect("non-empty remainder");
            out.push(c);
            consumed = c.len_utf8();
        }
        i += consumed;
    }
    (out, names)
}

/// Match a placeholder token at the start of `rest`; returns the original
/// name and the consumed byte length.
fn placeholder_at(rest: &str) -> Option<(String, usize)> {
    if let Some(after) = rest.strip_prefix("${") {
        let end = after.find('}')?;
        let name = &after[..end];
        if is_interpolation_name(name) {
            return Some((name.to_string(), 2 + end + 1));
        }
        return None;
    }
    if let Some(after) = rest.strip_prefix("#{") {
        let end = after.find('}')?;
        let name = &after[..end];
        if is_interpolation_name(name) {
            return Some((name.to_string(), 2 + end + 1));
        }
        return None;
    }
    if let Some(after) = rest.strip_prefix('{') {
        let end = after.find('}')?;
        let name = &after[..end];
        if is_interpolation_name(name) {
            return Some((name.to_string(), 1 + end + 1));
        }
        return None;
    }
    if let Some(after) = rest.strip_prefix('$') {
        let len = identifier_len(after);
        if len > 0 {
            return Some((after[..len].to_string(), 1 + len));
        }
    }
    if let Some(after) = rest.strip_prefix(':') {
        let len = identifier_len(after);
        if len > 0 {
            return Some((after[..len].to_string(), 1 + len));
        }
    }
    if let Some(after) = rest.strip_prefix('<') {
        let end = after.find('>')?;
        // "<int:id>" — strip the converter type, keep the name.
        let name = after[..end].rsplit(':').next().unwrap_or("");
        if is_interpolation_name(name) {
            return Some((name.to_string(), 1 + end + 1));
        }
    }
    None
}

/// Stage 5: replace sentinels in order with `{p1}`, `{p2}`, ….
fn stage_positional_markers(marked: &str) -> String {
    let mut out = String::with_capacity(marked.len());
    let mut n = 0usize;
    for c in marked.chars() {
        if c == PARAM_SENTINEL {
            n += 1;
            out.push_str(&format!("{{p{n}}}"));
        } else {
            out.push(c);
        }
    }
    out
}

/// Stage 6: ensure a leading slash, collapse duplicate slashes, and drop
/// trailing slashes (the root `/` is kept). Returns `None` when empty.
fn stage_ensure_shape(path: &str) -> Option<String> {
    let mut collapsed = String::with_capacity(path.len() + 1);
    let mut prev_slash = false;
    for c in path.chars() {
        if c == '/' {
            if !prev_slash && !collapsed.is_empty() {
                collapsed.push('/');
            }
            prev_slash = true;
        } else {
            collapsed.push(c);
            prev_slash = false;
        }
    }
    let trimmed = collapsed.trim_end_matches('/');
    if trimmed.is_empty() {
        // Root path — keep only when the input carried an explicit slash.
        return if path.contains('/') {
            Some("/".to_string())
        } else {
            None
        };
    }
    let mut shaped = String::with_capacity(trimmed.len() + 1);
    if !trimmed.starts_with('/') {
        shaped.push('/');
    }
    shaped.push_str(trimmed);
    Some(shaped)
}

// ---------------------------------------------------------------------------
// Document contracts (TASK-088, DQ1): .proto/.graphql/.yaml/.json documents
//
// No new crates (§4.24 constraint): these files get tiny line-oriented
// scanners over the raw text instead of a grammar. Files that yield no
// candidates stay un-indexed exactly as before — only contract-bearing
// documents gain a files row, which TASK-083 uses as its re-index anchor.
// ---------------------------------------------------------------------------

/// A document file kind recognized by extension (TASK-088).
///
/// `Proto` covers `.proto`; `Graphql` covers `.graphql`/`.gql`; `OpenApi`
/// covers `.yaml`/`.yml`/`.json` pending a content sniff — the extension
/// alone never proves OpenAPI, so CI/compose/package files that fail the
/// sniff yield no candidates and stay un-indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    /// Protocol-buffer IDL (`.proto`).
    Proto,
    /// GraphQL SDL or operation document (`.graphql`/`.gql`).
    Graphql,
    /// OpenAPI specification (`.yaml`/`.yml`/`.json`, content-sniffed).
    OpenApi,
}

impl DocumentKind {
    /// Language name stored in `files.language` and `meta.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            DocumentKind::Proto => "Proto",
            DocumentKind::Graphql => "GraphQL",
            DocumentKind::OpenApi => "OpenApi",
        }
    }
}

/// Whether `path` is a document file the contract scanner should read.
///
/// Extension gate only. Note this is separate from
/// [`crate::indexer::detect_language`]: a `.yaml` file is a document kind
/// here while remaining `None` (unparseable) to the grammar indexer. The
/// negative-case bounds (lock files, size cap) live in
/// [`scannable_document_kind`].
pub fn document_kind(path: &std::path::Path) -> Option<DocumentKind> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "proto" => Some(DocumentKind::Proto),
        "graphql" | "gql" => Some(DocumentKind::Graphql),
        "yaml" | "yml" | "json" => Some(DocumentKind::OpenApi),
        _ => None,
    }
}

/// Upper bound on a document file's byte size for contract scanning:
/// real proto/GraphQL/OpenAPI documents are at most a few hundred KB, while
/// their larger `.json`/`.yaml` siblings (data dumps, generated files) can
/// only produce a guaranteed-null sniff — they stay un-indexed, exactly as
/// before the document path existed.
pub const MAX_DOCUMENT_SCAN_BYTES: u64 = 4 * 1024 * 1024;

/// Lock/data files that match a document extension (or one day will) but
/// never carry contracts; skipped by name before any read. Several of these
/// carry no extension today — they are listed anyway so the set stays the
/// single answer to "which root files does the document path ignore".
const DOCUMENT_SKIP_FILE_NAMES: &[&str] = &[
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "composer.lock",
    "Cargo.lock",
    "poetry.lock",
];

/// Whether `path` is a document file the contract scanner should actually
/// read: [`document_kind`]'s extension gate bounded for the negative case —
/// known lock/data file names are skipped outright, and files above
/// [`MAX_DOCUMENT_SCAN_BYTES`] stay un-indexed (a path whose size cannot be
/// read classifies by extension; the subsequent read fails and the file
/// stays un-indexed).
pub fn scannable_document_kind(path: &std::path::Path) -> Option<DocumentKind> {
    if let Some(name) = path.file_name().and_then(|n| n.to_str())
        && DOCUMENT_SKIP_FILE_NAMES.contains(&name)
    {
        return None;
    }
    let oversized = std::fs::metadata(path)
        .map(|m| m.len() > MAX_DOCUMENT_SCAN_BYTES)
        .unwrap_or(false);
    if oversized {
        return None;
    }
    document_kind(path)
}

/// Extract contract candidates from a document file's text (TASK-088).
///
/// Each kind is gated by its `ContractOptions` flag; a document that yields
/// no candidates returns empty and the file stays un-indexed (pipeline
/// treats that as "no FileResult").
pub fn extract_document_contracts(
    kind: DocumentKind,
    content: &str,
    opts: &ContractOptions,
) -> Vec<ContractCandidate> {
    match kind {
        DocumentKind::Proto if opts.grpc => proto_providers(content),
        DocumentKind::Graphql if opts.graphql => graphql_document_contracts(content),
        DocumentKind::OpenApi if opts.openapi => openapi_providers(content),
        _ => Vec::new(),
    }
}

/// One `service` block found by the proto scanner.
struct ProtoService {
    /// Service name as written (package qualification is never composed —
    /// the canonical join relaxes it at match time instead).
    service: String,
    /// `(method name, 1-based line)` per `rpc` declaration.
    methods: Vec<(String, usize)>,
}

/// Scan a proto document for `service` blocks and their `rpc` methods
/// (plan 5.2). Line-oriented and comment-aware; `extend` blocks are not
/// services; rpc bodies with option blocks keep the service open until its
/// own closing brace (brace-depth tracking).
fn parse_proto_services(content: &str) -> Vec<ProtoService> {
    let mut services = Vec::new();
    let mut current: Option<ProtoService> = None;
    let mut depth: i64 = 0;
    for (idx, raw) in strip_proto_comments(content).iter().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if depth <= 0 {
            if let Some(name) = proto_service_opener(line) {
                let mut svc = ProtoService {
                    service: name,
                    methods: Vec::new(),
                };
                collect_proto_rpcs(line, line_no, &mut svc.methods);
                depth = brace_delta(line);
                if depth <= 0 {
                    services.push(svc);
                } else {
                    current = Some(svc);
                }
            }
        } else if let Some(svc) = current.as_mut() {
            collect_proto_rpcs(line, line_no, &mut svc.methods);
            depth += brace_delta(line);
            if depth <= 0 {
                services.push(current.take().expect("open service"));
            }
        }
    }
    services
}

/// Provider candidates for every method of every service in a proto document.
fn proto_providers(content: &str) -> Vec<ContractCandidate> {
    let mut out = Vec::new();
    for svc in parse_proto_services(content) {
        for (method, line) in svc.methods {
            out.push(grpc_candidate(
                &svc.service,
                &method,
                ContractRole::Provider,
                None,
                line,
            ));
        }
    }
    out
}

/// Build one grpc-family candidate with the canonical ID
/// `grpc::<service>::<method>` (developer spelling preserved — the join, not
/// the ID, tolerates qualification and casing).
fn grpc_candidate(
    service: &str,
    method: &str,
    role: ContractRole,
    owning: Option<&str>,
    line: usize,
) -> ContractCandidate {
    ContractCandidate {
        kind: ContractKind::Grpc,
        role,
        qualifier: service.to_string(),
        identifier: method.to_string(),
        canonical_id: canonical_contract_id(ContractKind::Grpc, service, method),
        params: Vec::new(),
        owning_symbol: owning.map(str::to_string),
        line,
        confidence: CONFIDENCE_FRAMEWORK,
    }
}

/// Contracts of a `.graphql`/`.gql` document (TASK-088): an SDL document
/// yields resolvers (providers) for the root operation types; an operation
/// document yields consumers for its top-level fields.
fn graphql_document_contracts(content: &str) -> Vec<ContractCandidate> {
    let trimmed = content.trim_start();
    if trimmed.starts_with("query")
        || trimmed.starts_with("mutation")
        || trimmed.starts_with("subscription")
        || trimmed.starts_with('{')
    {
        let mut out = Vec::new();
        if let Some(ops) = parse_graphql_operation(content) {
            for (root, field) in ops {
                out.push(graphql_candidate(
                    &root,
                    &field,
                    ContractRole::Consumer,
                    None,
                    1,
                ));
            }
        }
        return out;
    }
    scan_graphql_document(content)
        .into_iter()
        .map(|(root, field, line)| {
            graphql_candidate(&root, &field, ContractRole::Provider, None, line)
        })
        .collect()
}

/// One root-type field found by the SDL scan: `(root, field, 1-based line)`.
type GraphqlSdlField = (String, String, usize);

/// Scan an SDL document for `type Query|Mutation|Subscription {` and
/// `extend type <Root> {` blocks; each field definition inside yields one
/// resolver. Non-root types (`type User`) are ignored. One-line blocks
/// (`type Query { base: String }`) scan their inline field.
fn scan_graphql_document(content: &str) -> Vec<GraphqlSdlField> {
    let mut out = Vec::new();
    let mut root: Option<String> = None;
    for (idx, raw) in content.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if let Some(root_name) = root.as_ref() {
            if line.starts_with('}') {
                root = None;
                continue;
            }
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let name: String = line
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                out.push((root_name.clone(), name, line_no));
            }
            continue;
        }
        if let Some(root_name) = sdl_root_opener(line) {
            // One-line block? Scan the inline field and stay outside.
            let after_brace = line.split_once('{').map(|(_, rest)| rest).unwrap_or("");
            if let Some(close) = after_brace.rfind('}') {
                let inner = after_brace[..close].trim();
                let name: String = inner
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    out.push((root_name, name, line_no));
                }
            } else {
                root = Some(root_name);
            }
        }
    }
    out
}

/// `type Query {` / `extend type Mutation {` opener for a root operation
/// type; `None` for other declarations.
fn sdl_root_opener(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("extend type ")
        .or_else(|| line.strip_prefix("type "))?;
    let name: String = rest
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
        .collect();
    if !line.contains('{') {
        return None;
    }
    match name.as_str() {
        "Query" | "Mutation" | "Subscription" => Some(name),
        _ => None,
    }
}

/// Build one graphql candidate: `graphql::<Root>::<field>` with the root
/// operation type capitalized (plan 4).
fn graphql_candidate(
    root: &str,
    field: &str,
    role: ContractRole,
    owning: Option<&str>,
    line: usize,
) -> ContractCandidate {
    ContractCandidate {
        kind: ContractKind::Graphql,
        role,
        qualifier: root.to_string(),
        identifier: field.to_string(),
        canonical_id: canonical_contract_id(ContractKind::Graphql, root, field),
        params: Vec::new(),
        owning_symbol: owning.map(str::to_string),
        line,
        confidence: CONFIDENCE_FRAMEWORK,
    }
}

// ---------------------------------------------------------------------------
// OpenAPI scanner (TASK-088, plan 5.4): one line-oriented scanner for YAML
// and pretty-printed JSON documents
// ---------------------------------------------------------------------------

/// One operation found by the OpenAPI scan: `(method, raw path, 1-based
/// line)`.
struct OpenApiOperation {
    method: String,
    raw_path: String,
    line: usize,
}

/// Whether a document carries a top-level `openapi:`/`swagger:` version key
/// and a `paths:` block. Multi-document YAML (`---` separators) is skipped —
/// extraction, not validation.
fn looks_like_openapi(content: &str) -> bool {
    let mut version = false;
    let mut paths = false;
    for line in content.lines() {
        let t = line.trim();
        if t == "---" {
            return false;
        }
        match key_name(line) {
            Some("openapi") | Some("swagger") => version = true,
            Some("paths") => paths = true,
            _ => {}
        }
    }
    version && paths
}

/// Key name of a YAML/JSON line (`paths:`, `"paths": {`): trimmed, quotes
/// stripped, text before the first `:`. `None` for blank/comment/brace-only
/// lines and empty keys. Borrows from `line` — the OpenAPI sniff runs over
/// every line of files that usually are not OpenAPI, so it must not
/// allocate.
fn key_name(line: &str) -> Option<&str> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') || matches!(t, "{" | "}" | "[" | "]" | "," | "},") {
        return None;
    }
    let head = t.split(':').next()?;
    let name = head.trim_matches(|c| c == '"' || c == '\'').trim();
    if name.is_empty() { None } else { Some(name) }
}

/// Leading-space count of a line (YAML forbids tab indentation).
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// HTTP method tokens legal as OpenAPI path-item keys.
const OPENAPI_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Scan an OpenAPI document's `paths:` block: path keys sit one indent deeper
/// than `paths:`, method keys one level deeper again. Flow-style `{}` maps
/// and multi-document YAML are skipped by construction.
fn scan_openapi_document(content: &str) -> Vec<OpenApiOperation> {
    if !looks_like_openapi(content) {
        return Vec::new();
    }
    let mut ops = Vec::new();
    // 0 = outside paths, 1 = inside paths, 2 = inside a path item.
    let mut state = 0u8;
    let mut paths_indent = 0usize;
    let mut path = String::new();
    let mut path_indent = 0usize;
    for (idx, raw) in content.lines().enumerate() {
        if raw.trim().is_empty() || raw.trim_start_matches(' ').starts_with('#') {
            continue;
        }
        // Transitions re-enter the parent state on the same line, so a
        // dedent past one level lands in the right handler.
        loop {
            match state {
                0 => {
                    if key_name(raw) == Some("paths") {
                        paths_indent = indent_of(raw);
                        state = 1;
                    }
                    break;
                }
                1 => {
                    if indent_of(raw) <= paths_indent {
                        state = 0;
                        continue;
                    }
                    if let Some(key) = key_name(raw).filter(|k| k.starts_with('/')) {
                        path = key.to_string();
                        path_indent = indent_of(raw);
                        state = 2;
                    }
                    break;
                }
                _ => {
                    if indent_of(raw) <= path_indent {
                        state = 1;
                        continue;
                    }
                    if let Some(key) = key_name(raw)
                        && OPENAPI_METHODS.contains(&key)
                    {
                        ops.push(OpenApiOperation {
                            method: key.to_string(),
                            raw_path: path.clone(),
                            line: idx + 1,
                        });
                    }
                    break;
                }
            }
        }
    }
    ops
}

/// Provider candidates for every operation of an OpenAPI document
/// (`openapi::<METHOD>::<path>`, positional `{pN}` markers, params as
/// metadata — the HTTP pipeline verbatim, only the kind differs).
fn openapi_providers(content: &str) -> Vec<ContractCandidate> {
    scan_openapi_document(content)
        .into_iter()
        .filter_map(|op| {
            let norm = normalize_http_path(&op.raw_path)?;
            let qualifier = normalize_method(&op.method);
            Some(ContractCandidate {
                kind: ContractKind::Openapi,
                role: ContractRole::Provider,
                canonical_id: canonical_contract_id(ContractKind::Openapi, &qualifier, &norm.path),
                qualifier,
                identifier: norm.path,
                params: norm
                    .params
                    .into_iter()
                    .enumerate()
                    .map(|(i, name)| PathParam {
                        position: i + 1,
                        name,
                    })
                    .collect(),
                owning_symbol: None,
                line: op.line,
                confidence: CONFIDENCE_FRAMEWORK,
            })
        })
        .collect()
}

/// Blank out `//` line comments and `/* */` block comments, preserving line
/// structure so line numbers stay meaningful.
fn strip_proto_comments(content: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut in_block = false;
    for line in content.lines() {
        let mut out = String::with_capacity(line.len());
        let bytes: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if in_block {
                if c == '*' && i + 1 < bytes.len() && bytes[i + 1] == '/' {
                    in_block = false;
                    i += 2;
                } else {
                    i += 1;
                }
            } else if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == '/' {
                break;
            } else if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == '*' {
                in_block = true;
                i += 2;
            } else {
                out.push(c);
                i += 1;
            }
        }
        lines.push(out);
    }
    lines
}

/// `service <Name> {` opener — returns the bare service name, or `None` for
/// `extend` and other declarations.
fn proto_service_opener(line: &str) -> Option<String> {
    let rest = line.strip_prefix("service")?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let name: String = rest
        .trim()
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != '{')
        .collect();
    if name.is_empty() || !line.contains('{') {
        return None;
    }
    Some(name)
}

/// Record every `rpc <Name>(` occurrence on a line (usually one per line).
fn collect_proto_rpcs(line: &str, line_no: usize, methods: &mut Vec<(String, usize)>) {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    for (i, tok) in tokens.iter().enumerate() {
        if *tok == "rpc"
            && let Some(name) = tokens.get(i + 1)
            && !name.is_empty()
        {
            let method: String = name
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !method.is_empty() {
                methods.push((method, line_no));
            }
        }
    }
}

/// Net brace delta of a line (option blocks inside rpc bodies keep the
/// service depth accurate).
fn brace_delta(line: &str) -> i64 {
    line.chars().fold(0i64, |d, c| match c {
        '{' => d + 1,
        '}' => d - 1,
        _ => d,
    })
}

// ---------------------------------------------------------------------------
// GraphQL mini-parser and SDL scan (TASK-088, plan 5.3)
// ---------------------------------------------------------------------------

/// Parse one GraphQL operation, returning `(root, field)` pairs for its
/// top-level selection set (TASK-088).
///
/// Recognizes `query|mutation|subscription [Name][(args)] {…}` plus the
/// shorthand `{…}` (implicitly Query). Field arguments, aliases (`alias:`),
/// and nested selection sets are skipped; only depth-0 field names are
/// returned. Returns `None` when the text is not an operation — callers use
/// that to ignore plain look-alike strings. Only the FIRST operation in the
/// text is parsed (documents with several operations are rare; extraction,
/// not validation).
fn parse_graphql_operation(text: &str) -> Option<Vec<(String, String)>> {
    let t = text.trim();
    let (root, rest) = if let Some(r) = t.strip_prefix("query") {
        ("Query", r)
    } else if let Some(r) = t.strip_prefix("mutation") {
        ("Mutation", r)
    } else if let Some(r) = t.strip_prefix("subscription") {
        ("Subscription", r)
    } else if t.starts_with('{') {
        ("Query", t)
    } else {
        return None;
    };
    if !rest.starts_with(|c: char| c.is_whitespace() || c == '(' || c == '{') {
        // `queryx {…}` — an identifier, not the keyword.
        return None;
    }
    // Skip the optional operation name and variable declarations.
    let rest = rest.trim_start();
    let name_len = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .count();
    let rest = rest[name_len..].trim_start();
    let rest = skip_balanced(rest, '(', ')')?.trim_start();
    let body = rest.strip_prefix('{')?;
    Some(
        top_level_fields(body)
            .into_iter()
            .map(|f| (root.to_string(), f))
            .collect(),
    )
}

/// Skip a balanced `open…close` group at the start of `s` (whitespace
/// trimmed); `None` when unbalanced.
fn skip_balanced(s: &str, open: char, close: char) -> Option<&str> {
    let s = s.trim_start();
    if !s.starts_with(open) {
        return Some(s);
    }
    let mut depth = 0i32;
    for (i, c) in s.char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth -= 1;
            if depth == 0 {
                return Some(&s[i + c.len_utf8()..]);
            }
        }
    }
    None
}

/// Depth-0 field names of a selection-set body (the text after the opening
/// `{`). Field arguments, nested selection sets, aliases (`alias: field`
/// reports `field`), and spreads (`...name`) are skipped.
fn top_level_fields(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut fields = Vec::new();
    let mut depth = 0i32;
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '{' => {
                depth += 1;
                i += 1;
            }
            '}' => {
                depth -= 1;
                if depth < 0 {
                    break;
                }
                i += 1;
            }
            '(' => {
                // Balanced argument group (may nest default values).
                let mut d = 0i32;
                while i < chars.len() {
                    if chars[i] == '(' {
                        d += 1;
                    } else if chars[i] == ')' {
                        d -= 1;
                    }
                    i += 1;
                    if d == 0 {
                        break;
                    }
                }
            }
            _ if depth == 0 && (c.is_ascii_alphanumeric() || c == '_') => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let is_spread = start > 0 && chars[start - 1] == '.';
                if is_spread {
                    continue;
                }
                let name: String = chars[start..i].iter().collect();
                // Alias? `alias: field` — the next identifier is the field.
                let mut j = i;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ':' {
                    continue;
                }
                fields.push(name);
            }
            _ => {
                i += 1;
            }
        }
    }
    fields
}

/// The `query`/`mutation`/`subscription` string property of an Apollo-style
/// options object argument (JS).
fn js_object_operation_string(obj: Node, src: &[u8]) -> Option<String> {
    for i in 0..obj.named_child_count() {
        let Some(p) = obj.named_child(i as u32) else {
            continue;
        };
        if p.kind() != "pair" {
            continue;
        }
        let key = node_text(p.child_by_field_name("key"), src);
        if !matches!(key, "query" | "mutation" | "subscription") {
            continue;
        }
        let v = p.child_by_field_name("value")?;
        return match v.kind() {
            "string" => Some(string_content(v, src)),
            "template_string" => Some(template_content(v, src)),
            _ => None,
        };
    }
    None
}

/// GraphQL resolver decorators (Python): `@strawberry.field` /
/// `@strawberry.mutation` / `@strawberry.subscription` name the field after
/// the function; Ariadne `@Query.field("name")` / `@Mutation.mutation`
/// after the string argument. Returns `(root, field)`.
fn py_graphql_decorator(dec: Node, def_name: Option<Node>, src: &[u8]) -> Option<(String, String)> {
    let inner = dec.named_child(0)?;
    let def = node_text(def_name, src);
    match inner.kind() {
        // @strawberry.field (no call)
        "attribute" => {
            let obj = node_text(inner.child_by_field_name("object"), src);
            let attr = node_text(inner.child_by_field_name("attribute"), src);
            let root = match (obj, attr) {
                ("strawberry", "field") => "Query",
                ("strawberry", "mutation") => "Mutation",
                ("strawberry", "subscription") => "Subscription",
                _ => return None,
            };
            if def.is_empty() {
                return None;
            }
            Some((root.to_string(), def.to_string()))
        }
        // @Query.field("name") / @Mutation.mutation("name") (Ariadne)
        "call" => {
            let f = inner.child_by_field_name("function")?;
            if f.kind() != "attribute" {
                return None;
            }
            let root = match node_text(f.child_by_field_name("object"), src) {
                r @ ("Query" | "Mutation" | "Subscription") => r,
                _ => return None,
            };
            let arg_name = inner
                .child_by_field_name("arguments")
                .and_then(|args| positional_arg(args, 0))
                .and_then(|a| py_string_content(a, src));
            let field = match arg_name {
                Some(name) if !name.is_empty() => name,
                _ if !def.is_empty() => def.to_string(),
                _ => return None,
            };
            Some((root.to_string(), field))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// RPC canonical join (TASK-088, DQ2/DQ3, PRD-CTR-REQ-024)
//
// The SECOND matching pass, after exact canonical-ID equality. IDL
// definitions and generated stubs disagree on package qualification, method
// casing, and service- vs method-level registration; this pure in-memory
// join recovers those pairs. Candidates with an exact counterpart in their
// own workspace are EXCLUDED — the first pass (equality join, TASK-084)
// owns them and is never overridden. Workspace equality on normalized
// identifiers is the REQ-014 guard: the join relaxes names, never scope.
// ---------------------------------------------------------------------------

/// Whether `kind` belongs to the RPC family the canonical join pairs
/// (PRD-CTR-REQ-024): gRPC today; Thrift and tRPC flip this arm when added.
pub fn is_rpc_family(kind: ContractKind) -> bool {
    matches!(kind, ContractKind::Grpc)
}

/// Normalize a workspace identifier for comparison: trim surrounding
/// whitespace and case-fold (PRD-CTR-REQ-019).
pub fn normalize_workspace_id(raw: &str) -> String {
    raw.trim().to_lowercase()
}

/// One workspace's contracts offered to the canonical join.
///
/// `workspace` is the declared identifier (pre-normalized — the join folds
/// it itself); the join never fabricates one (PRD-CTR-REQ-015's repo-name
/// defaulting happens at scope construction, TASK-084).
pub struct RpcJoinScope<'a> {
    /// Workspace identifier as declared.
    pub workspace: String,
    /// Contract candidates of that workspace (mixed kinds/roles allowed).
    pub candidates: &'a [ContractCandidate],
}

/// Which tolerance recovered a relaxed pair (PRD-CTR-REQ-024).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcMatchBasis {
    /// Service names differ only by package qualification
    /// (`users.v1.UserService` vs `UserService` — compared on the last
    /// dot-segment, case-folded).
    PackageQualifiedService,
    /// Method names differ only by casing (`get_user` vs `GetUser`).
    CaseFoldedMethod,
    /// The provider registered the whole service (`*` identifier) and
    /// pairs with any method-level consumer of that service.
    ServiceLevelProvider,
}

/// One side of a relaxed link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcJoinSide {
    /// Canonical ID of the matched candidate (developer spelling).
    pub canonical_id: String,
    /// Workspace the candidate lives in (as declared).
    pub workspace: String,
    /// Provider or consumer.
    pub role: ContractRole,
}

/// A provider↔consumer pair recovered by the second matching pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcJoin {
    /// The serving side.
    pub provider: RpcJoinSide,
    /// The calling side.
    pub consumer: RpcJoinSide,
    /// Why the pair matched despite unequal canonical IDs.
    pub basis: RpcMatchBasis,
}

/// Run the canonical join over workspace-scoped candidate slices.
///
/// Deterministic: consumers are visited in input order (scope order, then
/// candidate order); the best provider is method-level before service-level,
/// then the lowest (scope, candidate) index. Pure — no storage, no mutation.
pub fn canonical_rpc_join(scopes: &[RpcJoinScope]) -> Vec<RpcJoin> {
    use std::collections::{HashMap, HashSet};

    // Exact ID sets per role and normalized workspace: a candidate whose
    // canonical ID has an OPPOSITE-ROLE counterpart in its own workspace
    // belongs to the first pass and never enters the join.
    let mut provider_ids: HashMap<String, HashSet<String>> = HashMap::new();
    let mut consumer_ids: HashMap<String, HashSet<String>> = HashMap::new();
    for scope in scopes {
        let ws = normalize_workspace_id(&scope.workspace);
        for cand in scope.candidates {
            if !is_rpc_family(cand.kind) {
                continue;
            }
            let slot = if cand.role == ContractRole::Provider {
                &mut provider_ids
            } else {
                &mut consumer_ids
            };
            slot.entry(ws.clone())
                .or_default()
                .insert(cand.canonical_id.clone());
        }
    }

    // Participants, with their scope index for deterministic tie-breaks.
    let mut providers: Vec<(usize, usize, String, &ContractCandidate)> = Vec::new();
    let mut consumers: Vec<(usize, usize, String, &ContractCandidate)> = Vec::new();
    for (si, scope) in scopes.iter().enumerate() {
        let ws = normalize_workspace_id(&scope.workspace);
        for (ci, cand) in scope.candidates.iter().enumerate() {
            if !is_rpc_family(cand.kind) {
                continue;
            }
            // Only an opposite-role exact counterpart excludes.
            let exact_other = match cand.role {
                ContractRole::Provider => &consumer_ids,
                ContractRole::Consumer => &provider_ids,
            };
            if exact_other
                .get(&ws)
                .is_some_and(|ids| ids.contains(&cand.canonical_id))
            {
                continue;
            }
            let slot = if cand.role == ContractRole::Provider {
                &mut providers
            } else {
                &mut consumers
            };
            slot.push((si, ci, ws.clone(), cand));
        }
    }

    let mut joins = Vec::new();
    for (csi, _cci, cws, consumer) in &consumers {
        // Best provider: method-level before service-level, then the lowest
        // (scope, candidate) index.
        let mut best: Option<(u8, &ContractCandidate, usize, RpcMatchBasis)> = None;
        for (psi, _pci, pws, provider) in &providers {
            if pws != cws {
                continue;
            }
            let Some(basis) = rpc_relaxed_match(consumer, provider) else {
                continue;
            };
            let rank = u8::from(basis != RpcMatchBasis::ServiceLevelProvider);
            let better = match best {
                None => true,
                // Strictly better rank only: equal rank keeps the earlier
                // provider (lowest (scope, candidate) index).
                Some((r, _, _, _)) => rank > r,
            };
            if better {
                best = Some((rank, provider, *psi, basis));
            }
        }
        if let Some((_, provider, psi, basis)) = best {
            joins.push(RpcJoin {
                provider: RpcJoinSide {
                    canonical_id: provider.canonical_id.clone(),
                    workspace: scopes[psi].workspace.clone(),
                    role: ContractRole::Provider,
                },
                consumer: RpcJoinSide {
                    canonical_id: consumer.canonical_id.clone(),
                    workspace: scopes[*csi].workspace.clone(),
                    role: ContractRole::Consumer,
                },
                basis,
            });
        }
    }
    joins
}

/// Relaxed match of one consumer against one provider (both RPC family):
/// service compared on the last dot-segment case-folded, method case-folded,
/// `*` = service-level registration. IDs are never rewritten here — only
/// compared tolerantly.
fn rpc_relaxed_match(
    consumer: &ContractCandidate,
    provider: &ContractCandidate,
) -> Option<RpcMatchBasis> {
    let consumer_service = last_segment_folded(&consumer.qualifier)?;
    let provider_service = last_segment_folded(&provider.qualifier)?;
    if consumer_service != provider_service {
        return None;
    }
    if provider.identifier == "*" {
        return Some(RpcMatchBasis::ServiceLevelProvider);
    }
    if rpc_method_key(&provider.identifier) == rpc_method_key(&consumer.identifier) {
        return Some(if provider.qualifier != consumer.qualifier {
            RpcMatchBasis::PackageQualifiedService
        } else {
            RpcMatchBasis::CaseFoldedMethod
        });
    }
    None
}

/// Match key for an RPC method name: case-folded with word separators
/// (`_`) removed, so the proto spelling, camelCase stubs, and tonic's
/// snake_case impls of one method compare equal
/// (`get_user` = `GetUser` = `getUser`).
fn rpc_method_key(method: &str) -> String {
    method
        .chars()
        .filter(|c| *c != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

/// Last dot-segment of a service qualifier, case-folded for comparison.
fn last_segment_folded(qualifier: &str) -> Option<String> {
    let last = qualifier.trim().rsplit('.').next()?.trim().to_lowercase();
    if last.is_empty() { None } else { Some(last) }
}

// ---------------------------------------------------------------------------
// Storage query API (TASK-083)
// ---------------------------------------------------------------------------

/// Filters for [`list_contracts`] (`wonk contracts` CLI, PRD-CTR-REQ-008).
///
/// `None` kind/role means no filter on that axis; `orphans` restricts the
/// result to consumers with no provider for the same canonical ID.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContractQuery {
    /// Restrict to one contract kind.
    pub kind: Option<ContractKind>,
    /// Restrict to one role.
    pub role: Option<ContractRole>,
    /// Only orphan consumers (no same-canonical_id provider in this repo).
    pub orphans: bool,
}

/// One stored contract row with the owning symbol's name resolved.
///
/// `symbol` is `None` for file-level contracts (documents, top-level
/// registrations) whose `symbol_id` is NULL.
#[derive(Debug, Clone, PartialEq)]
pub struct ContractRow {
    /// `<kind>::<qualifier>::<identifier>` canonical ID.
    pub canonical_id: String,
    /// Contract kind.
    pub kind: ContractKind,
    /// Provider or consumer.
    pub role: ContractRole,
    /// Owning symbol name, when `symbol_id` resolved at write time.
    pub symbol: Option<String>,
    /// Path relative to repo root.
    pub file: String,
    /// 1-based line of the detection site.
    pub line: usize,
    /// 1.0 framework-recognized / 0.5 heuristic (AR-018).
    pub confidence: f64,
}

/// List stored contracts matching `query`.
///
/// One static statement — NULL parameters disable their filter, so no SQL
/// is ever assembled dynamically. Reads only the passed connection: a
/// single-repo index answers from its own rows and never errors for lack
/// of sibling repos (PRD-CTR-REQ-012, the degenerate case of REQ-006's
/// workspace scoping that TASK-084 widens).
pub fn list_contracts(
    conn: &rusqlite::Connection,
    query: &ContractQuery,
) -> anyhow::Result<Vec<ContractRow>> {
    let mut stmt = conn.prepare(
        "SELECT c.canonical_id, c.kind, c.role, s.name, c.file, c.line, c.confidence \
         FROM contracts c LEFT JOIN symbols s ON s.id = c.symbol_id \
         WHERE (?1 IS NULL OR c.kind = ?1) \
           AND (?2 IS NULL OR c.role = ?2) \
           AND (?3 = 0 OR (c.role = 'consumer' AND NOT EXISTS ( \
                SELECT 1 FROM contracts p \
                WHERE p.canonical_id = c.canonical_id AND p.role = 'provider'))) \
         ORDER BY c.kind, c.canonical_id, c.file, c.line",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![
            query.kind.map(|k| k.as_str()),
            query.role.map(|r| r.as_str()),
            i64::from(query.orphans),
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, f64>(6)?,
            ))
        },
    )?;

    let mut out = Vec::new();
    for row in rows {
        let (canonical_id, kind, role, symbol, file, line, confidence) = row?;
        let kind = kind
            .parse()
            .map_err(|e| anyhow::anyhow!("corrupt contract row {canonical_id}: {e}"))?;
        let role = role
            .parse()
            .map_err(|e| anyhow::anyhow!("corrupt contract row {canonical_id}: {e}"))?;
        out.push(ContractRow {
            canonical_id,
            kind,
            role,
            symbol,
            file,
            line: line.max(0) as usize,
            confidence,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod extract_test_helpers {
    use super::*;
    use crate::indexer::{Lang, get_parser};

    pub(crate) fn extract(lang: Lang, src: &str) -> Vec<ContractCandidate> {
        extract_with(lang, src, &ContractOptions::default())
    }

    pub(crate) fn extract_with(
        lang: Lang,
        src: &str,
        opts: &ContractOptions,
    ) -> Vec<ContractCandidate> {
        let mut parser = get_parser(lang);
        let tree = parser.parse(src, None).expect("parse failed");
        extract_contracts(&tree, src, lang, opts)
    }

    pub(crate) fn find<'a>(
        cands: &'a [ContractCandidate],
        canonical_id: &str,
    ) -> Option<&'a ContractCandidate> {
        cands.iter().find(|c| c.canonical_id == canonical_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::Lang;
    use crate::types::{ContractKind, ContractRole, PathParam};

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

    // -- per-kind options (TASK-087, PRD-CTR-REQ-001) --------------------------

    #[test]
    fn contract_options_default_enables_all_kinds() {
        let opts = ContractOptions::default();
        assert!(opts.enabled(ContractKind::Http));
        assert!(opts.enabled(ContractKind::Env));
        assert!(opts.enabled(ContractKind::Queue));
        assert!(opts.enabled(ContractKind::WebSocket));
        assert!(opts.enabled(ContractKind::Job));
        assert!(opts.enabled(ContractKind::Grpc));
        assert!(opts.enabled(ContractKind::Graphql));
        assert!(opts.enabled(ContractKind::Openapi));
    }

    #[test]
    fn contract_options_from_config_maps_every_flag() {
        let cfg = crate::config::ContractsConfig {
            http: true,
            env: false,
            queue: false,
            websocket: true,
            job: false,
            grpc: false,
            graphql: true,
            openapi: false,
        };
        let opts = ContractOptions::from(&cfg);
        assert!(opts.enabled(ContractKind::Http));
        assert!(!opts.enabled(ContractKind::Env));
        assert!(!opts.enabled(ContractKind::Queue));
        assert!(opts.enabled(ContractKind::WebSocket));
        assert!(!opts.enabled(ContractKind::Job));
        assert!(!opts.enabled(ContractKind::Grpc));
        assert!(opts.enabled(ContractKind::Graphql));
        assert!(!opts.enabled(ContractKind::Openapi));
    }

    #[test]
    fn contract_options_is_copy() {
        let opts = ContractOptions::default();
        let copy = opts;
        assert_eq!(
            copy.enabled(ContractKind::Queue),
            opts.enabled(ContractKind::Queue)
        );
    }

    #[test]
    fn extract_with_all_kinds_disabled_returns_empty() {
        let src = "const app = express();\napp.get('/v1/users/:id', h);\nconst db = process.env.DATABASE_URL;\n";
        let opts = ContractOptions {
            http: false,
            env: false,
            queue: false,
            websocket: false,
            job: false,
            grpc: false,
            graphql: false,
            openapi: false,
        };
        assert!(extract_with(Lang::JavaScript, src, &opts).is_empty());
    }

    #[test]
    fn extract_with_http_disabled_keeps_env() {
        let src = "const app = express();\napp.get('/v1/users/:id', h);\nconst db = process.env.DATABASE_URL;\n";
        let opts = ContractOptions {
            http: false,
            ..ContractOptions::default()
        };
        let cands = extract_with(Lang::JavaScript, src, &opts);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].kind, ContractKind::Env);
    }

    #[test]
    fn extract_with_env_disabled_keeps_http() {
        let src = "const app = express();\napp.get('/v1/users/:id', h);\nconst db = process.env.DATABASE_URL;\n";
        let opts = ContractOptions {
            env: false,
            ..ContractOptions::default()
        };
        let cands = extract_with(Lang::JavaScript, src, &opts);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].kind, ContractKind::Http);
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
        ("{BASE_URL}/users", "users"),
        // Interpolation that is not the leading segment stays.
        ("/v1/${BASE}/users", "/v1/${BASE}/users"),
        // Lone token (no following path) stays — it is the route itself.
        ("/{id}", "/{id}"),
        ("${API_URL}", "${API_URL}"),
        // Curly tokens with parameter-like names stay for stage 4 to
        // rewrite — only base-URL-like names (underscore / ALL_CAPS)
        // are treated as interpolations.
        ("/{tenant}/users", "/{tenant}/users"),
        ("/{org}/{repo}", "/{org}/{repo}"),
        ("{BASE_URL}/x", "x"),
        ("/{BASE}/x", "x"),
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
        // Dollar-sigil and base-URL-like curly tokens still strip.
        assert_eq!(norm("${A}/x").map(|n| n.path), Some("/x".into()));
        assert_eq!(norm("$A/x").map(|n| n.path), Some("/x".into()));
        assert_eq!(norm("{BASE_URL}/x").map(|n| n.path), Some("/x".into()));
    }

    // -- walker: JavaScript / TypeScript (step 4) -------------------------------

    use extract_test_helpers::{extract, extract_with, find};

    #[test]
    fn express_provider() {
        let src = "const app = express();\napp.get('/v1/users/:id', handler);\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.kind, ContractKind::Http);
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.canonical_id, "http::GET::/v1/users/{p1}");
        assert_eq!(
            c.params,
            vec![PathParam {
                position: 1,
                name: "id".into()
            }]
        );
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.line, 2);
        assert_eq!(c.owning_symbol, None);
    }

    #[test]
    fn express_router_var_binding() {
        let src = "const r = express.Router();\nr.post('/orders', createOrder);\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "http::POST::/orders").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn fetch_consumer_absolute_url() {
        let src =
            "async function load() {\n  const r = await fetch('https://api.io/v1/users');\n}\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "http::GET::/v1/users").expect("route not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.owning_symbol.as_deref(), Some("load"));
    }

    #[test]
    fn express_template_consumer() {
        let src = "async function load() {\n  await fetch(`${API_URL}/v1/tags/${id}`);\n}\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "http::GET::/v1/tags/{p1}").expect("route not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(
            c.params,
            vec![PathParam {
                position: 1,
                name: "id".into()
            }]
        );
    }

    #[test]
    fn axios_member_consumer() {
        let src = "const d = await axios.get('/v1/users');\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "http::GET::/v1/users").expect("route not found");
        assert_eq!(c.role, ContractRole::Consumer);
    }

    #[test]
    fn process_env_member_read() {
        let src = "const url = process.env.DATABASE_URL;\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "env::::DATABASE_URL").expect("env not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.owning_symbol, None);
    }

    #[test]
    fn process_env_subscript_read() {
        let src = "const k = process.env['API_KEY'];\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "env::::API_KEY").expect("env not found");
        assert_eq!(c.role, ContractRole::Consumer);
    }

    #[test]
    fn import_meta_env_read() {
        let src = "const k = import.meta.env.VITE_API_KEY;\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "env::::VITE_API_KEY").expect("env not found");
        assert_eq!(c.role, ContractRole::Consumer);
    }

    #[test]
    fn process_env_write_is_ambiguous_provider() {
        let src = "process.env.FEATURE_FLAG = 'on';\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "env::::FEATURE_FLAG");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    // -- walker: queue, JavaScript (TASK-087 step 5) ---------------------------

    #[test]
    fn kafkajs_producer_send_topic_prop() {
        let src = "const producer = kafka.producer();\nawait producer.send({ topic: 'orders.created', messages: [m] });\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.kind, ContractKind::Queue);
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.canonical_id, "queue::kafka::orders.created");
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.line, 2);
    }

    #[test]
    fn kafkajs_consumer_subscribe_topic_prop() {
        let src = "await consumer.subscribe({ topic: 'orders.created', fromBeginning: true });\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
        assert_eq!(c.kind, ContractKind::Queue);
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn amqplib_send_to_queue() {
        let src = "ch.sendToQueue('orders.created', Buffer.from(msg));\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn amqplib_publish_routing_key() {
        let src = "ch.publish('orders', 'orders.created', Buffer.from(msg));\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].canonical_id, "queue::rabbitmq::orders.created");
        assert_eq!(cands[0].role, ContractRole::Consumer);
    }

    #[test]
    fn amqplib_consume() {
        let src = "ch.consume('orders.created', (msg) => {});\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn nats_js_publish() {
        let src = "nc.publish('orders.created', payload);\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn nats_js_subscribe() {
        let src = "nc.subscribe('orders.created', cb);\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn js_generic_send_heuristic() {
        let src = "svc.send('orders.created');\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "queue::::orders.created");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn js_generic_publish_broker_from_receiver() {
        let src = "rabbitChan.publish('orders.created');\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].canonical_id, "queue::rabbitmq::orders.created");
        assert_eq!(cands[0].confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn js_generic_subscribe_heuristic() {
        let src = "consumer.subscribe('orders.created');\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "queue::::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn js_socket_send_is_reserved_for_websocket() {
        // ws.send belongs to the websocket kind, never to queue.
        let cands = extract(Lang::JavaScript, "socket.send('hello');");
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].kind, ContractKind::WebSocket);
        assert_eq!(cands[0].canonical_id, "websocket::::hello");
        assert_eq!(cands[0].role, ContractRole::Provider);
    }

    #[test]
    fn js_socket_send_non_literal_skipped() {
        let cands = extract(Lang::JavaScript, "ws.send(JSON.stringify(data));");
        assert!(cands.is_empty(), "got {cands:?}");
    }

    #[test]
    fn js_queue_non_literal_topic_skipped() {
        let cands = extract(Lang::JavaScript, "svc.send(topic);");
        assert!(cands.is_empty(), "got {cands:?}");
    }

    #[test]
    fn js_queue_interpolated_topic_skipped() {
        let cands = extract(Lang::JavaScript, "svc.send(`orders.${id}`);");
        assert!(cands.is_empty(), "got {cands:?}");
    }

    // -- walker: websocket, JS/TS (TASK-087 step 6) ----------------------------

    #[test]
    fn js_io_emit_is_ws_provider() {
        let src = "io.emit('chat.message', payload);\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.kind, ContractKind::WebSocket);
        assert_eq!(c.canonical_id, "websocket::::chat.message");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn js_socket_emit_is_ws_provider() {
        let cands = extract(Lang::JavaScript, "socket.emit('chat.message', data);");
        let c = find(&cands, "websocket::::chat.message").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn js_socket_broadcast_emit_is_ws_provider() {
        let cands = extract(
            Lang::JavaScript,
            "socket.broadcast.emit('chat.message', data);",
        );
        let c = find(&cands, "websocket::::chat.message").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn js_io_to_room_emit_is_ws_provider() {
        let cands = extract(Lang::JavaScript, "io.to(room).emit('chat.message', d);");
        let c = find(&cands, "websocket::::chat.message").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn js_socket_on_is_ws_consumer() {
        let src = "socket.on('chat.message', (msg) => {});\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "websocket::::chat.message");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn js_ws_on_requires_handler_argument() {
        // `.on` with a bare event name and no handler is not a registration.
        let cands = extract(Lang::JavaScript, "socket.on('chat.message');");
        assert!(cands.is_empty(), "got {cands:?}");
    }

    #[test]
    fn js_generic_emit_heuristic() {
        let cands = extract(Lang::JavaScript, "events.emit('user.created', data);");
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "websocket::::user.created");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn js_generic_on_is_skipped() {
        // EventEmitter `.on` registrations flood every codebase — skipped.
        let cands = extract(Lang::JavaScript, "emitter.on('tick', cb);");
        assert!(cands.is_empty(), "got {cands:?}");
    }

    #[test]
    fn js_server_on_listening_skipped() {
        // Node http idiom: server.on('listening', cb) is a lifecycle hook,
        // not a websocket registration — falls through to the generic `.on`
        // skip like any other EventEmitter.
        let cands = extract(Lang::JavaScript, "server.on('listening', cb);");
        assert!(cands.is_empty(), "got {cands:?}");
    }

    #[test]
    fn js_conn_on_data_skipped() {
        // Node net idiom: conn.on('data', cb) is a plain stream read, not a
        // websocket registration — falls through to the generic `.on` skip.
        let cands = extract(Lang::JavaScript, "conn.on('data', (chunk) => {});");
        assert!(cands.is_empty(), "got {cands:?}");
    }

    #[test]
    fn js_ws_receiver_whitelist_stays_websocket() {
        // Every whitelisted receiver name emits websocket contracts.
        for recv in ["io", "socket", "ws", "wss", "websocket"] {
            let cands = extract(
                Lang::JavaScript,
                &format!("{recv}.emit('chat.message', d);"),
            );
            assert_eq!(cands.len(), 1, "{recv}: got {cands:?}");
            assert_eq!(cands[0].kind, ContractKind::WebSocket, "{recv}");
            assert_eq!(cands[0].canonical_id, "websocket::::chat.message", "{recv}");
            assert_eq!(cands[0].confidence, CONFIDENCE_FRAMEWORK, "{recv}");
        }
    }

    #[test]
    fn js_express_ws_route_is_ws_consumer() {
        let src = "const app = express();\napp.ws('/chat', handler);\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "websocket::::/chat");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    // -- walker: Python (step 5) ------------------------------------------------

    #[test]
    fn flask_decorator_provider() {
        let src = "\
from flask import Flask
app = Flask(__name__)

@app.get('/v1/users/<int:id>')
def get_user(id):
    return {}
";
        let cands = extract(Lang::Python, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.canonical_id, "http::GET::/v1/users/{p1}");
        assert_eq!(
            c.params,
            vec![PathParam {
                position: 1,
                name: "id".into()
            }]
        );
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.owning_symbol.as_deref(), Some("get_user"));
        assert_eq!(c.line, 4);
    }

    #[test]
    fn flask_route_methods_kwarg_sets_verb() {
        let src = "\
app = Flask(__name__)

@app.route('/orders', methods=['POST'])
def create_order():
    return {}
";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "http::POST::/orders").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.owning_symbol.as_deref(), Some("create_order"));
    }

    #[test]
    fn flask_route_without_methods_is_get() {
        let src = "\
@app.route('/health')
def health():
    return {}
";
        let cands = extract(Lang::Python, src);
        assert!(
            find(&cands, "http::GET::/health").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn requests_consumer_absolute_url() {
        let src = "\
def load():
    r = requests.get('https://api.io/v1/users')
";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "http::GET::/v1/users").expect("route not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.owning_symbol.as_deref(), Some("load"));
    }

    #[test]
    fn httpx_and_session_consumers() {
        let src = "\
def load():
    a = httpx.get('/v1/users')
    b = session.get('/v1/users')
    c = client.get('/v1/users')
    d = urlopen('/health')
";
        let cands = extract(Lang::Python, src);
        for id in ["http::GET::/v1/users", "http::GET::/health"] {
            assert!(find(&cands, id).is_some(), "missing {id}: {cands:?}");
        }
        assert_eq!(cands.len(), 4, "got {cands:?}");
        assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
    }

    #[test]
    fn python_fstring_consumer() {
        let src = "\
def load():
    r = requests.get(f'{BASE_URL}/users/{user_id}')
";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "http::GET::/users/{p1}").expect("route not found");
        assert_eq!(
            c.params,
            vec![PathParam {
                position: 1,
                name: "user_id".into()
            }]
        );
    }

    #[test]
    fn python_env_accessors() {
        let src = "\
def cfg():
    a = os.environ['DATABASE_URL']
    b = os.environ.get('FEATURE_FLAG')
    c = os.getenv('HOME')
";
        let cands = extract(Lang::Python, src);
        for name in ["DATABASE_URL", "FEATURE_FLAG", "HOME"] {
            let c = find(&cands, &format!("env::::{name}")).expect("env not found");
            assert_eq!(c.role, ContractRole::Consumer);
            assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        }
        assert_eq!(cands.len(), 3, "got {cands:?}");
    }

    #[test]
    fn python_env_setdefault_is_ambiguous_provider() {
        let src = "os.environ.setdefault('CACHE_DIR', '/tmp')\n";
        let cands = extract(Lang::Python, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "env::::CACHE_DIR");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn python_environ_assign_is_ambiguous_provider() {
        let src = "os.environ['TMP_SET'] = '1'\n";
        let cands = extract(Lang::Python, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "env::::TMP_SET");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn django_urlpatterns_providers() {
        let src = "\
from django.urls import path
import views

urlpatterns = [
    path('users/<int:id>', views.user_detail),
    path('health', views.health),
]
";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "http::ANY::/users/{p1}").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(
            c.params,
            vec![PathParam {
                position: 1,
                name: "id".into()
            }]
        );
        assert!(find(&cands, "http::ANY::/health").is_some());
        assert_eq!(cands.len(), 2, "got {cands:?}");
    }

    #[test]
    fn falcon_add_route_provider() {
        let src = "api.add_route('/things', ThingsResource())\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "http::ANY::/things").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    // -- walker: queue, Python (TASK-087 step 5) -------------------------------

    #[test]
    fn py_kafka_producer_send_heuristic() {
        let src = "def run():\n    producer.send('orders.created', value=msg)\n";
        let cands = extract(Lang::Python, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "queue::::orders.created");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
        assert_eq!(c.owning_symbol.as_deref(), Some("run"));
    }

    #[test]
    fn py_confluent_producer_produce() {
        let src = "producer.produce('orders.created', value=msg)\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn py_kafka_consumer_subscribe_list() {
        let src = "consumer.subscribe(['orders.created'])\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn py_consumer_subscribe_plain_string_heuristic() {
        let src = "consumer.subscribe('orders.created')\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "queue::::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn py_consumer_subscribe_multi_topic_list_skipped() {
        let cands = extract(
            Lang::Python,
            "consumer.subscribe(['a.created', 'b.created'])",
        );
        assert!(cands.is_empty(), "got {cands:?}");
    }

    #[test]
    fn py_nats_publish() {
        let src = "async def push():\n    await nc.publish('orders.created', b'x')\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn py_nats_subscribe() {
        let src = "async def listen():\n    await nc.subscribe('orders.created')\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn py_pika_basic_publish_kwarg() {
        let src = "ch.basic_publish(exchange='', routing_key='orders.created', body=msg)\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn py_pika_basic_publish_positional() {
        let src = "ch.basic_publish('', 'orders.created', msg)\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
    }

    #[test]
    fn py_pika_basic_consume_kwarg() {
        let src = "ch.basic_consume(queue='orders.created', on_message_callback=cb)\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn py_pika_basic_consume_positional() {
        let src = "ch.basic_consume('orders.created', cb)\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn py_generic_send_broker_from_receiver() {
        let src = "kafka_producer.send('orders.created')\n";
        let cands = extract(Lang::Python, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].canonical_id, "queue::kafka::orders.created");
        assert_eq!(cands[0].confidence, CONFIDENCE_HEURISTIC);
    }

    // -- walker: Ruby (step 6) ---------------------------------------------------

    #[test]
    fn ruby_sinatra_provider() {
        let src = "\
get '/v1/users/:id' do
  json
end
";
        let cands = extract(Lang::Ruby, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "http::GET::/v1/users/{p1}");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(
            c.params,
            vec![PathParam {
                position: 1,
                name: "id".into()
            }]
        );
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn ruby_rails_match_is_any() {
        let src = "match '/health', to: 'health#show', via: :all\n";
        let cands = extract(Lang::Ruby, src);
        let c = find(&cands, "http::ANY::/health").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn ruby_consumers() {
        let src = "\
def pull
  HTTParty.get('https://api.io/v1/users')
  RestClient.get('/v1/users')
  Faraday.get('/v1/users')
end
";
        let cands = extract(Lang::Ruby, src);
        assert_eq!(cands.len(), 3, "got {cands:?}");
        assert!(
            cands
                .iter()
                .all(|c| c.canonical_id == "http::GET::/v1/users")
        );
        assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
        assert!(
            cands
                .iter()
                .all(|c| c.owning_symbol.as_deref() == Some("pull"))
        );
    }

    #[test]
    fn ruby_env() {
        let src = "\
db = ENV['DATABASE_URL']
k = ENV.fetch('KEY')
ENV['TMP_SET'] = 'x'
";
        let cands = extract(Lang::Ruby, src);
        assert_eq!(cands.len(), 3, "got {cands:?}");
        let db = find(&cands, "env::::DATABASE_URL").expect("db not found");
        assert_eq!(db.role, ContractRole::Consumer);
        assert!(find(&cands, "env::::KEY").is_some());
        let w = find(&cands, "env::::TMP_SET").expect("write not found");
        assert_eq!(w.role, ContractRole::Provider);
        assert_eq!(w.confidence, CONFIDENCE_HEURISTIC);
    }

    // -- walker: queue, Ruby (TASK-087 step 5) ---------------------------------

    #[test]
    fn ruby_bunny_publish_routing_key_kwarg() {
        let src = "x.publish(payload, routing_key: 'orders.created')\n";
        let cands = extract(Lang::Ruby, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "queue::rabbitmq::orders.created");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn ruby_bunny_queue_subscribe_binding() {
        let src = "q = channel.queue('orders.created')\nq.subscribe do |info, props, body|\n  puts body\nend\n";
        let cands = extract(Lang::Ruby, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "queue::rabbitmq::orders.created");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn ruby_subscribe_on_unbound_var_skipped() {
        // No channel.queue binding: nothing to attribute the topic from.
        let cands = extract(Lang::Ruby, "q.subscribe do |info, body|\nend\n");
        assert!(cands.is_empty(), "got {cands:?}");
    }

    #[test]
    fn ruby_generic_publish_heuristic() {
        let src = "chan.publish('orders.created')\n";
        let cands = extract(Lang::Ruby, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].canonical_id, "queue::::orders.created");
        assert_eq!(cands[0].role, ContractRole::Consumer);
        assert_eq!(cands[0].confidence, CONFIDENCE_HEURISTIC);
    }

    // -- walker: Go (step 6) -----------------------------------------------------

    #[test]
    fn go_gin_provider() {
        let src = "\
func main() {
	r := gin.New()
	r.GET(\"/users/:id\", getUser)
	r.POST(\"/orders\", createOrder)
}
";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 2, "got {cands:?}");
        let c = find(&cands, "http::GET::/users/{p1}").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(
            c.params,
            vec![PathParam {
                position: 1,
                name: "id".into()
            }]
        );
        assert_eq!(c.owning_symbol.as_deref(), Some("main"));
        assert!(find(&cands, "http::POST::/orders").is_some());
    }

    #[test]
    fn go_handlefunc_is_any() {
        let src = "\
func main() {
	mux.HandleFunc(\"/health\", health)
	http.Handle(\"/static/\", files)
}
";
        let cands = extract(Lang::Go, src);
        assert!(
            find(&cands, "http::ANY::/health").is_some(),
            "got {cands:?}"
        );
        assert!(
            find(&cands, "http::ANY::/static").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn go_consumers() {
        let src = "\
func call() {
	resp, _ := http.Get(\"https://api.io/v1/users\")
	req, _ := http.NewRequest(\"POST\", \"/v1/orders\", nil)
	c, _ := client.Get(\"/v1/users\")
}
";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 3, "got {cands:?}");
        assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
        assert!(find(&cands, "http::GET::/v1/users").is_some());
        assert!(find(&cands, "http::POST::/v1/orders").is_some());
    }

    #[test]
    fn go_env() {
        let src = "\
func cfg() {
	k := os.Getenv(\"DATABASE_URL\")
	l, ok := os.LookupEnv(\"FLAG\")
	os.Setenv(\"TMP_SET\", \"x\")
}
";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 3, "got {cands:?}");
        assert!(find(&cands, "env::::DATABASE_URL").is_some());
        assert!(find(&cands, "env::::FLAG").is_some());
        let w = find(&cands, "env::::TMP_SET").expect("write not found");
        assert_eq!(w.role, ContractRole::Provider);
    }

    // -- walker: queue, Go (TASK-087 step 5) -----------------------------------

    #[test]
    fn go_nats_publish_two_args() {
        let src = "package main\n\nfunc push() {\n\tnat.Publish(\"orders.created\", data)\n}\n";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "queue::nats::orders.created");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.line, 4);
    }

    #[test]
    fn go_amqp_publish_three_args_is_rabbitmq() {
        let src = "package main\n\nfunc pub() {\n\tch.Publish(\"orders\", \"orders.created\", false, false, msg)\n}\n";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].canonical_id, "queue::rabbitmq::orders.created");
        assert_eq!(cands[0].role, ContractRole::Consumer);
    }

    #[test]
    fn go_amqp_publish_with_context_six_args_is_rabbitmq() {
        let src = "package main\n\nfunc pub() {\n\tch.PublishWithContext(ctx, \"orders\", \"orders.created\", false, false, msg)\n}\n";
        let cands = extract(Lang::Go, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
    }

    #[test]
    fn go_publish_with_context_three_args_is_nats() {
        // nats.go: PublishWithContext(ctx, subj, data) — exactly 3 args,
        // subject at position 1. The literal payload is never the topic.
        let src = "package main\n\nfunc pub() {\n\tnc.PublishWithContext(ctx, \"orders.created\", \"body\")\n}\n";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "queue::nats::orders.created");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert!(
            find(&cands, "queue::rabbitmq::body").is_none(),
            "literal data must not be read as the topic: {cands:?}"
        );
    }

    #[test]
    fn go_publish_with_context_three_args_variable_payload_is_nats() {
        let src = "package main\n\nfunc pub() {\n\tnc.PublishWithContext(ctx, \"orders.created\", msg)\n}\n";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].canonical_id, "queue::nats::orders.created");
        assert_eq!(cands[0].role, ContractRole::Consumer);
    }

    #[test]
    fn go_publish_with_context_four_plus_args_is_rabbitmq() {
        // amqp091: PublishWithContext(ctx, exchange, key, msg, ...) — the
        // ctx shifts the routing key to position 2.
        let src = "package main\n\nfunc pub() {\n\tch.PublishWithContext(ctx, \"orders\", \"orders.created\", msg)\n}\n";
        let cands = extract(Lang::Go, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn go_publish_four_args_rabbitmq_topic_position() {
        // Plain amqp Publish(exchange, key, ...): topic is arg 1, never the
        // exchange name at arg 0.
        let src = "package main\n\nfunc pub() {\n\tch.Publish(\"orders\", \"orders.created\", false, msg)\n}\n";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].canonical_id, "queue::rabbitmq::orders.created");
        assert!(
            find(&cands, "queue::rabbitmq::orders").is_none(),
            "exchange name must not be the topic: {cands:?}"
        );
        assert_eq!(cands[0].role, ContractRole::Consumer);
    }

    #[test]
    fn go_nats_subscribe() {
        let src = "package main\n\nfunc listen() {\n\tnc.Subscribe(\"orders.created\", cb)\n}\n";
        let cands = extract(Lang::Go, src);
        let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn go_nats_queue_subscribe() {
        let src = "package main\n\nfunc listen() {\n\tnc.QueueSubscribe(\"orders.created\", \"grp\", cb)\n}\n";
        let cands = extract(Lang::Go, src);
        let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn go_amqp_consume() {
        let src = "package main\n\nfunc listen() {\n\tmsgs, _ := ch.Consume(\"orders.created\", \"\", true, false, false, false, nil)\n\t_ = msgs\n}\n";
        let cands = extract(Lang::Go, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn go_sarama_consume_partition() {
        let src = "package main\n\nfunc listen() {\n\tpc, _ := consumer.ConsumePartition(\"orders.created\", 0, 0)\n\t_ = pc\n}\n";
        let cands = extract(Lang::Go, src);
        let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn go_sarama_producer_send_message() {
        let src = "package main\n\nfunc pub() {\n\tproducer.SendMessage(&ProducerMessage{Topic: \"orders.created\"})\n}\n";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].canonical_id, "queue::kafka::orders.created");
        assert_eq!(cands[0].role, ContractRole::Consumer);
    }

    #[test]
    fn go_publish_arg_count_disambiguates_broker() {
        // 2-positional Publish is nats; 3+ is amqp (paired disambiguation).
        let two = extract(
            Lang::Go,
            "package main\nfunc a() {\n\tnc.Publish(\"s.a\", d)\n}\n",
        );
        let three = extract(
            Lang::Go,
            "package main\nfunc b() {\n\tch.Publish(\"e\", \"s.a\", d)\n}\n",
        );
        assert!(find(&two, "queue::nats::s.a").is_some(), "got {two:?}");
        assert!(
            find(&three, "queue::rabbitmq::s.a").is_some(),
            "got {three:?}"
        );
    }

    // -- walker: Rust (step 6) ---------------------------------------------------

    #[test]
    fn rust_attribute_provider() {
        let src = "\
#[get(\"/v1/users/{id}\")]
async fn get_user() -> impl Responder {
    todo!()
}
";
        let cands = extract(Lang::Rust, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "http::GET::/v1/users/{p1}");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(
            c.params,
            vec![PathParam {
                position: 1,
                name: "id".into()
            }]
        );
        assert_eq!(c.owning_symbol.as_deref(), Some("get_user"));
    }

    #[test]
    fn rust_route_attribute_reads_method_kwarg() {
        let src = "\
#[route(\"/v1/orders\", method = \"GET\")]
async fn list_orders() -> impl Responder {
    todo!()
}
";
        let cands = extract(Lang::Rust, src);
        let c = find(&cands, "http::GET::/v1/orders").expect("route not found");
        assert_eq!(c.owning_symbol.as_deref(), Some("list_orders"));
    }

    #[test]
    fn rust_route_call_provider() {
        let src = "\
async fn app() {
    let app = Router::new().route(\"/users/{id}\", get(get_user));
}
";
        let cands = extract(Lang::Rust, src);
        let c = find(&cands, "http::GET::/users/{p1}").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn rust_consumers() {
        let src = "\
async fn calls() {
    let b = reqwest::get(\"https://api.io/v1/users\").await;
    let c = client.get(\"/v1/users\").send().await;
}
";
        let cands = extract(Lang::Rust, src);
        assert_eq!(cands.len(), 2, "got {cands:?}");
        assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
        assert!(
            cands
                .iter()
                .all(|c| c.canonical_id == "http::GET::/v1/users")
        );
    }

    #[test]
    fn rust_env() {
        let src = "\
fn cfg() {
    let u = std::env::var(\"DATABASE_URL\").unwrap();
    let e = env!(\"API_KEY\");
    std::env::set_var(\"TMP_SET\", \"x\");
}
";
        let cands = extract(Lang::Rust, src);
        assert_eq!(cands.len(), 3, "got {cands:?}");
        assert!(find(&cands, "env::::DATABASE_URL").is_some());
        assert!(find(&cands, "env::::API_KEY").is_some());
        let w = find(&cands, "env::::TMP_SET").expect("write not found");
        assert_eq!(w.role, ContractRole::Provider);
    }

    // -- walker: queue, Rust (TASK-087 step 5) ---------------------------------

    #[test]
    fn rust_rdkafka_future_record_to() {
        let src = "fn publish() {\n    let rec = FutureRecord::to(\"orders.created\", 0, payload);\n    producer.send(rec, Timeout::Never);\n}\n";
        let cands = extract(Lang::Rust, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "queue::kafka::orders.created");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.line, 2);
        assert_eq!(c.owning_symbol.as_deref(), Some("publish"));
    }

    #[test]
    fn rust_rdkafka_base_record_to() {
        let src = "fn publish() {\n    let rec = BaseRecord::to(\"orders.created\");\n}\n";
        let cands = extract(Lang::Rust, src);
        let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
    }

    #[test]
    fn rust_rdkafka_consumer_subscribe_slice() {
        let src = "fn listen() {\n    consumer.subscribe(&[\"orders.created\"])?;\n}\n";
        let cands = extract(Lang::Rust, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].canonical_id, "queue::kafka::orders.created");
        assert_eq!(cands[0].role, ContractRole::Provider);
        assert_eq!(cands[0].confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn rust_rdkafka_subscribe_multi_topic_slice_skipped() {
        let cands = extract(
            Lang::Rust,
            "fn listen() {\n    consumer.subscribe(&[\"a\", \"b\"])?;\n}\n",
        );
        assert!(cands.is_empty(), "got {cands:?}");
    }

    #[test]
    fn rust_async_nats_publish_into() {
        let src =
            "async fn push() {\n    client.publish(\"orders.created\".into(), bytes).await?;\n}\n";
        let cands = extract(Lang::Rust, src);
        let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn rust_async_nats_subscribe_into() {
        let src =
            "async fn listen() {\n    client.subscribe(\"orders.created\".into()).await?;\n}\n";
        let cands = extract(Lang::Rust, src);
        let c = find(&cands, "queue::nats::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn rust_subscribe_non_string_receiver_skipped() {
        let cands = extract(Lang::Rust, "fn f() {\n    bus.subscribe(handler);\n}\n");
        assert!(cands.is_empty(), "got {cands:?}");
    }

    // -- walker: Java (step 6) ---------------------------------------------------

    #[test]
    fn java_spring_provider() {
        let src = "\
public class UserController {

    @GetMapping(\"/users/{id}\")
    public String getUser(@PathVariable String id) { return \"\"; }

    @PostMapping(\"/orders\")
    public String create() { return \"\"; }
}
";
        let cands = extract(Lang::Java, src);
        assert_eq!(cands.len(), 2, "got {cands:?}");
        let c = find(&cands, "http::GET::/users/{p1}").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.owning_symbol.as_deref(), Some("getUser"));
        assert!(find(&cands, "http::POST::/orders").is_some());
    }

    #[test]
    fn java_jaxrs_provider() {
        let src = "\
@Path(\"/items\")
public class ItemsResource {

    @GET
    @Path(\"/{id}\")
    public String item() { return \"\"; }
}
";
        let cands = extract(Lang::Java, src);
        let c = find(&cands, "http::GET::/items/{p1}").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.owning_symbol.as_deref(), Some("item"));
    }

    #[test]
    fn java_consumers() {
        let src = "\
class Client {
    String call() {
        String r = restTemplate.getForObject(\"https://api.io/v1/users\", String.class);
        String p = restTemplate.postForObject(\"/v1/orders\", req, String.class);
        return r;
    }
}
";
        let cands = extract(Lang::Java, src);
        assert_eq!(cands.len(), 2, "got {cands:?}");
        assert!(find(&cands, "http::GET::/v1/users").is_some());
        assert!(find(&cands, "http::POST::/v1/orders").is_some());
        assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
    }

    #[test]
    fn java_env() {
        let src = "\
class Client {
    String cfg() {
        String e = System.getenv(\"DATABASE_URL\");
        return e;
    }
}
";
        let cands = extract(Lang::Java, src);
        let c = find(&cands, "env::::DATABASE_URL").expect("env not found");
        assert_eq!(c.role, ContractRole::Consumer);
    }

    // -- walker: queue, Java (TASK-087 step 5) ---------------------------------

    #[test]
    fn java_kafka_listener_topics_string() {
        let src = "\
@Component
class Orders {
    @KafkaListener(topics = \"orders.created\")
    public void handle(String msg) {}
}
";
        let cands = extract(Lang::Java, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "queue::kafka::orders.created");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.owning_symbol.as_deref(), Some("handle"));
    }

    #[test]
    fn java_kafka_listener_topics_array_emits_per_element() {
        let src = "\
class Orders {
    @KafkaListener(topics = {\"a.created\", \"b.created\"})
    public void handle(String msg) {}
}
";
        let cands = extract(Lang::Java, src);
        assert_eq!(cands.len(), 2, "got {cands:?}");
        assert!(find(&cands, "queue::kafka::a.created").is_some());
        assert!(find(&cands, "queue::kafka::b.created").is_some());
    }

    #[test]
    fn java_rabbit_listener_queues() {
        let src = "\
class Orders {
    @RabbitListener(queues = \"orders.created\")
    public void handle(String msg) {}
}
";
        let cands = extract(Lang::Java, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn java_kafka_template_send() {
        let src = "void publish() {\n    kafkaTemplate.send(\"orders.created\", key, value);\n}\n";
        let cands = extract(Lang::Java, src);
        let c = find(&cands, "queue::kafka::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.owning_symbol.as_deref(), Some("publish"));
    }

    #[test]
    fn java_rabbit_template_convert_and_send() {
        let src = "void publish() {\n    rabbitTemplate.convertAndSend(\"ex\", \"orders.created\", payload);\n}\n";
        let cands = extract(Lang::Java, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn java_rabbit_template_send() {
        let src = "void publish() {\n    rabbitTemplate.send(\"orders.created\", msg);\n}\n";
        let cands = extract(Lang::Java, src);
        let c = find(&cands, "queue::rabbitmq::orders.created").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
    }

    // -- walker: websocket, Java (TASK-087 step 6) ------------------------------

    #[test]
    fn java_message_mapping_is_ws_consumer() {
        let src = "\
@Controller
class Orders {
    @MessageMapping(\"orders.new\")
    public void handle(String msg) {}
}
";
        let cands = extract(Lang::Java, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "websocket::::orders.new");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.owning_symbol.as_deref(), Some("handle"));
    }

    #[test]
    fn java_send_to_is_ws_provider() {
        let src = "\
@Controller
class Orders {
    @SendTo(\"/topic/orders\")
    public void handle(String msg) {}
}
";
        let cands = extract(Lang::Java, src);
        let c = find(&cands, "websocket::::topic.orders").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn java_messaging_template_convert_and_send_is_ws_provider() {
        let src =
            "void push() {\n    messagingTemplate.convertAndSend(\"/topic/orders\", payload);\n}\n";
        let cands = extract(Lang::Java, src);
        let c = find(&cands, "websocket::::topic.orders").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    // -- walker: job (TASK-087 step 7) -----------------------------------------

    #[test]
    fn py_celery_task_decorator() {
        let src = "@app.task\ndef sync_orders():\n    pass\n";
        let cands = extract(Lang::Python, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.kind, ContractKind::Job);
        assert_eq!(c.canonical_id, "job::::sync_orders");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.owning_symbol.as_deref(), Some("sync_orders"));
    }

    #[test]
    fn py_celery_task_name_kwarg() {
        let src = "@app.task(name='orders.sync')\ndef sync():\n    pass\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "job::::orders.sync").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.owning_symbol.as_deref(), Some("sync"));
    }

    #[test]
    fn py_shared_task_decorator() {
        let src = "@shared_task\ndef sync():\n    pass\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "job::::sync").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn py_celery_delay_consumer() {
        let src = "def enqueue(order):\n    sync_orders.delay(order)\n";
        let cands = extract(Lang::Python, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "job::::sync_orders");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.owning_symbol.as_deref(), Some("enqueue"));
    }

    #[test]
    fn py_celery_apply_async_consumer() {
        let src = "def enqueue():\n    sync_orders.apply_async(kwargs={'o': 1})\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "job::::sync_orders").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
    }

    #[test]
    fn py_send_task_consumer() {
        let src = "def enqueue():\n    celery_app.send_task('orders.sync', args=[1])\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "job::::orders.sync").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn py_apscheduler_add_job() {
        let src = "def setup():\n    scheduler.add_job(sync_orders, trigger='interval')\n";
        let cands = extract(Lang::Python, src);
        let c = find(&cands, "job::::sync_orders").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn ruby_sidekiq_job_class() {
        let src = "class EmailWorker\n  include Sidekiq::Job\n\n  def perform(id)\n  end\nend\n";
        let cands = extract(Lang::Ruby, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "job::::EmailWorker");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn ruby_sidekiq_worker_module() {
        let src = "class CleanupWorker\n  include Sidekiq::Worker\nend\n";
        let cands = extract(Lang::Ruby, src);
        let c = find(&cands, "job::::CleanupWorker").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn ruby_application_job_superclass() {
        let src = "class NotifyJob < ApplicationJob\n  def perform(user)\n  end\nend\n";
        let cands = extract(Lang::Ruby, src);
        let c = find(&cands, "job::::NotifyJob").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn ruby_sidekiq_perform_async_consumer() {
        let cands = extract(Lang::Ruby, "EmailWorker.perform_async(1, 2)");
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].canonical_id, "job::::EmailWorker");
        assert_eq!(cands[0].role, ContractRole::Consumer);
        assert_eq!(cands[0].confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn ruby_active_job_perform_later_consumer() {
        let cands = extract(Lang::Ruby, "NotifyJob.perform_later(user)");
        let c = find(&cands, "job::::NotifyJob").expect("contract not found");
        assert_eq!(c.role, ContractRole::Consumer);
    }

    #[test]
    fn java_scheduled_fixed_rate() {
        let src = "\
class Poller {
    @Scheduled(fixedRate = 5000)
    public void pollOrders() {}
}
";
        let cands = extract(Lang::Java, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "job::::pollOrders");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.owning_symbol.as_deref(), Some("pollOrders"));
    }

    #[test]
    fn java_scheduled_cron_expression_is_not_identity() {
        let src = "\
class Poller {
    @Scheduled(cron = \"0 0 * * * *\")
    public void cleanup() {}
}
";
        let cands = extract(Lang::Java, src);
        let c = find(&cands, "job::::cleanup").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.line, 3, "job line follows the method name");
    }

    #[test]
    fn js_cron_schedule_callback_name() {
        let src = "cron.schedule('*/5 * * * *', fireTick);\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "job::::fireTick");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn js_cron_schedule_inline_callback_uses_owning() {
        let src = "function poll() {\n  cron.schedule(spec, () => run());\n}\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "job::::poll").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn js_agenda_define() {
        let src = "agenda.define('email-send', handler);\n";
        let cands = extract(Lang::JavaScript, src);
        let c = find(&cands, "job::::email-send").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn js_bullmq_queue_add_heuristic() {
        let src = "emailQueue.add('email-send', data);\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        assert_eq!(cands[0].canonical_id, "job::::email-send");
        assert_eq!(cands[0].role, ContractRole::Consumer);
        assert_eq!(cands[0].confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn js_cart_add_is_not_a_job() {
        let cands = extract(Lang::JavaScript, "cart.add('item');");
        assert!(cands.is_empty(), "got {cands:?}");
    }

    #[test]
    fn go_cron_add_func() {
        let src = "package main\n\nfunc setup() {\n\tc.AddFunc(\"*/5 * * * * *\", pollOrders)\n}\n";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "job::::pollOrders");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
    }

    #[test]
    fn go_cron_add_job() {
        let src = "package main\n\nfunc setup() {\n\tc.AddJob(spec, nightlyJob)\n}\n";
        let cands = extract(Lang::Go, src);
        let c = find(&cands, "job::::nightlyJob").expect("contract not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    // -- walker: PHP (step 6) ----------------------------------------------------

    #[test]
    fn php_laravel_provider() {
        let src = "\
<?php
Route::get('/v1/users/{id}', [UserController::class, 'show']);
Route::post('/orders', 'OrderController@store');
";
        let cands = extract(Lang::Php, src);
        assert_eq!(cands.len(), 2, "got {cands:?}");
        let c = find(&cands, "http::GET::/v1/users/{p1}").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert!(find(&cands, "http::POST::/orders").is_some());
    }

    #[test]
    fn php_slim_provider() {
        let src = "\
<?php
$app->get('/slim/x', function ($req, $res) { return $res; });
";
        let cands = extract(Lang::Php, src);
        let c = find(&cands, "http::GET::/slim/x").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn php_symfony_attribute() {
        let src = "\
<?php
class Ctrl {
    #[Route('/sym/x', methods: ['GET'])]
    public function show(): void {}
}
";
        let cands = extract(Lang::Php, src);
        let c = find(&cands, "http::GET::/sym/x").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.owning_symbol.as_deref(), Some("show"));
    }

    #[test]
    fn php_consumers() {
        let src = "\
<?php
function load() {
    $r = Http::get('https://api.io/v1/users');
    $c = $client->get('/v1/users');
}
";
        let cands = extract(Lang::Php, src);
        assert_eq!(cands.len(), 2, "got {cands:?}");
        assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
        assert!(
            cands
                .iter()
                .all(|c| c.canonical_id == "http::GET::/v1/users")
        );
    }

    #[test]
    fn php_env() {
        let src = "\
<?php
function load() {
    $k = $_ENV['DATABASE_URL'];
    putenv('TMP_SET=x');
}
";
        let cands = extract(Lang::Php, src);
        let k = find(&cands, "env::::DATABASE_URL").expect("env not found");
        assert_eq!(k.role, ContractRole::Consumer);
        let w = find(&cands, "env::::TMP_SET").expect("write not found");
        assert_eq!(w.role, ContractRole::Provider);
        assert_eq!(w.confidence, CONFIDENCE_HEURISTIC);
    }

    // -- walker: C# (step 6) -----------------------------------------------------

    #[test]
    fn csharp_attribute_provider() {
        let src = "\
public class UsersController : ControllerBase
{
    [HttpGet(\"/v1/users/{id}\")]
    public string GetUser(string id) { return \"\"; }
}
";
        let cands = extract(Lang::CSharp, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "http::GET::/v1/users/{p1}");
        assert_eq!(c.owning_symbol.as_deref(), Some("GetUser"));
    }

    #[test]
    fn csharp_route_attribute_is_any() {
        let src = "\
public class UsersController
{
    [Route(\"health\")]
    public string Health() { return \"\"; }
}
";
        let cands = extract(Lang::CSharp, src);
        assert!(
            find(&cands, "http::ANY::/health").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn csharp_mapget_provider() {
        let src = "\
class Program {
    static void Map() {
        app.MapGet(\"/mapped/x\", () => \"ok\");
        app.MapPost(\"/mapped/y\", () => \"ok\");
    }
}
";
        let cands = extract(Lang::CSharp, src);
        assert!(
            find(&cands, "http::GET::/mapped/x").is_some(),
            "got {cands:?}"
        );
        assert!(
            find(&cands, "http::POST::/mapped/y").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn csharp_consumers() {
        let src = "\
class Client {
    async Task Load() {
        var r = await httpClient.GetAsync(\"/v1/users\");
        var j = await httpClient.GetFromJsonAsync<string>(\"/v1/users\");
        var m = new HttpRequestMessage(HttpMethod.Get, \"/v1/users\");
    }
}
";
        let cands = extract(Lang::CSharp, src);
        assert_eq!(cands.len(), 3, "got {cands:?}");
        assert!(cands.iter().all(|c| c.role == ContractRole::Consumer));
        assert!(
            cands
                .iter()
                .all(|c| c.canonical_id == "http::GET::/v1/users")
        );
    }

    #[test]
    fn csharp_env() {
        let src = "\
class Client {
    void Cfg() {
        var e = Environment.GetEnvironmentVariable(\"DATABASE_URL\");
    }
}
";
        let cands = extract(Lang::CSharp, src);
        let c = find(&cands, "env::::DATABASE_URL").expect("env not found");
        assert_eq!(c.role, ContractRole::Consumer);
    }

    // -- walker: C / C++ (step 6) ------------------------------------------------

    #[test]
    fn c_curl_consumer() {
        let src = "\
void fetch_it(void) {
    CURL *h = curl_easy_init();
    curl_easy_setopt(h, CURLOPT_URL, \"https://api.io/v1/users\");
}
";
        let cands = extract(Lang::C, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "http::GET::/v1/users");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.owning_symbol.as_deref(), Some("fetch_it"));
    }

    #[test]
    fn c_env() {
        let src = "\
void cfg(void) {
    char *k = getenv(\"DATABASE_URL\");
    putenv(\"TMP_SET=x\");
}
";
        let cands = extract(Lang::C, src);
        let k = find(&cands, "env::::DATABASE_URL").expect("env not found");
        assert_eq!(k.role, ContractRole::Consumer);
        let w = find(&cands, "env::::TMP_SET").expect("write not found");
        assert_eq!(w.role, ContractRole::Provider);
        assert_eq!(w.confidence, CONFIDENCE_HEURISTIC);
    }

    // -- ambiguity rules: 0.5 heuristic + path-like noise gate (step 7) -------

    #[test]
    fn cache_like_receiver_is_not_a_contract() {
        let src = "function load() {
  const u = cache.get('user:1');
  const k = db.get('key');
  const t = cache.get(`${prefix}/key`);
}
";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 0, "got {cands:?}");
    }

    #[test]
    fn unknown_receiver_path_like_is_provider_at_05() {
        let src = "const x = registry.get('/users');\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "http::GET::/users");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn concat_single_literal_is_05() {
        let src = "const r = await fetch('/api/users' + id);\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "http::GET::/api/users");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn concat_multiple_literals_skipped() {
        let src = "const r = await fetch('/api/' + id + '/users');\n";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 0, "got {cands:?}");
    }

    #[test]
    fn python_unknown_receiver_ambiguity() {
        let src = "def load():
    a = store.get('/items')
    b = store.get('user:1')
";
        let cands = extract(Lang::Python, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "http::GET::/items");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn go_capitalized_verb_single_arg_is_05() {
        let src = "func load() {
	x := cache.Get(\"/items\")
	_ = x
}
";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "http::GET::/items");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn php_unknown_object_ambiguity() {
        let src = "<?php
function load() {
    $x = $store->get('/items');
}
";
        let cands = extract(Lang::Php, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "http::GET::/items");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    #[test]
    fn ruby_unknown_receiver_ambiguity() {
        let src = "x = svc.get('/items')\n";
        let cands = extract(Lang::Ruby, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "http::GET::/items");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_HEURISTIC);
    }

    // -- prefix composition (REQ-023, step 8) -----------------------------------

    #[test]
    fn gin_group_prefix() {
        let src = r#"func main() {
	r := gin.New()
	v1 := r.Group("/v1")
	v1.GET("/users/:id", getUser)
}
"#;
        let cands = extract(Lang::Go, src);
        let c = find(&cands, "http::GET::/v1/users/{p1}").expect("route not found");
        assert_eq!(c.role, ContractRole::Provider);
    }

    #[test]
    fn nested_group_two_levels() {
        let src = r#"func main() {
	r := gin.New()
	api := r.Group("/api")
	v1 := api.Group("/v1")
	v1.GET("/users/:id", getUser)
}
"#;
        let cands = extract(Lang::Go, src);
        assert!(
            find(&cands, "http::GET::/api/v1/users/{p1}").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn express_router_mount_same_file() {
        let src = "const app = express();
const router = express.Router();
app.use('/v1', router);
router.get('/users/:id', getUser);
";
        let cands = extract(Lang::JavaScript, src);
        assert!(
            find(&cands, "http::GET::/v1/users/{p1}").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn flask_blueprint_prefix() {
        let src = "bp = Blueprint('auth', __name__, url_prefix='/auth')

@bp.route('/login', methods=['POST'])
def login():
    return {}

app.register_blueprint(bp, url_prefix='/v1')
";
        let cands = extract(Lang::Python, src);
        assert!(
            find(&cands, "http::POST::/v1/auth/login").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn fastapi_includerouter_prefix() {
        let src = "router = APIRouter(prefix='/users')

@router.get('/{id}')
def get_user(id):
    return {}

app.include_router(router, prefix='/v1')
";
        let cands = extract(Lang::Python, src);
        assert!(
            find(&cands, "http::GET::/v1/users/{p1}").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn actix_scope_chain() {
        let src = r#"async fn app() {
    App::new().service(web::scope("/v1").route("/users/{id}", web::get().to(get_user)));
}
"#;
        let cands = extract(Lang::Rust, src);
        assert!(
            find(&cands, "http::GET::/v1/users/{p1}").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn actix_scope_binding() {
        let src = r#"async fn app() {
    let api = web::scope("/api");
    App::new().service(api.route("/users/{id}", web::get().to(get_user)));
}
"#;
        let cands = extract(Lang::Rust, src);
        assert!(
            find(&cands, "http::GET::/api/users/{p1}").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn axum_nest() {
        let src = r#"async fn app() {
    let user_routes = Router::new().route("/users/{id}", get(get_user));
    let app = Router::new().nest("/v1", user_routes);
}
"#;
        let cands = extract(Lang::Rust, src);
        assert!(
            find(&cands, "http::GET::/v1/users/{p1}").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn spring_classlevel_requestmapping() {
        let src = r#"@RestController
@RequestMapping("/v1")
public class UserController {

    @GetMapping("/users/{id}")
    public String getUser(@PathVariable String id) { return ""; }
}
"#;
        let cands = extract(Lang::Java, src);
        assert!(
            find(&cands, "http::GET::/v1/users/{p1}").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn rails_namespace_block() {
        let src = "Rails.application.routes.draw do
  namespace :v1 do
    get '/users', to: 'users#index'
  end
end
";
        let cands = extract(Lang::Ruby, src);
        assert!(
            find(&cands, "http::GET::/v1/users").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn sinatra_namespace_block() {
        let src = "namespace '/v1' do
  get '/users' do
    json
  end
end
";
        let cands = extract(Lang::Ruby, src);
        assert!(
            find(&cands, "http::GET::/v1/users").is_some(),
            "got {cands:?}"
        );
    }

    #[test]
    fn aspnet_controller_route_attribute() {
        let src = r#"[Route("api/[controller]")]
public class UsersController : ControllerBase
{
    [HttpGet("{id}")]
    public string GetUser(string id) { return ""; }
}
"#;
        let cands = extract(Lang::CSharp, src);
        let c = find(&cands, "http::GET::/api/{p1}/{p2}").expect("route not found");
        assert_eq!(
            c.params,
            vec![
                PathParam {
                    position: 1,
                    name: "controller".into()
                },
                PathParam {
                    position: 2,
                    name: "id".into()
                }
            ]
        );
    }
    // -- cross-framework corpus (AR-017, AR-029, step 10) ----------------------

    struct CorpusEntry {
        lang: Lang,
        source: &'static str,
        role: ContractRole,
        confidence: f64,
        canonical_id: &'static str,
        params: &'static [(&'static str, &'static str)],
    }

    const CORPUS: &[CorpusEntry] = &[
        // Providers of GET /v1/users/{id} across frameworks.
        CorpusEntry {
            lang: Lang::JavaScript,
            source: "const app = express(); app.get('/v1/users/:id', h);",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users/{p1}",
            params: &[("p1", "id")],
        },
        CorpusEntry {
            lang: Lang::Python,
            source: "@app.get('/v1/users/<int:id>')\ndef f(id):\n    return {}",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users/{p1}",
            params: &[("p1", "id")],
        },
        CorpusEntry {
            lang: Lang::Python,
            source: "@app.get('/v1/users/{id}')\ndef f(id):\n    return {}",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users/{p1}",
            params: &[("p1", "id")],
        },
        CorpusEntry {
            lang: Lang::Go,
            source: "func main() {\n\tr.GET(\"/v1/users/:id\", h)\n}",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users/{p1}",
            params: &[("p1", "id")],
        },
        CorpusEntry {
            lang: Lang::Rust,
            source: "#[get(\"/v1/users/{id}\")]\nasync fn f() -> impl Responder { todo!() }",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users/{p1}",
            params: &[("p1", "id")],
        },
        CorpusEntry {
            lang: Lang::Rust,
            source: "fn app() { let a = Router::new().route(\"/v1/users/{id}\", get(h)); }",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users/{p1}",
            params: &[("p1", "id")],
        },
        CorpusEntry {
            lang: Lang::Java,
            source: "class C {\n    @GetMapping(\"/v1/users/{id}\")\n    public String f() { return \"\"; }\n}",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users/{p1}",
            params: &[("p1", "id")],
        },
        CorpusEntry {
            lang: Lang::Java,
            source: "@Path(\"/v1/users\")\npublic class UsersResource {\n    @GET\n    @Path(\"/{id}\")\n    public String f() { return \"\"; }\n}",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users/{p1}",
            params: &[("p1", "id")],
        },
        CorpusEntry {
            lang: Lang::Ruby,
            source: "get '/v1/users/:id' do\n  json\nend",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users/{p1}",
            params: &[("p1", "id")],
        },
        CorpusEntry {
            lang: Lang::Php,
            source: "<?php\nRoute::get('/v1/users/{id}', 'UserController@show');",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users/{p1}",
            params: &[("p1", "id")],
        },
        CorpusEntry {
            lang: Lang::CSharp,
            source: "public class C {\n    [HttpGet(\"/v1/users/{id}\")]\n    public string F() { return \"\"; }\n}",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users/{p1}",
            params: &[("p1", "id")],
        },
        // Consumers of GET /v1/users across languages.
        CorpusEntry {
            lang: Lang::JavaScript,
            source: "async function f() { await fetch('https://api.io/v1/users'); }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::JavaScript,
            source: "async function f() { await fetch(`${API_URL}/v1/users`); }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::JavaScript,
            source: "const d = axios.get('/v1/users');",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Python,
            source: "def f():\n    r = requests.get('https://api.io/v1/users')",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Python,
            source: "def f():\n    r = httpx.get('/v1/users')",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Go,
            source: "func f() { r, _ := http.Get(\"https://api.io/v1/users\") }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Rust,
            source: "async fn f() { let r = reqwest::get(\"https://api.io/v1/users\").await; }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Java,
            source: "class C { String f() { return restTemplate.getForObject(\"https://api.io/v1/users\", String.class); } }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Ruby,
            source: "def f\n  HTTParty.get('https://api.io/v1/users')\nend",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Php,
            source: "<?php\nfunction f() { $r = Http::get('https://api.io/v1/users'); }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::CSharp,
            source: "class C { async Task F() { var r = await httpClient.GetAsync(\"/v1/users\"); } }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::C,
            source: "void f(void) { curl_easy_setopt(h, CURLOPT_URL, \"https://api.io/v1/users\"); }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/v1/users",
            params: &[],
        },
        // Leading-parameter route: provider and consumer pair on
        // GET /{tenant}/users across languages.
        CorpusEntry {
            lang: Lang::Java,
            source: "class C {\n    @GetMapping(\"/{tenant}/users\")\n    public String f() { return \"\"; }\n}",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/{p1}/users",
            params: &[("p1", "tenant")],
        },
        CorpusEntry {
            lang: Lang::Python,
            source: "def f(tenant):\n    r = httpx.get(f\"/{tenant}/users\")",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "http::GET::/{p1}/users",
            params: &[("p1", "tenant")],
        },
        // Env accessors across languages -> one ID.
        CorpusEntry {
            lang: Lang::JavaScript,
            source: "const d = process.env.DATABASE_URL;",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "env::::DATABASE_URL",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Python,
            source: "d = os.environ['DATABASE_URL']",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "env::::DATABASE_URL",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Ruby,
            source: "d = ENV['DATABASE_URL']",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "env::::DATABASE_URL",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Go,
            source: "func f() { d := os.Getenv(\"DATABASE_URL\") }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "env::::DATABASE_URL",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Rust,
            source: "fn f() { let d = std::env::var(\"DATABASE_URL\").unwrap(); }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "env::::DATABASE_URL",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Java,
            source: "class C { String f() { return System.getenv(\"DATABASE_URL\"); } }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "env::::DATABASE_URL",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Php,
            source: "<?php\n$d = $_ENV['DATABASE_URL'];",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "env::::DATABASE_URL",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::CSharp,
            source: "class C { string F() { return Environment.GetEnvironmentVariable(\"DATABASE_URL\"); } }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "env::::DATABASE_URL",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::C,
            source: "void f(void) { char *d = getenv(\"DATABASE_URL\"); }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "env::::DATABASE_URL",
            params: &[],
        },
        // -- message kinds (TASK-087) --------------------------------------
        // Kafka orders.created: producer and consumer of one topic ID.
        CorpusEntry {
            lang: Lang::JavaScript,
            source: "producer.send({ topic: 'orders.created', messages: [m] });",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "queue::kafka::orders.created",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Java,
            source: "class C {\n    @KafkaListener(topics = \"orders.created\")\n    public void handle(String m) {}\n}",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "queue::kafka::orders.created",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Rust,
            source: "fn f() { consumer.subscribe(&[\"orders.created\"]).unwrap(); }",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "queue::kafka::orders.created",
            params: &[],
        },
        // NATS orders.created.
        CorpusEntry {
            lang: Lang::Go,
            source: "package main\nfunc a() { nc.Publish(\"orders.created\", d) }",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "queue::nats::orders.created",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Go,
            source: "package main\nfunc a() { nc.Subscribe(\"orders.created\", cb) }",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "queue::nats::orders.created",
            params: &[],
        },
        // RabbitMQ orders.created.
        CorpusEntry {
            lang: Lang::JavaScript,
            source: "ch.consume('orders.created', (m) => {});",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "queue::rabbitmq::orders.created",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Python,
            source: "ch.basic_publish(exchange='', routing_key='orders.created', body=b)",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "queue::rabbitmq::orders.created",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Ruby,
            source: "q = channel.queue('orders.created')\nq.subscribe do |i, p, b|\nend",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "queue::rabbitmq::orders.created",
            params: &[],
        },
        // Websocket chat.message: emit and handler share one ID.
        CorpusEntry {
            lang: Lang::JavaScript,
            source: "io.emit('chat.message', payload);",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "websocket::::chat.message",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::JavaScript,
            source: "socket.on('chat.message', (m) => {});",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "websocket::::chat.message",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Java,
            source: "class C {\n    @SendTo(\"/topic/orders\")\n    public void handle(String m) {}\n}",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "websocket::::topic.orders",
            params: &[],
        },
        // Jobs: definition and enqueue share one ID.
        CorpusEntry {
            lang: Lang::Python,
            source: "@app.task(name='orders.sync')\ndef sync():\n    pass",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "job::::orders.sync",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Python,
            source: "def f():\n    celery_app.send_task('orders.sync')",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "job::::orders.sync",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Ruby,
            source: "class EmailWorker\n  include Sidekiq::Job\nend",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "job::::EmailWorker",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Ruby,
            source: "EmailWorker.perform_async(1)",
            role: ContractRole::Consumer,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "job::::EmailWorker",
            params: &[],
        },
        CorpusEntry {
            lang: Lang::Go,
            source: "package main\nfunc setup() { c.AddFunc(\"*/5 * * * * *\", pollOrders) }",
            role: ContractRole::Provider,
            confidence: CONFIDENCE_FRAMEWORK,
            canonical_id: "job::::pollOrders",
            params: &[],
        },
    ];

    #[test]
    fn corpus_entries_all_match() {
        for e in CORPUS {
            let cands = extract(e.lang, e.source);
            let c = find(&cands, e.canonical_id)
                .unwrap_or_else(|| panic!("{}: no {} in {cands:?}", e.lang.name(), e.canonical_id));
            assert_eq!(c.role, e.role, "{} {c:?}", e.lang.name());
            assert_eq!(c.confidence, e.confidence, "{} {c:?}", e.lang.name());
            let params: Vec<(String, String)> = c
                .params
                .iter()
                .map(|p| (format!("p{}", p.position), p.name.clone()))
                .collect();
            let want: Vec<(String, String)> = e
                .params
                .iter()
                .map(|(m, n)| (m.to_string(), n.to_string()))
                .collect();
            assert_eq!(params, want, "{} {c:?}", e.lang.name());
        }
    }

    #[test]
    fn corpus_provider_group_shares_one_id() {
        let provider_langs: std::collections::HashSet<Lang> = CORPUS
            .iter()
            .filter(|e| e.role == ContractRole::Provider)
            .map(|e| e.lang)
            .collect();
        assert!(
            provider_langs.len() >= 4,
            "need providers in 4+ languages, got {}",
            provider_langs.len()
        );
        let ids: std::collections::HashSet<&str> = CORPUS
            .iter()
            .filter(|e| e.role == ContractRole::Provider)
            .filter(|e| e.canonical_id.starts_with("http::GET::/v1/users/"))
            .map(|e| e.canonical_id)
            .collect();
        assert_eq!(ids.len(), 1, "provider IDs diverged: {ids:?}");
    }

    #[test]
    fn corpus_consumer_group_shares_one_id() {
        let entries: Vec<&CorpusEntry> = CORPUS
            .iter()
            .filter(|e| e.canonical_id == "http::GET::/v1/users")
            .collect();
        let langs: std::collections::HashSet<Lang> = entries.iter().map(|e| e.lang).collect();
        assert!(
            langs.len() >= 4,
            "need consumers in 4+ languages, got {}",
            langs.len()
        );
        assert!(entries.iter().all(|e| e.role == ContractRole::Consumer));
    }

    #[test]
    fn corpus_pairs_provider_and_consumer_across_languages() {
        // AR-029: same route spelled as provider in one language and consumer
        // in others resolves into the matching ID family.
        assert!(
            CORPUS
                .iter()
                .any(|e| e.canonical_id == "http::GET::/v1/users/{p1}"
                    && e.role == ContractRole::Provider)
        );
        assert!(
            CORPUS
                .iter()
                .any(|e| e.canonical_id == "http::GET::/v1/users"
                    && e.role == ContractRole::Consumer)
        );
    }

    #[test]
    fn corpus_env_group_shares_one_id() {
        let ids: std::collections::HashSet<&str> = CORPUS
            .iter()
            .filter(|e| e.canonical_id.starts_with("env::::"))
            .map(|e| e.canonical_id)
            .collect();
        let want: std::collections::HashSet<&str> = ["env::::DATABASE_URL"].into_iter().collect();
        assert_eq!(ids, want);
    }

    // -- acceptance tests (1:1 with TASK-082 criteria) --------------------------

    #[test]
    fn acceptance_four_frameworks_same_id() {
        // Express, Flask/FastAPI, gin, Actix — same canonical ID.
        let cases: &[(Lang, &str)] = &[
            (
                Lang::JavaScript,
                "const app = express(); app.get('/v1/users/:id', h);",
            ),
            (
                Lang::Python,
                "@app.get('/v1/users/<int:id>')\ndef f(id):\n    return {}",
            ),
            (Lang::Go, "func main() { r.GET(\"/v1/users/:id\", h) }"),
            (
                Lang::Rust,
                "#[get(\"/v1/users/{id}\")]\nasync fn f() -> impl Responder { todo!() }",
            ),
        ];
        let mut ids = Vec::new();
        for (lang, src) in cases {
            let cands = extract(*lang, src);
            ids.push(
                cands
                    .iter()
                    .map(|c| c.canonical_id.clone())
                    .max_by_key(|id| id.len())
                    .expect("at least one contract"),
            );
        }
        assert!(
            ids.iter().all(|id| id == &ids[0]),
            "canonical IDs diverged: {ids:?}"
        );
        assert_eq!(ids[0], "http::GET::/v1/users/{p1}");
    }

    #[test]
    fn acceptance_scheme_authority_matches_relative() {
        let a = extract(
            Lang::JavaScript,
            "async function f() { await fetch('http://api.example.com/v1/users'); }",
        );
        let b = extract(
            Lang::JavaScript,
            "async function f() { await fetch('/v1/users'); }",
        );
        assert_eq!(a[0].canonical_id, b[0].canonical_id);
        assert_eq!(a[0].canonical_id, "http::GET::/v1/users");
    }

    #[test]
    fn acceptance_base_interpolation_matches_colon_param() {
        let a = extract(
            Lang::JavaScript,
            "async function f() { await fetch(`${API_URL}/v1/tags/${id}`); }",
        );
        let b = extract(
            Lang::JavaScript,
            "const app = express(); app.get('/v1/tags/:id', h);",
        );
        assert_eq!(a[0].canonical_id, b[0].canonical_id);
        assert_eq!(a[0].canonical_id, "http::GET::/v1/tags/{p1}");
    }

    #[test]
    fn acceptance_positional_params_retain_names() {
        let a = extract(
            Lang::JavaScript,
            "const app = express(); app.get('/workspaces/{wid}/tags/{id}', h);",
        );
        let b = extract(
            Lang::JavaScript,
            "const app = express(); app.get('/workspaces/{workspaceId}/tags/{id}', h);",
        );
        assert_eq!(a[0].canonical_id, b[0].canonical_id);
        assert_eq!(a[0].canonical_id, "http::GET::/workspaces/{p1}/tags/{p2}");
        assert_eq!(
            a[0].params
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["wid", "id"]
        );
        assert_eq!(
            b[0].params
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["workspaceId", "id"]
        );
    }

    #[test]
    fn acceptance_group_prefix_v1() {
        let cands = extract(
            Lang::Go,
            "func main() {\n\tr := gin.New()\n\tv1 := r.Group(\"/v1\")\n\tv1.GET(\"/users/:id\", h)\n}",
        );
        assert!(
            cands
                .iter()
                .any(|c| c.canonical_id == "http::GET::/v1/users/{p1}"),
            "got {cands:?}"
        );
    }

    #[test]
    fn acceptance_ambiguous_confidence_05() {
        let http = extract(Lang::JavaScript, "const x = registry.get('/users');");
        assert_eq!(http.len(), 1);
        assert_eq!(http[0].confidence, 0.5);
        assert_eq!(http[0].role, ContractRole::Provider);
        let env = extract(Lang::JavaScript, "process.env.FEATURE_FLAG = 'on';");
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].confidence, 0.5);
        assert_eq!(env[0].role, ContractRole::Provider);
    }

    #[test]
    fn acceptance_extraction_runs_during_build() {
        // End-to-end: build_index extracts contracts on the pipeline path.
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/app.js"),
            "const app = express();\napp.get('/v1/users/:id', h);\n",
        )
        .unwrap();
        let stats = crate::pipeline::build_index(root, true).unwrap();
        assert_eq!(stats.contract_count, 1, "got {stats:?}");
    }

    // -- acceptance: message kinds (TASK-087) ---------------------------------

    #[test]
    fn acceptance_producer_consumer_same_topic_id_cross_repo() {
        // Framework pair: kafkajs producer + Spring @KafkaListener consumer
        // resolve to the same canonical ID across repos and languages.
        let producer = extract(
            Lang::JavaScript,
            "await producer.send({ topic: 'orders.created', messages: [m] });",
        );
        let consumer = extract(
            Lang::Java,
            "class C {\n    @KafkaListener(topics = \"orders.created\")\n    public void handle(String m) {}\n}",
        );
        assert_eq!(producer.len(), 1, "got {producer:?}");
        assert_eq!(consumer.len(), 1, "got {consumer:?}");
        assert_eq!(
            producer[0].canonical_id, consumer[0].canonical_id,
            "cross-repo pair diverged"
        );
        assert_eq!(producer[0].canonical_id, "queue::kafka::orders.created");
        assert_eq!(producer[0].role, ContractRole::Consumer);
        assert_eq!(consumer[0].role, ContractRole::Provider);

        // Generic pair: plain-string send/subscribe on unknown receivers —
        // empty qualifier, 0.5 on both sides, same ID.
        let g_prod = extract(Lang::Python, "producer.send('orders.created')");
        let g_cons = extract(Lang::Python, "consumer.subscribe('orders.created')");
        assert_eq!(g_prod.len(), 1, "got {g_prod:?}");
        assert_eq!(g_cons.len(), 1, "got {g_cons:?}");
        assert_eq!(g_prod[0].canonical_id, g_cons[0].canonical_id);
        assert_eq!(g_prod[0].canonical_id, "queue::::orders.created");
        assert_eq!(g_prod[0].confidence, 0.5);
        assert_eq!(g_cons[0].confidence, 0.5);
    }

    #[test]
    fn acceptance_string_literal_topic_confidence_05() {
        // AR-018: framework-shaped constructs score 1.0 even with string
        // literals; only the generic verb+literal tier scores 0.5.
        let framework = extract(
            Lang::JavaScript,
            "ch.sendToQueue('orders.created', Buffer.from(m));",
        );
        assert_eq!(framework[0].confidence, 1.0);
        let heuristic = extract(Lang::JavaScript, "svc.send('orders.created');");
        assert_eq!(heuristic[0].confidence, 0.5);
        // Jobs: the BullMQ add verb is generic, agenda.define is not.
        let agenda = extract(Lang::JavaScript, "agenda.define('email-send', fn);");
        assert_eq!(agenda[0].confidence, 1.0);
        let bullmq = extract(Lang::JavaScript, "emailQueue.add('email-send', d);");
        assert_eq!(bullmq[0].confidence, 0.5);
    }

    #[test]
    fn acceptance_websocket_pairing() {
        let emit = extract(Lang::JavaScript, "io.emit('chat.message', d);");
        let handler = extract(Lang::JavaScript, "socket.on('chat.message', (m) => {});");
        assert_eq!(emit[0].canonical_id, handler[0].canonical_id);
        assert_eq!(emit[0].canonical_id, "websocket::::chat.message");
        assert_eq!(emit[0].role, ContractRole::Provider);
        assert_eq!(handler[0].role, ContractRole::Consumer);
    }

    #[test]
    fn acceptance_job_pairing() {
        let define = extract(Lang::JavaScript, "agenda.define('email-send', fn);");
        let enqueue = extract(Lang::JavaScript, "emailQueue.add('email-send', d);");
        assert_eq!(define[0].canonical_id, enqueue[0].canonical_id);
        assert_eq!(define[0].canonical_id, "job::::email-send");
        assert_eq!(define[0].role, ContractRole::Provider);
        assert_eq!(enqueue[0].role, ContractRole::Consumer);
    }

    #[test]
    fn acceptance_kinds_independently_disableable() {
        // One idiom of every kind in one file; disabling a kind removes
        // exactly its own contracts and leaves the others byte-identical.
        let src = "\
const app = express();
app.get('/v1/users/:id', h);
const db = process.env.DATABASE_URL;
producer.send({ topic: 'orders.created', messages: [m] });
io.emit('chat.message', payload);
agenda.define('email-send', fn);
";
        let baseline = extract(Lang::JavaScript, src);
        assert_eq!(baseline.len(), 5, "got {baseline:?}");
        let mut kinds: Vec<ContractKind> = baseline.iter().map(|c| c.kind).collect();
        kinds.sort_by_key(|k| k.as_str());
        let mut all_kinds = vec![
            ContractKind::Env,
            ContractKind::Http,
            ContractKind::Job,
            ContractKind::Queue,
            ContractKind::WebSocket,
        ];
        all_kinds.sort_by_key(|k| k.as_str());
        assert_eq!(kinds, all_kinds, "expected one contract of each kind");

        type OptionSetter = fn(&mut ContractOptions) -> &mut ContractOptions;
        let cases: [(ContractKind, OptionSetter); 5] = [
            (ContractKind::Http, |o| {
                o.http = false;
                o
            }),
            (ContractKind::Env, |o| {
                o.env = false;
                o
            }),
            (ContractKind::Queue, |o| {
                o.queue = false;
                o
            }),
            (ContractKind::WebSocket, |o| {
                o.websocket = false;
                o
            }),
            (ContractKind::Job, |o| {
                o.job = false;
                o
            }),
        ];
        for (kind, disable) in cases {
            let mut opts = ContractOptions::default();
            disable(&mut opts);
            let cands = extract_with(Lang::JavaScript, src, &opts);
            let gone: Vec<&ContractCandidate> =
                baseline.iter().filter(|c| !cands.contains(c)).collect();
            assert_eq!(gone.len(), 1, "{kind:?}: got {gone:?}");
            assert_eq!(gone[0].kind, kind, "{kind:?}: removed {gone:?}");
            // Survivors are byte-identical to the baseline entries.
            for c in &cands {
                assert!(baseline.contains(c), "{kind:?}: mutated {c:?}");
            }
        }

        // All kinds off: nothing at all.
        let all_off = ContractOptions {
            http: false,
            env: false,
            queue: false,
            websocket: false,
            job: false,
            grpc: false,
            graphql: false,
            openapi: false,
        };
        assert!(extract_with(Lang::JavaScript, src, &all_off).is_empty());
    }

    #[test]
    fn acceptance_cross_kind_disambiguation() {
        // send on a websocket receiver is websocket, never queue.
        let ws = extract(Lang::JavaScript, "socket.send('hello');");
        assert_eq!(ws[0].kind, ContractKind::WebSocket);
        // A path-like literal on an unknown receiver is the ambiguous HTTP
        // tier, never queue.
        let http = extract(Lang::JavaScript, "registry.get('/users');");
        assert_eq!(http.len(), 1);
        assert_eq!(http[0].kind, ContractKind::Http);
        assert_eq!(http[0].confidence, 0.5);
    }

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
        let cands =
            extract_document_contracts(DocumentKind::Proto, src, &ContractOptions::default());
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
        let cands =
            extract_document_contracts(DocumentKind::Proto, src, &ContractOptions::default());
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
        let cands =
            extract_document_contracts(DocumentKind::Proto, src, &ContractOptions::default());
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
        let cands =
            extract_document_contracts(DocumentKind::Proto, src, &ContractOptions::default());
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

    // -- grpc generated/server code (TASK-088, plan 5.1) ------------------------

    #[test]
    fn grpc_java_impl_base_provider() {
        let src = "\
public class UserServiceImpl extends UserServiceGrpc.UserServiceImplBase {
    @Override
    public void getUser(GetUserRequest req, StreamObserver<User> obs) { }
}
";
        let cands = extract(Lang::Java, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "grpc::UserService::getUser");
        assert_eq!(c.kind, ContractKind::Grpc);
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        assert_eq!(c.owning_symbol.as_deref(), Some("getUser"));
        assert_eq!(c.line, 3);
    }

    #[test]
    fn grpc_java_stub_consumers_bound_and_inline() {
        let src = "\
class Client {
    void call(Channel channel) {
        UserServiceGrpc.UserServiceBlockingStub stub = UserServiceGrpc.newBlockingStub(channel);
        stub.getUser(request);
        UserServiceGrpc.newBlockingStub(channel).getUser(request);
    }
}
";
        let cands = extract(Lang::Java, src);
        assert_eq!(cands.len(), 2, "got {cands:?}");
        for c in &cands {
            assert_eq!(c.canonical_id, "grpc::UserService::getUser");
            assert_eq!(c.role, ContractRole::Consumer);
            assert_eq!(c.owning_symbol.as_deref(), Some("call"));
            assert_eq!(c.confidence, CONFIDENCE_FRAMEWORK);
        }
        assert_eq!(cands[0].line, 4);
        assert_eq!(cands[1].line, 5);
    }

    #[test]
    fn grpc_java_add_service_bind_service() {
        let src = "\
class Server {
    void start() {
        ServerBuilder.forPort(50051)
            .addService(UserServiceGrpc.bindService(new UserServiceImpl()))
            .build();
    }
}
";
        let cands = extract(Lang::Java, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "grpc::UserService::*");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.owning_symbol.as_deref(), Some("start"));
    }

    #[test]
    fn grpc_go_register_server_provider() {
        let src = "\
package main

func serve() {
    s := grpc.NewServer()
    pb.RegisterUserServiceServer(s, &server{})
}
";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "grpc::UserService::*");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.owning_symbol.as_deref(), Some("serve"));
    }

    #[test]
    fn grpc_go_client_consumers_bound_and_inline() {
        let src = "\
package main

func call(conn *grpc.ClientConn) error {
    client := pb.NewUserServiceClient(conn)
    _, err := client.GetUser(ctx, req)
    _, err2 := pb.NewUserServiceClient(conn).GetUser(ctx, req)
    return err
}
";
        let cands = extract(Lang::Go, src);
        assert_eq!(cands.len(), 2, "got {cands:?}");
        for c in &cands {
            assert_eq!(c.canonical_id, "grpc::UserService::GetUser");
            assert_eq!(c.role, ContractRole::Consumer);
            assert_eq!(c.owning_symbol.as_deref(), Some("call"));
        }
    }

    #[test]
    fn grpc_rust_tonic_impl_provider_snake_case() {
        let src = "\
use tonic::{Request, Response, Status};

impl user_service_server::UserService for MyService {
    async fn get_user(&self, request: Request<GetUserRequest>)
        -> Result<Response<User>, Status> {
        todo!()
    }
}
";
        let cands = extract(Lang::Rust, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        // snake_case preserved in the ID; the canonical join folds casing.
        assert_eq!(c.canonical_id, "grpc::UserService::get_user");
        assert_eq!(c.role, ContractRole::Provider);
        assert_eq!(c.owning_symbol.as_deref(), Some("get_user"));
        assert_eq!(c.line, 4);
    }

    #[test]
    fn grpc_rust_bound_client_await_consumer() {
        let src = "\
async fn call(channel: Channel) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = UserServiceClient::new(channel);
    let response = client.get_user(request).await?;
    Ok(())
}
";
        let cands = extract(Lang::Rust, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "grpc::UserService::get_user");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.owning_symbol.as_deref(), Some("call"));
    }

    #[test]
    fn grpc_python_servicer_and_add_to_server() {
        let src = "\
import grpc
import user_service_pb2

class UserServiceServicer(user_service_pb2.UserServiceServicer):
    def GetUser(self, request, context):
        return user_service_pb2.User()

def serve():
    server = grpc.server(futures.ThreadPoolExecutor(max_workers=10))
    add_UserServiceServicer_to_server(UserServiceServicer(), server)
";
        let cands = extract(Lang::Python, src);
        assert_eq!(cands.len(), 2, "got {cands:?}");
        let method = find(&cands, "grpc::UserService::GetUser").expect("method provider missing");
        assert_eq!(method.role, ContractRole::Provider);
        assert_eq!(method.owning_symbol.as_deref(), Some("GetUser"));
        assert_eq!(method.line, 5);
        let service = find(&cands, "grpc::UserService::*").expect("service provider missing");
        assert_eq!(service.role, ContractRole::Provider);
        assert_eq!(service.owning_symbol.as_deref(), Some("serve"));
    }

    #[test]
    fn grpc_python_bound_stub_consumer() {
        let src = "\
import user_service_pb2

def call(channel):
    stub = user_service_pb2.UserServiceStub(channel)
    return stub.GetUser(user_service_pb2.GetUserRequest())
";
        let cands = extract(Lang::Python, src);
        assert_eq!(cands.len(), 1, "got {cands:?}");
        let c = &cands[0];
        assert_eq!(c.canonical_id, "grpc::UserService::GetUser");
        assert_eq!(c.role, ContractRole::Consumer);
        assert_eq!(c.owning_symbol.as_deref(), Some("call"));
    }

    #[test]
    fn grpc_js_gated_client_and_add_service() {
        let src = "\
const grpc = require('@grpc/grpc-js');
const client = new user.UserServiceClient(host, creds);
client.getUser(arg, cb);
server.addService(user.UserService.service, { getUser: handler });
";
        let cands = extract(Lang::JavaScript, src);
        assert_eq!(cands.len(), 2, "got {cands:?}");
        let call = find(&cands, "grpc::user.UserService::getUser").expect("consumer missing");
        assert_eq!(call.role, ContractRole::Consumer);
        assert_eq!(call.confidence, CONFIDENCE_FRAMEWORK);
        // Package qualification preserved in the ID (plan 4); the canonical
        // join relaxes it at match time.
        let svc = find(&cands, "grpc::user.UserService::*").expect("service provider missing");
        assert_eq!(svc.role, ContractRole::Provider);
    }

    #[test]
    fn grpc_js_negative_without_grpc_marker() {
        // new XClient alone is not evidence: with no grpc marker in the file
        // nothing is detected.
        let src = "\
const client = new user.UserServiceClient(host, creds);
client.getUser(arg, cb);
server.addService(user.UserService.service, { getUser: handler });
";
        let cands = extract(Lang::JavaScript, src);
        assert!(cands.is_empty(), "got {cands:?}");
    }

    #[test]
    fn grpc_generated_code_disabled_by_option() {
        let opts = ContractOptions {
            grpc: false,
            ..ContractOptions::default()
        };
        let src = "public class UserServiceImpl extends UserServiceGrpc.UserServiceImplBase {\n    public void getUser(GetUserRequest req, StreamObserver<User> obs) { }\n}\n";
        assert!(extract_with(Lang::Java, src, &opts).is_empty());
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
        let cands =
            extract_document_contracts(DocumentKind::Graphql, src, &ContractOptions::default());
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
        let cands =
            extract_document_contracts(DocumentKind::Graphql, src, &ContractOptions::default());
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
        let cands =
            extract_document_contracts(DocumentKind::Graphql, src, &ContractOptions::default());
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
        let cands =
            extract_document_contracts(DocumentKind::OpenApi, src, &ContractOptions::default());
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
        let cands =
            extract_document_contracts(DocumentKind::OpenApi, src, &ContractOptions::default());
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
        let cands =
            extract_document_contracts(DocumentKind::OpenApi, src, &ContractOptions::default());
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
        let cands =
            extract_document_contracts(DocumentKind::OpenApi, src, &ContractOptions::default());
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
                rusqlite::params![r.canonical_id, r.kind, r.role, r.symbol_id, r.file, r.line, r.confidence],
            )
            .unwrap();
        }
        conn
    }

    fn query(
        kind: Option<ContractKind>,
        role: Option<ContractRole>,
        orphans: bool,
    ) -> ContractQuery {
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
}
