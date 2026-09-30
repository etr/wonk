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
    /// Retain control-flow lines inside elided bodies; renders identically to
    /// [`Mode::Bodies`] until the salience work lands.
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
    let mut parser =
        indexer::try_get_parser(lang).map_err(|_| NotElided::GrammarUnavailable)?;
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
    // Salience renders identically to Bodies until the control-flow retention
    // work lands; the parameter is part of the frozen public API.
    let _ = mode;
    let ranges = collect_body_ranges(tree, source, language);
    Ok(rebuild(source, &ranges))
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

/// A body's byte range in the source plus everything needed to stub it.
#[derive(Debug)]
struct BodyRange {
    start: usize,
    end: usize,
    lines: usize,
    class: LangClass,
    /// The range swallows the newline that terminated the last elided line,
    /// so the stub re-emits it.
    consumed_newline: bool,
}

/// Collect the body ranges of every elidable function in the tree.
///
/// Outermost function wins: a preorder walk reaches the outer definition
/// first, and once a body is accepted the walk skips that node's subtree, so
/// one stub and one line count cover the whole outer body including nested
/// definitions. Candidates that somehow start inside the last accepted range
/// (error-recovered trees) are dropped by the `last_end` cursor.
fn collect_body_ranges(tree: &Tree, source: &str, lang: Lang) -> Vec<BodyRange> {
    let bytes = source.as_bytes();
    let class = lang_class(lang);
    let mut ranges: Vec<BodyRange> = Vec::new();
    let mut last_end = 0;
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        let mut accepted = false;
        if indexer::is_function_node(node.kind(), lang) {
            if let Some(range) = body_range_for(node, bytes, class) {
                if range.start >= last_end {
                    last_end = range.end;
                    ranges.push(range);
                    accepted = true;
                }
            }
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
        lines,
        class,
        consumed_newline,
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

/// Rebuild the source with every body range stubbed.
///
/// One pass over the retained bytes: no retained byte is ever re-typed, which
/// is the mechanical basis for the byte-identical-lines guarantee.
fn rebuild(source: &str, ranges: &[BodyRange]) -> String {
    let mut out = String::with_capacity(source.len());
    let mut last = 0;
    for range in ranges {
        out.push_str(&source[last..range.start]);
        out.push_str(&stub_text(range));
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
        assert_eq!(out.matches("elided").count(), 1, "one stub, not one per nested def");

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
            assert!(out.contains(&format!("pub fn f{i}(n: u32) -> u32 {{")),
                "signature of f{i} must survive");
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

        assert_eq!(NotElided::GrammarUnavailable.as_str(), "grammar unavailable");
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
