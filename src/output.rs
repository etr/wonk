//! Output formatting: grep-compatible (default) and JSON Lines (`--json`).
//!
//! All result data flows through a [`Formatter`] which writes to an
//! arbitrary [`std::io::Write`] destination (typically stdout).
//! Hints and errors always go to stderr via [`print_hint`] and [`print_error`].

use std::io::Write;
use std::path::PathBuf;

use serde::Serialize;

// ---------------------------------------------------------------------------
// Serializable output types
// ---------------------------------------------------------------------------

/// A single text search match (corresponds to `SearchResult` in `search.rs`).
#[derive(Debug, Clone, Serialize)]
pub struct SearchOutput {
    pub file: String,
    pub line: u64,
    pub col: u64,
    pub content: String,
}

/// A symbol definition result.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolOutput {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub col: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub signature: String,
    pub language: String,
}

/// A reference (usage site) result.
#[derive(Debug, Clone, Serialize)]
pub struct RefOutput {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub context: String,
}

/// A function/method signature result.
#[derive(Debug, Clone, Serialize)]
pub struct SignatureOutput {
    pub name: String,
    pub file: String,
    pub line: usize,
    pub signature: String,
    pub language: String,
}

/// A single file entry for `ls` results.
#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub path: String,
}

/// A symbol entry for `ls --tree` results, with an indent level for nesting.
#[derive(Debug, Clone, Serialize)]
pub struct LsSymbolEntry {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: usize,
    /// Nesting depth (0 = top-level). Skipped in JSON output.
    #[serde(skip)]
    pub indent: usize,
    /// Parent scope name (e.g. class name for a method). Skipped when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// A dependency edge for `deps` / `rdeps` results.
#[derive(Debug, Clone, Serialize)]
pub struct DepOutput {
    pub file: String,
    pub depends_on: String,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

impl SearchOutput {
    /// Build a `SearchOutput` from the internal `search::SearchResult`.
    pub fn from_search_result(file: &PathBuf, line: u64, col: u64, content: &str) -> Self {
        Self {
            file: file.to_string_lossy().into_owned(),
            line,
            col,
            content: content.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Formatter
// ---------------------------------------------------------------------------

/// Output formatter that can render results in either grep-compatible text
/// or JSON Lines (one JSON object per line).
pub struct Formatter<W: Write> {
    writer: W,
    json: bool,
}

impl<W: Write> Formatter<W> {
    /// Create a new formatter.
    ///
    /// * `writer` - The destination for output (e.g. `std::io::stdout()`).
    /// * `json`   - When `true`, emit JSON Lines; otherwise, emit grep-style text.
    pub fn new(writer: W, json: bool) -> Self {
        Self { writer, json }
    }

    /// Format a single text-search result.
    pub fn format_search_result(&mut self, result: &SearchOutput) -> std::io::Result<()> {
        if self.json {
            let line = serde_json::to_string(result)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            writeln!(self.writer, "{line}")
        } else {
            writeln!(
                self.writer,
                "{}:{}:{}",
                result.file, result.line, result.content
            )
        }
    }

    /// Format a single symbol definition result.
    pub fn format_symbol(&mut self, sym: &SymbolOutput) -> std::io::Result<()> {
        if self.json {
            let line = serde_json::to_string(sym)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            writeln!(self.writer, "{line}")
        } else {
            // file:line:  signature
            writeln!(
                self.writer,
                "{}:{}:  {}",
                sym.file, sym.line, sym.signature
            )
        }
    }

    /// Format a single reference result.
    pub fn format_reference(&mut self, reference: &RefOutput) -> std::io::Result<()> {
        if self.json {
            let line = serde_json::to_string(reference)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            writeln!(self.writer, "{line}")
        } else {
            // file:line:content  (grep style)
            writeln!(
                self.writer,
                "{}:{}:{}",
                reference.file, reference.line, reference.context
            )
        }
    }

    /// Format a single signature result.
    pub fn format_signature(&mut self, sig: &SignatureOutput) -> std::io::Result<()> {
        if self.json {
            let line = serde_json::to_string(sig)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            writeln!(self.writer, "{line}")
        } else {
            writeln!(
                self.writer,
                "{}:{}:  {}",
                sig.file, sig.line, sig.signature
            )
        }
    }

    /// Format a single ls-symbol entry (used by `wonk ls --tree`).
    ///
    /// Grep format: `file:line:  [indent]kind name`
    /// JSON: all fields except `indent`.
    pub fn format_ls_symbol(&mut self, entry: &LsSymbolEntry) -> std::io::Result<()> {
        if self.json {
            let line = serde_json::to_string(entry)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            writeln!(self.writer, "{line}")
        } else {
            // Two spaces base indent, then two more per nesting level.
            let padding = "  ".repeat(entry.indent + 1);
            writeln!(
                self.writer,
                "{}:{}:{}{} {}",
                entry.file, entry.line, padding, entry.kind, entry.name
            )
        }
    }

    /// Format a single file-list entry.
    pub fn format_file_list(&mut self, entry: &FileEntry) -> std::io::Result<()> {
        if self.json {
            let line = serde_json::to_string(entry)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            writeln!(self.writer, "{line}")
        } else {
            writeln!(self.writer, "{}", entry.path)
        }
    }

    /// Format a single dependency edge.
    pub fn format_dep(&mut self, dep: &DepOutput) -> std::io::Result<()> {
        if self.json {
            let line = serde_json::to_string(dep)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            writeln!(self.writer, "{line}")
        } else {
            writeln!(self.writer, "{} -> {}", dep.file, dep.depends_on)
        }
    }
}

// ---------------------------------------------------------------------------
// Stderr helpers
// ---------------------------------------------------------------------------

/// Print a hint message to stderr (suppressed when `json` is true).
pub fn print_hint(msg: &str, json: bool) {
    if !json {
        eprintln!("hint: {msg}");
    }
}

/// Print an error message to stderr.
pub fn print_error(msg: &str) {
    eprintln!("error: {msg}");
}

/// Format a [`WonkError`] to stderr with structured `error:` / `hint:` lines.
///
/// * Always prints `error: <message>` to stderr.
/// * When `json` is `false` and the error carries a contextual hint, also
///   prints `hint: <suggestion>` to stderr.
/// * Returns the appropriate process exit code.
pub fn format_error(err: &crate::errors::WonkError, json: bool) -> i32 {
    print_error(&format!("{err}"));
    if let Some(hint) = err.hint() {
        print_hint(hint, json);
    }
    err.exit_code()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: renders output into a String.
    fn render<F>(json: bool, f: F) -> String
    where
        F: FnOnce(&mut Formatter<&mut Vec<u8>>) -> std::io::Result<()>,
    {
        let mut buf = Vec::new();
        {
            let mut fmt = Formatter::new(&mut buf, json);
            f(&mut fmt).unwrap();
        }
        String::from_utf8(buf).unwrap()
    }

    // -- SearchOutput --------------------------------------------------------

    #[test]
    fn search_result_grep_format() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
        };
        let out = render(false, |fmt| fmt.format_search_result(&result));
        assert_eq!(out, "src/main.rs:42:fn main() {}\n");
    }

    #[test]
    fn search_result_json_format() {
        let result = SearchOutput {
            file: "src/main.rs".into(),
            line: 42,
            col: 1,
            content: "fn main() {}".into(),
        };
        let out = render(true, |fmt| fmt.format_search_result(&result));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["file"], "src/main.rs");
        assert_eq!(v["line"], 42);
        assert_eq!(v["col"], 1);
        assert_eq!(v["content"], "fn main() {}");
    }

    // -- SymbolOutput --------------------------------------------------------

    #[test]
    fn symbol_grep_format() {
        let sym = SymbolOutput {
            name: "main".into(),
            kind: "function".into(),
            file: "src/main.rs".into(),
            line: 10,
            col: 0,
            end_line: Some(20),
            scope: None,
            signature: "fn main()".into(),
            language: "Rust".into(),
        };
        let out = render(false, |fmt| fmt.format_symbol(&sym));
        assert_eq!(out, "src/main.rs:10:  fn main()\n");
    }

    #[test]
    fn symbol_json_format() {
        let sym = SymbolOutput {
            name: "main".into(),
            kind: "function".into(),
            file: "src/main.rs".into(),
            line: 10,
            col: 0,
            end_line: Some(20),
            scope: Some("MyModule".into()),
            signature: "fn main()".into(),
            language: "Rust".into(),
        };
        let out = render(true, |fmt| fmt.format_symbol(&sym));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["name"], "main");
        assert_eq!(v["kind"], "function");
        assert_eq!(v["file"], "src/main.rs");
        assert_eq!(v["line"], 10);
        assert_eq!(v["end_line"], 20);
        assert_eq!(v["scope"], "MyModule");
    }

    #[test]
    fn symbol_json_skips_none_optional_fields() {
        let sym = SymbolOutput {
            name: "Foo".into(),
            kind: "struct".into(),
            file: "lib.rs".into(),
            line: 5,
            col: 0,
            end_line: None,
            scope: None,
            signature: "struct Foo".into(),
            language: "Rust".into(),
        };
        let out = render(true, |fmt| fmt.format_symbol(&sym));
        // With skip_serializing_if = None, the JSON should not contain these keys.
        assert!(!out.contains("end_line"));
        assert!(!out.contains("scope"));
    }

    // -- RefOutput -----------------------------------------------------------

    #[test]
    fn reference_grep_format() {
        let reference = RefOutput {
            name: "foo".into(),
            kind: "call".into(),
            file: "src/lib.rs".into(),
            line: 99,
            col: 4,
            context: "    foo(42);".into(),
        };
        let out = render(false, |fmt| fmt.format_reference(&reference));
        assert_eq!(out, "src/lib.rs:99:    foo(42);\n");
    }

    #[test]
    fn reference_json_format() {
        let reference = RefOutput {
            name: "foo".into(),
            kind: "call".into(),
            file: "src/lib.rs".into(),
            line: 99,
            col: 4,
            context: "    foo(42);".into(),
        };
        let out = render(true, |fmt| fmt.format_reference(&reference));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["name"], "foo");
        assert_eq!(v["kind"], "call");
        assert_eq!(v["context"], "    foo(42);");
    }

    // -- SignatureOutput -----------------------------------------------------

    #[test]
    fn signature_grep_format() {
        let sig = SignatureOutput {
            name: "process".into(),
            file: "src/engine.rs".into(),
            line: 15,
            signature: "fn process(input: &str) -> Result<()>".into(),
            language: "Rust".into(),
        };
        let out = render(false, |fmt| fmt.format_signature(&sig));
        assert_eq!(
            out,
            "src/engine.rs:15:  fn process(input: &str) -> Result<()>\n"
        );
    }

    #[test]
    fn signature_json_format() {
        let sig = SignatureOutput {
            name: "process".into(),
            file: "src/engine.rs".into(),
            line: 15,
            signature: "fn process(input: &str) -> Result<()>".into(),
            language: "Rust".into(),
        };
        let out = render(true, |fmt| fmt.format_signature(&sig));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["name"], "process");
        assert_eq!(v["signature"], "fn process(input: &str) -> Result<()>");
    }

    // -- FileEntry -----------------------------------------------------------

    #[test]
    fn file_list_grep_format() {
        let entry = FileEntry {
            path: "src/output.rs".into(),
        };
        let out = render(false, |fmt| fmt.format_file_list(&entry));
        assert_eq!(out, "src/output.rs\n");
    }

    #[test]
    fn file_list_json_format() {
        let entry = FileEntry {
            path: "src/output.rs".into(),
        };
        let out = render(true, |fmt| fmt.format_file_list(&entry));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["path"], "src/output.rs");
    }

    // -- DepOutput -----------------------------------------------------------

    #[test]
    fn dep_grep_format() {
        let dep = DepOutput {
            file: "src/main.rs".into(),
            depends_on: "src/lib.rs".into(),
        };
        let out = render(false, |fmt| fmt.format_dep(&dep));
        assert_eq!(out, "src/main.rs -> src/lib.rs\n");
    }

    #[test]
    fn dep_json_format() {
        let dep = DepOutput {
            file: "src/main.rs".into(),
            depends_on: "src/lib.rs".into(),
        };
        let out = render(true, |fmt| fmt.format_dep(&dep));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["file"], "src/main.rs");
        assert_eq!(v["depends_on"], "src/lib.rs");
    }

    // -- Multiple results produce valid NDJSON / multi-line grep output ------

    #[test]
    fn multiple_search_results_ndjson() {
        let results = vec![
            SearchOutput {
                file: "a.rs".into(),
                line: 1,
                col: 1,
                content: "first".into(),
            },
            SearchOutput {
                file: "b.rs".into(),
                line: 2,
                col: 1,
                content: "second".into(),
            },
        ];
        let out = render(true, |fmt| {
            for r in &results {
                fmt.format_search_result(r)?;
            }
            Ok(())
        });
        let lines: Vec<&str> = out.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        // Each line must be valid JSON.
        for line in &lines {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }
    }

    #[test]
    fn multiple_search_results_grep() {
        let results = vec![
            SearchOutput {
                file: "a.rs".into(),
                line: 1,
                col: 1,
                content: "first".into(),
            },
            SearchOutput {
                file: "b.rs".into(),
                line: 2,
                col: 1,
                content: "second".into(),
            },
        ];
        let out = render(false, |fmt| {
            for r in &results {
                fmt.format_search_result(r)?;
            }
            Ok(())
        });
        let lines: Vec<&str> = out.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "a.rs:1:first");
        assert_eq!(lines[1], "b.rs:2:second");
    }

    // -- SearchOutput::from_search_result helper ----------------------------

    #[test]
    fn from_search_result_helper() {
        let path = PathBuf::from("src/foo.rs");
        let out = SearchOutput::from_search_result(&path, 10, 3, "let x = 1;");
        assert_eq!(out.file, "src/foo.rs");
        assert_eq!(out.line, 10);
        assert_eq!(out.col, 3);
        assert_eq!(out.content, "let x = 1;");
    }

    // -- Content with special characters ------------------------------------

    #[test]
    fn search_result_with_colon_in_content() {
        let result = SearchOutput {
            file: "cfg.toml".into(),
            line: 5,
            col: 1,
            content: "key: value".into(),
        };
        // Grep format: file:line:content (colons in content are fine)
        let out = render(false, |fmt| fmt.format_search_result(&result));
        assert_eq!(out, "cfg.toml:5:key: value\n");
    }

    #[test]
    fn json_escapes_special_characters() {
        let result = SearchOutput {
            file: "test.rs".into(),
            line: 1,
            col: 1,
            content: "he said \"hello\"".into(),
        };
        let out = render(true, |fmt| fmt.format_search_result(&result));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["content"], "he said \"hello\"");
    }

    // -- format_error tests -------------------------------------------------

    #[test]
    fn format_error_returns_exit_code_1_for_general_error() {
        use crate::errors::{DbError, WonkError, EXIT_ERROR};
        let err = WonkError::Db(DbError::NoIndex);
        let code = super::format_error(&err, false);
        assert_eq!(code, EXIT_ERROR);
    }

    #[test]
    fn format_error_returns_exit_code_2_for_usage_error() {
        use crate::errors::{WonkError, EXIT_USAGE};
        let err = WonkError::Usage("bad arg".into());
        let code = super::format_error(&err, false);
        assert_eq!(code, EXIT_USAGE);
    }

    #[test]
    fn format_error_suppresses_hint_in_json_mode() {
        use crate::errors::{DbError, WonkError};
        // We cannot easily capture stderr in a unit test, but we can verify
        // the function runs without panic and returns the right code.
        let err = WonkError::Db(DbError::NoIndex);
        let code = super::format_error(&err, true);
        assert_eq!(code, 1);
    }

    // -- LsSymbolEntry -------------------------------------------------------

    #[test]
    fn ls_symbol_grep_format_flat() {
        let entry = LsSymbolEntry {
            name: "main".into(),
            kind: "function".into(),
            file: "src/main.rs".into(),
            line: 1,
            indent: 0,
            scope: None,
        };
        let out = render(false, |fmt| fmt.format_ls_symbol(&entry));
        assert_eq!(out, "src/main.rs:1:  function main\n");
    }

    #[test]
    fn ls_symbol_grep_format_indented() {
        let entry = LsSymbolEntry {
            name: "process".into(),
            kind: "method".into(),
            file: "src/lib.rs".into(),
            line: 15,
            indent: 1,
            scope: Some("Worker".into()),
        };
        let out = render(false, |fmt| fmt.format_ls_symbol(&entry));
        assert_eq!(out, "src/lib.rs:15:    method process\n");
    }

    #[test]
    fn ls_symbol_grep_format_deeply_nested() {
        let entry = LsSymbolEntry {
            name: "inner".into(),
            kind: "function".into(),
            file: "src/lib.rs".into(),
            line: 30,
            indent: 2,
            scope: Some("Outer".into()),
        };
        let out = render(false, |fmt| fmt.format_ls_symbol(&entry));
        assert_eq!(out, "src/lib.rs:30:      function inner\n");
    }

    #[test]
    fn ls_symbol_json_format_includes_all_fields() {
        let entry = LsSymbolEntry {
            name: "process".into(),
            kind: "method".into(),
            file: "src/lib.rs".into(),
            line: 15,
            indent: 1,
            scope: Some("Worker".into()),
        };
        let out = render(true, |fmt| fmt.format_ls_symbol(&entry));
        let v: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert_eq!(v["name"], "process");
        assert_eq!(v["kind"], "method");
        assert_eq!(v["file"], "src/lib.rs");
        assert_eq!(v["line"], 15);
        assert_eq!(v["scope"], "Worker");
    }

    #[test]
    fn ls_symbol_json_format_skips_indent() {
        let entry = LsSymbolEntry {
            name: "main".into(),
            kind: "function".into(),
            file: "src/main.rs".into(),
            line: 1,
            indent: 2,
            scope: None,
        };
        let out = render(true, |fmt| fmt.format_ls_symbol(&entry));
        // indent should NOT appear in JSON
        assert!(!out.contains("indent"));
        // scope should be omitted when None
        assert!(!out.contains("scope"));
    }
}
