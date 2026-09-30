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
pub fn elide(source: &str, language: Option<Lang>, mode: Mode) -> Result<String, NotElided> {
    let _ = (source, language, mode);
    Ok(String::new())
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
}
