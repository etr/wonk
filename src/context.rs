//! Symbol context aggregation for `wonk context`.
//!
//! Gathers definition, categorized incoming/outgoing references, flow
//! participation, and children for a symbol into a single [`SymbolContext`].

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::Result;
use rusqlite::Connection;

use crate::types::{
    ContextCallee, ContextCaller, ContextChild, ContextFlowParticipation, ContextImport,
    ContextImporter, ContextTypeUser, IncomingRefs, OutgoingRefs, SymbolContext, SymbolKind,
};

/// Options controlling context resolution.
#[derive(Debug, Clone, Default)]
pub struct ContextOptions {
    /// Restrict to symbols in this file.
    pub file: Option<String>,
    /// Restrict to this symbol kind.
    pub kind: Option<String>,
    /// Minimum confidence threshold for edge filtering.
    pub min_confidence: Option<f64>,
}

/// Sanitize a user-provided confidence threshold to a valid [0.0, 1.0] range.
fn sanitize_confidence(min_confidence: Option<f64>) -> f64 {
    match min_confidence {
        Some(c) if c.is_nan() || c.is_infinite() => 0.0,
        Some(c) => c.clamp(0.0, 1.0),
        None => 0.0,
    }
}

/// Escape SQLite LIKE metacharacters (`%` and `_`) in a string.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// Aggregate full context for all symbols matching `name`, applying optional
/// file/kind filters.  Returns one [`SymbolContext`] per matching symbol.
///
/// Note: queries are executed sequentially because `rusqlite::Connection` is
/// not `Send`/`Sync`, preventing per-query threading without connection cloning.
/// All statements use `prepare_cached()` for compiled-statement reuse across
/// iterations.
pub fn symbol_context(
    conn: &Connection,
    name: &str,
    options: &ContextOptions,
) -> Result<Vec<SymbolContext>> {
    let conf = sanitize_confidence(options.min_confidence);

    // 1. Resolve matching symbols.
    let symbols = resolve_symbols(conn, name, options)?;

    // 2. Hoist flow participation: detect entry points once, trace flows once,
    //    then share results across all matched symbols with the same name.
    let flow_map = gather_flow_participation_batch(conn, name, conf)?;

    // 3. Cache file imports by source file to avoid duplicate queries.
    let mut import_cache: HashMap<String, Vec<ContextImport>> = HashMap::new();

    let mut results = Vec::with_capacity(symbols.len());

    for (sym_id, sym_name, sym_kind, sym_file, sym_line, sym_end_line, sym_sig) in &symbols {
        let callers = gather_callers(conn, sym_name, conf)?;
        let importers = gather_importers(conn, sym_name)?;
        let type_users = gather_type_users(conn, sym_name, conf)?;
        let callees = gather_callees(conn, *sym_id, conf)?;

        // Reuse cached file imports for same source file.
        let imports = match import_cache.get(sym_file.as_str()) {
            Some(cached) => cached.clone(),
            None => {
                let fresh = gather_file_imports(conn, sym_file)?;
                import_cache.insert(sym_file.clone(), fresh.clone());
                fresh
            }
        };

        let flows = flow_map.get(sym_name).cloned().unwrap_or_default();
        let children = gather_children(conn, *sym_id)?;

        results.push(SymbolContext {
            name: sym_name.clone(),
            kind: *sym_kind,
            file: sym_file.clone(),
            line: *sym_line,
            end_line: *sym_end_line,
            signature: sym_sig.clone(),
            incoming: IncomingRefs {
                callers,
                importers,
                type_users,
            },
            outgoing: OutgoingRefs { callees, imports },
            flows,
            children,
        });
    }

    Ok(results)
}

/// A resolved symbol row: (id, name, kind, file, line, end_line, signature).
type SymbolRow = (
    i64,
    String,
    SymbolKind,
    String,
    usize,
    Option<usize>,
    String,
);

/// Resolve matching symbols from the `symbols` table.
fn resolve_symbols(
    conn: &Connection,
    name: &str,
    options: &ContextOptions,
) -> Result<Vec<SymbolRow>> {
    let kind_filter = options.kind.as_deref().unwrap_or("");

    // Build file filter: escape LIKE metacharacters and use ESCAPE clause.
    let file_filter = options
        .file
        .as_deref()
        .map(escape_like)
        .unwrap_or_default();

    let sql = "\
        SELECT id, name, kind, file, line, end_line, signature \
        FROM symbols \
        WHERE name = ?1 \
        AND (?2 = '' OR file LIKE '%' || ?2 ESCAPE '\\') \
        AND (?3 = '' OR kind = ?3) \
        ORDER BY file, line";

    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(rusqlite::params![name, file_filter, kind_filter], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;

    let mut symbols = Vec::new();
    for row in rows {
        let (id, sym_name, kind_str, file, line, end_line, sig) = row?;
        let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
        symbols.push((
            id,
            sym_name,
            kind,
            file,
            line as usize,
            end_line.map(|l| l as usize),
            sig,
        ));
    }
    Ok(symbols)
}

/// Callers: functions whose body references this symbol (via caller_id).
fn gather_callers(conn: &Connection, name: &str, conf: f64) -> Result<Vec<ContextCaller>> {
    let sql = "\
        SELECT DISTINCT s.name, s.kind, s.file, s.line \
        FROM \"references\" r \
        JOIN symbols s ON r.caller_id = s.id \
        WHERE r.name = ?1 AND r.confidence >= ?2 \
        ORDER BY s.file, s.line";

    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(rusqlite::params![name, conf], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
        ))
    })?;

    let mut callers = Vec::new();
    for row in rows {
        let (caller_name, kind_str, file, line) = row?;
        let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Function);
        callers.push(ContextCaller {
            name: caller_name,
            kind,
            file,
            line: line as usize,
        });
    }
    Ok(callers)
}

/// Importers: files that import this symbol via `file_imports`.
///
/// Uses a suffix match on `import_path` with LIKE metacharacters escaped
/// to prevent wildcard injection from symbol names containing `_` or `%`.
fn gather_importers(conn: &Connection, name: &str) -> Result<Vec<ContextImporter>> {
    let escaped_name = escape_like(name);

    let sql = "\
        SELECT DISTINCT source_file \
        FROM file_imports \
        WHERE import_path LIKE '%' || ?1 ESCAPE '\\' \
        ORDER BY source_file";

    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(rusqlite::params![escaped_name], |row| {
        row.get::<_, String>(0)
    })?;

    let mut importers = Vec::new();
    for row in rows {
        importers.push(ContextImporter { file: row? });
    }
    Ok(importers)
}

/// Type users: file-scope references with no caller_id (type annotations).
fn gather_type_users(conn: &Connection, name: &str, conf: f64) -> Result<Vec<ContextTypeUser>> {
    let sql = "\
        SELECT DISTINCT r.file, r.line, r.context \
        FROM \"references\" r \
        WHERE r.name = ?1 AND r.caller_id IS NULL AND r.confidence >= ?2 \
        ORDER BY r.file, r.line";

    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(rusqlite::params![name, conf], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut users = Vec::new();
    for row in rows {
        let (file, line, context) = row?;
        users.push(ContextTypeUser {
            file,
            line: line as usize,
            context,
        });
    }
    Ok(users)
}

/// Callees: symbols referenced within this function's body.
fn gather_callees(conn: &Connection, symbol_id: i64, conf: f64) -> Result<Vec<ContextCallee>> {
    let sql = "\
        SELECT DISTINCT r.name, s2.kind, s2.file, s2.line \
        FROM \"references\" r \
        LEFT JOIN symbols s2 ON s2.name = r.name AND s2.kind IN ('function', 'method') \
        WHERE r.caller_id = ?1 AND r.confidence >= ?2 \
        ORDER BY s2.file, s2.line";

    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(rusqlite::params![symbol_id, conf], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
        ))
    })?;

    let mut callees = Vec::new();
    for row in rows {
        let (callee_name, kind_str_opt, file_opt, line_opt) = row?;
        let kind = kind_str_opt
            .as_deref()
            .and_then(|s| SymbolKind::from_str(s).ok())
            .unwrap_or(SymbolKind::Function);
        callees.push(ContextCallee {
            name: callee_name,
            kind,
            file: file_opt.unwrap_or_default(),
            line: line_opt.unwrap_or(0) as usize,
        });
    }
    Ok(callees)
}

/// File imports: all import_path entries for the symbol's source file.
fn gather_file_imports(conn: &Connection, file: &str) -> Result<Vec<ContextImport>> {
    let sql = "\
        SELECT DISTINCT import_path \
        FROM file_imports \
        WHERE source_file = ?1 \
        ORDER BY import_path";

    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(rusqlite::params![file], |row| row.get::<_, String>(0))?;

    let mut imports = Vec::new();
    for row in rows {
        imports.push(ContextImport { path: row? });
    }
    Ok(imports)
}

/// Detect entry points once and trace flows once, returning a map of symbol
/// name → flow participations.  Called once per `symbol_context` invocation
/// (not per resolved symbol) to avoid redundant entry point scans.
fn gather_flow_participation_batch(
    conn: &Connection,
    name: &str,
    conf: f64,
) -> Result<HashMap<String, Vec<ContextFlowParticipation>>> {
    use crate::flows;

    let options = flows::FlowOptions {
        depth: 5,
        branching: 4,
        min_confidence: Some(conf),
        from_file: None,
    };

    let entries = flows::detect_entry_points(conn, &options)?;

    let mut map: HashMap<String, Vec<ContextFlowParticipation>> = HashMap::new();

    // Limit entry point probing to avoid unbounded computation.
    for entry in entries.iter().take(20) {
        if let Ok(Some(flow)) = flows::trace_flow(conn, &entry.name, &options) {
            for (idx, step) in flow.steps.iter().enumerate() {
                if step.name == name {
                    map.entry(name.to_string())
                        .or_default()
                        .push(ContextFlowParticipation {
                            entry_point: flow.entry_point.name.clone(),
                            step_index: idx,
                        });
                    break; // Only report first occurrence per flow.
                }
            }
        }
    }

    Ok(map)
}

/// Children: symbols extending or implementing this symbol via type_edges.
fn gather_children(conn: &Connection, symbol_id: i64) -> Result<Vec<ContextChild>> {
    let sql = "\
        SELECT s.name, s.kind, s.file, s.line, te.relationship \
        FROM type_edges te \
        JOIN symbols s ON te.child_id = s.id \
        WHERE te.parent_id = ?1 \
        ORDER BY s.file, s.line";

    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(rusqlite::params![symbol_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;

    let mut children = Vec::new();
    for row in rows {
        let (child_name, kind_str, file, line, relationship) = row?;
        let kind = SymbolKind::from_str(&kind_str).unwrap_or(SymbolKind::Class);
        children.push(ContextChild {
            name: child_name,
            kind,
            file,
            line: line as usize,
            relationship,
        });
    }
    Ok(children)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::pipeline;
    use crate::types::SymbolKind;
    use std::fs;
    use tempfile::TempDir;

    /// Create a minimal indexed repo and return (TempDir, Connection).
    fn make_indexed_repo(files: &[(&str, &str)]) -> (TempDir, Connection) {
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

    #[test]
    fn symbol_context_basic_definition() {
        let (_dir, conn) = make_indexed_repo(&[(
            "src/lib.rs",
            r#"
fn process_payment(amount: u32) -> bool {
    amount > 0
}
"#,
        )]);

        let results = symbol_context(&conn, "process_payment", &ContextOptions::default()).unwrap();
        assert_eq!(results.len(), 1);
        let ctx = &results[0];
        assert_eq!(ctx.name, "process_payment");
        assert_eq!(ctx.kind, SymbolKind::Function);
        assert!(ctx.file.contains("src/lib.rs"));
        assert!(ctx.signature.contains("process_payment"));
    }

    #[test]
    fn symbol_context_callers_and_callees() {
        let (_dir, conn) = make_indexed_repo(&[(
            "src/lib.rs",
            r#"
fn helper() -> i32 {
    42
}

fn caller_fn() -> i32 {
    helper()
}
"#,
        )]);

        let results = symbol_context(&conn, "helper", &ContextOptions::default()).unwrap();
        assert_eq!(results.len(), 1);
        let ctx = &results[0];
        // helper should have caller_fn as a caller
        assert!(
            ctx.incoming.callers.iter().any(|c| c.name == "caller_fn"),
            "expected caller_fn in callers, got: {:?}",
            ctx.incoming.callers
        );

        // caller_fn should have helper as a callee
        let caller_results =
            symbol_context(&conn, "caller_fn", &ContextOptions::default()).unwrap();
        assert_eq!(caller_results.len(), 1);
        assert!(
            caller_results[0]
                .outgoing
                .callees
                .iter()
                .any(|c| c.name == "helper"),
            "expected helper in callees, got: {:?}",
            caller_results[0].outgoing.callees
        );
    }

    #[test]
    fn symbol_context_file_filter() {
        let (_dir, conn) =
            make_indexed_repo(&[("src/a.rs", "fn foo() {}\n"), ("src/b.rs", "fn foo() {}\n")]);

        let opts = ContextOptions {
            file: Some("src/a.rs".into()),
            ..Default::default()
        };
        let results = symbol_context(&conn, "foo", &opts).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].file.contains("src/a.rs"));
    }

    #[test]
    fn symbol_context_kind_filter() {
        let (_dir, conn) = make_indexed_repo(&[(
            "src/lib.rs",
            r#"
struct Foo;

fn foo() {}
"#,
        )]);

        let opts = ContextOptions {
            kind: Some("function".into()),
            ..Default::default()
        };
        let results = symbol_context(&conn, "foo", &opts).unwrap();
        // Only the function, not the struct (different case: Foo vs foo)
        assert!(results.iter().all(|r| r.kind == SymbolKind::Function));
    }

    #[test]
    fn symbol_context_children_via_type_edges() {
        let (_dir, conn) = make_indexed_repo(&[(
            "src/lib.rs",
            r#"
trait Animal {
    fn speak(&self);
}

struct Dog;

impl Animal for Dog {
    fn speak(&self) {}
}
"#,
        )]);

        let results = symbol_context(&conn, "Animal", &ContextOptions::default()).unwrap();
        assert_eq!(results.len(), 1);
        let ctx = &results[0];
        // Dog should appear as a child of Animal
        let child_names: Vec<&str> = ctx.children.iter().map(|c| c.name.as_str()).collect();
        assert!(
            child_names.contains(&"Dog"),
            "expected Dog in children, got: {:?}",
            child_names
        );
    }

    #[test]
    fn symbol_context_no_match_returns_empty() {
        let (_dir, conn) = make_indexed_repo(&[("src/lib.rs", "fn foo() {}\n")]);
        let results = symbol_context(&conn, "nonexistent", &ContextOptions::default()).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn symbol_context_multiple_matches() {
        let (_dir, conn) = make_indexed_repo(&[
            ("src/a.rs", "fn process() {}\n"),
            ("src/b.rs", "fn process() {}\n"),
        ]);
        let results = symbol_context(&conn, "process", &ContextOptions::default()).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn symbol_context_importers() {
        let (_dir, conn) = make_indexed_repo(&[
            ("src/lib.rs", "pub fn helper() {}\n"),
            (
                "src/main.rs",
                "use crate::helper;\nfn main() { helper(); }\n",
            ),
        ]);
        let results = symbol_context(&conn, "helper", &ContextOptions::default()).unwrap();
        assert_eq!(results.len(), 1);
        // src/main.rs imports helper
        let importer_files: Vec<&str> = results[0]
            .incoming
            .importers
            .iter()
            .map(|i| i.file.as_str())
            .collect();
        assert!(
            importer_files.iter().any(|f| f.contains("src/main.rs")),
            "expected src/main.rs in importers, got: {:?}",
            importer_files
        );
    }
}
