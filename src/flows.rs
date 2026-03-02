//! Entry point detection and execution flow tracing for `wonk flows`.
//!
//! Detects entry points via SQL anti-join (functions/methods with no indexed
//! callers) and traces execution flows via BFS callee expansion.

use std::collections::{HashSet, VecDeque};
use std::str::FromStr;

use anyhow::Result;
use rusqlite::Connection;

use crate::types::{ExecutionFlow, FlowStep, SymbolKind};

/// Default BFS traversal depth.
pub const DEFAULT_DEPTH: usize = 10;

/// Maximum allowed BFS traversal depth.
pub const MAX_DEPTH: usize = 20;

/// Default maximum callees to follow per symbol.
pub const DEFAULT_BRANCHING: usize = 4;

/// Minimum number of steps for a flow to be included in output.
pub const MIN_FLOW_STEPS: usize = 2;

/// Options for flow detection and tracing.
#[derive(Debug, Clone)]
pub struct FlowOptions {
    /// Maximum BFS depth (clamped to MAX_DEPTH).
    pub depth: usize,
    /// Maximum callees to follow per symbol.
    pub branching: usize,
    /// Minimum confidence threshold for callee edges.
    pub min_confidence: Option<f64>,
    /// Restrict entry point detection to this file.
    pub from_file: Option<String>,
}

impl Default for FlowOptions {
    fn default() -> Self {
        Self {
            depth: DEFAULT_DEPTH,
            branching: DEFAULT_BRANCHING,
            min_confidence: None,
            from_file: None,
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

/// Detect entry points: functions/methods with no indexed callers.
///
/// Returns symbols sorted by file then line number.
pub fn detect_entry_points(conn: &Connection, options: &FlowOptions) -> Result<Vec<FlowStep>> {
    let conf_threshold = sanitize_confidence(options.min_confidence);

    // Build query dynamically: add file filter when from_file is set.
    let (sql, params) = if let Some(ref from_file) = options.from_file {
        (
            "SELECT s.id, s.name, s.kind, s.file, s.line \
             FROM symbols s \
             WHERE s.kind IN ('function', 'method') \
             AND s.file = ?1 \
             AND NOT EXISTS (\
                 SELECT 1 FROM \"references\" r \
                 WHERE r.name = s.name AND r.caller_id IS NOT NULL AND r.confidence >= ?2\
             ) \
             ORDER BY s.file, s.line"
                .to_string(),
            vec![
                rusqlite::types::Value::Text(from_file.clone()),
                rusqlite::types::Value::Real(conf_threshold),
            ],
        )
    } else {
        (
            "SELECT s.id, s.name, s.kind, s.file, s.line \
             FROM symbols s \
             WHERE s.kind IN ('function', 'method') \
             AND NOT EXISTS (\
                 SELECT 1 FROM \"references\" r \
                 WHERE r.name = s.name AND r.caller_id IS NOT NULL AND r.confidence >= ?1\
             ) \
             ORDER BY s.file, s.line"
                .to_string(),
            vec![rusqlite::types::Value::Real(conf_threshold)],
        )
    };

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        Ok((
            row.get::<_, String>(1)?, // name
            row.get::<_, String>(2)?, // kind
            row.get::<_, String>(3)?, // file
            row.get::<_, i64>(4)?,    // line
        ))
    })?;

    let mut steps = Vec::new();
    for row in rows {
        let (name, kind_str, file, line) = row?;
        let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
        steps.push(FlowStep {
            name,
            kind,
            file,
            line: line as usize,
            depth: 0,
        });
    }

    Ok(steps)
}

/// Trace the execution flow from an entry point via BFS callee expansion.
///
/// Returns `None` if the entry point is not found or the resulting flow has
/// fewer than [`MIN_FLOW_STEPS`] steps.
pub fn trace_flow(
    conn: &Connection,
    entry_name: &str,
    options: &FlowOptions,
) -> Result<Option<ExecutionFlow>> {
    let conf_threshold = sanitize_confidence(options.min_confidence);
    let max_depth = options.depth.min(MAX_DEPTH);

    // Resolve the entry point from the symbols table.
    let entry = {
        let mut stmt = conn.prepare(
            "SELECT name, kind, file, line FROM symbols \
             WHERE name = ?1 AND kind IN ('function', 'method') \
             LIMIT 1",
        )?;

        let row = stmt
            .query_row(rusqlite::params![entry_name], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .ok();

        match row {
            Some((name, kind_str, file, line)) => {
                let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
                FlowStep {
                    name,
                    kind,
                    file,
                    line: line as usize,
                    depth: 0,
                }
            }
            None => return Ok(None),
        }
    };

    let mut steps: Vec<FlowStep> = vec![entry.clone()];
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(entry.name.clone());

    let mut queue: VecDeque<(String, usize)> = VecDeque::new();
    queue.push_back((entry.name.clone(), 0));

    // BFS callee expansion with branching limit.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT r.name, s2.kind, s2.file, s2.line \
         FROM \"references\" r \
         JOIN symbols s ON s.id = r.caller_id \
         LEFT JOIN symbols s2 ON s2.name = r.name AND s2.kind IN ('function', 'method') \
         WHERE s.name = ?1 AND r.confidence >= ?2 \
         ORDER BY r.confidence DESC \
         LIMIT ?3",
    )?;

    while let Some((current_name, current_depth)) = queue.pop_front() {
        if current_depth >= max_depth {
            continue;
        }

        let rows: Vec<_> = stmt
            .query_map(
                rusqlite::params![&current_name, conf_threshold, options.branching as i64],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,         // callee name
                        row.get::<_, Option<String>>(1)?, // kind (may be NULL if no matching symbol)
                        row.get::<_, Option<String>>(2)?, // file
                        row.get::<_, Option<i64>>(3)?,    // line
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;

        for (callee_name, kind_str_opt, file_opt, line_opt) in rows {
            if visited.contains(&callee_name) {
                continue;
            }
            visited.insert(callee_name.clone());

            let kind = kind_str_opt
                .as_deref()
                .and_then(|s| SymbolKind::from_str(s).ok())
                .unwrap_or(SymbolKind::Function);

            let file = file_opt.unwrap_or_default();
            let line = line_opt.unwrap_or(0) as usize;

            let step = FlowStep {
                name: callee_name.clone(),
                kind,
                file,
                line,
                depth: current_depth + 1,
            };
            steps.push(step);
            queue.push_back((callee_name, current_depth + 1));
        }
    }

    if steps.len() < MIN_FLOW_STEPS {
        return Ok(None);
    }

    let step_count = steps.len();
    Ok(Some(ExecutionFlow {
        entry_point: entry,
        steps,
        step_count,
    }))
}

/// Sanitize a user-provided confidence threshold to a valid [0.0, 1.0] range.
fn sanitize_confidence(min_confidence: Option<f64>) -> f64 {
    match min_confidence {
        Some(c) if c.is_nan() || c.is_infinite() => 0.0,
        Some(c) => c.clamp(0.0, 1.0),
        None => 0.0,
    }
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

    /// Create a multi-file indexed repo.
    fn make_multi_file_repo(files: &[(&str, &str)]) -> (TempDir, Connection) {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        for (path, content) in files {
            let full_path = root.join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(full_path, content).unwrap();
        }

        pipeline::build_index(root, true).unwrap();

        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();
        (dir, conn)
    }

    // -- clamp_depth tests ----------------------------------------------------

    #[test]
    fn clamp_depth_within_max() {
        let (depth, clamped) = clamp_depth(10);
        assert_eq!(depth, 10);
        assert!(!clamped);
    }

    #[test]
    fn clamp_depth_exceeds_max() {
        let (depth, clamped) = clamp_depth(25);
        assert_eq!(depth, MAX_DEPTH);
        assert!(clamped);
    }

    #[test]
    fn clamp_depth_at_boundary() {
        let (depth, clamped) = clamp_depth(20);
        assert_eq!(depth, 20);
        assert!(!clamped);
    }

    // -- detect_entry_points tests --------------------------------------------

    #[test]
    fn detect_entry_points_basic() {
        // foo calls bar, so bar is NOT an entry point. foo IS an entry point.
        let source = r#"
fn foo() {
    bar();
}

fn bar() {
    println!("hello");
}
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let opts = FlowOptions::default();
        let entries = detect_entry_points(&conn, &opts).unwrap();

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"foo"),
            "foo should be an entry point (no callers)"
        );
        assert!(
            !names.contains(&"bar"),
            "bar should NOT be an entry point (called by foo)"
        );
    }

    #[test]
    fn detect_entry_points_all_called() {
        // a calls b, b calls a. Both have callers, so no entry points from these.
        let source = r#"
fn a() {
    b();
}

fn b() {
    a();
}
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let opts = FlowOptions::default();
        let entries = detect_entry_points(&conn, &opts).unwrap();

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            !names.contains(&"a"),
            "a has a caller (b), so not an entry point"
        );
        assert!(
            !names.contains(&"b"),
            "b has a caller (a), so not an entry point"
        );
    }

    #[test]
    fn detect_entry_points_multiple() {
        // main and handler both call helper. main and handler are entry points.
        let source = r#"
fn main() {
    helper();
}

fn handler() {
    helper();
}

fn helper() {
    println!("work");
}
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let opts = FlowOptions::default();
        let entries = detect_entry_points(&conn, &opts).unwrap();

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"main"), "main should be an entry point");
        assert!(
            names.contains(&"handler"),
            "handler should be an entry point"
        );
        assert!(
            !names.contains(&"helper"),
            "helper should NOT be an entry point"
        );
    }

    #[test]
    fn detect_entry_points_from_file() {
        // Two files: main.rs has main(), lib.rs has handler(). Both are entry points.
        // With from_file filtering, only the matching file's entries appear.
        let files = &[
            ("src/main.rs", "fn main() {\n    handler();\n}\n"),
            ("src/lib.rs", "fn handler() {\n    println!(\"hi\");\n}\n"),
        ];
        let (_dir, conn) = make_multi_file_repo(files);

        // Filter to src/main.rs only.
        let opts = FlowOptions {
            from_file: Some("src/main.rs".to_string()),
            ..FlowOptions::default()
        };
        let entries = detect_entry_points(&conn, &opts).unwrap();

        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"main"),
            "main should be detected from src/main.rs"
        );
        // handler is in src/lib.rs, not main.rs, so should not appear.
        assert!(
            !names.contains(&"handler"),
            "handler is in lib.rs, should not appear for main.rs filter"
        );
    }

    // -- trace_flow tests -----------------------------------------------------

    #[test]
    fn trace_flow_basic() {
        // main -> dispatch -> open_db. Should trace all three.
        let source = r#"
fn main() {
    dispatch();
}

fn dispatch() {
    open_db();
}

fn open_db() {
    println!("db");
}
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let opts = FlowOptions::default();
        let flow = trace_flow(&conn, "main", &opts).unwrap();

        assert!(flow.is_some(), "should find a flow from main");
        let flow = flow.unwrap();
        assert_eq!(flow.entry_point.name, "main");
        assert!(flow.step_count >= 2, "should have at least 2 steps");

        let names: Vec<&str> = flow.steps.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"main"), "flow should include main");
        assert!(names.contains(&"dispatch"), "flow should include dispatch");
    }

    #[test]
    fn trace_flow_depth_cap() {
        // Chain: a -> b -> c -> d. With depth=1, should only get a and b.
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
        let opts = FlowOptions {
            depth: 1,
            ..FlowOptions::default()
        };
        let flow = trace_flow(&conn, "a", &opts).unwrap();

        assert!(flow.is_some(), "should find a flow from a");
        let flow = flow.unwrap();
        // At depth 1, we get a (depth 0) and b (depth 1). c and d should be excluded.
        let names: Vec<&str> = flow.steps.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert!(!names.contains(&"c"), "c should be excluded at depth 1");
        assert!(!names.contains(&"d"), "d should be excluded at depth 1");
    }

    #[test]
    fn trace_flow_branching_limit() {
        // a calls b, c, d, e. With branching=2, should only follow 2 callees.
        let source = r#"
fn a() {
    b();
    c();
    d();
    e();
}

fn b() { }
fn c() { }
fn d() { }
fn e() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let opts = FlowOptions {
            branching: 2,
            ..FlowOptions::default()
        };
        let flow = trace_flow(&conn, "a", &opts).unwrap();

        assert!(flow.is_some(), "should find a flow from a");
        let flow = flow.unwrap();
        // Entry + at most 2 callees = at most 3 steps.
        assert!(
            flow.step_count <= 3,
            "branching=2 should limit to at most 3 steps, got {}",
            flow.step_count
        );
    }

    #[test]
    fn trace_flow_single_step_excluded() {
        // Leaf function with no callees: should produce None (1 step < MIN_FLOW_STEPS).
        let source = "fn leaf() { let x = 1; }\n";
        let (_dir, conn) = make_indexed_repo(source);
        let opts = FlowOptions::default();
        let flow = trace_flow(&conn, "leaf", &opts).unwrap();

        assert!(
            flow.is_none(),
            "single-step flow should be excluded (less than MIN_FLOW_STEPS)"
        );
    }

    #[test]
    fn trace_flow_cycle_terminates() {
        // a -> b -> a (mutual recursion). Should terminate without infinite loop.
        let source = r#"
fn a() {
    b();
}

fn b() {
    a();
}
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let opts = FlowOptions::default();
        let flow = trace_flow(&conn, "a", &opts).unwrap();

        assert!(flow.is_some(), "should find a flow despite cycle");
        let flow = flow.unwrap();
        // a and b, no duplicates.
        let names: Vec<&str> = flow.steps.iter().map(|s| s.name.as_str()).collect();
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "no duplicate steps in flow");
    }

    #[test]
    fn trace_flow_min_confidence() {
        // Same-file references have confidence ~0.85. Filtering at 0.9 should exclude.
        let source = r#"
fn foo() {
    bar();
}

fn bar() {
    println!("hello");
}
"#;
        let (_dir, conn) = make_indexed_repo(source);

        // With high min_confidence, the callee edge is excluded.
        let opts = FlowOptions {
            min_confidence: Some(0.9),
            ..FlowOptions::default()
        };
        let flow = trace_flow(&conn, "foo", &opts).unwrap();
        assert!(
            flow.is_none(),
            "high min_confidence should exclude all edges, resulting in single-step (None)"
        );

        // With low min_confidence, the callee edge is included.
        let opts = FlowOptions {
            min_confidence: Some(0.5),
            ..FlowOptions::default()
        };
        let flow = trace_flow(&conn, "foo", &opts).unwrap();
        assert!(
            flow.is_some(),
            "low min_confidence should include edges, producing a flow"
        );
    }

    #[test]
    fn trace_flow_nonexistent_entry() {
        let source = "fn foo() { }\n";
        let (_dir, conn) = make_indexed_repo(source);
        let opts = FlowOptions::default();
        let flow = trace_flow(&conn, "nonexistent_xyz", &opts).unwrap();
        assert!(flow.is_none(), "nonexistent entry point should return None");
    }

    #[test]
    fn flow_options_default_values() {
        let opts = FlowOptions::default();
        assert_eq!(opts.depth, DEFAULT_DEPTH);
        assert_eq!(opts.branching, DEFAULT_BRANCHING);
        assert!(opts.min_confidence.is_none());
        assert!(opts.from_file.is_none());
    }
}
