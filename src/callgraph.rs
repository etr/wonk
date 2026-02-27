//! Call graph traversal for `wonk callers` and `wonk callees`.
//!
//! Queries the `references.caller_id` join to find which functions call a
//! given symbol (callers) or which symbols are called by a given function
//! (callees). Supports transitive expansion via BFS up to a configurable
//! depth cap.

use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

use crate::types::{CallPathHop, CalleeResult, CallerResult, SymbolKind};

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
/// File-scope call sites (where `caller_id` is NULL) are returned with
/// `caller_name` set to `"<module>"`.
pub fn callers(conn: &Connection, name: &str, max_depth: usize) -> Result<Vec<CallerResult>> {
    if max_depth == 0 {
        return Ok(Vec::new());
    }

    let mut results = Vec::new();
    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut queued: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    queue.push_back((name.to_string(), 1));
    queued.insert(name.to_string());

    // Prepare statements once, reuse across all BFS iterations.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.name, s.kind, s.file, s.line, s.signature, r.file AS ref_file \
         FROM \"references\" r \
         JOIN symbols s ON r.caller_id = s.id \
         WHERE r.name = ?1",
    )?;

    // File-scope callers: references with no enclosing function.
    let mut stmt_module = conn.prepare(
        "SELECT DISTINCT r.file, r.line \
         FROM \"references\" r \
         WHERE r.name = ?1 AND r.caller_id IS NULL",
    )?;

    while let Some((target_name, depth)) = queue.pop_front() {
        // Named callers (functions/methods).
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
                file,
                line: line as usize,
                signature,
                depth,
                target_file: Some(ref_file),
            });

            if depth < max_depth && !queued.contains(&caller_name) {
                queued.insert(caller_name.clone());
                queue.push_back((caller_name, depth + 1));
            }
        }

        // File-scope callers (only at depth 1 — modules don't have callers).
        if depth == 1 {
            let module_rows: Vec<_> = stmt_module
                .query_map(rusqlite::params![&target_name], |row| {
                    Ok((
                        row.get::<_, String>(0)?, // file
                        row.get::<_, i64>(1)?,    // line
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            for (file, line) in module_rows {
                let key = ("<module>".to_string(), file.clone());
                if visited.contains(&key) {
                    continue;
                }
                visited.insert(key);

                results.push(CallerResult {
                    caller_name: "<module>".to_string(),
                    caller_kind: SymbolKind::Module,
                    file: file.clone(),
                    line: line as usize,
                    signature: format!("<module> {file}"),
                    depth,
                    target_file: Some(file),
                });
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
    if max_depth == 0 {
        return Ok(Vec::new());
    }

    let mut results = Vec::new();
    let mut visited: HashSet<(String, String)> = HashSet::new();
    let mut queued: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    queue.push_back((name.to_string(), 1));
    queued.insert(name.to_string());

    // Prepare statement once, reuse across all BFS iterations.
    // Uses explicit JOIN instead of correlated subquery for better query planning.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT r.name, r.file, r.line, r.context, s.file AS source_file \
         FROM \"references\" r \
         JOIN symbols s ON s.id = r.caller_id \
         WHERE s.name = ?1",
    )?;

    while let Some((source_name, depth)) = queue.pop_front() {
        let rows: Vec<_> = stmt
            .query_map(rusqlite::params![&source_name], |row| {
                Ok((
                    row.get::<_, String>(0)?,         // callee name
                    row.get::<_, String>(1)?,         // ref file
                    row.get::<_, i64>(2)?,            // ref line
                    row.get::<_, Option<String>>(3)?, // context
                    row.get::<_, String>(4)?,         // source file (caller's definition file)
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        for (callee_name, file, line, context, source_file) in rows {
            let key = (callee_name.clone(), file.clone());
            if visited.contains(&key) {
                continue;
            }
            visited.insert(key);

            results.push(CalleeResult {
                callee_name: callee_name.clone(),
                file,
                line: line as usize,
                context: context.unwrap_or_default(),
                depth,
                source_file: Some(source_file),
            });

            if depth < max_depth && !queued.contains(&callee_name) {
                queued.insert(callee_name.clone());
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

/// Find the shortest call chain from `from` to `to` via BFS callee expansion.
///
/// Returns `Some(path)` where `path` is a `Vec<CallPathHop>` representing the
/// chain `from -> hop1 -> ... -> to`. Returns `None` when no path exists within
/// the depth cap ([`MAX_DEPTH_CAP`]).
///
/// Special case: when `from == to`, returns a single-hop path.
pub fn callpath(conn: &Connection, from: &str, to: &str) -> Result<Option<Vec<CallPathHop>>> {
    // Degenerate case: same symbol.
    if from == to {
        return Ok(resolve_symbol_hop(conn, from)?.map(|hop| vec![hop]));
    }

    let mut visited: HashSet<String> = HashSet::new();
    let mut parent_map: HashMap<String, String> = HashMap::new();
    let mut queue: VecDeque<(String, usize)> = VecDeque::new();

    queue.push_back((from.to_string(), 0));
    visited.insert(from.to_string());

    // Reuse the same SQL statement across all BFS iterations.
    let mut stmt = conn.prepare(
        "SELECT DISTINCT r.name \
         FROM \"references\" r \
         JOIN symbols s ON s.id = r.caller_id \
         WHERE s.name = ?1",
    )?;

    while let Some((current, depth)) = queue.pop_front() {
        if depth >= MAX_DEPTH_CAP {
            continue;
        }

        let callees: Vec<String> = stmt
            .query_map(rusqlite::params![&current], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;

        for callee_name in callees {
            if visited.contains(&callee_name) {
                continue;
            }
            visited.insert(callee_name.clone());
            parent_map.insert(callee_name.clone(), current.clone());

            if callee_name == to {
                return reconstruct_path(conn, from, to, &parent_map);
            }

            queue.push_back((callee_name, depth + 1));
        }
    }

    Ok(None)
}

/// Reconstruct the call path from `from` to `to` using the parent map,
/// resolving each symbol to a `CallPathHop` with file/line/kind.
fn reconstruct_path(
    conn: &Connection,
    from: &str,
    to: &str,
    parent_map: &HashMap<String, String>,
) -> Result<Option<Vec<CallPathHop>>> {
    // Walk backwards from `to` to `from` via parent_map.
    let mut chain: Vec<String> = vec![to.to_string()];
    let mut current = to.to_string();
    while current != from {
        match parent_map.get(&current) {
            Some(parent) => {
                current = parent.clone();
                chain.push(current.clone());
            }
            None => return Ok(None), // should not happen if BFS is correct
        }
    }
    chain.reverse();

    // Resolve each name to a CallPathHop. Prepare statement once for reuse.
    let mut stmt =
        conn.prepare("SELECT name, kind, file, line FROM symbols WHERE name = ?1 LIMIT 1")?;
    let mut hops = Vec::with_capacity(chain.len());
    for name in &chain {
        let row = stmt
            .query_row(rusqlite::params![name], |row| {
                let sym_name: String = row.get(0)?;
                let kind_str: String = row.get(1)?;
                let file: String = row.get(2)?;
                let line: i64 = row.get(3)?;
                Ok((sym_name, kind_str, file, line))
            })
            .optional()?;

        match row {
            Some((sym_name, kind_str, file, line)) => {
                let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
                hops.push(CallPathHop {
                    symbol_name: sym_name,
                    symbol_kind: kind,
                    file,
                    line: line as usize,
                });
            }
            None => {
                // Symbol not found in index; use placeholder.
                hops.push(CallPathHop {
                    symbol_name: name.clone(),
                    symbol_kind: SymbolKind::Function,
                    file: String::new(),
                    line: 0,
                });
            }
        }
    }

    Ok(Some(hops))
}

/// Clamp a requested depth to `MAX_DEPTH_CAP`, returning the capped value and
/// whether clamping occurred.
pub fn clamp_depth(requested: usize) -> (usize, bool) {
    if requested > MAX_DEPTH_CAP {
        (MAX_DEPTH_CAP, true)
    } else {
        (requested, false)
    }
}

/// Look up a symbol by name and return a `CallPathHop` with its metadata.
/// Used for the degenerate `from == to` case in `callpath`.
fn resolve_symbol_hop(conn: &Connection, name: &str) -> Result<Option<CallPathHop>> {
    let mut stmt =
        conn.prepare("SELECT name, kind, file, line FROM symbols WHERE name = ?1 LIMIT 1")?;

    let row = stmt
        .query_row(rusqlite::params![name], |row| {
            let sym_name: String = row.get(0)?;
            let kind_str: String = row.get(1)?;
            let file: String = row.get(2)?;
            let line: i64 = row.get(3)?;
            Ok((sym_name, kind_str, file, line))
        })
        .optional()?;

    match row {
        Some((sym_name, kind_str, file, line)) => {
            let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
            Ok(Some(CallPathHop {
                symbol_name: sym_name,
                symbol_kind: kind,
                file,
                line: line as usize,
            }))
        }
        None => Ok(None),
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
    fn callers_depth_0_returns_empty() {
        let source = "fn foo() { bar(); }\nfn bar() { }\n";
        let (_dir, conn) = make_indexed_repo(source);
        let results = callers(&conn, "bar", 0).unwrap();
        assert!(results.is_empty(), "depth 0 should return no results");
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
    fn callees_depth_1_no_expand() {
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
        let results = callees(&conn, "a", 1).unwrap();

        let names: Vec<&str> = results.iter().map(|r| r.callee_name.as_str()).collect();
        assert!(names.contains(&"b"), "b should be a direct callee");
        assert!(!names.contains(&"c"), "c should NOT appear at depth 1");
    }

    #[test]
    fn callees_depth_0_returns_empty() {
        let source = "fn foo() { bar(); }\nfn bar() { }\n";
        let (_dir, conn) = make_indexed_repo(source);
        let results = callees(&conn, "foo", 0).unwrap();
        assert!(results.is_empty(), "depth 0 should return no results");
    }

    #[test]
    fn callees_populates_source_file() {
        let source = r#"
fn foo() {
    bar();
}

fn bar() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let results = callees(&conn, "foo", 1).unwrap();
        assert!(!results.is_empty());
        // source_file should be populated with the caller's definition file.
        assert!(
            results[0].source_file.is_some(),
            "source_file should be populated"
        );
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

    // -- callpath tests -------------------------------------------------------

    #[test]
    fn callpath_basic() {
        // Chain: a -> b -> c. callpath("a", "c") should return [a, b, c].
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
        let path = callpath(&conn, "a", "c").unwrap();
        assert!(path.is_some(), "should find a path from a to c");
        let hops = path.unwrap();
        assert_eq!(hops.len(), 3);
        assert_eq!(hops[0].symbol_name, "a");
        assert_eq!(hops[1].symbol_name, "b");
        assert_eq!(hops[2].symbol_name, "c");
    }

    #[test]
    fn callpath_direct() {
        // a calls b directly. callpath("a", "b") should return [a, b].
        let source = r#"
fn a() {
    b();
}

fn b() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let path = callpath(&conn, "a", "b").unwrap();
        assert!(path.is_some());
        let hops = path.unwrap();
        assert_eq!(hops.len(), 2);
        assert_eq!(hops[0].symbol_name, "a");
        assert_eq!(hops[1].symbol_name, "b");
    }

    #[test]
    fn callpath_no_path() {
        // Disconnected functions, no path exists.
        let source = r#"
fn a() { }

fn b() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let path = callpath(&conn, "a", "b").unwrap();
        assert!(
            path.is_none(),
            "should return None for disconnected symbols"
        );
    }

    #[test]
    fn callpath_same_symbol() {
        // callpath("a", "a") should return a single hop.
        let source = "fn a() { }\n";
        let (_dir, conn) = make_indexed_repo(source);
        let path = callpath(&conn, "a", "a").unwrap();
        assert!(path.is_some());
        let hops = path.unwrap();
        assert_eq!(hops.len(), 1);
        assert_eq!(hops[0].symbol_name, "a");
    }

    #[test]
    fn callpath_cycle_does_not_hang() {
        // Mutual recursion: a -> b -> a. callpath("a", "c") should terminate and find no path.
        let source = r#"
fn a() {
    b();
}

fn b() {
    a();
}

fn c() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let path = callpath(&conn, "a", "c").unwrap();
        assert!(path.is_none(), "should not find path through cycle");
    }

    #[test]
    fn callpath_shortest_path() {
        // a -> b -> c and a -> c directly. Should return shortest: [a, c].
        let source = r#"
fn a() {
    b();
    c();
}

fn b() {
    c();
}

fn c() { }
"#;
        let (_dir, conn) = make_indexed_repo(source);
        let path = callpath(&conn, "a", "c").unwrap();
        assert!(path.is_some());
        let hops = path.unwrap();
        assert_eq!(hops.len(), 2, "should find shortest path [a, c]");
        assert_eq!(hops[0].symbol_name, "a");
        assert_eq!(hops[1].symbol_name, "c");
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
        assert_eq!(depth, MAX_DEPTH_CAP);
        assert!(clamped);
    }
}
