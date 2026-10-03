//! Rank-weight tuning bench (TASK-095, PRD-RANK-REQ-017).
//!
//! Measures precision@10 over the labeled query set
//! (`tests/fixtures/labeled_queries`) for the legacy ordering and a
//! curated candidate grid of pipeline weight tables × per-class channel
//! multipliers (kind anchored at 1.0 per the frozen equivalence contract).
//! Coarse-to-fine: the grid below is the refined pass; the recorded
//! results in `bench/rank-tuning-results.md` are the measurement the
//! default flip is gated on.
//!
//! Run: `cargo bench --bench rank_tune -- --verbose`

mod fixture_config;

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Deserialize;
use tempfile::TempDir;
use wonk::db;
use wonk::pipeline;
use wonk::rerank::{self, ChannelMultipliers, ClassMultipliers, RankSettings, WeightTable};
use wonk::search::{self, SearchResult};

const CORPUS: &str = "tests/fixtures/labeled_queries/corpus";
const LABELS: &str = "tests/fixtures/labeled_queries/labels.toml";

#[derive(Debug, Deserialize)]
struct Labels {
    query: Vec<LabeledQuery>,
}

#[derive(Debug, Deserialize)]
struct LabeledQuery {
    text: String,
    class: String,
    relevant: Vec<String>,
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

fn setup() -> Result<(TempDir, Connection)> {
    let dir = TempDir::new()?;
    let root = dir.path();
    copy_dir(Path::new(CORPUS), root);
    fs::create_dir(root.join(".git"))?;
    fixture_config::build_index(root, true)?;
    let index_path = db::find_existing_index(root).context("fixture index")?;
    let conn = db::open(&index_path)?;
    let provider = wonk::embedding::BundledProvider;
    pipeline::build_embeddings(&conn, root, &provider, wonk::progress::ProgressMode::Silent)?;
    Ok((dir, conn))
}

fn candidates(root: &Path, query: &str) -> Vec<SearchResult> {
    let root_str = root.display().to_string();
    let mut results = search::text_search(query, false, false, &[root_str]).unwrap();
    for result in &mut results {
        if let Ok(rel) = result.file.strip_prefix(root) {
            result.file = rel.to_path_buf();
        }
    }
    results
}

fn precision_at_10(ranked: &[String], relevant: &[String]) -> f32 {
    ranked
        .iter()
        .take(10)
        .filter(|f| relevant.contains(f))
        .count() as f32
        / 10.0
}

fn measure(root: &Path, conn: &Connection, settings: &RankSettings, labels: &Labels) -> Vec<f32> {
    labels
        .query
        .iter()
        .map(|lq| {
            let found = candidates(root, &lq.text);
            let ranked = rerank::rank_and_explain_classed(&found, Some(conn), &lq.text, settings);
            let files: Vec<String> = ranked
                .groups
                .iter()
                .flat_map(|(_, items)| items.iter())
                .map(|s| s.classified.result.file.to_string_lossy().into_owned())
                .take(10)
                .collect();
            precision_at_10(&files, &lq.relevant)
        })
        .collect()
}

fn weights(entries: &[(&str, f32)]) -> WeightTable {
    WeightTable::from_config(&entries.iter().map(|(n, w)| (n.to_string(), *w)).collect()).unwrap()
}

fn mult(symbol: (f32, f32), path: (f32, f32), signature: (f32, f32)) -> ClassMultipliers {
    ClassMultipliers {
        symbol: ChannelMultipliers {
            lexical: symbol.0,
            semantic: symbol.1,
        },
        path: ChannelMultipliers {
            lexical: path.0,
            semantic: path.1,
        },
        signature: ChannelMultipliers {
            lexical: signature.0,
            semantic: signature.1,
        },
    }
}

struct Candidate {
    name: &'static str,
    weights: WeightTable,
    multipliers: ClassMultipliers,
}

fn report(name: &str, per_query: &[f32], labels: &Labels) -> (f32, Vec<(&'static str, f32)>) {
    let mut by_class: HashMap<&str, (f32, usize)> = HashMap::new();
    for (lq, p) in labels.query.iter().zip(per_query) {
        let entry = by_class.entry(lq.class.as_str()).or_insert((0.0, 0));
        entry.0 += p;
        entry.1 += 1;
    }
    let mut per_class = vec![];
    for class in ["symbol", "path", "signature", "conceptual"] {
        let (sum, count) = by_class.get(class).copied().unwrap_or((0.0, 0));
        per_class.push((class, sum / count.max(1) as f32));
    }
    let overall = per_query.iter().sum::<f32>() / per_query.len() as f32;
    println!("{name:<44} overall={overall:.4}  {}", {
        per_class
            .iter()
            .map(|(c, v)| format!("{c}={v:.4}"))
            .collect::<Vec<_>>()
            .join("  ")
    });
    (overall, per_class)
}

fn main() -> Result<()> {
    let (dir, conn) = setup()?;
    let labels: Labels = toml::from_str(&fs::read_to_string(LABELS)?)?;

    // Baseline: the legacy ordering (what the default config ships today).
    let legacy = RankSettings::default();
    let legacy_p = measure(dir.path(), &conn, &legacy, &labels);
    let (legacy_overall, legacy_class) = report("legacy (default)", &legacy_p, &labels);

    // Candidate grid: kind anchored at 1.0 (the frozen equivalence anchor),
    // coarse-to-fine over the content/structural blend and the per-class
    // channel multipliers.
    let candidates = vec![
        Candidate {
            name: "A lexical-lean",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.8),
                ("semantic", 0.2),
                ("prominence", 1.0),
                ("proximity", 0.5),
            ]),
            multipliers: ClassMultipliers::neutral(),
        },
        Candidate {
            name: "B balanced",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.5),
                ("semantic", 0.5),
                ("prominence", 0.8),
                ("proximity", 0.3),
                ("centrality", 0.3),
                ("signature", 0.5),
                ("path_character", 0.5),
            ]),
            multipliers: ClassMultipliers::neutral(),
        },
        Candidate {
            name: "C structural-rich",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.3),
                ("semantic", 0.4),
                ("prominence", 1.2),
                ("proximity", 0.4),
                ("centrality", 0.5),
                ("signature", 0.6),
                ("path_character", 0.8),
            ]),
            multipliers: ClassMultipliers::neutral(),
        },
        Candidate {
            name: "D content-lean",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.2),
                ("semantic", 0.3),
                ("prominence", 1.5),
                ("proximity", 0.3),
                ("centrality", 0.4),
                ("signature", 0.4),
                ("path_character", 0.4),
            ]),
            multipliers: ClassMultipliers::neutral(),
        },
        Candidate {
            name: "E C+symbol-split",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.3),
                ("semantic", 0.4),
                ("prominence", 1.2),
                ("proximity", 0.4),
                ("centrality", 0.5),
                ("signature", 0.6),
                ("path_character", 0.8),
            ]),
            multipliers: mult((2.0, 0.5), (1.0, 1.0), (1.5, 0.5)),
        },
        Candidate {
            name: "F B+symbol-split",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.5),
                ("semantic", 0.5),
                ("prominence", 0.8),
                ("proximity", 0.3),
                ("centrality", 0.3),
                ("signature", 0.5),
                ("path_character", 0.5),
            ]),
            multipliers: mult((1.5, 0.5), (1.0, 1.0), (1.2, 0.8)),
        },
        Candidate {
            name: "G C+stronger-split",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.3),
                ("semantic", 0.4),
                ("prominence", 1.2),
                ("proximity", 0.4),
                ("centrality", 0.5),
                ("signature", 0.6),
                ("path_character", 0.8),
            ]),
            multipliers: mult((3.0, 0.2), (1.5, 0.5), (2.0, 0.3)),
        },
        Candidate {
            name: "H D+symbol-split",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.2),
                ("semantic", 0.3),
                ("prominence", 1.5),
                ("proximity", 0.3),
                ("centrality", 0.4),
                ("signature", 0.4),
                ("path_character", 0.4),
            ]),
            multipliers: mult((2.5, 0.3), (1.2, 0.8), (1.5, 0.5)),
        },
        Candidate {
            name: "I semantic-forward+split",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.25),
                ("semantic", 0.9),
                ("prominence", 1.0),
                ("proximity", 0.3),
                ("centrality", 0.3),
                ("signature", 0.3),
                ("path_character", 0.5),
            ]),
            multipliers: mult((2.5, 0.3), (1.0, 1.0), (1.5, 0.4)),
        },
        Candidate {
            name: "J prominence-heavy",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.3),
                ("semantic", 0.3),
                ("prominence", 2.0),
                ("proximity", 0.3),
                ("centrality", 0.6),
                ("signature", 0.5),
                ("path_character", 0.6),
            ]),
            multipliers: mult((2.0, 0.3), (1.2, 1.0), (1.5, 0.5)),
        },
        Candidate {
            name: "K kind+content-minimal",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.4),
                ("semantic", 0.3),
                ("prominence", 1.0),
                ("centrality", 0.4),
                ("signature", 0.8),
                ("path_character", 0.6),
            ]),
            multipliers: mult((1.8, 0.6), (1.3, 0.8), (1.4, 0.6)),
        },
        Candidate {
            name: "L everything-mild",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.4),
                ("semantic", 0.4),
                ("prominence", 1.0),
                ("proximity", 0.4),
                ("centrality", 0.4),
                ("signature", 0.7),
                ("path_character", 0.7),
            ]),
            multipliers: mult((1.6, 0.6), (1.1, 0.9), (1.3, 0.7)),
        },
        // Fine pass around the coarse winners (F/G/I/K plateau).
        Candidate {
            name: "M K+prominence-up",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.4),
                ("semantic", 0.3),
                ("prominence", 1.4),
                ("centrality", 0.4),
                ("signature", 0.8),
                ("path_character", 0.6),
            ]),
            multipliers: mult((1.8, 0.6), (1.3, 0.8), (1.4, 0.6)),
        },
        Candidate {
            name: "N K+lexical-up",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.6),
                ("semantic", 0.3),
                ("prominence", 1.0),
                ("centrality", 0.4),
                ("signature", 0.8),
                ("path_character", 0.6),
            ]),
            multipliers: mult((1.8, 0.6), (1.3, 0.8), (1.4, 0.6)),
        },
        Candidate {
            name: "O K+centrality-up",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.4),
                ("semantic", 0.3),
                ("prominence", 1.0),
                ("centrality", 0.7),
                ("signature", 0.8),
                ("path_character", 0.6),
            ]),
            multipliers: mult((1.8, 0.6), (1.3, 0.8), (1.4, 0.6)),
        },
        Candidate {
            name: "P K+symbol-stronger",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.4),
                ("semantic", 0.3),
                ("prominence", 1.0),
                ("centrality", 0.4),
                ("signature", 0.8),
                ("path_character", 0.6),
            ]),
            multipliers: mult((2.4, 0.4), (1.3, 0.8), (1.6, 0.5)),
        },
        Candidate {
            name: "Q K+proximity",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.4),
                ("semantic", 0.3),
                ("prominence", 1.0),
                ("proximity", 0.5),
                ("centrality", 0.4),
                ("signature", 0.8),
                ("path_character", 0.6),
            ]),
            multipliers: mult((1.8, 0.6), (1.3, 0.8), (1.4, 0.6)),
        },
        Candidate {
            name: "R G+path-character-up",
            weights: weights(&[
                ("kind", 1.0),
                ("lexical", 0.3),
                ("semantic", 0.4),
                ("prominence", 1.2),
                ("proximity", 0.4),
                ("centrality", 0.5),
                ("signature", 0.6),
                ("path_character", 1.2),
            ]),
            multipliers: mult((3.0, 0.2), (1.5, 0.5), (2.0, 0.3)),
        },
    ];

    // Per-query diff of the first candidate vs legacy (diagnostics).
    {
        let settings = RankSettings {
            use_pipeline: true,
            feedback_free: true,
            weights: candidates[0].weights.clone(),
            class_multipliers: candidates[0].multipliers,
            ..Default::default()
        };
        let tuned_p = measure(dir.path(), &conn, &settings, &labels);
        for (lq, (a, b)) in labels.query.iter().zip(legacy_p.iter().zip(&tuned_p)) {
            if a != b {
                println!(
                    "DIFF {:?} [{}] legacy={a:.2} tuned={b:.2}",
                    lq.text, lq.class
                );
            }
        }
    }

    type ClassMeans = Vec<(&'static str, f32)>;
    let mut best: Option<(&str, f32, ClassMeans)> = None;
    for candidate in &candidates {
        let settings = RankSettings {
            use_pipeline: true,
            feedback_free: true,
            weights: candidate.weights.clone(),
            class_multipliers: candidate.multipliers,
            ..Default::default()
        };
        let p = measure(dir.path(), &conn, &settings, &labels);
        let (overall, per_class) = report(candidate.name, &p, &labels);
        // Winner selection: strictly beats legacy overall AND no class
        // regresses below its legacy mean (the flip gate's own criteria).
        let no_regression = per_class
            .iter()
            .zip(&legacy_class)
            .all(|((_, v), (_, b))| v >= b);
        if overall > legacy_overall && no_regression {
            match best {
                Some((_, best_overall, _)) if best_overall >= overall => {}
                _ => best = Some((candidate.name, overall, per_class)),
            }
        }
    }

    println!("\nlegacy overall={legacy_overall:.4}");
    match best {
        Some((name, overall, _)) => println!("winner: {name} overall={overall:.4}"),
        None => println!("winner: none — no candidate beat legacy on every class"),
    }
    Ok(())
}
