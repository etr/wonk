use super::*;

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
pub(super) const DOCUMENT_SKIP_FILE_NAMES: &[&str] = &[
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
pub(super) struct ProtoService {
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
pub(super) fn parse_proto_services(content: &str) -> Vec<ProtoService> {
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
pub(super) fn proto_providers(content: &str) -> Vec<ContractCandidate> {
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
pub(super) fn grpc_candidate(
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
pub(super) fn graphql_document_contracts(content: &str) -> Vec<ContractCandidate> {
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
pub(super) type GraphqlSdlField = (String, String, usize);

/// Scan an SDL document for `type Query|Mutation|Subscription {` and
/// `extend type <Root> {` blocks; each field definition inside yields one
/// resolver. Non-root types (`type User`) are ignored. One-line blocks
/// (`type Query { base: String }`) scan their inline field.
pub(super) fn scan_graphql_document(content: &str) -> Vec<GraphqlSdlField> {
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
pub(super) fn sdl_root_opener(line: &str) -> Option<String> {
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
pub(super) fn graphql_candidate(
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
pub(super) struct OpenApiOperation {
    method: String,
    raw_path: String,
    line: usize,
}

/// Whether a document carries a top-level `openapi:`/`swagger:` version key
/// and a `paths:` block. Multi-document YAML (`---` separators) is skipped —
/// extraction, not validation.
pub(super) fn looks_like_openapi(content: &str) -> bool {
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
pub(super) fn key_name(line: &str) -> Option<&str> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') || matches!(t, "{" | "}" | "[" | "]" | "," | "},") {
        return None;
    }
    let head = t.split(':').next()?;
    let name = head.trim_matches(|c| c == '"' || c == '\'').trim();
    if name.is_empty() { None } else { Some(name) }
}

/// Leading-space count of a line (YAML forbids tab indentation).
pub(super) fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// HTTP method tokens legal as OpenAPI path-item keys.
pub(super) const OPENAPI_METHODS: &[&str] = &[
    "get", "put", "post", "delete", "options", "head", "patch", "trace",
];

/// Scan an OpenAPI document's `paths:` block: path keys sit one indent deeper
/// than `paths:`, method keys one level deeper again. Flow-style `{}` maps
/// and multi-document YAML are skipped by construction.
pub(super) fn scan_openapi_document(content: &str) -> Vec<OpenApiOperation> {
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
pub(super) fn openapi_providers(content: &str) -> Vec<ContractCandidate> {
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
pub(super) fn strip_proto_comments(content: &str) -> Vec<String> {
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
pub(super) fn proto_service_opener(line: &str) -> Option<String> {
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
pub(super) fn collect_proto_rpcs(line: &str, line_no: usize, methods: &mut Vec<(String, usize)>) {
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
pub(super) fn brace_delta(line: &str) -> i64 {
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
pub(super) fn parse_graphql_operation(text: &str) -> Option<Vec<(String, String)>> {
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
pub(super) fn skip_balanced(s: &str, open: char, close: char) -> Option<&str> {
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
/// reports `field`), directives (`@include`), spreads (`...F` and
/// `... F`), and inline-fragment type conditions (`... on T`) are
/// skipped.
pub(super) fn top_level_fields(body: &str) -> Vec<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut fields = Vec::new();
    let mut depth = 0i32;
    let mut i = 0usize;
    // The last significant (non-whitespace) character and whether the
    // previous depth-0 token was the `on` of a spread — together they
    // recognize tight/spaced spreads, directives, and inline-fragment
    // type conditions so none of them emit phantom fields (TASK-088
    // review debt).
    let mut last_sig = '\0';
    let mut after_spread_on = false;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '{' => {
                depth += 1;
                last_sig = '{';
                i += 1;
            }
            '}' => {
                depth -= 1;
                if depth < 0 {
                    break;
                }
                last_sig = '}';
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
                last_sig = ')';
            }
            _ if depth == 0 && (c.is_ascii_alphanumeric() || c == '_') => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                let spread = last_sig == '.';
                let directive = last_sig == '@';
                let type_condition = after_spread_on;
                after_spread_on = spread && name == "on";
                last_sig = chars[i - 1];
                if spread || directive || type_condition {
                    continue;
                }
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
                if !c.is_whitespace() {
                    last_sig = c;
                }
                i += 1;
            }
        }
    }
    fields
}

#[cfg(test)]
#[path = "document_tests.rs"]
mod tests;
