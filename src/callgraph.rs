//! Call graph traversal for `wonk callers` and `wonk callees`.
//!
//! Queries the `references.caller_id` join to find which functions call a
//! given symbol (callers) or which symbols are called by a given function
//! (callees). Supports transitive expansion via BFS up to a configurable
//! depth cap.

use std::collections::{HashSet, VecDeque};
use std::str::FromStr;

use anyhow::Result;
use rusqlite::Connection;

use crate::types::{CalleeResult, CallerResult, SymbolKind};

/// Maximum allowed depth for transitive expansion.
pub const MAX_DEPTH_CAP: usize = 10;

/// Check whether the index has any `caller_id` data populated.
///
/// Returns `false` for old indexes that were built before call graph support
/// was added, indicating the user should re-index with `wonk update`.
pub fn has_caller_id_data(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM \"references\" WHERE caller_id IS NOT NULL LIMIT 1)",
        [],
        |row| row.get(0),
    )
    .unwrap_or(false)
}

/// Find all callers of the given symbol name, with BFS transitive expansion.
///
/// At depth 1, returns direct callers (functions whose body references `name`).
/// At depth N > 1, also returns callers of callers up to N levels.
pub fn callers(conn: &Connection, name: &str, max_depth: usize) -> Result<Vec<CallerResult>> {
    let mut results = Vec::new();
    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    queue.push_back((name.to_string(), 1));

    while let Some((target_name, depth)) = queue.pop_front() {
        if depth > max_depth {
            continue;
        }

        let mut stmt = conn.prepare(
            "SELECT DISTINCT s.name, s.kind, s.file, s.line, s.signature, r.file AS ref_file \
             FROM \"references\" r \
             JOIN symbols s ON r.caller_id = s.id \
             WHERE r.name = ?1",
        )?;

        let rows: Vec<_> = stmt
            .query_map(rusqlite::params![&target_name], |row| {
                Ok((
                    row.get::<_, String>(0)?, // caller name
                    row.get::<_, String>(1)?, // caller kind
                    row.get::<_, String>(2)?, // caller file
                    row.get::<_, i64>(3)?,    // caller line
                    row.get::<_, String>(4)?, // caller signature
                    row.get::<_, String>(5)?, // ref file (where the call happens)
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        for (caller_name, kind_str, file, line, signature, ref_file) in rows {
            let key = (caller_name.clone(), file.clone());
            if visited.contains(&key) {
                continue;
            }
            visited.insert(key);

            let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);

            results.push(CallerResult {
                caller_name: caller_name.clone(),
                caller_kind: kind,
                file: file.clone(),
                line: line as usize,
                signature,
                depth,
                target_file: Some(ref_file),
            });

            // Enqueue for next depth level.
            if depth < max_depth {
                queue.push_back((caller_name, depth + 1));
            }
        }
    }

    // Sort by depth, then file, then line for deterministic output.
    results.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    Ok(results)
}

/// Find all callees of the given symbol name, with BFS transitive expansion.
///
/// At depth 1, returns direct callees (symbols referenced within the body of
/// functions named `name`). At depth N > 1, also returns callees of callees.
pub fn callees(conn: &Connection, name: &str, max_depth: usize) -> Result<Vec<CalleeResult>> {
    let mut results = Vec::new();
    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    queue.push_back((name.to_string(), 1));

    while let Some((source_name, depth)) = queue.pop_front() {
        if depth > max_depth {
            continue;
        }

        let mut stmt = conn.prepare(
            "SELECT DISTINCT r.name, r.file, r.line, r.context \
             FROM \"references\" r \
             WHERE r.caller_id IN (SELECT id FROM symbols WHERE name = ?1)",
        )?;

        let rows: Vec<_> = stmt
            .query_map(rusqlite::params![&source_name], |row| {
                Ok((
                    row.get::<_, String>(0)?,         // callee name
                    row.get::<_, String>(1)?,         // ref file
                    row.get::<_, i64>(2)?,            // ref line
                    row.get::<_, Option<String>>(3)?, // context
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        for (callee_name, file, line, context) in rows {
            let key = (callee_name.clone(), file.clone());
            if visited.contains(&key) {
                continue;
            }
            visited.insert(key);

            results.push(CalleeResult {
                callee_name: callee_name.clone(),
                file: file.clone(),
                line: line as usize,
                context: context.unwrap_or_default(),
                depth,
                source_file: None,
            });

            // Enqueue for next depth level.
            if depth < max_depth {
                queue.push_back((callee_name, depth + 1));
            }
        }
    }

    // Sort by depth, then file, then line for deterministic output.
    results.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    Ok(results)
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

    #[test]
    fn callers_basic() {
        // foo calls bar, so callers("bar") should return foo.
        let source = r#"
fn foo() {
    bar();
}

fn bar() {
    println!("hello");
}
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let results = callers(&conn, "bar", 1).unwrap();

        assert!(
            !results.is_empty(),
            "callers('bar') should find foo as a caller"
        );
        let names: Vec<&str> = results.iter().map(|r| r.caller_name.as_str()).collect();
        assert!(names.contains(&"foo"), "foo should be a caller of bar");
        assert_eq!(results[0].depth, 1);
    }

    #[test]
    fn callers_empty() {
        // No one calls standalone_fn.
        let source = "fn standalone_fn() { }\n";
        let (_dir, conn) = make_indexed_repo(source);
        let results = callers(&conn, "standalone_fn", 1).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn callers_transitive_depth_2() {
        // a calls b, b calls c. callers("c", 2) should return both b (depth 1) and a (depth 2).
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
        let results = callers(&conn, "c", 2).unwrap();

        let names: Vec<&str> = results.iter().map(|r| r.caller_name.as_str()).collect();
        assert!(names.contains(&"b"), "b should be a direct caller of c");
        assert!(names.contains(&"a"), "a should be a transitive caller of c");

        let b_result = results.iter().find(|r| r.caller_name == "b").unwrap();
        let a_result = results.iter().find(|r| r.caller_name == "a").unwrap();
        assert_eq!(b_result.depth, 1);
        assert_eq!(a_result.depth, 2);
    }

    #[test]
    fn callers_depth_1_no_expand() {
        // Same chain a->b->c, but depth=1 should only return b.
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
        let results = callers(&conn, "c", 1).unwrap();

        let names: Vec<&str> = results.iter().map(|r| r.caller_name.as_str()).collect();
        assert!(names.contains(&"b"));
        assert!(!names.contains(&"a"), "a should NOT appear at depth 1");
    }

    #[test]
    fn callees_basic() {
        // foo calls bar and baz.
        let source = r#"
fn foo() {
    bar();
    baz();
}

fn bar() { }
fn baz() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let results = callees(&conn, "foo", 1).unwrap();

        let names: Vec<&str> = results.iter().map(|r| r.callee_name.as_str()).collect();
        assert!(names.contains(&"bar"), "bar should be a callee of foo");
        assert!(names.contains(&"baz"), "baz should be a callee of foo");
    }

    #[test]
    fn callees_empty() {
        // leaf_fn calls nothing.
        let source = "fn leaf_fn() { let x = 1; }\n";
        let (_dir, conn) = make_indexed_repo(source);
        let results = callees(&conn, "leaf_fn", 1).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn callees_transitive_depth_2() {
        // a calls b, b calls c. callees("a", 2) should return both b and c.
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
        let results = callees(&conn, "a", 2).unwrap();

        let names: Vec<&str> = results.iter().map(|r| r.callee_name.as_str()).collect();
        assert!(names.contains(&"b"), "b should be a direct callee of a");
        assert!(names.contains(&"c"), "c should be a transitive callee of a");
    }

    #[test]
    fn has_caller_id_data_true() {
        let source = r#"
fn foo() {
    bar();
}

fn bar() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        assert!(has_caller_id_data(&conn));
    }

    #[test]
    fn has_caller_id_data_false() {
        let source = r#"
fn foo() {
    bar();
}

fn bar() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);

        // Null out all caller_ids to simulate an old index.
        conn.execute("UPDATE \"references\" SET caller_id = NULL", [])
            .unwrap();

        assert!(!has_caller_id_data(&conn));
    }

    #[test]
    fn cycle_detection() {
        // a calls b, b calls a (mutual recursion). Should not infinite loop.
        let source = r#"
fn a() {
    b();
}

fn b() {
    a();
}
"#;
        let (_dir, conn) = make_indexed_repo(source);

        // callers of "a" at depth 5 should terminate (visited set prevents cycles).
        let results = callers(&conn, "a", 5).unwrap();
        // b is a caller of a, a is a caller of b.
        let names: Vec<&str> = results.iter().map(|r| r.caller_name.as_str()).collect();
        assert!(names.contains(&"b"), "b should be a caller of a");
        // Should not have duplicates.
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "no duplicate callers");

        // callees of "a" at depth 5 should also terminate.
        let results = callees(&conn, "a", 5).unwrap();
        let names: Vec<&str> = results.iter().map(|r| r.callee_name.as_str()).collect();
        assert!(names.contains(&"b"), "b should be a callee of a");
        let unique: HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "no duplicate callees");
    }

    #[test]
    fn multiple_definitions() {
        // Two files with same function name "helper", both called by different functions.
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::create_dir(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();

        fs::write(
            root.join("src/lib.rs"),
            "fn caller_a() {\n    helper();\n}\n\nfn helper() { }\n",
        )
        .unwrap();
        fs::write(
            root.join("src/other.rs"),
            "fn caller_b() {\n    helper();\n}\n\nfn helper() { }\n",
        )
        .unwrap();

        pipeline::build_index(root, true).unwrap();
        let index_path = db::local_index_path(root);
        let conn = db::open_existing(&index_path).unwrap();

        let results = callers(&conn, "helper", 1).unwrap();
        let names: Vec<&str> = results.iter().map(|r| r.caller_name.as_str()).collect();
        assert!(
            names.contains(&"caller_a"),
            "caller_a should be a caller of helper"
        );
        assert!(
            names.contains(&"caller_b"),
            "caller_b should be a caller of helper"
        );
    }
}
