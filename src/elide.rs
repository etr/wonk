//! Body elision engine.
//!
//! Replaces function and method bodies in an already-parsed source file with
//! per-language stubs that report the number of elided lines, leaving every
//! retained byte of the original source untouched.

use tree_sitter::{Node, Tree};

use crate::indexer::{self, Lang};

/// Rendering strategy for elided bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Replace every multi-line body with a counted stub.
    Bodies,
    /// Retain control-flow lines verbatim inside elided bodies while the
    /// remaining runs collapse to stubs that report their own counts.
    Salience,
}

/// Reasons elision was not applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotElided {
    /// The caller did not recognize the file's language.
    UnsupportedLanguage,
    /// No Tree-sitter grammar is available for the language.
    GrammarUnavailable,
    /// The source could not be parsed.
    ParseFailure,
}

impl NotElided {
    /// Human-readable signal explaining why elision was skipped.
    pub fn as_str(&self) -> &'static str {
        match self {
            NotElided::UnsupportedLanguage => "unsupported language",
            NotElided::GrammarUnavailable => "grammar unavailable",
            NotElided::ParseFailure => "parse failure",
        }
    }
}

/// Parse `source` and elide function bodies, when the language is known.
///
/// Convenience wrapper for callers holding raw text: an unknown language,
/// missing grammar, or failed parse is reported through [`NotElided`] so the
/// caller can fall back to the original source. All rendering happens in
/// [`elide_tree`], which never parses twice.
pub fn elide(source: &str, language: Option<Lang>, mode: Mode) -> Result<String, NotElided> {
    let lang = language.ok_or(NotElided::UnsupportedLanguage)?;
    let mut parser = indexer::try_get_parser(lang).map_err(|_| NotElided::GrammarUnavailable)?;
    let tree = parser.parse(source, None).ok_or(NotElided::ParseFailure)?;
    elide_tree(&tree, source, lang, mode)
}

/// Elide function bodies in an already-parsed tree — no second parse.
pub fn elide_tree(
    tree: &Tree,
    source: &str,
    language: Lang,
    mode: Mode,
) -> Result<String, NotElided> {
    let ranges = collect_body_ranges(tree, source, language, mode);
    Ok(rebuild(source, &ranges))
}

/// Cross-grammar node kinds that salience mode retains verbatim inside
/// otherwise-elided bodies (PRD-ELIDE-REQ-004, AR-032).
///
/// Deliberately a flat union over every supported grammar: a kind that only
/// exists in grammar A can never appear in grammar B's trees, so the worst
/// case of an over-broad entry is over-retention — the cheap direction under
/// AR-032's risk asymmetry. The per-language probe test pins the names so a
/// misspelling fails CI loudly instead of silently retaining nothing.
const CONTROL_FLOW_KINDS: &[&str] = &[
    // Conditionals.
    "if_statement",
    "if_expression",
    "elif_clause",
    "else_clause",
    "unless",
    "conditional_expression",
    "ternary_expression",
    "if_modifier",
    "unless_modifier",
    // Ruby spells its clauses as bare keyword kinds.
    "if",
    "elsif",
    "else",
    // Loops.
    "for_statement",
    "while_statement",
    "while_expression",
    "for_expression",
    "loop_expression",
    "do_statement",
    "for_in_statement",
    "enhanced_for_statement",
    "for_range_loop",
    "foreach_statement",
    "while_modifier",
    "until_modifier",
    "until",
    "while",
    "for",
    // Switch/match.
    "switch_statement",
    "expression_switch_statement",
    "switch_expression",
    "switch_case",
    "switch_default",
    "switch_label",
    "switch_rule",
    "switch_block_statement_group",
    "switch_section",
    "switch_expression_arm",
    "case_statement",
    "default_statement",
    "case_clause",
    "case",
    "match_expression",
    "match_arm",
    "match_statement",
    "when",
    "expression_case",
    "type_case",
    "default_case",
    "communication_case",
    "type_switch_statement",
    "select_statement",
    // Try/catch.
    "try_statement",
    "catch_clause",
    "finally_clause",
    "except_clause",
    "begin",
    "rescue",
    "ensure",
];

/// `true` when a tree-sitter node kind names a control-flow construct.
///
/// This membership test is the walker's only kind operation: unknown and
/// ERROR kinds answer `false`, so an unrecognized construct degrades to a
/// counted gap, never a failure.
pub fn is_control_flow_kind(kind: &str) -> bool {
    CONTROL_FLOW_KINDS.contains(&kind)
}

/// How a language marks the extent of a body: a brace-delimited block or an
/// indented statement run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LangClass {
    Brace,
    Indent,
}

fn lang_class(lang: Lang) -> LangClass {
    match lang {
        Lang::Python | Lang::Ruby => LangClass::Indent,
        _ => LangClass::Brace,
    }
}

/// Whether control-flow nodes contribute their closing row as well as their
/// header row. Brace bodies close with `}` and Ruby blocks close with `end`,
/// so both stay balanced (REQ-003); Python blocks close by indentation, so
/// only header rows are retained there.
fn retains_closing_rows(lang: Lang) -> bool {
    lang_class(lang) == LangClass::Brace || lang == Lang::Ruby
}

/// A body's byte range in the source plus everything needed to stub it.
#[derive(Debug)]
struct BodyRange {
    start: usize,
    end: usize,
    start_row: usize,
    end_row: usize,
    lines: usize,
    class: LangClass,
    /// The range swallows the newline that terminated the last elided line,
    /// so the stub re-emits it.
    consumed_newline: bool,
    /// File-absolute 0-based rows that salience mode retains verbatim
    /// (control-flow headers, plus closing-brace rows in brace languages).
    /// Empty for [`Mode::Bodies`] and for bodies without control flow — in
    /// both cases salience renders byte-identically to Bodies.
    retained_rows: Vec<usize>,
}

/// Collect the body ranges of every elidable function in the tree.
///
/// Outermost function wins: a preorder walk reaches the outer definition
/// first, and once a body is accepted the walk skips that node's subtree, so
/// one stub and one line count cover the whole outer body including nested
/// definitions. Candidates that somehow start inside the last accepted range
/// (error-recovered trees) are dropped by the `last_end` cursor.
fn collect_body_ranges(tree: &Tree, source: &str, lang: Lang, mode: Mode) -> Vec<BodyRange> {
    let bytes = source.as_bytes();
    let class = lang_class(lang);
    let mut ranges: Vec<BodyRange> = Vec::new();
    let mut last_end = 0;
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        let mut accepted = false;
        if indexer::is_function_node(node.kind(), lang)
            && let Some(mut range) = body_range_for(node, bytes, class)
            && range.start >= last_end
        {
            if mode == Mode::Salience
                && let Some(body) = node.child_by_field_name("body")
            {
                range.retained_rows = collect_retained_rows(body, retains_closing_rows(lang));
            }
            last_end = range.end;
            ranges.push(range);
            accepted = true;
        }
        if !accepted && cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() {
                return ranges;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Rows to retain verbatim inside one accepted body: a preorder walk of the
/// body subtree recording the start row of every control-flow node, plus its
/// end row when [`retains_closing_rows`] says the language's blocks close
/// with a terminator (the closing brace or `end` keeps the retained text
/// balanced, REQ-003). Nested definitions inside the body are not separately
/// stubbed — the outermost body owns the whole span — but control flow inside
/// them still contributes rows. Sorted and deduped on the way out.
fn collect_retained_rows(body: Node, keep_end_rows: bool) -> Vec<usize> {
    let mut rows: Vec<usize> = Vec::new();
    let mut cursor = body.walk();
    loop {
        let node = cursor.node();
        if is_control_flow_kind(node.kind()) {
            rows.push(node.start_position().row);
            if keep_end_rows {
                rows.push(node.end_position().row);
            }
        }
        if cursor.goto_first_child() {
            continue;
        }
        if cursor.goto_next_sibling() {
            continue;
        }
        loop {
            if !cursor.goto_parent() || cursor.node().id() == body.id() {
                rows.sort_unstable();
                rows.dedup();
                return rows;
            }
            if cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// The elidable body range of one function node, or `None` when the body must
/// be left intact.
fn body_range_for(node: Node, bytes: &[u8], class: LangClass) -> Option<BodyRange> {
    let body = node.child_by_field_name("body")?;
    let start = body.start_byte();
    let mut end = body.end_byte();
    if end <= start {
        return None;
    }
    if class == LangClass::Brace && bytes.get(start) != Some(&b'{') {
        // Expression-bodied member (e.g. TS `=> n * 2`): the body field holds
        // an expression, not a block. Leave it intact.
        return None;
    }
    if body.end_position().row == body.start_position().row {
        // A stub would not shorten a single-line body.
        return None;
    }
    if end < bytes.len() && bytes[end] == b'\n' {
        end += 1;
    }
    let lines = body.end_position().row - body.start_position().row + 1;
    let consumed_newline = bytes[end - 1] == b'\n';
    Some(BodyRange {
        start,
        end,
        start_row: body.start_position().row,
        end_row: body.end_position().row,
        lines,
        class,
        consumed_newline,
        retained_rows: Vec::new(),
    })
}

/// The stub replacing a body: a comment-in-braces block for brace languages,
/// an indented comment line for indentation-sensitive ones.
fn stub_text(range: &BodyRange) -> String {
    let mut stub = match range.class {
        LangClass::Brace => format!("{{ /* {} lines elided */ }}", range.lines),
        LangClass::Indent => format!("# {} lines elided", range.lines),
    };
    if range.consumed_newline {
        stub.push('\n');
    }
    stub
}

/// The stub replacing one collapsed run of unretained lines inside a
/// salience-rendered body — no braces: the retained frame rows carry them.
fn comment_stub(class: LangClass, lines: usize) -> String {
    match class {
        LangClass::Brace => format!("/* {lines} lines elided */"),
        LangClass::Indent => format!("# {lines} lines elided"),
    }
}

/// Byte offset of the start of every line, index = 0-based row.
fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, b) in source.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

/// The leading whitespace of the line starting at `line_start`, verbatim.
fn line_indent(source: &str, line_start: usize) -> &str {
    let bytes = source.as_bytes();
    let mut end = line_start;
    while end < bytes.len() && (bytes[end] == b' ' || bytes[end] == b'\t') {
        end += 1;
    }
    &source[line_start..end]
}

/// Render one body range: retained rows verbatim, collapsed runs as counted
/// stub comments. A body with no retained control flow renders exactly as
/// [`stub_text`], so no-control-flow salience is byte-identical to Bodies.
///
/// The rows `start_row..=end_row` partition into retained rows (control-flow
/// rows, plus the brace frame for brace languages) and runs of unretained
/// rows; every original line of the body is either emitted byte-identically
/// or counted in exactly one stub, so the stub counts plus the retained row
/// count always sum to `range.lines`.
fn render_range(source: &str, range: &BodyRange, starts: Option<&[usize]>) -> String {
    if range.retained_rows.is_empty() {
        return stub_text(range);
    }
    let starts = starts.expect("salience rendering needs line starts");
    let mut retained = range.retained_rows.clone();
    if range.class == LangClass::Brace {
        // The brace frame keeps the output balanced (REQ-003): the `{` line
        // and the `}` line always survive.
        retained.push(range.start_row);
        retained.push(range.end_row);
        retained.sort_unstable();
        retained.dedup();
    }
    let line_start = |row: usize| starts.get(row).copied().unwrap_or(source.len());
    let mut out = String::new();
    let mut row = range.start_row;
    while row <= range.end_row {
        if !retained.contains(&row) {
            let run_start = row;
            while row <= range.end_row && !retained.contains(&row) {
                row += 1;
            }
            // The gap before this range already emitted the first row's
            // leading whitespace (a body starts mid-line), so only interior
            // runs re-state their indentation.
            if run_start > range.start_row {
                out.push_str(line_indent(source, line_start(run_start)));
            }
            out.push_str(&comment_stub(range.class, row - run_start));
            out.push('\n');
            continue;
        }
        // Retained row: the first continues from the body start byte (the
        // gap ended there, mid-line); the last runs to the range end, which
        // already swallows a consumed trailing newline.
        let from = if row == range.start_row {
            range.start
        } else {
            line_start(row)
        };
        let to = if row == range.end_row {
            range.end
        } else {
            line_start(row + 1).min(range.end)
        };
        out.push_str(&source[from..to]);
        row += 1;
    }
    out
}

/// Rebuild the source with every body range stubbed.
///
/// One pass over the retained bytes: no retained byte is ever re-typed, which
/// is the mechanical basis for the byte-identical-lines guarantee.
fn rebuild(source: &str, ranges: &[BodyRange]) -> String {
    let mut out = String::with_capacity(source.len());
    // Only salience rendering maps rows to bytes; Bodies never pays the scan.
    let starts = ranges
        .iter()
        .any(|r| !r.retained_rows.is_empty())
        .then(|| line_starts(source));
    let mut last = 0;
    for range in ranges {
        out.push_str(&source[last..range.start]);
        out.push_str(&render_range(source, range, starts.as_deref()));
        last = range.end;
    }
    out.push_str(&source[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::get_parser;

    #[test]
    fn control_flow_kind_probe_matrix() {
        // Pins the real grammar names behind CONTROL_FLOW_KINDS (the union is
        // safe, but a misspelled entry would silently retain nothing): every
        // expected kind must (a) actually appear in the parsed fixture's tree
        // and (b) be recognized by is_control_flow_kind. A grammar rename
        // fails (a); a set typo fails (b). Either way CI catches it loudly.
        let cases: Vec<(Lang, &str, &[&str])> = vec![
            (
                Lang::Rust,
                "fn f(t: u32) {\n    if t == 0 {\n        1;\n    } else {\n        2;\n    }\n    while t < 3 {\n        t += 1;\n    }\n    for i in 0..3 {\n        t += i;\n    }\n    loop {\n        break;\n    }\n    match t {\n        0 => 1,\n        _ => 2,\n    }\n}\n",
                &[
                    "if_expression",
                    "else_clause",
                    "while_expression",
                    "for_expression",
                    "loop_expression",
                    "match_expression",
                    "match_arm",
                ],
            ),
            (
                Lang::Python,
                "def f(t):\n    if t == 0:\n        a = 1\n    elif t == 1:\n        a = 2\n    else:\n        a = 3\n    while t < 3:\n        t += 1\n    for i in range(3):\n        t += i\n    try:\n        b = risky()\n    except ValueError:\n        b = 0\n    finally:\n        c = 1\n    match t:\n        case 0:\n            d = 1\n        case _:\n            d = 2\n",
                &[
                    "if_statement",
                    "elif_clause",
                    "else_clause",
                    "while_statement",
                    "for_statement",
                    "try_statement",
                    "except_clause",
                    "finally_clause",
                    "match_statement",
                    "case_clause",
                ],
            ),
            (
                Lang::JavaScript,
                "function f(t) {\n    if (t === 0) {\n        a();\n    } else {\n        b();\n    }\n    while (t < 3) {\n        t++;\n    }\n    for (let i = 0; i < 3; i++) {\n        t += i;\n    }\n    for (const k in t) {\n        t += k;\n    }\n    for (const v of t) {\n        t += v;\n    }\n    do {\n        t--;\n    } while (t > 0);\n    switch (t) {\n        case 0:\n            break;\n        default:\n            break;\n    }\n    try {\n        c();\n    } catch (e) {\n        d();\n    } finally {\n        e();\n    }\n    const q = t > 0 ? 1 : 2;\n}\n",
                &[
                    "if_statement",
                    "else_clause",
                    "while_statement",
                    "for_statement",
                    "for_in_statement",
                    "do_statement",
                    "switch_statement",
                    "switch_case",
                    "switch_default",
                    "try_statement",
                    "catch_clause",
                    "finally_clause",
                    "ternary_expression",
                ],
            ),
            (
                Lang::TypeScript,
                "function f(t: number): number {\n    if (t === 0) {\n        return 1;\n    } else {\n        return 2;\n    }\n    while (t < 3) {\n        t++;\n    }\n    for (const v of [1, 2]) {\n        t += v;\n    }\n    switch (t) {\n        case 0:\n            return 0;\n        default:\n            return 1;\n    }\n    try {\n        risky();\n    } catch (e) {\n        return 3;\n    }\n    const q = t > 0 ? 1 : 2;\n}\n",
                &[
                    "if_statement",
                    "else_clause",
                    "while_statement",
                    "for_in_statement",
                    "switch_statement",
                    "switch_case",
                    "switch_default",
                    "try_statement",
                    "catch_clause",
                    "ternary_expression",
                ],
            ),
            (
                Lang::Tsx,
                "function f(t: number): number {\n    if (t === 0) {\n        return 1;\n    }\n    while (t < 3) {\n        t++;\n    }\n    for (const v of [1, 2]) {\n        t += v;\n    }\n    return t;\n}\n",
                &["if_statement", "while_statement", "for_in_statement"],
            ),
            (
                Lang::Go,
                "func f(t int) int {\n    if t == 0 {\n        t = 1\n    } else {\n        t = 2\n    }\n    for i := 0; i < 3; i++ {\n        t += i\n    }\n    for _, v := range []int{1, 2} {\n        t += v\n    }\n    switch t {\n    case 0:\n        t = 1\n    default:\n        t = 2\n    }\n    switch v := any(t).(type) {\n    case int:\n        t = v\n    }\n    ch := make(chan int)\n    select {\n    case m := <-ch:\n        t = m\n    default:\n        t = 0\n    }\n    return t\n}\n",
                &[
                    "if_statement",
                    "for_statement",
                    "expression_switch_statement",
                    "expression_case",
                    "default_case",
                    "type_switch_statement",
                    "type_case",
                    "select_statement",
                    "communication_case",
                ],
            ),
            (
                Lang::Java,
                "class D {\n    int f(int t) {\n        if (t == 0) {\n            t = 1;\n        } else {\n            t = 2;\n        }\n        while (t < 3) {\n            t++;\n        }\n        for (int i = 0; i < 3; i++) {\n            t += i;\n        }\n        for (int v : new int[]{1, 2}) {\n            t += v;\n        }\n        do {\n            t--;\n        } while (t > 0);\n        switch (t) {\n            case 0:\n                t = 1;\n                break;\n            default:\n                t = 2;\n        }\n        switch (t) {\n            case 0 -> t = 1;\n            default -> t = 2;\n        }\n        try {\n            risky();\n        } catch (Exception e) {\n            t = 0;\n        } finally {\n            t = 9;\n        }\n        int q = t > 0 ? 1 : 2;\n        return t;\n    }\n}\n",
                &[
                    "if_statement",
                    "while_statement",
                    "for_statement",
                    "enhanced_for_statement",
                    "do_statement",
                    "switch_expression",
                    "switch_block_statement_group",
                    "switch_label",
                    "switch_rule",
                    "try_statement",
                    "catch_clause",
                    "finally_clause",
                    "ternary_expression",
                ],
            ),
            (
                Lang::C,
                "int f(int t) {\n    if (t == 0) {\n        t = 1;\n    } else {\n        t = 2;\n    }\n    while (t < 3) {\n        t++;\n    }\n    for (int i = 0; i < 3; i++) {\n        t += i;\n    }\n    do {\n        t--;\n    } while (t > 0);\n    switch (t) {\n        case 0:\n            t = 1;\n            break;\n        default:\n            t = 2;\n    }\n    int q = t > 0 ? 1 : 2;\n    return t;\n}\n",
                &[
                    "if_statement",
                    "else_clause",
                    "while_statement",
                    "for_statement",
                    "do_statement",
                    "switch_statement",
                    "case_statement",
                    "conditional_expression",
                ],
            ),
            (
                Lang::Cpp,
                "int f(int t) {\n    if (t == 0) {\n        t = 1;\n    } else {\n        t = 2;\n    }\n    while (t < 3) {\n        t++;\n    }\n    for (int i = 0; i < 3; i++) {\n        t += i;\n    }\n    for (int v : {1, 2}) {\n        t += v;\n    }\n    switch (t) {\n        case 0:\n            t = 1;\n            break;\n        default:\n            t = 2;\n    }\n    try {\n        t = risky();\n    } catch (...) {\n        t = 0;\n    }\n    return t;\n}\n",
                &[
                    "if_statement",
                    "else_clause",
                    "while_statement",
                    "for_statement",
                    "for_range_loop",
                    "switch_statement",
                    "case_statement",
                    "try_statement",
                    "catch_clause",
                ],
            ),
            (
                Lang::Ruby,
                "def f(t)\n  if t == 0\n    a = 1\n  elsif t == 1\n    a = 2\n  else\n    a = 3\n  end\n  unless t > 5\n    a = 4\n  end\n  while t < 3\n    t += 1\n  end\n  until t > 9\n    t += 1\n  end\n  t += 1 while t < 20\n  t += 1 until t > 30\n  a = 5 if t > 0\n  a = 6 unless t > 0\n  for k in [1, 2]\n    t += k\n  end\n  case t\n  when 0\n    a = 7\n  when 1, 2\n    a = 8\n  else\n    a = 9\n  end\n  begin\n    risky\n  rescue StandardError => e\n    a = 10\n  ensure\n    a = 11\n  end\n  a = 12\nend\n",
                &[
                    "if",
                    "elsif",
                    "else",
                    "unless",
                    "while",
                    "until",
                    "while_modifier",
                    "until_modifier",
                    "if_modifier",
                    "unless_modifier",
                    "for",
                    "case",
                    "when",
                    "begin",
                    "rescue",
                    "ensure",
                ],
            ),
            (
                Lang::Php,
                "<?php\nfunction f($t) {\n    if ($t === 0) {\n        $t = 1;\n    } else {\n        $t = 2;\n    }\n    while ($t < 3) {\n        $t++;\n    }\n    for ($i = 0; $i < 3; $i++) {\n        $t += $i;\n    }\n    foreach ([1, 2] as $v) {\n        $t += $v;\n    }\n    do {\n        $t--;\n    } while ($t > 0);\n    switch ($t) {\n        case 0:\n            $t = 1;\n            break;\n        default:\n            $t = 2;\n    }\n    try {\n        risky();\n    } catch (Exception $e) {\n        $t = 0;\n    } finally {\n        $t = 9;\n    }\n    $q = $t > 0 ? 1 : 2;\n    return $t;\n}\n",
                &[
                    "if_statement",
                    "else_clause",
                    "while_statement",
                    "for_statement",
                    "foreach_statement",
                    "do_statement",
                    "switch_statement",
                    "case_statement",
                    "default_statement",
                    "try_statement",
                    "catch_clause",
                    "finally_clause",
                    "conditional_expression",
                ],
            ),
            (
                Lang::CSharp,
                "class D {\n    int F(int t) {\n        if (t == 0) {\n            t = 1;\n        } else {\n            t = 2;\n        }\n        while (t < 3) {\n            t++;\n        }\n        for (int i = 0; i < 3; i++) {\n            t += i;\n        }\n        foreach (int v in new[] { 1, 2 }) {\n            t += v;\n        }\n        do {\n            t--;\n        } while (t > 0);\n        switch (t) {\n            case 0:\n                t = 1;\n                break;\n            default:\n                t = 2;\n        }\n        t = t switch {\n            0 => 1,\n            _ => 2,\n        };\n        try {\n            Risky();\n        } catch (Exception e) {\n            t = 0;\n        } finally {\n            t = 9;\n        }\n        var q = t > 0 ? 1 : 2;\n        return t;\n    }\n}\n",
                &[
                    "if_statement",
                    "while_statement",
                    "for_statement",
                    "foreach_statement",
                    "do_statement",
                    "switch_statement",
                    "switch_section",
                    "switch_expression",
                    "switch_expression_arm",
                    "try_statement",
                    "catch_clause",
                    "finally_clause",
                    "conditional_expression",
                ],
            ),
        ];
        assert_eq!(cases.len(), 12);

        for (lang, src, expected_kinds) in cases {
            let mut parser = get_parser(lang);
            let tree = parser.parse(src, None).unwrap();
            let mut kinds: Vec<&str> = Vec::new();
            let mut cursor = tree.walk();
            'walk: loop {
                let kind = cursor.node().kind();
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
                if cursor.goto_first_child() {
                    continue;
                }
                if cursor.goto_next_sibling() {
                    continue;
                }
                loop {
                    if !cursor.goto_parent() {
                        break 'walk;
                    }
                    if cursor.goto_next_sibling() {
                        continue 'walk;
                    }
                }
            }
            for expected in expected_kinds {
                assert!(
                    kinds.contains(expected),
                    "{lang:?}: grammar no longer produces node kind {expected:?} — \
                     update CONTROL_FLOW_KINDS and this probe"
                );
                assert!(
                    is_control_flow_kind(expected),
                    "{lang:?}: CONTROL_FLOW_KINDS does not recognize {expected:?} — \
                     a misspelling would silently retain nothing"
                );
            }
        }
    }

    #[test]
    fn unrecognized_and_error_kinds_never_fail() {
        // The walker's only kind operation is a membership test: unknown and
        // ERROR kinds answer false, so garbage input degrades to counted gaps
        // (or no elision at all), never a panic or an Err.
        let garbage = "@@ {{{ def !!! ???\n\x00total nonsense )))\n";
        for lang in [
            Lang::TypeScript,
            Lang::Tsx,
            Lang::JavaScript,
            Lang::Python,
            Lang::Rust,
            Lang::Go,
            Lang::Java,
            Lang::C,
            Lang::Cpp,
            Lang::Ruby,
            Lang::Php,
            Lang::CSharp,
        ] {
            let mut parser = get_parser(lang);
            if let Some(tree) = parser.parse(garbage, None) {
                for mode in [Mode::Bodies, Mode::Salience] {
                    let out = elide_tree(&tree, garbage, lang, mode).unwrap();
                    let stub_total: usize = out
                        .lines()
                        .filter(|l| l.contains("lines elided"))
                        .map(|l| {
                            l.split_whitespace()
                                .find_map(|t| t.parse::<usize>().ok())
                                .unwrap_or(0)
                        })
                        .sum();
                    let retained = out.lines().filter(|l| !l.contains("lines elided")).count();
                    assert!(
                        stub_total + retained <= garbage.lines().count() + 1,
                        "{lang:?} {mode:?}: garbage input must not invent lines"
                    );
                }
            }
        }
        assert!(!is_control_flow_kind("ERROR"));
        assert!(!is_control_flow_kind("totally_unknown_kind"));
        assert!(!is_control_flow_kind(""));
    }

    #[test]
    fn rust_bodies_replaced_with_counted_brace_stub() {
        let src = "\
// module docs
use std::collections::HashMap;

fn alpha() {
    let m = HashMap::new();
    let n = m.len() + 1;
    n
}

fn beta(x: u32) -> u32 {
    let y = x + 1;
    y * 2
}
";
        let mut parser = get_parser(Lang::Rust);
        let tree = parser.parse(src, None).unwrap();
        let out = elide_tree(&tree, src, Lang::Rust, Mode::Bodies).unwrap();
        let expected = "\
// module docs
use std::collections::HashMap;

fn alpha() { /* 5 lines elided */ }

fn beta(x: u32) -> u32 { /* 4 lines elided */ }
";
        assert_eq!(out, expected);
    }

    #[test]
    fn python_bodies_replaced_with_indented_comment_stub() {
        let src = "\
# module docs
import os
import sys

def alpha(flag):
    a = os.getcwd()
    b = len(sys.argv)
    return a if flag else b

def beta(x):
    return x + 1
";
        let mut parser = get_parser(Lang::Python);
        let tree = parser.parse(src, None).unwrap();
        let out = elide_tree(&tree, src, Lang::Python, Mode::Bodies).unwrap();
        let expected = "\
# module docs
import os
import sys

def alpha(flag):
    # 3 lines elided

def beta(x):
    return x + 1
";
        assert_eq!(out, expected);
    }

    #[test]
    fn ruby_methods_elided() {
        let src = "\
# module docs
require 'set'

def outer(items)
  s = Set.new(items)
  total = s.sum { |x| x * 2 }
  total += 1
  total
end

class Calculator
  def compute(x)
    y = x + 1
    z = y * 3
    z - 2
  end

  def self.build
    c = Calculator.new
    c.compute(1)
    c
  end
end
";
        let mut parser = get_parser(Lang::Ruby);
        let tree = parser.parse(src, None).unwrap();
        let out = elide_tree(&tree, src, Lang::Ruby, Mode::Bodies).unwrap();
        let expected = "\
# module docs
require 'set'

def outer(items)
  # 4 lines elided
end

class Calculator
  def compute(x)
    # 3 lines elided
  end

  def self.build
    # 3 lines elided
  end
end
";
        assert_eq!(out, expected);
    }

    #[test]
    fn nested_functions_collapse_to_outermost_stub() {
        let py = "\
# module docs

def outer(n):
    def inner(k):
        t = k + 1
        return t
    x = inner(n)
    return x * 2

def solo():
    return 7
";
        let mut parser = get_parser(Lang::Python);
        let tree = parser.parse(py, None).unwrap();
        let out = elide_tree(&tree, py, Lang::Python, Mode::Bodies).unwrap();
        let expected = "\
# module docs

def outer(n):
    # 5 lines elided

def solo():
    return 7
";
        assert_eq!(out, expected);
        assert_eq!(
            out.matches("elided").count(),
            1,
            "one stub, not one per nested def"
        );

        let js = "\
// module docs
import fs from 'fs';

function outer(n) {
  function inner(k) {
    const t = k + 1;
    return t;
  }
  const x = inner(n);
  return x * 2;
}

function solo() {
  return 7;
}
";
        let mut parser = get_parser(Lang::JavaScript);
        let tree = parser.parse(js, None).unwrap();
        let out = elide_tree(&tree, js, Lang::JavaScript, Mode::Bodies).unwrap();
        let expected = "\
// module docs
import fs from 'fs';

function outer(n) { /* 8 lines elided */ }

function solo() { /* 3 lines elided */ }
";
        assert_eq!(out, expected);
        assert!(!out.contains("inner"));
        // outer's stub covers its whole span including the nested def
        assert!(out.contains("/* 8 lines elided */"));
    }

    fn elided(src: &str, lang: Lang) -> String {
        let mut parser = get_parser(lang);
        let tree = parser.parse(src, None).unwrap();
        elide_tree(&tree, src, lang, Mode::Bodies).unwrap()
    }

    #[test]
    fn bodyless_members_left_intact() {
        let java = "\
// module docs
package com.example;

public interface Greeter {
    String greet(String name);
    int count();
}
";
        let out = elided(java, Lang::Java);
        assert_eq!(out, java, "interface signatures carry no body to elide");

        let cs = "\
// module docs
namespace Shapes;

public abstract class Shape {
    public abstract double Area();
}
";
        let out = elided(cs, Lang::CSharp);
        assert_eq!(out, cs, "abstract members carry no body to elide");

        let go = "\
// module docs
package main

func Add(x, y int64) int64
";
        let out = elided(go, Lang::Go);
        assert_eq!(
            out, go,
            "a bodyless declaration leaves the signature as the whole declaration"
        );
    }

    #[test]
    fn single_line_bodies_left_intact() {
        let rust = "\
// module docs
fn empty() {}
fn beta() -> u32 { 7 }
";
        let out = elided(rust, Lang::Rust);
        assert_eq!(out, rust, "a stub would lengthen single-line bodies");

        let python = "\
# module docs
def b(): return 42
";
        let out = elided(python, Lang::Python);
        assert_eq!(out, python);
    }

    #[test]
    fn expression_arrow_left_intact_and_block_arrow_elided() {
        let ts = "\
// module docs
const double = (n: number): number => n * 2;
const pick = (n: number): number =>
  n > 0
    ? n * 2
    : 0 - n;
const run = (n: number): number => {
  const m = n + 1;
  return m * 2;
};
";
        let out = elided(ts, Lang::TypeScript);
        let expected = "\
// module docs
const double = (n: number): number => n * 2;
const pick = (n: number): number =>
  n > 0
    ? n * 2
    : 0 - n;
const run = (n: number): number => { /* 4 lines elided */ };
";
        assert_eq!(out, expected);
    }

    #[test]
    fn matrix_retained_lines_byte_identical_all_languages() {
        struct Case {
            lang: Lang,
            src: &'static str,
            imports: &'static [&'static str],
            signatures: &'static [&'static str],
            expected_stubs: usize,
        }
        let cases = [
            Case {
                lang: Lang::Rust,
                src: "\
// module docs
use std::fmt;

pub fn alpha(n: u32) -> u32 {
    let m = n + 1;
    let k = m * 2;
    k + 3
}

fn beta() {
    let a = 1;
    let b = 2;
    let c = a + b;
    let _ = c;
}

fn tiny() -> u32 { 7 }
",
                imports: &["use std::fmt;"],
                signatures: &[
                    "pub fn alpha(n: u32) -> u32",
                    "fn beta()",
                    "fn tiny() -> u32 { 7 }",
                ],
                expected_stubs: 2,
            },
            Case {
                lang: Lang::TypeScript,
                src: "\
// module docs
import { readFile } from 'fs';

export function alpha(n: number): number {
  const m = n + 1;
  const k = m * 2;
  return k + 3;
}

function beta(): void {
  const a = 1;
  const b = 2;
  console.log(a + b);
}

function tiny(): number { return 7; }
",
                imports: &["import { readFile } from 'fs';"],
                signatures: &[
                    "export function alpha(n: number): number",
                    "function beta(): void",
                    "function tiny(): number { return 7; }",
                ],
                expected_stubs: 2,
            },
            Case {
                lang: Lang::Tsx,
                src: "\
// module docs
import React from 'react';

export function alpha(n: number): number {
  const m = n + 1;
  const k = m * 2;
  return k + 3;
}

export function Beta(): React.ReactNode {
  const items = [1, 2, 3];
  const list = items.map((i) => <li key={i}>{i}</li>);
  return <ul>{list}</ul>;
}

function tiny(): number { return 7; }
",
                imports: &["import React from 'react';"],
                signatures: &[
                    "export function alpha(n: number): number",
                    "export function Beta(): React.ReactNode",
                    "function tiny(): number { return 7; }",
                ],
                expected_stubs: 2,
            },
            Case {
                lang: Lang::JavaScript,
                src: "\
// module docs
import fs from 'fs';

export function alpha(n) {
  const m = n + 1;
  const k = m * 2;
  return k + 3;
}

function beta() {
  const a = 1;
  const b = 2;
  console.log(a + b);
}

function tiny() { return 7; }
",
                imports: &["import fs from 'fs';"],
                signatures: &[
                    "export function alpha(n)",
                    "function beta()",
                    "function tiny() { return 7; }",
                ],
                expected_stubs: 2,
            },
            Case {
                lang: Lang::Python,
                src: "\
# module docs
import os
import sys

def alpha(flag):
    a = os.getcwd()
    b = len(sys.argv)
    return a if flag else b

def beta(n):
    total = 0
    for i in range(n):
        total += i
    return total

def tiny():
    return 7
",
                imports: &["import os", "import sys"],
                signatures: &["def alpha(flag):", "def beta(n):", "def tiny():"],
                expected_stubs: 2,
            },
            Case {
                lang: Lang::Go,
                src: "\
// module docs
package main

import \"fmt\"

func Alpha(n int) int {
\tm := n + 1
\tk := m * 2
\treturn k + 3
}

func beta(w string) {
\ta := len(w)
\tb := a * 3
\tfmt.Println(b)
}

func tiny() int { return 7 }
",
                imports: &["import \"fmt\""],
                signatures: &[
                    "func Alpha(n int) int",
                    "func beta(w string)",
                    "func tiny() int { return 7 }",
                ],
                expected_stubs: 2,
            },
            Case {
                lang: Lang::Java,
                src: "\
// module docs
package com.example;

import java.util.List;

public class Demo {
    public int alpha(int n) {
        int m = n + 1;
        int k = m * 2;
        return k + 3;
    }

    public void beta(String w) {
        int a = w.length();
        int b = a * 3;
        System.out.println(b);
    }

    public int tiny() { return 7; }
}
",
                imports: &["import java.util.List;"],
                signatures: &[
                    "public int alpha(int n)",
                    "public void beta(String w)",
                    "public int tiny() { return 7; }",
                ],
                expected_stubs: 2,
            },
            Case {
                lang: Lang::C,
                src: "\
/* module docs */
#include <stdio.h>
#include <string.h>

int alpha(int n) {
    int m = n + 1;
    int k = m * 2;
    return k + 3;
}

void beta(const char *w) {
    size_t a = strlen(w);
    size_t b = a * 3;
    printf(\"%zu\\n\", b);
}

int tiny(void) { return 7; }
",
                imports: &["#include <stdio.h>", "#include <string.h>"],
                signatures: &[
                    "int alpha(int n)",
                    "void beta(const char *w)",
                    "int tiny(void) { return 7; }",
                ],
                expected_stubs: 2,
            },
            Case {
                lang: Lang::Cpp,
                src: "\
// module docs
#include <iostream>
#include <vector>

int alpha(int n) {
    int m = n + 1;
    int k = m * 2;
    return k + 3;
}

void beta(const std::vector<int>& v) {
    int total = 0;
    for (int x : v) {
        total += x;
    }
    std::cout << total << \"\\n\";
}

int tiny() { return 7; }
",
                imports: &["#include <iostream>", "#include <vector>"],
                signatures: &[
                    "int alpha(int n)",
                    "void beta(const std::vector<int>& v)",
                    "int tiny() { return 7; }",
                ],
                expected_stubs: 2,
            },
            Case {
                lang: Lang::Ruby,
                src: "\
# module docs
require 'set'
require 'json'

def alpha(items)
  s = Set.new(items)
  total = s.size * 2
  total + 1
end

def beta(w)
  parsed = JSON.parse(w)
  size = parsed.length
  puts size
end

def tiny
  7
end
",
                imports: &["require 'set'", "require 'json'"],
                signatures: &["def alpha(items)", "def beta(w)", "def tiny"],
                expected_stubs: 2,
            },
            Case {
                lang: Lang::Php,
                src: "\
<?php
// module docs
require_once 'set.php';
use Foo\\Bar;

function alpha($n) {
    $m = $n + 1;
    $k = $m * 2;
    return $k + 3;
}

function beta($w) {
    $a = strlen($w);
    $b = $a * 3;
    echo $b . \"\\n\";
}

function tiny() { return 7; }
",
                imports: &["require_once 'set.php';", "use Foo\\Bar;"],
                signatures: &[
                    "function alpha($n)",
                    "function beta($w)",
                    "function tiny() { return 7; }",
                ],
                expected_stubs: 2,
            },
            Case {
                lang: Lang::CSharp,
                src: "\
// module docs
using System;
using System.Collections.Generic;

public class Demo {
    public int Alpha(int n) {
        int m = n + 1;
        int k = m * 2;
        return k + 3;
    }

    public void Beta(string w) {
        int a = w.Length;
        int b = a * 3;
        Console.WriteLine(b);
    }

    public int Tiny() { return 7; }
}
",
                imports: &["using System;", "using System.Collections.Generic;"],
                signatures: &[
                    "public int Alpha(int n)",
                    "public void Beta(string w)",
                    "public int Tiny() { return 7; }",
                ],
                expected_stubs: 2,
            },
        ];
        assert_eq!(cases.len(), 12);

        for case in cases {
            let lang = case.lang;
            let out = elided(case.src, lang);
            let out_lines: Vec<&str> = out.lines().collect();
            let in_lines: Vec<&str> = case.src.lines().collect();

            let stub_lines: Vec<&str> = out_lines
                .iter()
                .copied()
                .filter(|l| l.contains("lines elided"))
                .collect();
            assert_eq!(
                stub_lines.len(),
                case.expected_stubs,
                "{lang:?}: every multi-line body must be stubbed and counted, no more, no fewer"
            );

            let indent = matches!(lang, Lang::Python | Lang::Ruby);
            let format_ok = stub_lines.iter().any(|l| {
                if indent {
                    let t = l.trim();
                    t.starts_with("# ") && t.ends_with(" elided")
                } else {
                    l.contains("{ /* ") && l.contains(" lines elided */ }")
                }
            });
            assert!(format_ok, "{lang:?}: no stub in the language's format");

            let stripped: Vec<&str> = out_lines
                .iter()
                .copied()
                .filter(|l| !l.contains("lines elided"))
                .collect();
            let mut remaining = in_lines.iter();
            for line in &stripped {
                assert!(
                    remaining.any(|original| original == line),
                    "{lang:?}: retained line {line:?} is not an in-order byte-identical original line"
                );
            }

            for import in case.imports {
                assert!(
                    stripped.contains(import),
                    "{lang:?}: import {import:?} not retained byte-identical"
                );
            }

            for sig in case.signatures {
                assert!(
                    out_lines.iter().any(|l| l.trim_start().starts_with(sig)),
                    "{lang:?}: signature {sig:?} not preserved on its line"
                );
            }

            let mut parser = get_parser(lang);
            let reparsed = parser.parse(&out, None).unwrap();
            assert!(
                !reparsed.root_node().has_error(),
                "{lang:?}: elided output must re-parse cleanly under its own grammar:\n{out}"
            );
        }
    }

    #[test]
    fn body_heavy_majority_reduction() {
        let mut src = String::from("// module docs\nuse std::fmt;\n\n");
        for i in 0..30 {
            src.push_str(&format!(
                "pub fn f{i}(n: u32) -> u32 {{\n    let a = n + {i};\n    let b = a * 2;\n    let c = b + 1;\n    let d = c * 3;\n    let e = d + 2;\n    let f = e * 4;\n    let g = f + 5;\n    let h = g * 6;\n    let j = h + 7;\n    let k = j * 8;\n    let m = k + 9;\n    m\n}}\n\n"
            ));
        }
        let out = elided(&src, Lang::Rust);
        let in_lines = src.lines().count();
        let out_lines = out.lines().count();
        assert!(out.contains("use std::fmt;"));
        for i in 0..30 {
            assert!(
                out.contains(&format!("pub fn f{i}(n: u32) -> u32 {{")),
                "signature of f{i} must survive"
            );
        }
        assert!(
            out_lines * 2 < in_lines,
            "expected a majority reduction: {out_lines} of {in_lines} lines remain"
        );
        let ratio = out_lines as f64 / in_lines as f64;
        println!("body-heavy reduction: {out_lines}/{in_lines} lines retained (ratio {ratio:.3})");
    }

    #[test]
    fn unsupported_language_signal_and_byte_identical_fallback() {
        let src = "fn alpha() {\n    1\n}\n";
        let err = elide(src, None, Mode::Bodies).unwrap_err();
        assert_eq!(err, NotElided::UnsupportedLanguage);
        assert_eq!(err.as_str(), "unsupported language");
        // Caller fail-soft idiom: the original bytes come back untouched.
        let fallback = elide(src, None, Mode::Bodies).unwrap_or_else(|_| src.to_string());
        assert_eq!(fallback, src);

        assert_eq!(
            NotElided::GrammarUnavailable.as_str(),
            "grammar unavailable"
        );
        assert_eq!(NotElided::ParseFailure.as_str(), "parse failure");

        // All twelve supported languages route through elide without a signal.
        let py = "def alpha(flag):\n    a = 1\n    b = 2\n    return b\n";
        assert!(elide(py, Some(Lang::Python), Mode::Bodies).is_ok());
    }

    #[test]
    fn elision_under_20ms_on_parsed_tree() {
        let bodies: Vec<(Lang, String)> = [Lang::Rust, Lang::TypeScript, Lang::Python]
            .iter()
            .map(|&lang| {
                let mut src = String::new();
                for i in 0..400 {
                    src.push_str(&match lang {
                        Lang::Rust => format!(
                            "pub fn f{i}(n: u32) -> u32 {{\n    let a = n + {i};\n    let b = a * 2;\n    let c = b + 1;\n    let d = c * 3;\n    let e = d + 2;\n    let f = e * 4;\n    let g = f + 5;\n    let h = g * 6;\n    let j = h + 7;\n    let k = j * 8;\n    let m = k + 9;\n    m\n}}\n\n"
                        ),
                        Lang::TypeScript => format!(
                            "export function f{i}(n: number): number {{\n  const a = n + {i};\n  const b = a * 2;\n  const c = b + 1;\n  const d = c * 3;\n  const e = d + 2;\n  const f = e * 4;\n  const g = f + 5;\n  const h = g * 6;\n  const j = h + 7;\n  const k = j * 8;\n  const m = k + 9;\n  return m;\n}}\n\n"
                        ),
                        _ => format!(
                            "def f{i}(n):\n    a = n + {i}\n    b = a * 2\n    c = b + 1\n    d = c * 3\n    e = d + 2\n    f = e * 4\n    g = f + 5\n    h = g * 6\n    j = h + 7\n    k = j * 8\n    m = k + 9\n    return m\n\n"
                        ),
                    });
                }
                (lang, src)
            })
            .collect();

        for (lang, src) in &bodies {
            let mut parser = get_parser(*lang);
            let tree = parser.parse(src, None).unwrap();
            // The budget covers the rebuild over the already-parsed tree, so
            // the parse happens in setup and one warm-up precedes the timing.
            let _ = elide_tree(&tree, src, *lang, Mode::Bodies).unwrap();
            let start = std::time::Instant::now();
            let out = elide_tree(&tree, src, *lang, Mode::Bodies).unwrap();
            let elapsed = start.elapsed();
            println!(
                "{lang:?}: {} bytes, {} lines -> {} lines in {:.2?}",
                src.len(),
                src.lines().count(),
                out.lines().count(),
                elapsed
            );
            assert!(
                elapsed < std::time::Duration::from_millis(20),
                "{lang:?}: elision took {elapsed:?}, budget is 20ms"
            );
        }
    }

    #[test]
    fn salience_retains_control_flow_skeleton() {
        // Golden worked example (plan TASK-091 §2.2): a Rust body with a
        // while/if-else pair. Control-flow header and closing-brace lines
        // survive verbatim (Rust's else_clause keeps `} else {` too); every
        // collapsed run reports its own line count.
        let src = "\
fn process(items: &[u32]) -> u32 {
    let setup = 1;
    let mut total = 0;
    while total < 10 {
        if total % 2 == 0 {
            total += 1;
        } else {
            total += 2;
        }
    }
    let tail = 3;
    total + tail
}
";
        let mut parser = get_parser(Lang::Rust);
        let tree = parser.parse(src, None).unwrap();
        let out = elide_tree(&tree, src, Lang::Rust, Mode::Salience).unwrap();
        let expected = "\
fn process(items: &[u32]) -> u32 {
    /* 2 lines elided */
    while total < 10 {
        if total % 2 == 0 {
            /* 1 lines elided */
        } else {
            /* 1 lines elided */
        }
    }
    /* 2 lines elided */
}
";
        assert_eq!(out, expected);
    }

    #[test]
    fn salience_gaps_report_their_own_counts() {
        // Partition invariant (PRD-ELIDE-REQ-005): every original line is
        // either retained verbatim or counted in exactly one stub, so the
        // stub counts plus the retained lines always sum to the original
        // line count — retained lines can never read as the whole body.
        let cases: Vec<(Lang, &str)> = vec![
            (
                Lang::Rust,
                "fn f(t: u32) {\n    let a = 1;\n    if t == 0 {\n        a = 2;\n    }\n    let b = 3;\n    while t < 9 {\n        t += 1;\n    }\n}\n",
            ),
            (
                Lang::Python,
                "def f(t):\n    a = 1\n    for i in range(3):\n        a += i\n    b = 2\n    return a\n",
            ),
            (
                Lang::Go,
                "func f(t int) {\n    a := 1\n    if t == 0 {\n        a = 2\n    }\n    b := 3\n}\n",
            ),
        ];
        for (lang, src) in cases {
            let mut parser = get_parser(lang);
            let tree = parser.parse(src, None).unwrap();
            for mode in [Mode::Bodies, Mode::Salience] {
                let out = elide_tree(&tree, src, lang, mode).unwrap();
                let stub_total: usize = out
                    .lines()
                    .filter(|l| l.contains("lines elided"))
                    .map(|l| {
                        l.split_whitespace()
                            .find_map(|t| t.parse::<usize>().ok())
                            .unwrap_or_else(|| panic!("stub without count: {l:?}"))
                    })
                    .sum();
                let retained = out.lines().filter(|l| !l.contains("lines elided")).count();
                assert_eq!(
                    stub_total + retained,
                    src.lines().count(),
                    "{lang:?} {mode:?}: stub counts + retained lines must cover every original line"
                );
            }
        }
    }

    #[test]
    fn salience_retained_lines_byte_identical_matrix() {
        struct Case {
            lang: Lang,
            src: &'static str,
        }
        let cases = [
            Case {
                lang: Lang::Rust,
                src: "fn f(t: u32) -> u32 {\n    let a = 1;\n    while t < 9 {\n        t += 1;\n    }\n    if t == 0 {\n        2\n    } else {\n        3\n    }\n}\n",
            },
            Case {
                lang: Lang::TypeScript,
                src: "function f(t: number): number {\n  const a = 1;\n  for (const v of [1, 2]) {\n    t += v;\n  }\n  if (t === 0) {\n    return 2;\n  }\n  return 3;\n}\n",
            },
            Case {
                lang: Lang::Tsx,
                src: "export function f(t: number): number {\n  const a = 1;\n  if (t === 0) {\n    return 2;\n  }\n  for (const v of [1, 2]) {\n    t += v;\n  }\n  return t;\n}\n",
            },
            Case {
                lang: Lang::JavaScript,
                src: "function f(t) {\n  const a = 1;\n  switch (t) {\n    case 0:\n      return 1;\n    default:\n      return 2;\n  }\n}\n",
            },
            Case {
                lang: Lang::Python,
                src: "def f(t):\n    a = 1\n    for i in range(3):\n        a += i\n    if t > 0:\n        a = 2\n    return a\n",
            },
            Case {
                lang: Lang::Go,
                src: "func f(t int) int {\n    a := 1\n    if t == 0 {\n        a = 2\n    }\n    for i := 0; i < 3; i++ {\n        a += i\n    }\n    return a\n}\n",
            },
            Case {
                lang: Lang::Java,
                src: "class D {\n    int f(int t) {\n        int a = 1;\n        if (t == 0) {\n            a = 2;\n        }\n        for (int v : new int[]{1, 2}) {\n            a += v;\n        }\n        return a;\n    }\n}\n",
            },
            Case {
                lang: Lang::C,
                src: "int f(int t) {\n    int a = 1;\n    if (t == 0) {\n        a = 2;\n    }\n    for (int i = 0; i < 3; i++) {\n        a += i;\n    }\n    return a;\n}\n",
            },
            Case {
                lang: Lang::Cpp,
                src: "int f(int t) {\n    int a = 1;\n    for (int v : {1, 2}) {\n        a += v;\n    }\n    switch (t) {\n        case 0:\n            a = 2;\n            break;\n        default:\n            a = 3;\n    }\n    return a;\n}\n",
            },
            Case {
                lang: Lang::Ruby,
                src: "def f(t)\n  a = 1\n  if t == 0\n    a = 2\n  end\n  while a < 9\n    a += 1\n  end\n  a\nend\n",
            },
            Case {
                lang: Lang::Php,
                src: "<?php\nfunction f($t) {\n    $a = 1;\n    foreach ([1, 2] as $v) {\n        $a += $v;\n    }\n    if ($t === 0) {\n        $a = 2;\n    }\n    return $a;\n}\n",
            },
            Case {
                lang: Lang::CSharp,
                src: "class D {\n    int F(int t) {\n        int a = 1;\n        foreach (int v in new[] { 1, 2 }) {\n            a += v;\n        }\n        if (t == 0) {\n            a = 2;\n        }\n        return a;\n    }\n}\n",
            },
        ];
        assert_eq!(cases.len(), 12);

        for case in cases {
            let lang = case.lang;
            let mut parser = get_parser(lang);
            let tree = parser.parse(case.src, None).unwrap();
            for mode in [Mode::Bodies, Mode::Salience] {
                let out = elide_tree(&tree, case.src, lang, mode).unwrap();
                let stripped: Vec<&str> = out
                    .lines()
                    .filter(|l| !l.contains("lines elided"))
                    .collect();
                assert!(
                    stripped.len() < case.src.lines().count() || mode == Mode::Bodies,
                    "{lang:?} {mode:?}: expected some elision"
                );
                let mut remaining = case.src.lines();
                for line in &stripped {
                    assert!(
                        remaining.any(|original| original == *line),
                        "{lang:?} {mode:?}: retained line {line:?} is not an in-order byte-identical original line"
                    );
                }
            }
        }
    }

    #[test]
    fn salience_indent_languages_retain_headers() {
        let src = "def f(n):\n    a = 1\n    if n > 0:\n        b = 2\n    elif n < 0:\n        b = 3\n    else:\n        b = 4\n    for i in range(3):\n        b += i\n    while b < 9:\n        b += 1\n    match n:\n        case 0:\n            b = 0\n        case _:\n            b = 1\n    return b\n";
        let mut parser = get_parser(Lang::Python);
        let tree = parser.parse(src, None).unwrap();
        let out = elide_tree(&tree, src, Lang::Python, Mode::Salience).unwrap();
        let expected = "\
def f(n):
    # 1 lines elided
    if n > 0:
        # 1 lines elided
    elif n < 0:
        # 1 lines elided
    else:
        # 1 lines elided
    for i in range(3):
        # 1 lines elided
    while b < 9:
        # 1 lines elided
    match n:
        case 0:
            # 1 lines elided
        case _:
            # 2 lines elided
";
        assert_eq!(out, expected);

        let ruby = "def f(t)\n  a = 1\n  if t == 0\n    a = 2\n  elsif t == 1\n    a = 3\n  else\n    a = 4\n  end\n  unless a > 5\n    a = 6\n  end\n  a\nend\n";
        let mut parser = get_parser(Lang::Ruby);
        let tree = parser.parse(ruby, None).unwrap();
        let out = elide_tree(&tree, ruby, Lang::Ruby, Mode::Salience).unwrap();
        // Ruby blocks close with `end`, so control-flow nodes keep their end
        // rows too (retains_closing_rows) — the skeleton stays balanced. The
        // `else` node's extent includes its arm, so the arm's last line
        // survives with it: over-retention, AR-032's cheap direction.
        let expected = "\
def f(t)
  # 1 lines elided
  if t == 0
    # 1 lines elided
  elsif t == 1
    # 1 lines elided
  else
    a = 4
  end
  unless a > 5
    # 1 lines elided
  end
  # 1 lines elided
end
";
        assert_eq!(out, expected);
    }

    #[test]
    fn salience_match_and_switch_arms() {
        let rust = "fn f(t: u32) -> u32 {\n    let x = 1;\n    match t {\n        0 => 1,\n        1 => {\n            2\n        }\n        _ => 3,\n    }\n    let y = 2;\n    y\n}\n";
        let mut parser = get_parser(Lang::Rust);
        let tree = parser.parse(rust, None).unwrap();
        let out = elide_tree(&tree, rust, Lang::Rust, Mode::Salience).unwrap();
        let expected = "\
fn f(t: u32) -> u32 {
    /* 1 lines elided */
    match t {
        0 => 1,
        1 => {
            /* 1 lines elided */
        }
        _ => 3,
    }
    /* 2 lines elided */
}
";
        assert_eq!(out, expected);

        let js = "function pick(t) {\n    let r = 0;\n    switch (t) {\n        case 0:\n            r = 1;\n            break;\n        default:\n            r = 2;\n    }\n    return r;\n}\n";
        let mut parser = get_parser(Lang::JavaScript);
        let tree = parser.parse(js, None).unwrap();
        let out = elide_tree(&tree, js, Lang::JavaScript, Mode::Salience).unwrap();
        let expected = "\
function pick(t) {
    /* 1 lines elided */
    switch (t) {
        case 0:
            /* 1 lines elided */
            break;
        default:
            r = 2;
    }
    /* 1 lines elided */
}
";
        assert_eq!(out, expected);
    }

    #[test]
    fn salience_without_control_flow_equals_bodies() {
        let src = "\
// module docs
use std::fmt;

fn alpha(n: u32) -> u32 {
    let m = n + 1;
    let k = m * 2;
    k + 3
}
";
        let mut parser = get_parser(Lang::Rust);
        let tree = parser.parse(src, None).unwrap();
        let bodies = elide_tree(&tree, src, Lang::Rust, Mode::Bodies).unwrap();
        let salience = elide_tree(&tree, src, Lang::Rust, Mode::Salience).unwrap();
        assert_eq!(bodies, salience);
    }

    #[test]
    fn elide_parses_source_and_matches_elide_tree() {
        let src = "\
// module docs
use std::fmt;

pub fn alpha(n: u32) -> u32 {
    let m = n + 1;
    let k = m * 2;
    k + 3
}
";
        let via_elide = elide(src, Some(Lang::Rust), Mode::Bodies).unwrap();
        let mut parser = get_parser(Lang::Rust);
        let tree = parser.parse(src, None).unwrap();
        let via_tree = elide_tree(&tree, src, Lang::Rust, Mode::Bodies).unwrap();
        assert_eq!(via_elide, via_tree);
    }
}
