//! Blast radius analysis for `wonk blast`.
//!
//! Performs depth-annotated BFS from a target symbol to discover all affected
//! symbols, grouping them by severity tier (depth) and computing an overall
//! risk level. Supports upstream (callers + type hierarchy children) and
//! downstream (callees) traversal directions.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::str::FromStr;

use anyhow::Result;
use rusqlite::Connection;

use crate::ranker;
use crate::types::{
    BlastAffectedSymbol, BlastAnalysis, BlastDirection, BlastRiskLevel, BlastSeverity, BlastTier,
    SymbolKind,
};

/// Default traversal depth.
pub const DEFAULT_DEPTH: usize = 3;

/// Maximum allowed depth.
pub const MAX_DEPTH: usize = 10;

/// Options for blast radius analysis.
#[derive(Debug, Clone)]
pub struct BlastOptions {
    /// Maximum BFS depth (default: 3, max: 10).
    pub depth: usize,
    /// Direction of traversal (default: Upstream).
    pub direction: BlastDirection,
    /// Whether to include test files in results (default: false).
    pub include_tests: bool,
    /// Minimum confidence threshold for edge filtering.
    pub min_confidence: Option<f64>,
}

impl Default for BlastOptions {
    fn default() -> Self {
        Self {
            depth: DEFAULT_DEPTH,
            direction: BlastDirection::Upstream,
            include_tests: false,
            min_confidence: None,
        }
    }
}

/// Clamp a requested depth to [`MAX_DEPTH`], returning the capped value and
/// whether clamping occurred.
pub fn clamp_depth(requested: usize) -> (usize, bool) {
    if requested > MAX_DEPTH {
        (MAX_DEPTH, true)
    } else {
        (requested, false)
    }
}

/// Sanitize a user-provided confidence threshold to a valid [0.0, 1.0] range.
/// Returns 0.0 (no filtering) when None. Rejects NaN and infinity.
fn sanitize_confidence(min_confidence: Option<f64>) -> f64 {
    match min_confidence {
        Some(c) if c.is_nan() || c.is_infinite() => 0.0,
        Some(c) => c.clamp(0.0, 1.0),
        None => 0.0,
    }
}

/// Map BFS depth to a severity tier.
fn severity_for_depth(depth: usize) -> BlastSeverity {
    match depth {
        1 => BlastSeverity::WillBreak,
        2 => BlastSeverity::LikelyAffected,
        _ => BlastSeverity::MayNeedTesting,
    }
}

/// Map total affected count to a risk level.
fn risk_level_for_count(count: usize) -> BlastRiskLevel {
    match count {
        0..=3 => BlastRiskLevel::Low,
        4..=10 => BlastRiskLevel::Medium,
        11..=25 => BlastRiskLevel::High,
        _ => BlastRiskLevel::Critical,
    }
}

/// Perform blast radius analysis from a target symbol.
///
/// BFS traverses the call graph (upstream or downstream) from the target,
/// collecting all affected symbols with their depth and grouping them into
/// severity tiers.
pub fn analyze_blast(
    conn: &Connection,
    symbol: &str,
    options: &BlastOptions,
) -> Result<BlastAnalysis> {
    if options.depth == 0 {
        return Ok(BlastAnalysis {
            target: symbol.to_string(),
            direction: options.direction,
            risk_level: BlastRiskLevel::Low,
            total_affected: 0,
            tiers: vec![],
            affected_files: vec![],
        });
    }

    let conf_threshold = sanitize_confidence(options.min_confidence);

    let mut affected: Vec<BlastAffectedSymbol> = Vec::new();
    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut queued: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    queue.push_back((symbol.to_string(), 1));
    queued.insert(symbol.to_string());

    match options.direction {
        BlastDirection::Upstream => {
            // Callers via references.caller_id JOIN symbols
            let mut stmt_callers = conn.prepare(
                "SELECT DISTINCT s.name, s.kind, s.file, s.line, r.confidence \
                 FROM \"references\" r \
                 JOIN symbols s ON r.caller_id = s.id \
                 WHERE r.name = ?1 AND r.confidence >= ?2",
            )?;

            // Type hierarchy children: type_edges WHERE parent_id matches
            let mut stmt_children = conn.prepare(
                "SELECT DISTINCT child.name, child.kind, child.file, child.line \
                 FROM type_edges te \
                 JOIN symbols parent ON te.parent_id = parent.id \
                 JOIN symbols child ON te.child_id = child.id \
                 WHERE parent.name = ?1",
            )?;

            while let Some((target_name, depth)) = queue.pop_front() {
                if depth > options.depth {
                    continue;
                }

                // Query callers
                let rows: Vec<_> = stmt_callers
                    .query_map(rusqlite::params![&target_name, conf_threshold], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, f64>(4)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                for (name, kind_str, file, line, confidence) in rows {
                    let key = (name.clone(), file.clone());
                    if visited.contains(&key) {
                        continue;
                    }
                    visited.insert(key);

                    // Test file exclusion
                    if !options.include_tests && ranker::is_test_file(Path::new(&file)) {
                        continue;
                    }

                    let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);

                    affected.push(BlastAffectedSymbol {
                        name: name.clone(),
                        kind,
                        file,
                        line: line as usize,
                        depth,
                        confidence,
                    });

                    if depth < options.depth && !queued.contains(&name) {
                        queued.insert(name.clone());
                        queue.push_back((name, depth + 1));
                    }
                }

                // Query type hierarchy children (only at depth 1 for direct inheritance)
                if depth == 1 {
                    let child_rows: Vec<_> = stmt_children
                        .query_map(rusqlite::params![&target_name], |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, i64>(3)?,
                            ))
                        })?
                        .collect::<Result<Vec<_>, _>>()?;

                    for (name, kind_str, file, line) in child_rows {
                        let key = (name.clone(), file.clone());
                        if visited.contains(&key) {
                            continue;
                        }
                        visited.insert(key);

                        if !options.include_tests && ranker::is_test_file(Path::new(&file)) {
                            continue;
                        }

                        let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);

                        affected.push(BlastAffectedSymbol {
                            name: name.clone(),
                            kind,
                            file,
                            line: line as usize,
                            depth,
                            confidence: 1.0, // Type edges are certain
                        });

                        if !queued.contains(&name) {
                            queued.insert(name.clone());
                            queue.push_back((name, depth + 1));
                        }
                    }
                }
            }
        }
        BlastDirection::Downstream => {
            // Callees via references WHERE caller_id IN (SELECT id FROM symbols WHERE name = ?)
            let mut stmt_callees = conn.prepare(
                "SELECT DISTINCT r.name, s_def.kind, r.file, r.line, r.confidence \
                 FROM \"references\" r \
                 JOIN symbols s ON s.id = r.caller_id \
                 LEFT JOIN symbols s_def ON s_def.name = r.name \
                 WHERE s.name = ?1 AND r.confidence >= ?2",
            )?;

            while let Some((target_name, depth)) = queue.pop_front() {
                if depth > options.depth {
                    continue;
                }

                let rows: Vec<_> = stmt_callees
                    .query_map(rusqlite::params![&target_name, conf_threshold], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, f64>(4)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                for (name, kind_str, file, line, confidence) in rows {
                    let key = (name.clone(), file.clone());
                    if visited.contains(&key) {
                        continue;
                    }
                    visited.insert(key);

                    if !options.include_tests && ranker::is_test_file(Path::new(&file)) {
                        continue;
                    }

                    let kind = kind_str
                        .as_deref()
                        .and_then(|k| SymbolKind::from_str(k).ok())
                        .unwrap_or(SymbolKind::Function);

                    affected.push(BlastAffectedSymbol {
                        name: name.clone(),
                        kind,
                        file,
                        line: line as usize,
                        depth,
                        confidence,
                    });

                    if depth < options.depth && !queued.contains(&name) {
                        queued.insert(name.clone());
                        queue.push_back((name, depth + 1));
                    }
                }
            }
        }
    }

    // Sort by depth, then file, then line for deterministic output.
    affected.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    // Group into severity tiers.
    let mut tier_map: Vec<(BlastSeverity, Vec<BlastAffectedSymbol>)> = Vec::new();
    for sym in &affected {
        let severity = severity_for_depth(sym.depth);
        if let Some(tier) = tier_map.iter_mut().find(|(s, _)| *s == severity) {
            tier.1.push(sym.clone());
        } else {
            tier_map.push((severity, vec![sym.clone()]));
        }
    }

    let tiers: Vec<BlastTier> = tier_map
        .into_iter()
        .map(|(severity, symbols)| BlastTier { severity, symbols })
        .collect();

    // Deduplicated affected files.
    let mut files_seen: HashSet<String> = HashSet::new();
    let mut affected_files: Vec<String> = Vec::new();
    for sym in &affected {
        if files_seen.insert(sym.file.clone()) {
            affected_files.push(sym.file.clone());
        }
    }
    affected_files.sort();

    let total_affected = affected.len();
    let risk_level = risk_level_for_count(total_affected);

    Ok(BlastAnalysis {
        target: symbol.to_string(),
        direction: options.direction,
        risk_level,
        total_affected,
        tiers,
        affected_files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::pipeline;
    use std::fs;
    use tempfile::TempDir;

    /// Create a minimal Rust repo, index it, and return (TempDir, Connection).
    fn make_indexed_repo(source: &str) -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), source).unwrap();

        pipeline::build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        (dir, conn)
    }

    /// Create a multi-file repo, index it, and return (TempDir, Connection).
    fn make_multi_file_repo(files: &[(&str, &str)]) -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        fs::create_dir(root.join(".git")).unwrap();

        for (path, content) in files {
            if let Some(parent) = Path::new(path).parent() {
                fs::create_dir_all(root.join(parent)).unwrap();
            }
            fs::write(root.join(path), content).unwrap();
        }

        pipeline::build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        (dir, conn)
    }

    #[test]
    fn blast_upstream_basic() {
        // foo calls bar, so blast("bar", upstream) should find foo.
        let source = r#"
fn foo() {
    bar();
}

fn bar() {
    println!("hello");
}
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let options = BlastOptions {
            direction: BlastDirection::Upstream,
            ..Default::default()
        };
        let result = analyze_blast(&conn, "bar", &options).unwrap();

        assert_eq!(result.target, "bar");
        assert_eq!(result.direction, BlastDirection::Upstream);
        assert!(
            result.total_affected > 0,
            "should find at least one affected symbol"
        );

        let names: Vec<&str> = result
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(
            names.contains(&"foo"),
            "foo should be in blast radius of bar"
        );
    }

    #[test]
    fn blast_downstream_basic() {
        // foo calls bar and baz, so blast("foo", downstream) should find bar and baz.
        let source = r#"
fn foo() {
    bar();
    baz();
}

fn bar() { }
fn baz() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let options = BlastOptions {
            direction: BlastDirection::Downstream,
            ..Default::default()
        };
        let result = analyze_blast(&conn, "foo", &options).unwrap();

        assert_eq!(result.direction, BlastDirection::Downstream);

        let names: Vec<&str> = result
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(names.contains(&"bar"), "bar should be in downstream blast");
        assert!(names.contains(&"baz"), "baz should be in downstream blast");
    }

    #[test]
    fn blast_depth_cap() {
        // Chain: a -> b -> c -> d. With depth=2, should not reach d.
        let source = r#"
fn a() {
    b();
}

fn b() {
    c();
}

fn c() {
    d();
}

fn d() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let options = BlastOptions {
            direction: BlastDirection::Downstream,
            depth: 2,
            ..Default::default()
        };
        let result = analyze_blast(&conn, "a", &options).unwrap();

        let names: Vec<&str> = result
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(names.contains(&"b"), "b should be at depth 1");
        assert!(names.contains(&"c"), "c should be at depth 2");
        assert!(!names.contains(&"d"), "d should be excluded at depth 3");
    }

    #[test]
    fn blast_severity_tiers() {
        // a -> b -> c. upstream from c with depth=2 gives b(WillBreak) and a(LikelyAffected).
        let source = r#"
fn a() {
    b();
}

fn b() {
    c();
}

fn c() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let options = BlastOptions {
            direction: BlastDirection::Upstream,
            depth: 2,
            ..Default::default()
        };
        let result = analyze_blast(&conn, "c", &options).unwrap();

        // Should have tiers for depth 1 (WillBreak) and depth 2 (LikelyAffected).
        let will_break = result
            .tiers
            .iter()
            .find(|t| t.severity == BlastSeverity::WillBreak);
        let likely = result
            .tiers
            .iter()
            .find(|t| t.severity == BlastSeverity::LikelyAffected);

        assert!(will_break.is_some(), "should have WILL BREAK tier");
        assert!(likely.is_some(), "should have LIKELY AFFECTED tier");

        let wb_names: Vec<&str> = will_break
            .unwrap()
            .symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(wb_names.contains(&"b"), "b is a depth-1 caller of c");

        let la_names: Vec<&str> = likely
            .unwrap()
            .symbols
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(la_names.contains(&"a"), "a is a depth-2 caller of c");
    }

    #[test]
    fn blast_risk_level_low() {
        // Only 1 caller -> LOW risk.
        let source = r#"
fn foo() {
    bar();
}

fn bar() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let result = analyze_blast(&conn, "bar", &BlastOptions::default()).unwrap();
        assert_eq!(result.risk_level, BlastRiskLevel::Low);
    }

    #[test]
    fn blast_risk_level_medium() {
        // 4-10 callers -> MEDIUM risk.
        let count = risk_level_for_count(5);
        assert_eq!(count, BlastRiskLevel::Medium);
        let count = risk_level_for_count(10);
        assert_eq!(count, BlastRiskLevel::Medium);
    }

    #[test]
    fn blast_empty_results() {
        // No callers for a standalone function.
        let source = "fn standalone() { }\n";
        let (_dir, conn) = make_indexed_repo(source);
        let result = analyze_blast(&conn, "standalone", &BlastOptions::default()).unwrap();
        assert_eq!(result.total_affected, 0);
        assert!(result.tiers.is_empty());
        assert!(result.affected_files.is_empty());
        assert_eq!(result.risk_level, BlastRiskLevel::Low);
    }

    #[test]
    fn blast_cycle_terminates() {
        // a -> b -> a (mutual recursion). Should not hang.
        let source = r#"
fn a() {
    b();
}

fn b() {
    a();
}
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let options = BlastOptions {
            depth: 5,
            ..Default::default()
        };
        let result = analyze_blast(&conn, "a", &options).unwrap();

        // Should find b as a caller but not duplicate.
        let names: Vec<&str> = result
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(names.contains(&"b"), "b should be a caller of a");
        // No duplicates.
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "no duplicate affected symbols");
    }

    #[test]
    fn blast_test_file_exclusion() {
        // Two files: src/lib.rs with prod code, tests/test_foo.rs with test code.
        let files = &[
            (
                "src/lib.rs",
                "fn target() { }\nfn prod_caller() { target(); }\n",
            ),
            ("tests/test_foo.rs", "fn test_caller() { target(); }\n"),
        ];
        let (_dir, conn) = make_multi_file_repo(files);
        // Default: tests excluded.
        let result = analyze_blast(&conn, "target", &BlastOptions::default()).unwrap();
        let names: Vec<&str> = result
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(
            names.contains(&"prod_caller"),
            "prod caller should be included"
        );
        assert!(
            !names.contains(&"test_caller"),
            "test caller should be excluded by default"
        );

        // With include_tests: test caller should appear.
        let options = BlastOptions {
            include_tests: true,
            ..Default::default()
        };
        let result = analyze_blast(&conn, "target", &options).unwrap();
        let names: Vec<&str> = result
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.as_str()))
            .collect();
        assert!(
            names.contains(&"test_caller"),
            "test caller should be included with --include-tests"
        );
    }

    #[test]
    fn blast_affected_files_dedup() {
        // Multiple callers in the same file should yield deduplicated file list.
        let source = r#"
fn caller1() { target(); }
fn caller2() { target(); }
fn target() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let result = analyze_blast(&conn, "target", &BlastOptions::default()).unwrap();
        // All callers are in src/lib.rs.
        assert_eq!(
            result.affected_files.len(),
            1,
            "affected files should be deduplicated"
        );
    }

    #[test]
    fn blast_min_confidence_filter() {
        // Same-file references have confidence 0.85, so filtering at 0.9 should exclude them.
        let source = r#"
fn foo() {
    bar();
}

fn bar() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let options = BlastOptions {
            min_confidence: Some(0.9),
            ..Default::default()
        };
        let result = analyze_blast(&conn, "bar", &options).unwrap();
        assert_eq!(
            result.total_affected, 0,
            "high confidence filter should exclude 0.85 refs"
        );
    }

    #[test]
    fn blast_depth_0_returns_empty() {
        let source = "fn foo() { bar(); }\nfn bar() { }\n";
        let (_dir, conn) = make_indexed_repo(source);
        let options = BlastOptions {
            depth: 0,
            ..Default::default()
        };
        let result = analyze_blast(&conn, "bar", &options).unwrap();
        assert_eq!(result.total_affected, 0);
        assert!(result.tiers.is_empty());
    }

    #[test]
    fn blast_options_default_values() {
        let opts = BlastOptions::default();
        assert_eq!(opts.depth, DEFAULT_DEPTH);
        assert_eq!(opts.direction, BlastDirection::Upstream);
        assert!(!opts.include_tests);
        assert!(opts.min_confidence.is_none());
    }

    // -- Helper function tests ------------------------------------------------

    #[test]
    fn severity_for_depth_mapping() {
        assert_eq!(severity_for_depth(1), BlastSeverity::WillBreak);
        assert_eq!(severity_for_depth(2), BlastSeverity::LikelyAffected);
        assert_eq!(severity_for_depth(3), BlastSeverity::MayNeedTesting);
        assert_eq!(severity_for_depth(10), BlastSeverity::MayNeedTesting);
    }

    #[test]
    fn risk_level_for_count_mapping() {
        assert_eq!(risk_level_for_count(0), BlastRiskLevel::Low);
        assert_eq!(risk_level_for_count(3), BlastRiskLevel::Low);
        assert_eq!(risk_level_for_count(4), BlastRiskLevel::Medium);
        assert_eq!(risk_level_for_count(10), BlastRiskLevel::Medium);
        assert_eq!(risk_level_for_count(11), BlastRiskLevel::High);
        assert_eq!(risk_level_for_count(25), BlastRiskLevel::High);
        assert_eq!(risk_level_for_count(26), BlastRiskLevel::Critical);
        assert_eq!(risk_level_for_count(100), BlastRiskLevel::Critical);
    }

    #[test]
    fn clamp_depth_within_cap() {
        let (depth, clamped) = clamp_depth(5);
        assert_eq!(depth, 5);
        assert!(!clamped);
    }

    #[test]
    fn clamp_depth_exceeds_cap() {
        let (depth, clamped) = clamp_depth(15);
        assert_eq!(depth, MAX_DEPTH);
        assert!(clamped);
    }
}
