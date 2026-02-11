//! Text search engine wrapping the `grep` crate.
//!
//! Provides full-text search across files using:
//! - `grep-regex` for pattern compilation (literal and regex modes)
//! - `grep-searcher` for efficient file searching
//! - `Walker` from the `walker` module for file enumeration
//!
//! Supports case-insensitive matching, regex patterns, and path restriction.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use grep::regex::RegexMatcherBuilder;
use grep::searcher::{BinaryDetection, SearcherBuilder, Searcher, Sink, SinkMatch};

use crate::walker::Walker;

/// A single search hit: one matching line in a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// The file path where the match was found.
    pub file: PathBuf,
    /// 1-based line number within the file.
    pub line: u64,
    /// 1-based column of the first byte of the match within the line (reserved
    /// for future use; currently always 1).
    pub col: u64,
    /// The matched line content (with trailing newline stripped).
    pub content: String,
}

/// Execute a text search over files.
///
/// # Arguments
///
/// * `pattern` - The search pattern (literal string or regex).
/// * `regex` - When `true`, treat `pattern` as a regular expression.
///             When `false`, metacharacters are escaped so the pattern is
///             matched literally.
/// * `ignore_case` - When `true`, match case-insensitively.
/// * `paths` - Directories/files to search. If empty, searches the current
///             directory (`.`).
///
/// # Returns
///
/// A vector of [`SearchResult`] values, one per matching line, in the order
/// they are discovered (file walk order, then line order within each file).
pub fn text_search(
    pattern: &str,
    regex: bool,
    ignore_case: bool,
    paths: &[String],
) -> Result<Vec<SearchResult>> {
    // Build the regex matcher.
    let mut builder = RegexMatcherBuilder::new();
    builder.case_insensitive(ignore_case);
    builder.line_terminator(Some(b'\n'));

    // When regex mode is off, treat the pattern as a fixed string so that
    // metacharacters (e.g. `.`, `*`) are matched literally.
    if !regex {
        builder.fixed_strings(true);
    }

    let matcher = builder
        .build(pattern)
        .with_context(|| format!("invalid search pattern: {pattern}"))?;

    // Build the searcher (line-oriented, with line numbers enabled).
    let mut searcher = SearcherBuilder::new()
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .line_number(true)
        .build();

    // Determine search roots.
    let roots: Vec<&Path> = if paths.is_empty() {
        vec![Path::new(".")]
    } else {
        paths.iter().map(|p| Path::new(p.as_str())).collect()
    };

    let mut results: Vec<SearchResult> = Vec::new();

    for root in roots {
        // Enumerate files using the walker (respects .gitignore, default
        // exclusions, etc.)
        let files = Walker::new(root).collect_paths();

        for file_path in files {
            let mut sink = CollectSink {
                file: file_path.clone(),
                results: &mut results,
            };
            // Silently skip files that cannot be read (e.g. permission errors).
            let _ = searcher.search_path(&matcher, &file_path, &mut sink);
        }
    }

    Ok(results)
}

/// A [`Sink`] implementation that collects matching lines into a
/// `Vec<SearchResult>`.
struct CollectSink<'a> {
    file: PathBuf,
    results: &'a mut Vec<SearchResult>,
}

impl<'a> Sink for CollectSink<'a> {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        let line_number = mat.line_number().unwrap_or(0);

        // Convert matched bytes to a string, stripping trailing newline(s).
        let content = match std::str::from_utf8(mat.bytes()) {
            Ok(s) => s.trim_end_matches(&['\n', '\r'][..]).to_string(),
            Err(_) => {
                // Lossy conversion for non-UTF-8 content.
                String::from_utf8_lossy(mat.bytes())
                    .trim_end_matches(&['\n', '\r'][..])
                    .to_string()
            }
        };

        self.results.push(SearchResult {
            file: self.file.clone(),
            line: line_number,
            col: 1,
            content,
        });

        Ok(true) // continue searching
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Helper: creates a temp directory with test files.
    struct TestDir {
        dir: tempfile::TempDir,
    }

    impl TestDir {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        /// Create a file with the given relative path and content.
        fn create_file(&self, relative: &str, content: &str) {
            let p = self.dir.path().join(relative);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&p, content).unwrap();
        }
    }

    #[test]
    fn literal_search_finds_exact_match() {
        let td = TestDir::new();
        td.create_file("hello.txt", "Hello World\nfoo bar\nHello Again\n");

        let results = text_search(
            "Hello",
            false,
            false,
            &[td.path().to_string_lossy().into_owned()],
        )
        .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].content, "Hello World");
        assert_eq!(results[0].line, 1);
        assert_eq!(results[1].content, "Hello Again");
        assert_eq!(results[1].line, 3);
    }

    #[test]
    fn case_insensitive_search() {
        let td = TestDir::new();
        td.create_file("test.txt", "Hello\nhello\nHELLO\nworld\n");

        let results = text_search(
            "hello",
            false,
            true,
            &[td.path().to_string_lossy().into_owned()],
        )
        .unwrap();

        assert_eq!(results.len(), 3, "expected 3 case-insensitive matches, got: {results:?}");
    }

    #[test]
    fn case_sensitive_search() {
        let td = TestDir::new();
        td.create_file("test.txt", "Hello\nhello\nHELLO\nworld\n");

        let results = text_search(
            "hello",
            false,
            false,
            &[td.path().to_string_lossy().into_owned()],
        )
        .unwrap();

        assert_eq!(results.len(), 1, "expected 1 case-sensitive match, got: {results:?}");
        assert_eq!(results[0].content, "hello");
    }

    #[test]
    fn regex_search() {
        let td = TestDir::new();
        td.create_file("code.rs", "fn main() {}\nfn helper() {}\nlet x = 42;\n");

        let results = text_search(
            r"fn \w+\(\)",
            true,
            false,
            &[td.path().to_string_lossy().into_owned()],
        )
        .unwrap();

        assert_eq!(results.len(), 2, "expected 2 regex matches, got: {results:?}");
        assert!(results[0].content.contains("fn main()"));
        assert!(results[1].content.contains("fn helper()"));
    }

    #[test]
    fn literal_mode_does_not_interpret_regex_metacharacters() {
        let td = TestDir::new();
        td.create_file("meta.txt", "a.b\nacb\na*b\n");

        // In literal mode, "a.b" should only match the literal "a.b", not "acb".
        let results = text_search(
            "a.b",
            false,
            false,
            &[td.path().to_string_lossy().into_owned()],
        )
        .unwrap();

        assert_eq!(results.len(), 1, "expected 1 literal match for 'a.b', got: {results:?}");
        assert_eq!(results[0].content, "a.b");
    }

    #[test]
    fn regex_mode_interprets_dot_as_wildcard() {
        let td = TestDir::new();
        td.create_file("meta.txt", "a.b\nacb\na*b\n");

        // In regex mode, "a.b" matches any single char between a and b, so it
        // matches "a.b", "acb", and "a*b" (all three lines).
        let results = text_search(
            "a.b",
            true,
            false,
            &[td.path().to_string_lossy().into_owned()],
        )
        .unwrap();

        assert_eq!(results.len(), 3, "expected 3 regex matches for 'a.b', got: {results:?}");
    }

    #[test]
    fn path_restriction_works() {
        let td = TestDir::new();
        td.create_file("src/main.rs", "fn main() {}\n");
        td.create_file("tests/test.rs", "fn main() {}\n");

        // Search only in src/
        let src_path = td.path().join("src").to_string_lossy().into_owned();
        let results = text_search("fn main", false, false, &[src_path]).unwrap();

        assert_eq!(results.len(), 1, "expected 1 match restricted to src/, got: {results:?}");
        assert!(
            results[0].file.to_string_lossy().contains("src"),
            "match should be in src/ directory"
        );
    }

    #[test]
    fn multiple_paths() {
        let td = TestDir::new();
        td.create_file("a/file.txt", "needle\n");
        td.create_file("b/file.txt", "needle\n");
        td.create_file("c/file.txt", "needle\n");

        let a_path = td.path().join("a").to_string_lossy().into_owned();
        let b_path = td.path().join("b").to_string_lossy().into_owned();
        let results =
            text_search("needle", false, false, &[a_path, b_path]).unwrap();

        assert_eq!(results.len(), 2, "expected 2 matches from a/ and b/, got: {results:?}");
    }

    #[test]
    fn no_matches_returns_empty_vec() {
        let td = TestDir::new();
        td.create_file("file.txt", "nothing relevant here\n");

        let results = text_search(
            "xyzzy",
            false,
            false,
            &[td.path().to_string_lossy().into_owned()],
        )
        .unwrap();

        assert!(results.is_empty());
    }

    #[test]
    fn search_result_fields() {
        let td = TestDir::new();
        td.create_file("data.txt", "line one\nline two\ntarget line\nline four\n");

        let results = text_search(
            "target",
            false,
            false,
            &[td.path().to_string_lossy().into_owned()],
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert!(r.file.ends_with("data.txt"));
        assert_eq!(r.line, 3);
        assert_eq!(r.col, 1);
        assert_eq!(r.content, "target line");
    }

    #[test]
    fn skips_binary_files() {
        let td = TestDir::new();
        // Create a file with a NUL byte (binary).
        let binary_content = b"match\x00this\n";
        let p = td.path().join("binary.dat");
        fs::write(&p, binary_content).unwrap();

        // Also create a normal text file with the same word.
        td.create_file("text.txt", "match this\n");

        let results = text_search(
            "match",
            false,
            false,
            &[td.path().to_string_lossy().into_owned()],
        )
        .unwrap();

        // Only the text file should match; the binary file is skipped.
        assert_eq!(results.len(), 1, "expected only text file match, got: {results:?}");
        assert!(results[0].file.ends_with("text.txt"));
    }

    #[test]
    fn content_has_no_trailing_newline() {
        let td = TestDir::new();
        td.create_file("file.txt", "hello world\r\ngoodbye\n");

        let results = text_search(
            "hello",
            false,
            false,
            &[td.path().to_string_lossy().into_owned()],
        )
        .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].content, "hello world", "trailing newline should be stripped");
    }
}
