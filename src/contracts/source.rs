use super::*;

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
pub(super) struct RouterContext {
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
pub(super) const PREFIX_DEPTH_CAP: usize = 8;

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

    /// Every prefix a router variable is reachable under: ONE PER MOUNT
    /// PATH — a router mounted under several prefixes serves each of
    /// them, not their concatenation (`app.use('/v1', r)` +
    /// `app.use('/v2', r)` + `r.get('/users')` yields /v1/users AND
    /// /v2/users — TASK-082 review debt) — each joined with the
    /// variable's own binding, de-duplicated in first-seen order. A
    /// binding-only variable yields its single binding; an unbound
    /// variable yields one empty prefix so consumers keep today's
    /// behavior.
    fn effective_prefixes(&self, var: &str) -> Vec<String> {
        let binding = self.bindings.get(var).cloned().unwrap_or_default();
        let mut out: Vec<String> = match self.mounts.get(var) {
            Some(mounts) if !mounts.is_empty() => {
                mounts.iter().map(|m| format!("{m}{binding}")).collect()
            }
            _ => vec![binding],
        };
        out.dedup();
        out
    }
}

pub(super) fn collect_router_context(root: Node, src: &[u8], lang: Lang) -> RouterContext {
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
pub(super) fn collect_js_router_facts(node: Node, src: &[u8], ctx: &mut RouterContext) {
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
pub(super) fn collect_py_router_facts(node: Node, src: &[u8], ctx: &mut RouterContext) {
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
pub(super) fn collect_go_router_facts(node: Node, src: &[u8], ctx: &mut RouterContext) {
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
pub(super) fn collect_rust_router_facts(node: Node, src: &[u8], ctx: &mut RouterContext) {
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
pub(super) fn collect_ruby_queue_facts(node: Node, src: &[u8], ctx: &mut RouterContext) {
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
pub(super) struct RpcContext {
    /// Client variable -> service qualifier as written by the developer
    /// (`stub` -> `UserService`, JS `client` -> `user.UserService`).
    stubs: HashMap<String, String>,
}

/// Languages whose generated-stub bindings the RPC pre-pass resolves.
/// C/C++/Ruby/PHP/C# have no gRPC arms, so their files never pay the walk.
pub(super) fn lang_collects_rpc_facts(lang: Lang) -> bool {
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

pub(super) fn collect_rpc_context(root: Node, src: &[u8], lang: Lang) -> RpcContext {
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
pub(super) fn java_grpc_service_from_object(object: Option<Node>, src: &[u8]) -> Option<String> {
    let rest = node_text(object, src).strip_suffix("Grpc")?;
    if rest.rsplit('.').next().unwrap_or("").is_empty() {
        return None;
    }
    Some(rest.to_string())
}

/// `New<S>Client` generated constructor -> `S` (Go: always the bare service).
pub(super) fn go_service_from_new_client(func: Node, src: &[u8]) -> Option<String> {
    let last = node_text(Some(func), src).rsplit('.').next().unwrap_or("");
    let service = last.strip_prefix("New")?.strip_suffix("Client")?;
    if service.is_empty() {
        return None;
    }
    Some(service.to_string())
}

/// `Register<S>Server(...)` registration function -> `S`.
pub(super) fn go_service_from_register(name: &str) -> Option<String> {
    let service = name.strip_prefix("Register")?.strip_suffix("Server")?;
    if service.is_empty() {
        return None;
    }
    Some(service.to_string())
}

/// `UserServiceGrpc.newBlockingStub(channel)` initializer binding (Java).
pub(super) fn collect_java_rpc_facts(node: Node, src: &[u8], ctx: &mut RpcContext) {
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
pub(super) fn collect_go_rpc_facts(node: Node, src: &[u8], ctx: &mut RpcContext) {
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
pub(super) fn collect_rust_rpc_facts(node: Node, src: &[u8], ctx: &mut RpcContext) {
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
pub(super) fn collect_py_rpc_facts(node: Node, src: &[u8], ctx: &mut RpcContext) {
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
pub(super) fn collect_js_rpc_facts(node: Node, src: &[u8], ctx: &mut RpcContext) {
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
pub(super) fn positional_arg<'t>(args: Node<'t>, i: usize) -> Option<Node<'t>> {
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
pub(super) fn first_descendant_of_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
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
pub(super) fn unwrap_argument(node: Node) -> Node {
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
pub(super) fn py_job_decorator<'a>(
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
pub(super) fn kwarg_string(args: Node, name: &str, src: &[u8]) -> Option<String> {
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
pub(super) fn kwarg_string_node<'t>(args: Node<'t>, name: &str, src: &[u8]) -> Option<Node<'t>> {
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
/// `${expr}` at the start, `{expr}` within a path).
pub(super) fn py_string_content(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string" {
        return None;
    }
    let mut out = String::new();
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            match child.kind() {
                "string_content" => out.push_str(node_text(Some(child), src)),
                "interpolation" => {
                    out.push_str(if out.is_empty() { "${" } else { "{" });
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
pub(super) fn py_route_verb(args: Node, src: &[u8]) -> &'static str {
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
pub(super) struct Extractor<'a> {
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
pub(super) const JS_ROUTER_VARS: &[&str] = &["app", "router", "api", "server", "r"];
/// Verb-named methods that register routes on a router receiver.
pub(super) const JS_PROVIDER_VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "all"];
/// Verb-named methods on HTTP client receivers (provider verbs minus the
/// `all` catch-all, which has no consumer meaning).
pub(super) const JS_CONSUMER_VERBS: &[&str] = &["get", "post", "put", "patch", "delete"];
/// HTTP client receivers whose verb-named methods are outbound calls.
pub(super) const JS_CONSUMER_RECEIVERS: &[&str] = &["axios", "got", "http", "https"];
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
pub(super) const WS_RECEIVERS: &[&str] = &["io", "socket", "ws", "wss", "websocket"];
/// Verb-like callee names used by the 0.5 heuristic on unknown receivers.
pub(super) const AMBIGUOUS_VERBS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "any", "all", "request",
];
/// Receiver names treated as Flask/FastAPI routers without a tracked binding.
pub(super) const PY_ROUTER_VARS: &[&str] = &["app", "bp", "router", "api"];
/// HTTP client receivers for Python outbound calls.
pub(super) const PY_CONSUMER_RECEIVERS: &[&str] = &["requests", "httpx", "session", "client"];
/// Verb-named methods on those receivers.
pub(super) const PY_CONSUMER_VERBS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "request",
];

/// Text of a node, or empty string.
pub(super) fn node_text<'a>(node: Option<Node<'a>>, src: &'a [u8]) -> &'a str {
    node.and_then(|n| n.utf8_text(src).ok())
        .filter(|t| !t.is_empty())
        .unwrap_or("")
}

/// Extracted textual content of a call's path argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PathArg {
    /// Sole string literal or rendered template — keeps caller confidence.
    Direct(String),
    /// Concatenation carrying exactly one path-like literal — 0.5 confidence.
    Concat(String),
}

/// Heuristic gate deciding whether a string could be an HTTP path:
/// it starts with `/` (including protocol-relative `//`) or carries a scheme.
pub(super) fn is_path_like(s: &str) -> bool {
    let t = s.trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`'));
    t.starts_with('/') || t.contains("://")
}

/// Append a raw segment to a prefix with exactly one separating slash.
/// Never produces a leading `//` — stage 2 reads that as a protocol-relative
/// URL and would strip the first segment as an authority.
pub(super) fn append_segment(prefix: &mut String, seg: &str) {
    let seg = seg.trim_start_matches('/');
    if prefix.is_empty() || !prefix.ends_with('/') {
        prefix.push('/');
    }
    prefix.push_str(seg);
}

/// Concatenate a raw prefix and a route literal; stage 6 collapses slashes.
pub(super) fn join_raw(prefix: &str, literal: &str) -> String {
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
                    // The noise gate the ambiguous arm already applies
                    // (TASK-082 review debt): `JS_ROUTER_VARS` includes
                    // the single letter `r`, so `redis`-style clients
                    // named `r` used to emit `http::GET::user:1` providers
                    // at full confidence for non-path arguments.
                    && matches!(
                        self.path_arg(arg),
                        Some(PathArg::Direct(ref s)) if is_path_like(s)
                    )
                {
                    for mount_prefix in self.ctx.effective_prefixes(recv) {
                        self.emit_http(
                            node,
                            arg,
                            ContractRole::Provider,
                            prop,
                            &join_raw(prefix, &mount_prefix),
                            CONFIDENCE_FRAMEWORK,
                            None,
                        );
                    }
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
                if let Some((t, raw)) = self.topic_at(first) {
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
                if let Some((t, raw)) = self.topic_at(first) {
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
        // The lowercase receiver is needed only by the three job verbs;
        // every other member call (.map/.then/.push/…) falls through here
        // and must not pay the allocation (TASK-087 review debt).
        let recv_lower = matches!(prop, "schedule" | "define" | "add")
            .then(|| recv.to_lowercase())
            .unwrap_or_default();
        let first = positional_arg(args, 0);
        match prop {
            "schedule" if recv == "cron" || recv_lower.contains("cron") => {
                let Some((name, name_node)) =
                    self.job_name_from_callback(node, positional_arg(args, 1))
                else {
                    return;
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
                if let Some((t, raw)) = self.topic_at(first) {
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
                if let Some((t, raw)) = self.topic_at(first) {
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
                if let Some((t, raw)) = self.topic_at(first) {
                    self.emit_queue(node, t, &raw, cons, "rabbitmq", CONFIDENCE_FRAMEWORK, None);
                }
            }
            "publish" if argc >= 3 => {
                // amqplib: publish(exchange, routingKey, content)
                if let Some((t, raw)) = self.topic_at(positional_arg(args, 1)) {
                    self.emit_queue(node, t, &raw, cons, "rabbitmq", CONFIDENCE_FRAMEWORK, None);
                }
            }
            "consume" => {
                // amqplib: consume(queue, callback) — the only idiomatic
                // `.consume` in JS clients.
                if let Some((t, raw)) = self.topic_at(first) {
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
            for mount_prefix in self.ctx.effective_prefixes(recv) {
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
                        if let Some((t, raw)) = self.topic_at(first) {
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
                        if let Some((t, raw)) = self.topic_at(t) {
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
                        if let Some((t, raw)) = self.topic_at(t) {
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
                        if let Some((t, raw)) = self.topic_at(first) {
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
                        if let Some((t, raw)) = self.topic_at(first) {
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
                        if let Some((t, raw)) = self.topic_at(first) {
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
                        let Some((name, name_node)) =
                            self.job_name_from_callback(node, positional_arg(args, 0))
                        else {
                            return;
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
                        } else if let Some((t, raw)) = self.topic_at(first) {
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
                    for mount_prefix in self.ctx.effective_prefixes(recv) {
                        let pfx = join_raw(prefix, &mount_prefix);
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
                if let Some((t, raw)) = self.topic_at(positional_arg(args, 2)) {
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
                if let Some((t, raw)) = self.topic_at(positional_arg(args, 1)) {
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
                if let Some((t, raw)) = self.topic_at(positional_arg(args, 1)) {
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
                if let Some((t, raw)) = self.topic_at(first) {
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
                if let Some((t, raw)) = self.topic_at(first) {
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
                if let Some((t, raw)) = self.topic_at(first) {
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
                if let Some((t, raw)) = self.topic_at(first) {
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
                if let Some((t, raw)) = self.topic_at(first) {
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
                let Some((name, name_node)) =
                    self.job_name_from_callback(node, positional_arg(args, 1))
                else {
                    return;
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
                    for route_prefix in
                        self.rust_receiver_prefixes(func.child_by_field_name("value"))
                    {
                        self.emit_http(
                            node,
                            arg,
                            ContractRole::Provider,
                            verb,
                            &join_raw(prefix, &route_prefix),
                            CONFIDENCE_FRAMEWORK,
                            None,
                        );
                    }
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

    /// Prefixes carried by a `.route` receiver: inline `web::scope("/p")`
    /// chain segments, plus every prefix of the variable whose
    /// initializer the chain belongs to (Axum `let user_routes =
    /// Router::new()…`) — one per mount (TASK-082 review debt).
    fn rust_receiver_prefixes(&self, recv: Option<Node>) -> Vec<String> {
        // A bound variable (`api.route(…)`) carries its own prefixes.
        if let Some(r) = recv
            && r.kind() == "identifier"
        {
            return self.ctx.effective_prefixes(node_text(Some(r), self.src));
        }
        let mut scope_prefix = String::new();
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
                    &mut scope_prefix,
                    &render_string_node(path_node, self.src, self.lang),
                );
            }
            current = next;
        }
        // Attribute the chain's root to the `let` variable it initializes.
        if let Some(root) = current.or(last)
            && let Some(var) = self.ctx.var_for_range(root.start_byte())
        {
            return self
                .ctx
                .effective_prefixes(var)
                .into_iter()
                .map(|p| format!("{scope_prefix}{p}"))
                .collect();
        }
        vec![scope_prefix]
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
        // are the consumer side). The lowercase is computed only behind the
        // name gate — nearly every member call falls through here (TASK-087
        // review debt).
        let object_lower = matches!(name, "send" | "convertAndSend")
            .then(|| object.to_lowercase())
            .unwrap_or_default();
        if object_lower.contains("kafka") && name == "send" {
            if let Some((t, raw)) = self.topic_at(first) {
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
            if let Some((t, raw)) = self.topic_at(java_last_leading_string(args)) {
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
            if let Some((t, raw)) = self.topic_at(first) {
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
        if let Some(verb) =
            java_client_verb(name).or_else(|| java_generic_client_verb(name, object))
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

    /// The job-name fallback shared by `cron.schedule`,
    /// `scheduler.add_job`, and gocron `AddFunc`/`AddJob` (TASK-087
    /// review debt): the callback identifier names the job, else the
    /// enclosing function does; no name anywhere means no job contract.
    fn job_name_from_callback(
        &self,
        node: Node<'a>,
        cb: Option<Node<'a>>,
    ) -> Option<(String, Node<'a>)> {
        match cb.filter(|cb| cb.kind() == "identifier") {
            Some(id) => Some((node_text(Some(id), self.src).to_string(), id)),
            None => {
                let own = crate::indexer::find_enclosing_function(node, self.src, self.lang)?;
                Some((own, node))
            }
        }
    }

    /// The topic argument and its rendered string, paired once
    /// (TASK-087 review debt): every emitter needs both — the node for
    /// line attribution, the string for the canonical id — and the
    /// hand-rolled pairing drifted across ~25 matcher arms before this
    /// existed.
    fn topic_at(&self, arg: Option<Node<'a>>) -> Option<(Node<'a>, String)> {
        let t = arg?;
        let raw = self.topic_arg(t)?;
        Some((t, raw))
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
pub(super) fn is_assignment_target(node: Node, assign_kind: &str) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == assign_kind
            && parent.child_by_field_name("left").map(|n| n.id()) == Some(node.id())
    })
}

/// Env-var names: non-empty, single token, no whitespace.
pub(super) fn is_env_name(name: &str) -> bool {
    !name.is_empty() && !name.chars().any(char::is_whitespace)
}

/// Uppercase gin/chi verbs accepted as route registrations.
pub(super) const GO_PROVIDER_VERBS: &[&str] = &[
    "GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "Any",
];
/// Ruby HTTP client libraries (constant receivers).
pub(super) const RUBY_CONSUMER_RECEIVERS: &[&str] = &["HTTParty", "RestClient", "Faraday"];
/// Capitalized Go verbs for the single-argument 0.5 heuristic.
pub(super) const GO_AMBIGUOUS_VERBS: &[&str] = &[
    "Get", "Post", "Put", "Patch", "Delete", "Head", "Options", "Any", "Request",
];
/// C# HTTP client receiver names.
pub(super) const CSHARP_CONSUMER_RECEIVERS: &[&str] = &["httpClient", "client", "http"];

/// Map a lowercase verb name to its canonical form; `None` if not a
/// verb. Shared across language walkers (Ruby, Rust, PHP, C#).
pub(super) fn canonical_verb(name: &str) -> Option<&'static str> {
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
pub(super) fn go_client_verb(recv: &str, meth: &str) -> Option<&'static str> {
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
pub(super) fn rust_handler_verb<'t>(handler: Node<'t>, src: &'t [u8]) -> Option<&'t str> {
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
pub(super) fn rust_callee_name<'t>(call: Node<'t>, src: &'t [u8]) -> Option<&'t str> {
    let func = call.child_by_field_name("function")?;
    match func.kind() {
        "identifier" => Some(node_text(Some(func), src)),
        "scoped_identifier" => Some(node_text(func.child_by_field_name("name"), src)),
        "field_expression" => Some(node_text(func.child_by_field_name("field"), src)),
        _ => None,
    }
}

/// Leftmost node of a Rust method chain, or the node itself.
pub(super) fn rust_chain_root<'t>(mut node: Option<Node<'t>>) -> Option<Node<'t>> {
    while let Some(current) = node
        && current.kind() == "field_expression"
    {
        node = current.child_by_field_name("value");
    }
    node
}

/// String literals inside a Rust token tree (attributes / macros).
pub(super) fn tree_strings(tree: Node) -> Vec<Node> {
    (0..tree.named_child_count())
        .filter_map(|i| tree.named_child(i as u32))
        .filter(|n| n.kind() == "string_literal")
        .collect()
}

/// Path argument of a Java annotation: direct string or `value=`/`path=`.
pub(super) fn java_annotation_path<'t>(args: Node<'t>, src: &'t [u8]) -> Option<Node<'t>> {
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
pub(super) fn java_annotation_kwarg_text(args: Node, name: &str, src: &[u8]) -> Option<String> {
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
pub(super) fn java_annotation_kwarg_node<'t>(
    args: Node<'t>,
    name: &str,
    src: &[u8],
) -> Option<Node<'t>> {
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
pub(super) fn java_string_literals(value: Node) -> Vec<Node> {
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
/// The routing-key/topic argument of a Rabbit `convertAndSend`-style
/// call: the last leading string literal before the non-literal payload
/// — and, when the payload is ITSELF a string literal (the leading run
/// reaches the end of the arguments), the second-to-last literal, never
/// the payload (TASK-087 review debt:
/// `convertAndSend("orders.created", "payload")` used to emit the queue
/// under `"payload"`, corrupting the canonical id).
pub(super) fn java_last_leading_string(args: Node) -> Option<Node> {
    let mut leading: Vec<Node> = Vec::new();
    let mut ended_at_non_literal = false;
    for j in 0..args.named_child_count() {
        let Some(arg) = args.named_child(j as u32).map(unwrap_argument) else {
            ended_at_non_literal = true;
            break;
        };
        if arg.kind() == "string_literal" {
            leading.push(arg);
        } else {
            ended_at_non_literal = true;
            break;
        }
    }
    if leading.is_empty() {
        return None;
    }
    if ended_at_non_literal || leading.len() == 1 {
        // The payload is non-literal (or there is only the addressing
        // literal): the last leading literal IS the routing key.
        leading.pop()
    } else {
        // Every argument is a string literal, so the FINAL one is the
        // payload itself — the routing key is the one before it.
        let n = leading.len();
        Some(leading.remove(n - 2))
    }
}

/// Java RestTemplate-style client method verbs. Only the unambiguous
/// `*ForObject`/`*ForEntity` family is receiver-independent.
pub(super) fn java_client_verb(name: &str) -> Option<&'static str> {
    match name {
        "getForObject" | "getForEntity" => Some("get"),
        "postForObject" | "postForEntity" => Some("post"),
        _ => None,
    }
}

/// The generic Java verbs (`put`/`delete`/`exchange`/`execute`) are
/// client calls only on an HTTP-looking receiver — `Map.put` and
/// repository deletes are ubiquitous otherwise, and Java uniquely
/// bypassed the receiver allowlist every other language applies
/// (TASK-082 review debt).
pub(super) fn java_generic_client_verb(name: &str, object: &str) -> Option<&'static str> {
    let verb = match name {
        "put" => "put",
        "delete" => "delete",
        "exchange" | "execute" => "ANY",
        _ => return None,
    };
    let object_lower = object.to_lowercase();
    (object_lower.contains("resttemplate")
        || object_lower.contains("httpclient")
        || object_lower.contains("webclient")
        || object_lower.contains("client"))
    .then_some(verb)
}

/// First entry of the `methods: ['GET']` named argument of a PHP attribute.
pub(super) fn php_attribute_kwarg_verb(params: Node, src: &[u8]) -> Option<&'static str> {
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
pub(super) fn csharp_name_text(node: Option<Node>, src: &[u8]) -> String {
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
pub(super) fn rewrite_aspnet_tokens(raw: String) -> String {
    raw.replace("[controller]", "{controller}")
        .replace("[action]", "{action}")
}

/// C# HttpClient method verbs.
pub(super) fn csharp_client_verb(name: &str) -> Option<&'static str> {
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
pub(super) fn string_content(node: Node, src: &[u8]) -> String {
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
pub(super) fn template_content(node: Node, src: &[u8]) -> String {
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
pub(super) fn concat_literal(
    node: Node,
    src: &[u8],
    lang: Lang,
    leaf_kinds: &[&str],
) -> Option<PathArg> {
    let mut literals = Vec::new();
    if !collect_string_leaves(node, src, lang, leaf_kinds, &mut literals, 0) {
        return None;
    }
    if literals.len() == 1 && is_path_like(&literals[0]) {
        Some(PathArg::Concat(literals.into_iter().next()?))
    } else {
        None
    }
}

/// Deepest concatenation chain we will walk. A left-nested chain of ~10k
/// terms (bundled/minified JS, well under 100KB) overflows the thread
/// stack one frame per binary-expression level, so the walk is bounded
/// (TASK-087 review debt, PRD-CTR threat model).
pub(super) const MAX_CONCAT_DEPTH: u32 = 256;

/// Collect the string literals of a concatenation tree. Returns false
/// when the depth cap tripped — the leaf set is then partial, so the
/// caller must treat the literal as unresolvable rather than act on it.
pub(super) fn collect_string_leaves(
    node: Node,
    src: &[u8],
    lang: Lang,
    leaf_kinds: &[&str],
    out: &mut Vec<String>,
    depth: u32,
) -> bool {
    if depth > MAX_CONCAT_DEPTH {
        return false;
    }
    if leaf_kinds.contains(&node.kind()) {
        out.push(render_string_node(node, src, lang));
        return true;
    }
    if node.kind().starts_with("binary") {
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i as u32)
                && !collect_string_leaves(child, src, lang, leaf_kinds, out, depth + 1)
            {
                return false;
            }
        }
    }
    true
}

/// Render any language's string node to its content. Content children are
/// matched by kind suffix (`*_content` / `*_fragment`), which covers every
/// bundled grammar; interpolations render per-language (`{x}` for Python,
/// `#{x}` for Ruby).
pub(super) fn render_string_node(node: Node, src: &[u8], lang: Lang) -> String {
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
                        out.push_str(if matches!(lang, Lang::Python) && out.is_empty() {
                            "${"
                        } else {
                            "{"
                        });
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
pub(super) fn ruby_string_content(node: Node, src: &[u8]) -> String {
    render_string_node(node, src, Lang::Ruby)
}

/// Whether a class body directly includes Sidekiq::Job / Sidekiq::Worker.
pub(super) fn ruby_includes_sidekiq(body: Node, src: &[u8]) -> bool {
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
pub(super) fn ruby_kwarg_string<'t>(
    args: Node<'t>,
    name: &str,
    src: &[u8],
) -> Option<(Node<'t>, String)> {
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
pub(super) fn broker_token(recv: &str) -> &'static str {
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
pub(super) fn js_chain_root<'t>(mut node: Node<'t>) -> Node<'t> {
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
pub(super) fn js_prop_string_node<'t>(arg: Node<'t>, prop: &str, src: &[u8]) -> Option<Node<'t>> {
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
pub(super) fn js_string_wrap(arg: Node, src: &[u8]) -> Option<String> {
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
pub(super) fn rust_string_wrap(arg: Node, src: &[u8]) -> Option<String> {
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
pub(super) fn go_keyed_topic_literal(arg: Node, src: &[u8]) -> Option<String> {
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

/// The `query`/`mutation`/`subscription` string property of an Apollo-style
/// options object argument (JS).
pub(super) fn js_object_operation_string(obj: Node, src: &[u8]) -> Option<String> {
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
pub(super) fn py_graphql_decorator(
    dec: Node,
    def_name: Option<Node>,
    src: &[u8],
) -> Option<(String, String)> {
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

#[cfg(test)]
#[path = "source_tests.rs"]
mod tests;

#[cfg(test)]
pub(super) mod extract_test_helpers {
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
