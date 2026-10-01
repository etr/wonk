//! Rank latency bench (TASK-095, PRD-RANK-REQ-016).
//!
//! Measures the ADDED wall-clock cost of the classed pipeline (full tuned
//! defaults, all context preparation included) over the legacy ordering,
//! per query, warm: the corpus is indexed and embedded once, every query
//! runs one untimed pass through BOTH paths first, then >= 50 timed
//! iterations. The gate: mean added latency < 20 ms.
//!
//! Run: `cargo bench --bench rank_latency`

use std::fs;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use tempfile::TempDir;
use wonk::db;
use wonk::pipeline;
use wonk::rerank::{self, RankSettings};
use wonk::search;

const CORPUS: &str = "tests/fixtures/labeled_queries/corpus";
const LABELS: &str = "tests/fixtures/labeled_queries/labels.toml";
const ITERATIONS: usize = 60;

#[derive(Debug, Deserialize)]
struct Labels {
    query: Vec<LabeledQuery>,
}

#[derive(Debug, Deserialize)]
struct LabeledQuery {
    text: String,
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn main() -> Result<()> {
    let dir = TempDir::new()?;
    let root = dir.path();
    copy_dir(Path::new(CORPUS), root);
    fs::create_dir(root.join(".git"))?;
    pipeline::build_index(root, true)?;
    let index_path = db::find_existing_index(root).context("fixture index")?;
    let conn = db::open(&index_path)?;
    let provider = wonk::embedding::BundledProvider;
    pipeline::build_embeddings(&conn, root, &provider, wonk::progress::ProgressMode::Silent)?;

    let labels: Labels = toml::from_str(&fs::read_to_string(LABELS)?)?;
    let queries: Vec<String> = labels.query.iter().map(|q| q.text.clone()).collect();

    let legacy = RankSettings::default();
    let tuned = RankSettings {
        use_pipeline: true,
        ..RankSettings::from_config(
            &wonk::config::RankConfig {
                enabled: true,
                ..Default::default()
            },
            &wonk::config::SearchConfig::default(),
            wonk::embedding::EmbeddingProviderKind::Bundled,
            None,
        )?
    };

    // Every query's candidate set is fetched ONCE (warm cache, warm
    // connection) — latency is measured on the ranking stage, the cost a
    // warm interactive query actually pays.
    let root_str = root.display().to_string();
    let mut candidate_sets = Vec::with_capacity(queries.len());
    for query in &queries {
        let mut found = search::text_search(query, false, false, std::slice::from_ref(&root_str))?;
        for result in &mut found {
            if let Ok(rel) = result.file.strip_prefix(root) {
                result.file = rel.to_path_buf();
            }
        }
        candidate_sets.push(found);
    }

    // One untimed warm pass of every query through BOTH paths.
    for (query, found) in queries.iter().zip(&candidate_sets) {
        let _ = rerank::rank_and_explain_classed(found, Some(&conn), query, &legacy);
        let _ = rerank::rank_and_explain_classed(found, Some(&conn), query, &tuned);
    }

    let mut deltas_ms: Vec<f32> = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let mut legacy_ns = 0u128;
        let mut tuned_ns = 0u128;
        for (query, found) in queries.iter().zip(&candidate_sets) {
            let start = Instant::now();
            let _ = rerank::rank_and_explain_classed(found, Some(&conn), query, &legacy);
            legacy_ns += start.elapsed().as_nanos();

            let start = Instant::now();
            let _ = rerank::rank_and_explain_classed(found, Some(&conn), query, &tuned);
            tuned_ns += start.elapsed().as_nanos();
        }
        deltas_ms.push((tuned_ns.saturating_sub(legacy_ns)) as f32 / 1_000_000.0);
    }
    deltas_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = deltas_ms.iter().sum::<f32>() / deltas_ms.len() as f32;
    let p95 = deltas_ms[(deltas_ms.len() as f32 * 0.95) as usize];
    println!(
        "added latency over {} queries x {} iterations: mean={mean:.3} ms  p95={p95:.3} ms",
        queries.len(),
        ITERATIONS
    );
    ensure!(
        mean < 20.0,
        "PRD-RANK-REQ-016 violated: mean added latency {mean:.3} ms >= 20 ms"
    );
    Ok(())
}
