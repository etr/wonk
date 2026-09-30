use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

#[derive(Serialize)]
struct ExportedChunk {
    symbol_id: i64,
    file: String,
    symbol: String,
    text: String,
}

fn main() -> Result<()> {
    let repo_root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .context("usage: cargo run --example export_embedding_corpus -- <repository>")?
        .canonicalize()
        .context("canonicalizing repository path")?;
    let index_path = wonk::db::index_path_for(&repo_root, true)?;
    let conn = wonk::db::open(&index_path)?;
    let chunks = wonk::embedding::chunk_all_symbols(&conn, &repo_root)?;

    let mut symbol_name = conn.prepare("SELECT name FROM symbols WHERE id = ?1")?;
    for (symbol_id, file, text) in chunks {
        let symbol = symbol_name.query_row([symbol_id], |row| row.get(0))?;
        println!(
            "{}",
            serde_json::to_string(&ExportedChunk {
                symbol_id,
                file,
                symbol,
                text,
            })?
        );
    }
    Ok(())
}
