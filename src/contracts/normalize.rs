use super::*;

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
pub(super) const TOPIC_SEPARATORS: &[char] = &['.', ':', '/'];

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
pub(super) const PARAM_SENTINEL: char = '\u{1}';

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
pub(super) fn stage_trim(raw: &str) -> String {
    raw.trim_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`'))
        .to_string()
}

/// Stage 2: strip `scheme://authority`, protocol-relative `//authority`,
/// query (`?…`), and fragment (`#…`).
///
/// A `#` immediately followed by `{` is a Ruby interpolation, not a
/// fragment, and is kept for stage 4.
pub(super) fn stage_strip_scheme_authority(path: &str) -> String {
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
pub(super) fn strip_scheme(s: &str) -> Option<&str> {
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

/// Strip a leading explicit interpolation followed by a path separator.
/// Bare braces are literal route parameters, regardless of their spelling.
pub(super) fn stage_strip_base_interpolation(path: &str) -> String {
    let body = path.strip_prefix('/').unwrap_or(path);
    if body.starts_with('{') {
        return path.to_string();
    }
    let Some(len) = interpolation_token_len(body) else {
        return path.to_string();
    };
    if !body[len..].starts_with('/') {
        return path.to_string();
    }
    body[len + 1..].to_string()
}

/// Length of an interpolation token (`${…}`, `{…}`, `$name`) at the start of
/// `s`, if any.
pub(super) fn interpolation_token_len(s: &str) -> Option<usize> {
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
pub(super) fn is_interpolation_name(name: &str) -> bool {
    !name.is_empty() && !name.contains('/')
}

/// Length of a maximal `[A-Za-z_][A-Za-z0-9_]*` run at the start of `s`.
pub(super) fn identifier_len(s: &str) -> usize {
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
pub(super) fn stage_rewrite_placeholders(path: &str) -> (String, Vec<String>) {
    rewrite_with_scan(path, &mut DelimiterScan::new(path))
}

struct DelimiterScan<'a> {
    path: &'a str,
    visits: usize,
    cursors: [usize; 3],
    last_colon: [Option<usize>; 3],
    last_slash: [Option<usize>; 3],
}

impl<'a> DelimiterScan<'a> {
    fn new(path: &'a str) -> Self {
        Self {
            path,
            visits: 0,
            cursors: [0; 3],
            last_colon: [None; 3],
            last_slash: [None; 3],
        }
    }
    fn find(&mut self, start: usize, delimiter: u8) -> Option<usize> {
        let slot = match delimiter {
            b'}' => 0,
            b'>' => 1,
            b')' => 2,
            _ => unreachable!(),
        };
        let bytes = self.path.as_bytes();
        let mut at = self.cursors[slot].max(start);
        while at < bytes.len() {
            self.visits += 1;
            match bytes[at] {
                b':' => self.last_colon[slot] = Some(at),
                b'/' => self.last_slash[slot] = Some(at),
                _ => {}
            }
            if bytes[at] == delimiter {
                self.cursors[slot] = at;
                return Some(at);
            }
            at += 1;
        }
        self.cursors[slot] = bytes.len();
        None
    }
}

fn rewrite_with_scan(path: &str, scan: &mut DelimiterScan<'_>) -> (String, Vec<String>) {
    let bytes = path.as_bytes();
    let mut out = String::with_capacity(path.len());
    let mut names = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &path[i..];
        scan.visits += 1;
        let at_segment_start = i == 0 || bytes[i - 1] == b'/';
        // Rails optional-format group: drop "(.:format)" entirely.
        if rest.starts_with("(.:")
            && let Some(close) = scan.find(i, b')').map(|end| end - i)
        {
            i += close + 1;
            continue;
        }
        let mut consumed = 0usize;
        if at_segment_start && let Some((name, len)) = placeholder_at(rest, i, scan) {
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
fn placeholder_at(
    rest: &str,
    start: usize,
    scan: &mut DelimiterScan<'_>,
) -> Option<(String, usize)> {
    let delimited = if rest.starts_with("${") || rest.starts_with("#{") {
        Some((2, b'}', 0))
    } else if rest.starts_with('{') {
        Some((1, b'}', 0))
    } else if rest.starts_with('<') {
        Some((1, b'>', 1))
    } else {
        None
    };
    if let Some((prefix, delimiter, slot)) = delimited {
        let body_start = start + prefix;
        let close = scan.find(body_start, delimiter)?;
        // The delimiter pass remembers the final colon and slash before the
        // closer. Rejected overlapping tokens therefore validate in O(1).
        // Converters may contain slashes; only the final name must not.
        let name_start = if delimiter == b'>' {
            scan.last_colon[slot]
                .filter(|at| *at >= body_start)
                .map_or(body_start, |at| at + 1)
        } else {
            body_start
        };
        scan.visits += 1;
        if name_start < close && !scan.last_slash[slot].is_some_and(|at| at >= name_start) {
            let name = &scan.path[name_start..close];
            scan.visits += name.len(); // Successful copying also visits input bytes.
            return Some((name.to_owned(), close - start + 1));
        }
        return None;
    }
    if rest.starts_with('$') || rest.starts_with(':') {
        let after = &rest[1..];
        let len = identifier_len(after);
        scan.visits += len + usize::from(len < after.len());
        if len > 0 {
            scan.visits += len;
            return Some((after[..len].to_owned(), 1 + len));
        }
    }
    None
}

/// Stage 5: replace sentinels in order with `{p1}`, `{p2}`, ….
pub(super) fn stage_positional_markers(marked: &str) -> String {
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
pub(super) fn stage_ensure_shape(path: &str) -> Option<String> {
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
mod bounded_scan_tests {
    use super::*;
    #[test]
    fn audit_f22_all_name_scans_are_linear_with_distant_closers() {
        for fragment in ["/<", "/<a", "/<é"] {
            for size in [128, 256, 1024, 16_000, 32_000, 64_000] {
                let input = format!("{}>", fragment.repeat(size));
                let mut scan = DelimiterScan::new(&input);
                let (_, names) = rewrite_with_scan(&input, &mut scan);
                assert!(
                    scan.visits <= 12 * input.len(),
                    "{fragment:?}: {} visits for {} bytes",
                    scan.visits,
                    input.len()
                );
                assert_eq!(names.len(), usize::from(fragment != "/<"));
                println!(
                    "NORMALIZATION_WORK {}",
                    serde_json::json!({"fragment":fragment,"repetitions":size,"input_bytes":input.len(),"scan_visits":scan.visits})
                );
            }
        }
    }
    #[test]
    fn audit_f22_delimiter_visits_are_linear_for_malformed_utf8_paths() {
        for fragment in ["(.:é", "/{é", "/${é", "/<é"] {
            for size in [128, 256, 1024] {
                let input = fragment.repeat(size);
                let mut scan = DelimiterScan::new(&input);
                let (marked, names) = rewrite_with_scan(&input, &mut scan);
                assert!(
                    scan.visits <= 12 * input.len(),
                    "{fragment:?}: {} visits for {} bytes",
                    scan.visits,
                    input.len()
                );
                assert!(marked.len() <= input.len());
                assert!(names.is_empty());
                assert!(normalize_http_path(&input).unwrap().path.len() <= input.len() + 1);
            }
        }
    }
}

#[cfg(test)]
#[path = "normalize_tests.rs"]
mod tests;
