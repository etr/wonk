//! Blast radius analysis for `wonk blast`.
//!
//! Performs depth-annotated BFS from a target symbol to discover all affected
//! symbols, grouping them by severity tier (depth) and computing an overall
//! risk level. Supports upstream (callers + type hierarchy children) and
//! downstream (callees) traversal directions.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use anyhow::Result;
use rusqlite::Connection;

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
    /// Whether qualifying queries may be answered from the precomputed
    /// reach table (default: true; PRD-REACH-REQ-003).
    pub use_reach: bool,
}

impl Default for BlastOptions {
    fn default() -> Self {
        Self {
            depth: DEFAULT_DEPTH,
            direction: BlastDirection::Upstream,
            include_tests: false,
            min_confidence: None,
            use_reach: true,
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

/// Try to add a discovered symbol to the affected set.
///
/// Visited/queued dedup and enqueue run through the shared
/// [`crate::reach::NameBfs`] traversal core. Edge eligibility (confidence
/// floor, test-file exclusion) is decided by [`crate::reach::edge_eligible`]
/// before this is called, so both the live BFS and the precomputed build
/// share one predicate and one enqueue rule (AR-021).
fn push_if_new(
    bfs: &mut crate::reach::NameBfs,
    affected: &mut Vec<BlastAffectedSymbol>,
    sym: BlastAffectedSymbol,
    max_depth: usize,
) {
    if bfs.admit(&sym.name, &sym.file, sym.depth, max_depth) {
        affected.push(sym);
    }
}

/// Sort, tier, and summarize a discovered affected set into a
/// [`BlastAnalysis`]. Shared by the live BFS path and the precomputed
/// reach-table path so both produce identical output shape.
fn assemble_analysis(
    target: &str,
    direction: BlastDirection,
    affected: Vec<BlastAffectedSymbol>,
    truncated: bool,
) -> BlastAnalysis {
    // Sort by depth, then file, then line for deterministic output.
    let mut affected = affected;
    affected.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    // Group into severity tiers using HashMap for clarity.
    let mut tier_map: HashMap<BlastSeverity, Vec<BlastAffectedSymbol>> = HashMap::new();
    for sym in &affected {
        tier_map
            .entry(severity_for_depth(sym.depth))
            .or_default()
            .push(sym.clone());
    }

    // Emit tiers in severity order: WillBreak, LikelyAffected, MayNeedTesting.
    let tiers: Vec<BlastTier> = [
        BlastSeverity::WillBreak,
        BlastSeverity::LikelyAffected,
        BlastSeverity::MayNeedTesting,
    ]
    .into_iter()
    .filter_map(|severity| {
        tier_map
            .remove(&severity)
            .map(|symbols| BlastTier { severity, symbols })
    })
    .collect();

    // Deduplicated affected files.
    let mut affected_files: Vec<String> = affected
        .iter()
        .map(|s| s.file.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    affected_files.sort();

    let total_affected = affected.len();

    BlastAnalysis {
        target: target.to_string(),
        direction,
        risk_level: risk_level_for_count(total_affected),
        total_affected,
        tiers,
        affected_files,
        truncated,
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
            truncated: false,
        });
    }

    let conf_threshold = sanitize_confidence(options.min_confidence);
    let max_depth = options.depth;

    // Routing matrix (PRD-REACH-REQ-003/004/006/007): the table answers iff
    // the kill switch allows it, the query is upstream with default
    // semantics (no test inclusion, no confidence narrowing), and the table
    // is built deep enough and not stale. Otherwise: unchanged BFS.
    if options.use_reach
        && options.direction == BlastDirection::Upstream
        && !options.include_tests
        && conf_threshold <= 0.0
        && let Some(answer) = crate::reach::lookup_upstream(conn, symbol, max_depth)?
    {
        return Ok(assemble_analysis(
            symbol,
            options.direction,
            answer.affected,
            answer.truncated,
        ));
    }

    let filter = crate::reach::EdgeFilter {
        min_confidence: conf_threshold,
        include_tests: options.include_tests,
    };

    let mut affected: Vec<BlastAffectedSymbol> = Vec::new();
    // Shared visited/queued/FIFO state — the same NameBfs core the reach
    // build traverses with, so both engines enqueue identically (AR-021).
    let mut bfs = crate::reach::NameBfs::new(symbol);

    match options.direction {
        BlastDirection::Upstream => {
            // Deterministic candidate order: (file, line, confidence DESC) so
            // a caller with several refs records the max confidence and
            // duplicate (name, file) rows record the min-line representative.
            let mut stmt_callers = conn.prepare(
                "SELECT DISTINCT s.name, s.kind, s.file, s.line, r.confidence \
                 FROM \"references\" r \
                 JOIN symbols s ON r.caller_id = s.id \
                 WHERE r.name = ?1 \
                 ORDER BY s.file, s.line, r.confidence DESC",
            )?;

            // Type hierarchy children: include direct children of the target
            // symbol only (PRD-HRTG-REQ-003 scopes this to depth-1 dependants).
            let mut stmt_children = conn.prepare(
                "SELECT DISTINCT child.name, child.kind, child.file, child.line \
                 FROM type_edges te \
                 JOIN symbols parent ON te.parent_id = parent.id \
                 JOIN symbols child ON te.child_id = child.id \
                 WHERE parent.name = ?1 \
                 ORDER BY child.file, child.line",
            )?;

            while let Some((target_name, depth)) = bfs.pop() {
                if depth > max_depth {
                    continue;
                }

                let rows: Vec<_> = stmt_callers
                    .query_map(rusqlite::params![&target_name], |row| {
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
                    if !crate::reach::edge_eligible(&file, confidence, &filter) {
                        continue;
                    }
                    let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
                    push_if_new(
                        &mut bfs,
                        &mut affected,
                        BlastAffectedSymbol {
                            name,
                            kind,
                            file,
                            line: line as usize,
                            depth,
                            confidence,
                        },
                        max_depth,
                    );
                }

                // Include type hierarchy children only for the initial target
                // symbol (depth == 1), per PRD-HRTG-REQ-003.
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
                        if !crate::reach::edge_eligible(&file, 1.0, &filter) {
                            continue;
                        }
                        let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
                        push_if_new(
                            &mut bfs,
                            &mut affected,
                            BlastAffectedSymbol {
                                name,
                                kind,
                                file,
                                line: line as usize,
                                depth,
                                confidence: 1.0,
                            },
                            max_depth,
                        );
                    }
                }
            }
        }
        BlastDirection::Downstream => {
            let mut stmt_callees = conn.prepare(
                "SELECT DISTINCT r.name, s_def.kind, r.file, r.line, r.confidence \
                 FROM \"references\" r \
                 JOIN symbols s ON s.id = r.caller_id \
                 LEFT JOIN symbols s_def ON s_def.name = r.name \
                 WHERE s.name = ?1 \
                 ORDER BY r.file, r.line, r.confidence DESC",
            )?;

            while let Some((target_name, depth)) = bfs.pop() {
                if depth > max_depth {
                    continue;
                }

                let rows: Vec<_> = stmt_callees
                    .query_map(rusqlite::params![&target_name], |row| {
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
                    if !crate::reach::edge_eligible(&file, confidence, &filter) {
                        continue;
                    }
                    let kind = kind_str
                        .as_deref()
                        .and_then(|k| SymbolKind::from_str(k).ok())
                        .unwrap_or(SymbolKind::Function);
                    push_if_new(
                        &mut bfs,
                        &mut affected,
                        BlastAffectedSymbol {
                            name,
                            kind,
                            file,
                            line: line as usize,
                            depth,
                            confidence,
                        },
                        max_depth,
                    );
                }
            }
        }
    }

    Ok(assemble_analysis(
        symbol,
        options.direction,
        affected,
        false,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::pipeline;
    use std::fs;
    use std::path::Path;
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

    // -- Determinism tests (TASK-080 refactor) -----------------------------

    /// Index a repo, then insert a second reference from the same caller to
    /// the same name at a lower confidence. The deterministic traversal must
    /// record the caller once with the MAX confidence among its refs.
    #[test]
    fn blast_records_max_confidence_per_caller() {
        let source = "fn foo() { bar(); }\nfn bar() { }\n";
        let (_dir, conn) = make_indexed_repo(source);

        let foo_id: i64 = conn
            .query_row("SELECT id FROM symbols WHERE name = 'foo'", [], |row| {
                row.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, caller_id, confidence) \
             VALUES ('bar', 'src/lib.rs', 99, 10, ?1, 0.5)",
            rusqlite::params![foo_id],
        )
        .unwrap();

        let result = analyze_blast(&conn, "bar", &BlastOptions::default()).unwrap();
        let foo: Vec<&BlastAffectedSymbol> = result
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter())
            .filter(|s| s.name == "foo")
            .collect();
        assert_eq!(foo.len(), 1, "caller recorded exactly once");
        assert_eq!(foo[0].confidence, 0.85, "max confidence wins");
    }

    /// Two same-named caller symbols in one file: the (file, line)-min row is
    /// the deterministic representative and its own confidence is recorded.
    #[test]
    fn blast_duplicate_name_file_records_min_line_row() {
        let source = "fn foo() { bar(); }\nfn bar() { }\n";
        let (_dir, conn) = make_indexed_repo(source);

        // The pipeline-indexed foo sits at line 1; plant a second foo at a
        // later line with a higher-confidence edge to bar.
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) \
             VALUES ('foo', 'function', 'src/lib.rs', 50, 1, 'rust')",
            [],
        )
        .unwrap();
        let later_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO \"references\" (name, file, line, col, caller_id, confidence) \
             VALUES ('bar', 'src/lib.rs', 51, 5, ?1, 0.95)",
            rusqlite::params![later_id],
        )
        .unwrap();

        let result = analyze_blast(&conn, "bar", &BlastOptions::default()).unwrap();
        let foos: Vec<&BlastAffectedSymbol> = result
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter())
            .filter(|s| s.name == "foo")
            .collect();
        assert_eq!(foos.len(), 1, "duplicate (name, file) recorded once");
        assert_eq!(foos[0].line, 1, "min-line row is the representative");
        assert_eq!(foos[0].confidence, 0.85, "representative's own confidence");
    }

    // -- Reach-table routing tests (TASK-080) --------------------------------

    /// Repo whose reach table is prepared by hand: a bogus row that only the
    /// table path could ever return. The BFS (no ref exists) finds nothing.
    fn make_table_repo() -> (TempDir, Connection) {
        let source = "fn target() { }\nfn other() { }\n";
        let (dir, conn) = make_indexed_repo(source);

        let target_id: i64 = conn
            .query_row("SELECT id FROM symbols WHERE name = 'target'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let other_id: i64 = conn
            .query_row("SELECT id FROM symbols WHERE name = 'other'", [], |row| {
                row.get(0)
            })
            .unwrap();

        // The pipeline build already populated the real (empty) table for
        // this fixture; replace its contents rather than insert alongside.
        conn.execute("DELETE FROM reach", []).unwrap();
        conn.execute("DELETE FROM reach_truncated", []).unwrap();
        conn.execute(
            "INSERT INTO reach (source_id, target_id, min_depth, confidence) \
             VALUES (?1, ?2, 1, 0.42)",
            rusqlite::params![target_id, other_id],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO reach_meta (key, value) VALUES ('built_depth', '3')",
            [],
        )
        .unwrap();
        (dir, conn)
    }

    fn names(result: &BlastAnalysis) -> Vec<String> {
        result
            .tiers
            .iter()
            .flat_map(|t| t.symbols.iter().map(|s| s.name.clone()))
            .collect()
    }

    #[test]
    fn blast_uses_reach_table_when_eligible() {
        let (_dir, conn) = make_table_repo();
        // Default options: table answers with the planted bogus row.
        let result = analyze_blast(&conn, "target", &BlastOptions::default()).unwrap();
        assert_eq!(
            names(&result),
            vec!["other".to_string()],
            "the planted reach row is only reachable via the table path"
        );
        assert!(!result.truncated);
    }

    #[test]
    fn blast_falls_back_to_bfs_when_reach_disabled() {
        let (_dir, conn) = make_table_repo();
        let options = BlastOptions {
            use_reach: false,
            ..Default::default()
        };
        let result = analyze_blast(&conn, "target", &options).unwrap();
        assert!(names(&result).is_empty(), "BFS sees no real callers");
    }

    #[test]
    fn blast_falls_back_to_bfs_for_include_tests() {
        let (_dir, conn) = make_table_repo();
        let options = BlastOptions {
            include_tests: true,
            ..Default::default()
        };
        let result = analyze_blast(&conn, "target", &options).unwrap();
        assert!(
            names(&result).is_empty(),
            "include_tests changes semantics: must BFS, not use the table"
        );
    }

    #[test]
    fn blast_falls_back_to_bfs_for_min_confidence() {
        let (_dir, conn) = make_table_repo();
        let options = BlastOptions {
            min_confidence: Some(0.9),
            ..Default::default()
        };
        let result = analyze_blast(&conn, "target", &options).unwrap();
        assert!(
            names(&result).is_empty(),
            "confidence-filtered queries change min-depths: must BFS"
        );

        // A non-filtering threshold of 0 stays on the table.
        let options = BlastOptions {
            min_confidence: Some(0.0),
            ..Default::default()
        };
        let result = analyze_blast(&conn, "target", &options).unwrap();
        assert_eq!(names(&result), vec!["other".to_string()]);
    }

    #[test]
    fn blast_falls_back_to_bfs_beyond_built_depth() {
        let (_dir, conn) = make_table_repo();
        let options = BlastOptions {
            depth: 4,
            ..Default::default()
        };
        let result = analyze_blast(&conn, "target", &options).unwrap();
        assert!(
            names(&result).is_empty(),
            "beyond built_depth the table cannot answer (REQ-004)"
        );
    }

    #[test]
    fn blast_falls_back_to_bfs_for_downstream() {
        let (_dir, conn) = make_table_repo();
        let options = BlastOptions {
            direction: BlastDirection::Downstream,
            ..Default::default()
        };
        let result = analyze_blast(&conn, "target", &options).unwrap();
        assert!(
            names(&result).is_empty(),
            "the table materializes upstream only"
        );
    }

    #[test]
    fn blast_falls_back_to_bfs_when_stale() {
        let (_dir, conn) = make_table_repo();
        conn.execute(
            "INSERT INTO reach_meta (key, value) VALUES ('stale', '1')",
            [],
        )
        .unwrap();
        let result = analyze_blast(&conn, "target", &BlastOptions::default()).unwrap();
        assert!(names(&result).is_empty(), "stale table must not answer");
    }

    #[test]
    fn blast_reach_table_truncation_carries_through() {
        let (_dir, conn) = make_table_repo();
        let target_id: i64 = conn
            .query_row("SELECT id FROM symbols WHERE name = 'target'", [], |row| {
                row.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO reach_truncated (source_id) VALUES (?1)",
            rusqlite::params![target_id],
        )
        .unwrap();
        let result = analyze_blast(&conn, "target", &BlastOptions::default()).unwrap();
        assert!(result.truncated, "truncation marker reaches the analysis");
    }

    // -- End-to-end equivalence (TASK-080, AR-021) ---------------------------

    /// Over a pipeline-indexed multi-file repo, the default (table-routed)
    /// path and the BFS path must agree exactly at every depth 1..=3.
    #[test]
    fn blast_table_path_equivalent_to_bfs_across_depths() {
        let files = &[
            ("src/one.rs", "fn one() { two(); }\n"),
            ("src/two.rs", "fn two() { three(); }\n"),
            ("src/three.rs", "fn three() { four(); }\n"),
            ("src/four.rs", "fn four() { }\n"),
            ("src/deep.rs", "fn deep_caller() { one(); }\n"),
            ("tests/chain_test.rs", "fn chain_suite() { two(); }\n"),
        ];
        let (_dir, conn) = make_multi_file_repo(files);

        for target in ["four", "three", "two", "one", "deep_caller", "chain_suite"] {
            for depth in [1usize, 2, 3] {
                assert!(
                    crate::reach::lookup_upstream(&conn, target, depth)
                        .unwrap()
                        .is_some(),
                    "pipeline build must cover {target} at depth {depth}"
                );
                let via_table = analyze_blast(
                    &conn,
                    target,
                    &BlastOptions {
                        depth,
                        ..Default::default()
                    },
                )
                .unwrap();
                let via_bfs = analyze_blast(
                    &conn,
                    target,
                    &BlastOptions {
                        depth,
                        use_reach: false,
                        ..Default::default()
                    },
                )
                .unwrap();
                assert_eq!(via_table, via_bfs, "target {target} depth {depth}");
                assert!(!via_table.truncated);
            }
        }

        // The chain gives the depths real content: four reaches deep_caller
        // at depth 4, i.e. not at all within depth 3.
        let four = analyze_blast(&conn, "four", &BlastOptions::default()).unwrap();
        let names = names(&four);
        assert!(names.contains(&"three".to_string()), "depth-1 caller");
        assert!(names.contains(&"two".to_string()), "depth-2 caller");
        assert!(names.contains(&"one".to_string()), "depth-3 caller");
        assert!(
            !names.contains(&"deep_caller".to_string()),
            "depth-4 is beyond the built depth"
        );
    }
}
