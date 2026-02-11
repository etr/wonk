//! File indexing and tree-sitter parsing.
//!
//! Provides multi-language parsing infrastructure: language detection by file
//! extension, parser construction with the correct grammar, and file parsing.

use std::path::Path;

use tree_sitter::{Language, Parser, Tree};

/// Supported programming languages with bundled Tree-sitter grammars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Rust,
    Go,
    Java,
    C,
    Cpp,
    Ruby,
    Php,
}

impl Lang {
    /// Returns the human-readable name for this language.
    pub fn name(self) -> &'static str {
        match self {
            Lang::TypeScript => "TypeScript",
            Lang::Tsx => "TSX",
            Lang::JavaScript => "JavaScript",
            Lang::Python => "Python",
            Lang::Rust => "Rust",
            Lang::Go => "Go",
            Lang::Java => "Java",
            Lang::C => "C",
            Lang::Cpp => "C++",
            Lang::Ruby => "Ruby",
            Lang::Php => "PHP",
        }
    }
}

/// Detect the programming language of a file based on its extension.
///
/// Returns `None` for unsupported or missing extensions.
pub fn detect_language(path: &Path) -> Option<Lang> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "ts" => Some(Lang::TypeScript),
        "tsx" => Some(Lang::Tsx),
        "js" | "jsx" => Some(Lang::JavaScript),
        "py" => Some(Lang::Python),
        "rs" => Some(Lang::Rust),
        "go" => Some(Lang::Go),
        "java" => Some(Lang::Java),
        "c" | "h" => Some(Lang::C),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Lang::Cpp),
        "rb" => Some(Lang::Ruby),
        "php" => Some(Lang::Php),
        _ => None,
    }
}

/// Return the Tree-sitter [`Language`] grammar for the given language.
fn grammar_for(lang: Lang) -> Language {
    match lang {
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
        Lang::Java => tree_sitter_java::LANGUAGE.into(),
        Lang::C => tree_sitter_c::LANGUAGE.into(),
        Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        Lang::Php => tree_sitter_php::LANGUAGE_PHP.into(),
    }
}

/// Create a new [`Parser`] configured for the given language.
pub fn get_parser(lang: Lang) -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&grammar_for(lang))
        .expect("Error loading grammar — ABI version mismatch");
    parser
}

/// Parse a source file, returning the syntax tree and detected language.
///
/// Returns `None` when:
/// - the file extension is unsupported
/// - the file cannot be read
/// - the parser fails to produce a tree
pub fn parse_file(path: &Path) -> Option<(Tree, Lang)> {
    let lang = detect_language(path)?;
    let source = std::fs::read(path).ok()?;
    let mut parser = get_parser(lang);
    let tree = parser.parse(&source, None)?;
    Some((tree, lang))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ---------- detect_language tests ----------

    #[test]
    fn detect_typescript() {
        assert_eq!(detect_language(Path::new("a.ts")), Some(Lang::TypeScript));
    }

    #[test]
    fn detect_tsx() {
        assert_eq!(detect_language(Path::new("a.tsx")), Some(Lang::Tsx));
    }

    #[test]
    fn detect_javascript_js() {
        assert_eq!(detect_language(Path::new("a.js")), Some(Lang::JavaScript));
    }

    #[test]
    fn detect_javascript_jsx() {
        assert_eq!(detect_language(Path::new("a.jsx")), Some(Lang::JavaScript));
    }

    #[test]
    fn detect_python() {
        assert_eq!(detect_language(Path::new("a.py")), Some(Lang::Python));
    }

    #[test]
    fn detect_rust() {
        assert_eq!(detect_language(Path::new("a.rs")), Some(Lang::Rust));
    }

    #[test]
    fn detect_go() {
        assert_eq!(detect_language(Path::new("a.go")), Some(Lang::Go));
    }

    #[test]
    fn detect_java() {
        assert_eq!(detect_language(Path::new("a.java")), Some(Lang::Java));
    }

    #[test]
    fn detect_c() {
        assert_eq!(detect_language(Path::new("a.c")), Some(Lang::C));
    }

    #[test]
    fn detect_c_header() {
        assert_eq!(detect_language(Path::new("a.h")), Some(Lang::C));
    }

    #[test]
    fn detect_cpp_extensions() {
        for ext in &["cpp", "cc", "cxx", "hpp", "hh", "hxx"] {
            let p = PathBuf::from(format!("a.{ext}"));
            assert_eq!(detect_language(&p), Some(Lang::Cpp), "failed for .{ext}");
        }
    }

    #[test]
    fn detect_ruby() {
        assert_eq!(detect_language(Path::new("a.rb")), Some(Lang::Ruby));
    }

    #[test]
    fn detect_php() {
        assert_eq!(detect_language(Path::new("a.php")), Some(Lang::Php));
    }

    #[test]
    fn detect_unsupported_returns_none() {
        assert_eq!(detect_language(Path::new("a.txt")), None);
        assert_eq!(detect_language(Path::new("a.md")), None);
        assert_eq!(detect_language(Path::new("Makefile")), None);
    }

    #[test]
    fn detect_no_extension_returns_none() {
        assert_eq!(detect_language(Path::new("Dockerfile")), None);
    }

    // ---------- get_parser / grammar loading tests ----------

    #[test]
    fn parser_loads_all_grammars() {
        let langs = [
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
        ];
        for lang in langs {
            let _parser = get_parser(lang); // should not panic
        }
    }

    // ---------- parse_file tests ----------

    /// Helper: write `source` to a temp file with the given extension and parse it.
    fn parse_temp(ext: &str, source: &str) -> Option<(Tree, Lang)> {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join(format!("test.{ext}"));
        std::fs::write(&file_path, source).unwrap();
        parse_file(&file_path)
    }

    #[test]
    fn parse_rust_file() {
        let (tree, lang) = parse_temp("rs", "fn main() {}").unwrap();
        assert_eq!(lang, Lang::Rust);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_typescript_file() {
        let src = "function greet(name: string): void { console.log(name); }";
        let (tree, lang) = parse_temp("ts", src).unwrap();
        assert_eq!(lang, Lang::TypeScript);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_tsx_file() {
        let src = "const App = () => <div>hello</div>;";
        let (tree, lang) = parse_temp("tsx", src).unwrap();
        assert_eq!(lang, Lang::Tsx);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_javascript_file() {
        let src = "function add(a, b) { return a + b; }";
        let (tree, lang) = parse_temp("js", src).unwrap();
        assert_eq!(lang, Lang::JavaScript);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_python_file() {
        let src = "def hello():\n    pass\n";
        let (tree, lang) = parse_temp("py", src).unwrap();
        assert_eq!(lang, Lang::Python);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_go_file() {
        let src = "package main\n\nfunc main() {}\n";
        let (tree, lang) = parse_temp("go", src).unwrap();
        assert_eq!(lang, Lang::Go);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_java_file() {
        let src = "class Hello { public static void main(String[] args) {} }";
        let (tree, lang) = parse_temp("java", src).unwrap();
        assert_eq!(lang, Lang::Java);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_c_file() {
        let src = "int main() { return 0; }";
        let (tree, lang) = parse_temp("c", src).unwrap();
        assert_eq!(lang, Lang::C);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_cpp_file() {
        let src = "#include <iostream>\nint main() { return 0; }";
        let (tree, lang) = parse_temp("cpp", src).unwrap();
        assert_eq!(lang, Lang::Cpp);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_ruby_file() {
        let src = "def hello\n  puts 'hi'\nend\n";
        let (tree, lang) = parse_temp("rb", src).unwrap();
        assert_eq!(lang, Lang::Ruby);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_php_file() {
        let src = "<?php\nfunction hello() { echo 'hi'; }\n?>";
        let (tree, lang) = parse_temp("php", src).unwrap();
        assert_eq!(lang, Lang::Php);
        assert!(!tree.root_node().has_error());
    }

    #[test]
    fn parse_unsupported_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readme.txt");
        std::fs::write(&path, "hello world").unwrap();
        assert!(parse_file(&path).is_none());
    }

    #[test]
    fn parse_missing_file_returns_none() {
        assert!(parse_file(Path::new("/nonexistent/file.rs")).is_none());
    }
}
