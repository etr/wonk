//! Contract extraction: canonical ID normalization plus the `http` and `env`
//! contract kinds (TASK-082, PRD-CTR-REQ-001..004).
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

use std::collections::HashMap;

use tree_sitter::{Node, Tree};

use crate::indexer::Lang;
use crate::types::{ContractCandidate, ContractKind, ContractRole, PathParam};

/// Confidence for framework-recognized constructs (DR-028 / AR-018).
pub const CONFIDENCE_FRAMEWORK: f64 = 1.0;
/// Confidence for string-literal heuristics and role-ambiguous constructs.
pub const CONFIDENCE_HEURISTIC: f64 = 0.5;

/// Extract contract candidates from an already-parsed tree.
///
/// `source` must be the exact byte string the tree was parsed from.
/// One binding pre-pass collects router context (REQ-023), then a single
/// iterative DFS walks the tree and dispatches per-language matchers.
pub fn extract_contracts(tree: &Tree, source: &str, lang: Lang) -> Vec<ContractCandidate> {
    let src = source.as_bytes();
    let ctx = collect_router_context(tree.root_node(), src, lang);
    let mut ex = Extractor {
        src,
        lang,
        ctx,
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
}

impl RouterContext {
    fn is_router_var(&self, name: &str) -> bool {
        self.bindings.contains_key(name) || self.mounts.contains_key(name)
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
            return match method.to_lowercase().as_str() {
                "post" => "post",
                "put" => "put",
                "patch" => "patch",
                "delete" => "delete",
                "head" => "head",
                "options" => "options",
                _ => "get",
            };
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
    out: Vec<ContractCandidate>,
}

/// Receiver names treated as routers even without a tracked binding.
const JS_ROUTER_VARS: &[&str] = &["app", "router", "api", "server", "r"];
/// Verb-named methods that register routes on a router receiver.
const JS_PROVIDER_VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "all"];
/// HTTP client receivers whose verb-named methods are outbound calls.
const JS_CONSUMER_RECEIVERS: &[&str] = &["axios", "got", "http", "https"];
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
                    && JS_PROVIDER_VERBS[..5].contains(&prop)
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
                } else if AMBIGUOUS_VERBS.contains(&prop)
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
            _ => {}
        }
    }

    /// `process.env.NAME` / `import.meta.env.NAME` reads.
    fn js_env_member(&mut self, node: Node) {
        if let Some(parent) = node.parent()
            && parent.kind() == "assignment_expression"
            && parent.child_by_field_name("left").map(|n| n.id()) == Some(node.id())
        {
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
        if let Some(parent) = node.parent()
            && parent.kind() == "assignment_expression"
            && parent.child_by_field_name("left").map(|n| n.id()) == Some(node.id())
        {
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
            "call" => self.py_call(node, prefix),
            "subscript" => self.py_env_subscript(node),
            "assignment" => self.py_assignment(node, prefix),
            _ => {}
        }
        prefix.to_string()
    }

    /// `@app.get('/x')` / `@app.route('/x', methods=['POST'])` decorators.
    fn py_decorated(&mut self, node: Node, prefix: &str) -> String {
        let owning = node
            .child_by_field_name("definition")
            .and_then(|def| def.child_by_field_name("name"))
            .map(|n| node_text(Some(n), self.src).to_string());
        for i in 0..node.child_count() {
            let Some(dec) = node.child(i as u32) else {
                continue;
            };
            if dec.kind() != "decorator" {
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
        if let Some(parent) = node.parent()
            && parent.kind() == "assignment"
            && parent.child_by_field_name("left").map(|n| n.id()) == Some(node.id())
        {
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
            "call" => {
                self.ruby_call(node, prefix);
            }
            "element_reference" => {
                self.ruby_env_ref(node);
            }
            "assignment" => {
                self.ruby_env_assign(node);
            }
            _ => {}
        }
        prefix.to_string()
    }

    /// Sinatra `get '/x' do`, Rails `get '/x', to: …` / `match`, client
    /// libraries with constant receivers, and `ENV.fetch`.
    fn ruby_call(&mut self, node: Node, prefix: &str) {
        let method = node_text(node.child_by_field_name("method"), self.src);
        let receiver = node.child_by_field_name("receiver");
        let args = node.child_by_field_name("arguments");
        let Some(args) = args else { return };
        let first = positional_arg(args, 0);
        match receiver {
            None => {
                let verb = match method {
                    "get" | "post" | "put" | "patch" | "delete" | "match" => method,
                    _ => return,
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
                    _ if RUBY_CONSUMER_RECEIVERS.contains(&recv_text) => {
                        if let (Some(arg), Some(verb)) = (first, ruby_verb(method)) {
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
    }

    /// `ENV['X']` reads.
    fn ruby_env_ref(&mut self, node: Node) {
        if let Some(parent) = node.parent()
            && parent.kind() == "assignment"
            && parent.child_by_field_name("left").map(|n| n.id()) == Some(node.id())
        {
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
                self.rust_call(node, prefix);
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
                        .and_then(|handler| rust_callee_name(handler, self.src))
                        .and_then(ruby_verb)
                        .unwrap_or("ANY");
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Provider,
                        verb,
                        prefix,
                        CONFIDENCE_FRAMEWORK,
                        None,
                    );
                } else if let Some(verb) = ruby_verb(field) {
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
                }
            }
            "scoped_identifier" => {
                let text = node_text(Some(func), self.src);
                match text {
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
                            && let Some(verb) = ruby_verb(text.rsplit("::").next().unwrap_or(""))
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
            "method_declaration" => {
                self.java_method(node, prefix);
            }
            "method_invocation" => {
                self.java_call(node, prefix);
            }
            _ => {}
        }
        prefix.to_string()
    }

    /// Spring mapping annotations and JAX-RS `@Path` + verb markers.
    fn java_method(&mut self, node: Node, prefix: &str) {
        // tree-sitter-java exposes modifiers as a positional child.
        let Some(modifiers) = node.named_child(0).filter(|n| n.kind() == "modifiers") else {
            return;
        };
        let mut verb: Option<String> = None;
        let mut path: Option<Node> = None;
        for i in 0..modifiers.named_child_count() {
            let Some(annot) = modifiers.named_child(i as u32) else {
                continue;
            };
            let name = node_text(annot.child_by_field_name("name"), self.src);
            let args = annot.child_by_field_name("arguments");
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
            let owning = node
                .child_by_field_name("name")
                .map(|n| node_text(Some(n), self.src).to_string());
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

    /// `restTemplate.getForObject(…)`, `System.getenv(…)`.
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
                if let (Some(verb), Some(arg)) = (ruby_verb(name), first) {
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
        let Some(verb) = ruby_verb(name) else { return };
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
        if let Some(parent) = node.parent()
            && parent.kind() == "assignment_expression"
            && parent.child_by_field_name("left").map(|n| n.id()) == Some(node.id())
        {
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
            && let Some(verb) = ruby_verb(name.trim_start_matches("Map").to_lowercase().as_str())
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
                "string_literal" => Some(PathArg::Direct(render_string_node(arg, src, lang))),
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
        if !is_env_name(name) {
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

/// Map a lowercase verb name to its canonical form; `None` if not a verb.
fn ruby_verb(name: &str) -> Option<&'static str> {
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
            return ruby_verb(&method.to_lowercase());
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

/// Stage 3: strip a single leading base-URL interpolation token
/// (`${VAR}`, `$VAR`, `{VAR}`) — only when a `/` follows it, so a lone
/// `/{id}` route remains a parameter route.
fn stage_strip_base_interpolation(path: &str) -> String {
    let body = path.strip_prefix('/').unwrap_or(path);
    let token_len = interpolation_token_len(body);
    match token_len {
        Some(len) if body[len..].starts_with('/') => body[len + 1..].to_string(),
        _ => path.to_string(),
    }
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

#[cfg(test)]
mod extract_test_helpers {
    use super::*;
    use crate::indexer::{Lang, get_parser};

    pub(crate) fn extract(lang: Lang, src: &str) -> Vec<ContractCandidate> {
        let mut parser = get_parser(lang);
        let tree = parser.parse(src, None).expect("parse failed");
        extract_contracts(&tree, src, lang)
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

    // -- walker: JavaScript / TypeScript (step 4) -------------------------------

    use extract_test_helpers::{extract, find};

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
        let c = find(&cands, "http::GET::/{p1}").expect("route not found");
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
}
