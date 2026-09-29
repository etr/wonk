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
            _ => prefix.to_string(),
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
                    );
                } else if AMBIGUOUS_VERBS.contains(&prop)
                    && !self.ctx.is_router_var(recv)
                    && !JS_ROUTER_VARS.contains(&recv)
                    && !JS_CONSUMER_RECEIVERS.contains(&recv)
                    && let Some(arg) = first_arg
                    && matches!(self.js_path_arg(arg), Some(PathArg::Direct(ref s)) if is_path_like(s))
                {
                    self.emit_http(
                        node,
                        arg,
                        ContractRole::Provider,
                        prop,
                        prefix,
                        CONFIDENCE_HEURISTIC,
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

    /// Extract the textual path of a JS call argument.
    fn js_path_arg(&self, arg: Node) -> Option<PathArg> {
        match arg.kind() {
            "string" => Some(PathArg::Direct(string_content(arg, self.src))),
            "template_string" => Some(PathArg::Direct(template_content(arg, self.src))),
            "binary_expression" if node_text(arg.child(1), self.src) == "+" => {
                concat_literal(arg, self.src)
            }
            _ => None,
        }
    }

    /// Record an HTTP contract from a call site.
    fn emit_http(
        &mut self,
        node: Node,
        arg: Node,
        role: ContractRole,
        verb: &str,
        prefix: &str,
        confidence: f64,
    ) {
        let (raw, confidence) = match self.js_path_arg(arg) {
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
            owning_symbol: crate::indexer::find_enclosing_function(node, self.src, self.lang),
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
/// path-like string literal (PRD-CTR-REQ-004 skip rules).
fn concat_literal(node: Node, src: &[u8]) -> Option<PathArg> {
    let mut literals = Vec::new();
    collect_string_leaves(node, src, &mut literals);
    if literals.len() == 1 && is_path_like(&literals[0]) {
        Some(PathArg::Concat(literals.into_iter().next()?))
    } else {
        None
    }
}

fn collect_string_leaves(node: Node, src: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "string" => out.push(string_content(node, src)),
        "template_string" => out.push(template_content(node, src)),
        "binary_expression" => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i as u32) {
                    collect_string_leaves(child, src, out);
                }
            }
        }
        _ => {}
    }
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

// temporary shape-dump helper appended as a test
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
}
