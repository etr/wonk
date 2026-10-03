mod fixture_config;

use std::fmt::Write as _;
use std::fs;
use std::time::Instant;

use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use wonk::embedding::BundledProvider;
use wonk::progress::ProgressMode;

const SYMBOL_COUNT: usize = 10_000;
const MODEL_BYTES: &[u8] = include_bytes!("../assets/models/bundled-embedding-v1.bin.zst");

fn main() -> Result<()> {
    let repository = tempfile::tempdir()?;
    fs::create_dir(repository.path().join(".git"))?;
    let mut source = String::with_capacity(SYMBOL_COUNT * 120);
    for index in 0..SYMBOL_COUNT {
        writeln!(
            source,
            "/// Handler {index} authenticates request {index}.\npub fn handler_{index}(token: &str) -> bool {{ !token.is_empty() }}\n"
        )?;
    }
    fs::write(repository.path().join("handlers.rs"), source)?;

    let structural_start = Instant::now();
    let index_stats = fixture_config::build_index(repository.path(), true)?;
    ensure!(
        index_stats.symbol_count == SYMBOL_COUNT,
        "expected {SYMBOL_COUNT} symbols, indexed {}",
        index_stats.symbol_count
    );
    let structural_elapsed = structural_start.elapsed();

    let index_path = wonk::db::local_index_path(repository.path());
    let conn = wonk::db::open(&index_path)?;
    let embedding_start = Instant::now();
    let embedding_stats = wonk::pipeline::build_embeddings(
        &conn,
        repository.path(),
        &BundledProvider,
        ProgressMode::Silent,
    )?;
    let embedding_elapsed = embedding_start.elapsed();
    ensure!(
        embedding_stats.embedded_count == SYMBOL_COUNT,
        "expected {SYMBOL_COUNT} embeddings, stored {}",
        embedding_stats.embedded_count
    );
    ensure!(
        embedding_elapsed.as_secs_f64() < 60.0,
        "10k-symbol embedding took {:.3}s (limit: 60s)",
        embedding_elapsed.as_secs_f64()
    );
    let metadata: (String, i64, i64) = conn.query_row(
        "SELECT provider, dim, COUNT(*) FROM embeddings GROUP BY provider, dim",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    ensure!(metadata == ("bundled".to_string(), 256, SYMBOL_COUNT as i64));

    println!(
        "os={} arch={}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!(
        "available_parallelism={} rayon_threads={}",
        std::thread::available_parallelism()?.get(),
        rayon::current_num_threads()
    );
    println!("model_sha256={:x}", Sha256::digest(MODEL_BYTES));
    println!("structural_seconds={:.3}", structural_elapsed.as_secs_f64());
    println!("embedding_seconds={:.3}", embedding_elapsed.as_secs_f64());
    println!(
        "symbols_per_second={:.1}",
        SYMBOL_COUNT as f64 / embedding_elapsed.as_secs_f64()
    );
    Ok(())
}
