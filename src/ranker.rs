//! Result classification, ranking, and deduplication engine.
//!
//! Classifies each search result line into a category (Definition, CallSite,
//! Import, Comment, Test, Other) using index metadata and path/content
//! heuristics, then ranks results by relevance tier, deduplicates re-exported
//! symbols, and groups results by category for display with section headers.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use rusqlite::Connection;

use crate::search::SearchResult;

/// Category assigned to a classified search result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResultCategory {
    /// The line is a symbol definition (function, class, etc.).
    Definition,
    /// The line is a call site or usage reference.
    CallSite,
    /// The line is an import/require/use statement.
    Import,
    /// The line is inside a comment.
    Comment,
    /// The file is a test file (detected by path heuristics).
    Test,
    /// Unclassified.
    Other,
}

impl ResultCategory {
    /// Return a numeric tier for sort ordering.
    ///
    /// Lower values appear first in ranked output:
    /// Definition(0) > CallSite(1) > Import(2) > Other(3) > Comment(4) > Test(5)
    pub fn tier(&self) -> u8 {
        match self {
            ResultCategory::Definition => 0,
            ResultCategory::CallSite => 1,
            ResultCategory::Import => 2,
            ResultCategory::Other => 3,
            ResultCategory::Comment => 4,
            ResultCategory::Test => 5,
        }
    }
}

impl PartialOrd for ResultCategory {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ResultCategory {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.tier().cmp(&other.tier())
    }
}

impl std::fmt::Display for ResultCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ResultCategory::Definition => "definition",
            ResultCategory::CallSite => "call_site",
            ResultCategory::Import => "import",
            ResultCategory::Comment => "comment",
            ResultCategory::Test => "test",
            ResultCategory::Other => "other",
        };
        write!(f, "{s}")
    }
}

/// A search result paired with its classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedResult {
    /// The original search result.
    pub result: SearchResult,
    /// The assigned category.
    pub category: ResultCategory,
    /// Optional annotation (e.g. "(+3 other locations)") added by dedup.
    pub annotation: Option<String>,
}

impl PartialOrd for ClassifiedResult {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ClassifiedResult {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.category
            .cmp(&other.category)
            .then_with(|| self.result.file.cmp(&other.result.file))
            .then_with(|| self.result.line.cmp(&other.result.line))
    }
}

// ---------------------------------------------------------------------------
// Index lookup helper
// ---------------------------------------------------------------------------

/// Preloaded maps of file -> {line numbers} from the DB for O(1) lookups.
/// Only loads data for files present in the result set.
struct IndexLookup {
    definitions: HashMap<String, HashSet<i64>>,
    references: HashMap<String, HashSet<i64>>,
}

impl IndexLookup {
    /// Bulk-query the symbols and references tables, filtered to only the
    /// files present in the result set. Two SQL queries are executed.
    fn load(conn: &Connection, files: &HashSet<&str>) -> Self {
        if files.is_empty() {
            return IndexLookup {
                definitions: HashMap::new(),
                references: HashMap::new(),
            };
        }

        let placeholders: Vec<&str> = files.iter().map(|_| "?").collect();
        let in_clause = placeholders.join(", ");
        let file_params: Vec<&str> = files.iter().copied().collect();

        let definitions = Self::query_map(
            conn,
            &format!("SELECT file, line FROM symbols WHERE file IN ({in_clause})"),
            &file_params,
        );
        let references = Self::query_map(
            conn,
            &format!("SELECT file, line FROM \"references\" WHERE file IN ({in_clause})"),
            &file_params,
        );
        IndexLookup { definitions, references }
    }

    fn query_map(conn: &Connection, sql: &str, params: &[&str]) -> HashMap<String, HashSet<i64>> {
        let mut map: HashMap<String, HashSet<i64>> = HashMap::new();
        if let Ok(mut stmt) = conn.prepare(sql) {
            let boxed_params: Vec<Box<dyn rusqlite::types::ToSql>> =
                params.iter().map(|s| Box::new(s.to_string()) as Box<dyn rusqlite::types::ToSql>).collect();
            let param_refs: Vec<&dyn rusqlite::types::ToSql> = boxed_params.iter().map(|b| b.as_ref()).collect();
            if let Ok(rows) = stmt.query_map(param_refs.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            }) {
                for row in rows.flatten() {
                    map.entry(row.0).or_default().insert(row.1);
                }
            }
        }
        map
    }

    fn is_definition(&self, file: &str, line: i64) -> bool {
        self.definitions.get(file).is_some_and(|lines| lines.contains(&line))
    }

    fn is_reference(&self, file: &str, line: i64) -> bool {
        self.references.get(file).is_some_and(|lines| lines.contains(&line))
    }
}

// ---------------------------------------------------------------------------
// Content heuristics
// ---------------------------------------------------------------------------

/// Compiled regex for import/require/use patterns across languages.
///
/// Matches (after optional leading whitespace):
/// - `use ...`          (Rust, Go)
/// - `import ...`       (JS/TS, Python, Java, Go)
/// - `from ... import`  (Python)
/// - `#include ...`     (C/C++)
/// - `require ...`      (Ruby, JS/Node)
static IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?x)
        ^\s*(?:
            use\s              |
            import\s           |
            from\s             |
            \#include\s*[<"]   |
            require\s*[('"]
        )"#
    ).expect("import regex should compile")
});

/// Check if a line is an import/require/use/include statement.
pub fn is_import_line(line: &str) -> bool {
    IMPORT_RE.is_match(line)
}

/// Check if a line is (heuristically) a comment-only line.
///
/// Looks at the trimmed content to see if it starts with a comment leader.
/// Lines with code followed by a trailing comment are NOT classified as
/// comments.
pub fn is_comment_line(line: &str) -> bool {
    let trimmed = line.trim_start();

    // Guard: #include is not a comment
    if trimmed.starts_with("#include") {
        return false;
    }

    // Note: starts_with("//") also matches "///" doc comments
    trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("*/")
        || trimmed.starts_with('#')
}

/// Check if a file path matches test directory/filename heuristics.
///
/// Matches:
/// - `test/`, `tests/`, `__tests__/` in path components
/// - `*_test.*` filename suffix (e.g. `foo_test.go`)
/// - `*.test.*` filename (e.g. `foo.test.ts`)
/// - `*.spec.*` filename (e.g. `foo.spec.js`)
pub fn is_test_file(path: &Path) -> bool {
    // Directory-based heuristics: check path components
    for component in path.components() {
        let s = component.as_os_str().to_string_lossy();
        if s == "test" || s == "tests" || s == "__tests__" {
            return true;
        }
    }

    // Filename-based heuristics
    if let Some(file_name) = path.file_name() {
        let name = file_name.to_string_lossy();
        // Split on the first dot to get stem vs rest
        if let Some(stem) = path.file_stem() {
            let stem_str = stem.to_string_lossy();
            // *_test.* pattern (e.g. foo_test.go)
            if stem_str.ends_with("_test") {
                return true;
            }
            // *.test.* or *.spec.* pattern (e.g. foo.test.ts, foo.spec.js)
            // Need to check if ".test." or ".spec." appears in filename
            if name.contains(".test.") || name.contains(".spec.") {
                return true;
            }
        }
    }

    false
}

// ---------------------------------------------------------------------------
// Classification engine
// ---------------------------------------------------------------------------

/// Classify a batch of search results.
///
/// When a DB connection is provided, symbol definitions and references are
/// looked up via bulk queries (2 SQL queries total). Content and path
/// heuristics are always applied.
///
/// **Priority order:** Test > Definition > Import > Comment > CallSite > Other
///
/// Import is checked before Comment so that `#include` lines are not
/// false-positived as comments.
pub fn classify_results(
    results: &[SearchResult],
    conn: Option<&Connection>,
) -> Vec<ClassifiedResult> {
    let index = conn.map(|c| {
        let files: HashSet<&str> = results
            .iter()
            .map(|r| r.file.to_str().unwrap_or(""))
            .collect();
        IndexLookup::load(c, &files)
    });

    results
        .iter()
        .map(|r| {
            let file_str = r.file.to_string_lossy();
            let line_i64 = r.line as i64;

            let category = classify_one(
                &file_str,
                line_i64,
                &r.content,
                &r.file,
                index.as_ref(),
            );

            ClassifiedResult {
                result: r.clone(),
                category,
                annotation: None,
            }
        })
        .collect()
}

/// Classify a single result according to the priority chain.
fn classify_one(
    file_str: &str,
    line: i64,
    content: &str,
    file_path: &Path,
    index: Option<&IndexLookup>,
) -> ResultCategory {
    // 1. Test (highest priority - path heuristic)
    if is_test_file(file_path) {
        return ResultCategory::Test;
    }

    // 2. Definition (from index)
    if let Some(idx) = index {
        if idx.is_definition(file_str, line) {
            return ResultCategory::Definition;
        }
    }

    // 3. Import (content heuristic, checked before Comment)
    if is_import_line(content) {
        return ResultCategory::Import;
    }

    // 4. Comment (content heuristic)
    if is_comment_line(content) {
        return ResultCategory::Comment;
    }

    // 5. CallSite (from index)
    if let Some(idx) = index {
        if idx.is_reference(file_str, line) {
            return ResultCategory::CallSite;
        }
    }

    // 6. Other (default)
    ResultCategory::Other
}

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

/// Sort classified results by category tier, then file path, then line number.
pub fn rank_results(mut results: Vec<ClassifiedResult>) -> Vec<ClassifiedResult> {
    results.sort();
    results
}

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

/// Deduplicate re-exported/aliased symbols.
///
/// When the same symbol name appears as both a Definition and one or more
/// Import re-exports, the imports are collapsed and the definition is
/// annotated with "(+N other location(s))".
///
/// Non-import, non-definition results are never deduplicated.
pub fn dedup_reexports(results: Vec<ClassifiedResult>, _pattern: &str) -> Vec<ClassifiedResult> {
    let has_definition = results.iter().any(|r| r.category == ResultCategory::Definition);
    let import_count = results.iter().filter(|r| r.category == ResultCategory::Import).count();

    // Only collapse when there is at least one definition and at least one import
    if !has_definition || import_count == 0 {
        return results;
    }

    let mut out = Vec::with_capacity(results.len());

    for mut r in results {
        if r.category == ResultCategory::Import {
            // Collapse this import (skip it)
            continue;
        }
        if r.category == ResultCategory::Definition && r.annotation.is_none() {
            let label = if import_count == 1 {
                format!("(+{import_count} other location)")
            } else {
                format!("(+{import_count} other locations)")
            };
            r.annotation = Some(label);
        }
        out.push(r);
    }

    out
}

// ---------------------------------------------------------------------------
// Grouping
// ---------------------------------------------------------------------------

/// Group sorted results by category, returning (category, results) pairs.
/// Empty categories are omitted.
pub fn group_by_category(
    results: Vec<ClassifiedResult>,
) -> Vec<(ResultCategory, Vec<ClassifiedResult>)> {
    let mut groups: Vec<(ResultCategory, Vec<ClassifiedResult>)> = Vec::new();

    for r in results {
        if let Some(last) = groups.last_mut() {
            if last.0 == r.category {
                last.1.push(r);
                continue;
            }
        }
        let cat = r.category;
        groups.push((cat, vec![r]));
    }

    groups
}

/// Map a category to its display header string.
pub fn category_header(cat: ResultCategory) -> &'static str {
    match cat {
        ResultCategory::Definition => "-- definitions --",
        ResultCategory::CallSite | ResultCategory::Other => "-- usages --",
        ResultCategory::Import => "-- imports --",
        ResultCategory::Comment => "-- comments --",
        ResultCategory::Test => "-- tests --",
    }
}

// ---------------------------------------------------------------------------
// Full pipeline
// ---------------------------------------------------------------------------

/// Full ranking pipeline: classify -> sort -> dedup -> group.
pub fn rank_and_dedup(
    results: &[SearchResult],
    conn: Option<&Connection>,
    pattern: &str,
) -> Vec<(ResultCategory, Vec<ClassifiedResult>)> {
    let classified = classify_results(results, conn);
    let sorted = rank_results(classified);
    let deduped = dedup_reexports(sorted, pattern);
    group_by_category(deduped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn make_result(file: &str, line: u64, content: &str) -> SearchResult {
        SearchResult {
            file: PathBuf::from(file),
            line,
            col: 1,
            content: content.to_string(),
        }
    }

    fn make_classified(file: &str, line: u64, content: &str, cat: ResultCategory) -> ClassifiedResult {
        ClassifiedResult {
            result: make_result(file, line, content),
            category: cat,
            annotation: None,
        }
    }

    #[test]
    fn result_category_display() {
        assert_eq!(ResultCategory::Definition.to_string(), "definition");
        assert_eq!(ResultCategory::CallSite.to_string(), "call_site");
        assert_eq!(ResultCategory::Import.to_string(), "import");
        assert_eq!(ResultCategory::Comment.to_string(), "comment");
        assert_eq!(ResultCategory::Test.to_string(), "test");
        assert_eq!(ResultCategory::Other.to_string(), "other");
    }

    #[test]
    fn classify_definition_from_index() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = crate::db::open(&db_path).unwrap();

        // Insert a symbol definition at src/main.rs:10
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["my_func", "function", "src/main.rs", 10, 0, "rust"],
        ).unwrap();

        let results = vec![
            make_result("src/main.rs", 10, "fn my_func() {}"),
        ];

        let classified = classify_results(&results, Some(&conn));
        assert_eq!(classified.len(), 1);
        assert_eq!(classified[0].category, ResultCategory::Definition);
    }

    #[test]
    fn classify_call_site_from_index() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = crate::db::open(&db_path).unwrap();

        // Insert a reference at src/main.rs:20
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, context) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["my_func", "src/main.rs", 20, 4, "let x = my_func();"],
        ).unwrap();

        let results = vec![
            make_result("src/main.rs", 20, "let x = my_func();"),
        ];

        let classified = classify_results(&results, Some(&conn));
        assert_eq!(classified.len(), 1);
        assert_eq!(classified[0].category, ResultCategory::CallSite);
    }

    #[test]
    fn classify_import_by_content() {
        let results = vec![
            make_result("src/main.rs", 1, "use std::collections::HashMap;"),
            make_result("src/app.ts", 1, "import { foo } from './bar';"),
            make_result("src/app.py", 1, "from os import path"),
            make_result("src/app.py", 2, "import json"),
            make_result("src/main.c", 1, "#include <stdio.h>"),
            make_result("src/app.rb", 1, "require 'json'"),
            make_result("src/app.go", 1, "import \"fmt\""),
        ];

        let classified = classify_results(&results, None);
        for (i, c) in classified.iter().enumerate() {
            assert_eq!(
                c.category,
                ResultCategory::Import,
                "result {} should be Import: {:?}",
                i,
                c.result.content
            );
        }
    }

    #[test]
    fn classify_comment_by_content() {
        let results = vec![
            make_result("src/main.rs", 1, "// this is a comment"),
            make_result("src/main.py", 1, "# this is a comment"),
            make_result("src/main.c", 1, "/* block comment */"),
            make_result("src/main.c", 2, " * continuation line"),
            make_result("src/main.rs", 3, "   /// doc comment"),
        ];

        let classified = classify_results(&results, None);
        for (i, c) in classified.iter().enumerate() {
            assert_eq!(
                c.category,
                ResultCategory::Comment,
                "result {} should be Comment: {:?}",
                i,
                c.result.content
            );
        }
    }

    #[test]
    fn classify_test_by_path() {
        let results = vec![
            make_result("tests/test_foo.rs", 10, "fn some_code()"),
            make_result("test/helper.js", 5, "function helper()"),
            make_result("__tests__/app.test.js", 1, "describe('app')"),
            make_result("src/foo_test.go", 1, "func TestFoo()"),
            make_result("src/foo.test.ts", 1, "it('works')"),
            make_result("src/foo.spec.js", 1, "describe('foo')"),
        ];

        let classified = classify_results(&results, None);
        for (i, c) in classified.iter().enumerate() {
            assert_eq!(
                c.category,
                ResultCategory::Test,
                "result {} should be Test: {:?}",
                i,
                c.result.file
            );
        }
    }

    #[test]
    fn classify_other_default() {
        let results = vec![
            make_result("src/main.rs", 5, "let x = 42;"),
        ];

        let classified = classify_results(&results, None);
        assert_eq!(classified.len(), 1);
        assert_eq!(classified[0].category, ResultCategory::Other);
    }

    #[test]
    fn classification_priority_test_over_definition() {
        // A symbol definition in a test file should be classified as Test
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("index.db");
        let conn = crate::db::open(&db_path).unwrap();

        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params!["test_func", "function", "tests/test_foo.rs", 10, 0, "rust"],
        ).unwrap();

        let results = vec![
            make_result("tests/test_foo.rs", 10, "fn test_func() {}"),
        ];

        let classified = classify_results(&results, Some(&conn));
        assert_eq!(classified[0].category, ResultCategory::Test);
    }

    #[test]
    fn classification_priority_import_over_comment() {
        // #include looks like a comment (starts with #) but should be Import
        let results = vec![
            make_result("src/main.c", 1, "#include <stdio.h>"),
        ];

        let classified = classify_results(&results, None);
        assert_eq!(classified[0].category, ResultCategory::Import);
    }

    #[test]
    fn classify_without_connection() {
        // Without a DB connection, definitions/call_sites can't be detected
        let results = vec![
            make_result("src/main.rs", 10, "fn my_func() {}"),
            make_result("src/main.rs", 20, "let x = my_func();"),
        ];

        let classified = classify_results(&results, None);
        // Without index, these should fall through to Other
        assert_eq!(classified[0].category, ResultCategory::Other);
        assert_eq!(classified[1].category, ResultCategory::Other);
    }

    #[test]
    fn classify_preserves_original_result() {
        let results = vec![
            make_result("src/main.rs", 5, "let x = 42;"),
        ];

        let classified = classify_results(&results, None);
        assert_eq!(classified[0].result, results[0]);
    }

    #[test]
    fn is_test_file_heuristics() {
        assert!(is_test_file(Path::new("tests/foo.rs")));
        assert!(is_test_file(Path::new("test/foo.js")));
        assert!(is_test_file(Path::new("__tests__/foo.js")));
        assert!(is_test_file(Path::new("src/foo_test.go")));
        assert!(is_test_file(Path::new("src/foo.test.ts")));
        assert!(is_test_file(Path::new("src/foo.spec.js")));
        assert!(!is_test_file(Path::new("src/main.rs")));
        assert!(!is_test_file(Path::new("src/testing.rs")));
        assert!(!is_test_file(Path::new("src/contest.rs")));
    }

    #[test]
    fn is_import_line_heuristics() {
        assert!(is_import_line("use std::collections::HashMap;"));
        assert!(is_import_line("import { foo } from './bar';"));
        assert!(is_import_line("from os import path"));
        assert!(is_import_line("import json"));
        assert!(is_import_line("#include <stdio.h>"));
        assert!(is_import_line("require 'json'"));
        assert!(is_import_line("require('express')"));
        assert!(is_import_line("  import \"fmt\""));
        assert!(!is_import_line("let x = 42;"));
        assert!(!is_import_line("fn useful() {}"));
        assert!(!is_import_line("// import foo"));
    }

    #[test]
    fn is_comment_line_heuristics() {
        assert!(is_comment_line("// single line comment"));
        assert!(is_comment_line("# python comment"));
        assert!(is_comment_line("/* block comment */"));
        assert!(is_comment_line(" * continuation"));
        assert!(is_comment_line("   /// doc comment"));
        assert!(is_comment_line("  // indented comment"));
        assert!(!is_comment_line("let x = 42; // trailing comment"));
        assert!(!is_comment_line("fn main() {}"));
        assert!(!is_comment_line("#include <stdio.h>"));
    }

    // -----------------------------------------------------------------------
    // Tier ordering tests
    // -----------------------------------------------------------------------

    #[test]
    fn tier_ordering_definition_first() {
        assert!(ResultCategory::Definition.tier() < ResultCategory::CallSite.tier());
        assert!(ResultCategory::Definition.tier() < ResultCategory::Import.tier());
        assert!(ResultCategory::Definition.tier() < ResultCategory::Other.tier());
        assert!(ResultCategory::Definition.tier() < ResultCategory::Comment.tier());
        assert!(ResultCategory::Definition.tier() < ResultCategory::Test.tier());
    }

    #[test]
    fn tier_ordering_full_sequence() {
        // Definition < CallSite < Import < Other < Comment < Test
        let tiers: Vec<u8> = vec![
            ResultCategory::Definition.tier(),
            ResultCategory::CallSite.tier(),
            ResultCategory::Import.tier(),
            ResultCategory::Other.tier(),
            ResultCategory::Comment.tier(),
            ResultCategory::Test.tier(),
        ];
        for i in 0..tiers.len() - 1 {
            assert!(
                tiers[i] < tiers[i + 1],
                "tier {} should be less than tier {}",
                i,
                i + 1,
            );
        }
    }

    #[test]
    fn result_category_ord_matches_tier() {
        assert!(ResultCategory::Definition < ResultCategory::CallSite);
        assert!(ResultCategory::CallSite < ResultCategory::Import);
        assert!(ResultCategory::Import < ResultCategory::Other);
        assert!(ResultCategory::Other < ResultCategory::Comment);
        assert!(ResultCategory::Comment < ResultCategory::Test);
    }

    // -----------------------------------------------------------------------
    // ClassifiedResult sorting tests
    // -----------------------------------------------------------------------

    #[test]
    fn classified_result_sorts_by_category_then_file_then_line() {
        let mut results = vec![
            make_classified("src/b.rs", 10, "let x = foo();", ResultCategory::CallSite),
            make_classified("src/a.rs", 5, "fn foo() {}", ResultCategory::Definition),
            make_classified("src/a.rs", 20, "foo();", ResultCategory::CallSite),
            make_classified("src/a.rs", 10, "foo();", ResultCategory::CallSite),
        ];

        results.sort();

        // Definition first
        assert_eq!(results[0].category, ResultCategory::Definition);
        assert_eq!(results[0].result.file.to_str().unwrap(), "src/a.rs");

        // Then CallSite, sorted by file (a before b) then line
        assert_eq!(results[1].category, ResultCategory::CallSite);
        assert_eq!(results[1].result.file.to_str().unwrap(), "src/a.rs");
        assert_eq!(results[1].result.line, 10);

        assert_eq!(results[2].category, ResultCategory::CallSite);
        assert_eq!(results[2].result.file.to_str().unwrap(), "src/a.rs");
        assert_eq!(results[2].result.line, 20);

        assert_eq!(results[3].category, ResultCategory::CallSite);
        assert_eq!(results[3].result.file.to_str().unwrap(), "src/b.rs");
    }

    // -----------------------------------------------------------------------
    // rank_results tests
    // -----------------------------------------------------------------------

    #[test]
    fn rank_results_sorts_by_tier() {
        let input = vec![
            make_classified("src/main.rs", 1, "use foo;", ResultCategory::Import),
            make_classified("src/main.rs", 5, "fn foo() {}", ResultCategory::Definition),
            make_classified("tests/t.rs", 10, "foo();", ResultCategory::Test),
        ];

        let ranked = rank_results(input);
        assert_eq!(ranked[0].category, ResultCategory::Definition);
        assert_eq!(ranked[1].category, ResultCategory::Import);
        assert_eq!(ranked[2].category, ResultCategory::Test);
    }

    // -----------------------------------------------------------------------
    // dedup_reexports tests
    // -----------------------------------------------------------------------

    #[test]
    fn dedup_collapses_import_reexports_when_definition_exists() {
        let input = vec![
            make_classified("src/lib.rs", 10, "pub fn foo() {}", ResultCategory::Definition),
            make_classified("src/reexport1.rs", 1, "pub use crate::foo;", ResultCategory::Import),
            make_classified("src/reexport2.rs", 1, "pub use crate::foo;", ResultCategory::Import),
            make_classified("src/main.rs", 5, "foo();", ResultCategory::CallSite),
        ];

        let deduped = dedup_reexports(input, "foo");
        // Should keep definition (with annotation), call site, and collapse 2 imports
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].category, ResultCategory::Definition);
        assert_eq!(
            deduped[0].annotation.as_deref(),
            Some("(+2 other locations)")
        );
        assert_eq!(deduped[1].category, ResultCategory::CallSite);
    }

    #[test]
    fn dedup_keeps_all_when_no_definition_exists() {
        let input = vec![
            make_classified("src/a.rs", 1, "pub use foo;", ResultCategory::Import),
            make_classified("src/b.rs", 1, "pub use foo;", ResultCategory::Import),
        ];

        let deduped = dedup_reexports(input, "foo");
        // No definitions => no dedup, keep all
        assert_eq!(deduped.len(), 2);
    }

    #[test]
    fn dedup_keeps_all_when_no_imports() {
        let input = vec![
            make_classified("src/lib.rs", 10, "pub fn foo() {}", ResultCategory::Definition),
            make_classified("src/main.rs", 5, "foo();", ResultCategory::CallSite),
        ];

        let deduped = dedup_reexports(input, "foo");
        // No imports => nothing to collapse
        assert_eq!(deduped.len(), 2);
        assert!(deduped[0].annotation.is_none());
    }

    #[test]
    fn dedup_single_import_not_collapsed() {
        // Only one import + definition: no need for "(+1 other locations)"
        let input = vec![
            make_classified("src/lib.rs", 10, "pub fn foo() {}", ResultCategory::Definition),
            make_classified("src/index.rs", 1, "pub use crate::foo;", ResultCategory::Import),
        ];

        let deduped = dedup_reexports(input, "foo");
        // Single import should still be collapsed, annotating the definition
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].category, ResultCategory::Definition);
        assert_eq!(
            deduped[0].annotation.as_deref(),
            Some("(+1 other location)")
        );
    }

    // -----------------------------------------------------------------------
    // group_by_category tests
    // -----------------------------------------------------------------------

    #[test]
    fn group_by_category_groups_and_preserves_order() {
        let input = vec![
            make_classified("src/a.rs", 5, "fn foo() {}", ResultCategory::Definition),
            make_classified("src/b.rs", 10, "foo();", ResultCategory::CallSite),
            make_classified("src/c.rs", 1, "use foo;", ResultCategory::Import),
        ];

        // Input must be sorted first
        let sorted = rank_results(input);
        let groups = group_by_category(sorted);

        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].0, ResultCategory::Definition);
        assert_eq!(groups[0].1.len(), 1);
        assert_eq!(groups[1].0, ResultCategory::CallSite);
        assert_eq!(groups[1].1.len(), 1);
        assert_eq!(groups[2].0, ResultCategory::Import);
        assert_eq!(groups[2].1.len(), 1);
    }

    #[test]
    fn group_by_category_omits_empty_categories() {
        let input = vec![
            make_classified("src/a.rs", 5, "fn foo() {}", ResultCategory::Definition),
            make_classified("tests/t.rs", 10, "foo();", ResultCategory::Test),
        ];

        let sorted = rank_results(input);
        let groups = group_by_category(sorted);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, ResultCategory::Definition);
        assert_eq!(groups[1].0, ResultCategory::Test);
    }

    // -----------------------------------------------------------------------
    // category_header tests
    // -----------------------------------------------------------------------

    #[test]
    fn category_header_mappings() {
        assert_eq!(category_header(ResultCategory::Definition), "-- definitions --");
        assert_eq!(category_header(ResultCategory::CallSite), "-- usages --");
        assert_eq!(category_header(ResultCategory::Import), "-- imports --");
        assert_eq!(category_header(ResultCategory::Other), "-- usages --");
        assert_eq!(category_header(ResultCategory::Comment), "-- comments --");
        assert_eq!(category_header(ResultCategory::Test), "-- tests --");
    }

    // -----------------------------------------------------------------------
    // rank_and_dedup end-to-end test
    // -----------------------------------------------------------------------

    #[test]
    fn rank_and_dedup_full_pipeline() {
        let results = vec![
            make_result("tests/t.rs", 10, "foo();"),
            make_result("src/main.rs", 1, "use foo;"),
            make_result("src/lib.rs", 5, "let x = 42;"),
            make_result("src/lib.rs", 3, "// comment about foo"),
        ];

        let groups = rank_and_dedup(&results, None, "foo");

        // Without DB: Import, Comment, Other, Test (no Definition/CallSite)
        assert!(!groups.is_empty());
        // First group should be Import (tier 2)
        assert_eq!(groups[0].0, ResultCategory::Import);
    }
}
