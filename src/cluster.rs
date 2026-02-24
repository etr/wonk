//! K-Means clustering of symbol embeddings with automatic k selection.
//!
//! Partitions symbol embeddings into clusters using the K-Means algorithm
//! with K-Means++ initialization. The optimal number of clusters is selected
//! automatically via silhouette scoring.

use std::collections::HashMap;

use linfa::prelude::*;
use linfa_clustering::KMeans;
use ndarray::Array2;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rusqlite::Connection;

use crate::errors::EmbeddingError;
use crate::types::{Cluster, ClusterMember, SymbolKind};

/// Maximum number of clusters to consider, regardless of max_k argument.
const ABSOLUTE_MAX_K: usize = 20;

/// Number of representative symbols to select per cluster (closest to centroid).
const NUM_REPRESENTATIVES: usize = 5;

/// Compute the Euclidean distance between two f32 slices.
pub fn euclidean_distance(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "euclidean_distance: dimension mismatch");
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x - y) * (x - y))
        .sum::<f32>()
        .sqrt()
}

/// Compute the mean silhouette score for a clustering.
///
/// For each point, computes `(b - a) / max(a, b)` where:
/// - `a` = average distance to other points in the same cluster
/// - `b` = average distance to points in the nearest other cluster
///
/// Points in singleton clusters (where `a` is undefined) get a score of 0.
/// Returns the mean silhouette coefficient across all points.
pub fn silhouette_score(data: &Array2<f32>, assignments: &[usize], k: usize) -> f32 {
    let n = data.nrows();
    if n <= 1 || k <= 1 {
        return 0.0;
    }

    let mut total = 0.0_f32;
    for i in 0..n {
        let my_cluster = assignments[i];
        let point_i = data.row(i);

        // Compute average distance to same-cluster points (a)
        // and average distance to each other cluster (to find b)
        let mut same_sum = 0.0_f32;
        let mut same_count = 0_usize;
        let mut other_sums = vec![0.0_f32; k];
        let mut other_counts = vec![0_usize; k];

        for j in 0..n {
            if i == j {
                continue;
            }
            let dist =
                euclidean_distance(point_i.as_slice().unwrap(), data.row(j).as_slice().unwrap());
            if assignments[j] == my_cluster {
                same_sum += dist;
                same_count += 1;
            } else {
                other_sums[assignments[j]] += dist;
                other_counts[assignments[j]] += 1;
            }
        }

        // Singleton cluster: score is 0 by convention
        if same_count == 0 {
            continue;
        }

        let a = same_sum / same_count as f32;

        // Find nearest other cluster
        let mut b = f32::INFINITY;
        for c in 0..k {
            if c == my_cluster || other_counts[c] == 0 {
                continue;
            }
            let avg = other_sums[c] / other_counts[c] as f32;
            if avg < b {
                b = avg;
            }
        }

        if b.is_infinite() {
            // Only one cluster has points -- shouldn't happen with k >= 2
            continue;
        }

        let s = (b - a) / a.max(b);
        total += s;
    }

    total / n as f32
}

/// Sort cluster members by ascending distance to centroid.
fn sort_members_by_distance(members: &mut [ClusterMember]) {
    members.sort_by(|a, b| {
        a.distance_to_centroid
            .partial_cmp(&b.distance_to_centroid)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// Check whether all embedding vectors are identical.
fn all_identical(embeddings: &[(i64, Vec<f32>)]) -> bool {
    if embeddings.len() <= 1 {
        return true;
    }
    let first = &embeddings[0].1;
    embeddings[1..].iter().all(|e| e.1 == *first)
}

/// Build a single cluster containing all embeddings.
///
/// Used as a fallback for edge cases: fewer than 3 points or all identical
/// vectors (where K-Means cannot meaningfully partition).
fn make_single_cluster(embeddings: &[(i64, Vec<f32>)]) -> Vec<Cluster> {
    if embeddings.is_empty() {
        return Vec::new();
    }

    let dim = embeddings[0].1.len();

    // Centroid is the mean of all points.
    let mut centroid = vec![0.0_f32; dim];
    for (_, vec) in embeddings {
        for (c, v) in centroid.iter_mut().zip(vec.iter()) {
            *c += v;
        }
    }
    let n = embeddings.len() as f32;
    for c in &mut centroid {
        *c /= n;
    }

    let mut members: Vec<ClusterMember> = embeddings
        .iter()
        .map(|(id, vec)| ClusterMember {
            symbol_id: *id,
            symbol_name: String::new(),
            symbol_kind: SymbolKind::Function,
            file: String::new(),
            line: 0,
            distance_to_centroid: euclidean_distance(vec, &centroid),
        })
        .collect();

    sort_members_by_distance(&mut members);

    let representative_symbols = members.iter().take(NUM_REPRESENTATIVES).cloned().collect();

    vec![Cluster {
        cluster_id: 0,
        centroid,
        members,
        representative_symbols,
    }]
}

/// Build `Cluster` structs from K-Means results.
///
/// Given the original embeddings, cluster assignments, and centroid matrix,
/// constructs `Cluster` values with members sorted by ascending distance
/// to centroid and representative symbols as the closest members.
fn build_clusters(
    embeddings: &[(i64, Vec<f32>)],
    assignments: &[usize],
    centroids: &Array2<f32>,
    k: usize,
) -> Vec<Cluster> {
    let mut cluster_members: Vec<Vec<ClusterMember>> = vec![Vec::new(); k];

    for (idx, (id, vec)) in embeddings.iter().enumerate() {
        let cluster_idx = assignments[idx];
        let centroid = centroids.row(cluster_idx);
        let dist = euclidean_distance(vec, centroid.as_slice().unwrap());
        cluster_members[cluster_idx].push(ClusterMember {
            symbol_id: *id,
            symbol_name: String::new(),
            symbol_kind: SymbolKind::Function,
            file: String::new(),
            line: 0,
            distance_to_centroid: dist,
        });
    }

    let mut clusters = Vec::with_capacity(k);
    for (i, mut members) in cluster_members.into_iter().enumerate() {
        if members.is_empty() {
            continue;
        }

        sort_members_by_distance(&mut members);

        let representative_symbols = members.iter().take(NUM_REPRESENTATIVES).cloned().collect();

        let centroid_row = centroids.row(i);
        clusters.push(Cluster {
            cluster_id: i,
            centroid: centroid_row.to_vec(),
            members,
            representative_symbols,
        });
    }

    clusters
}

/// Cluster symbol embeddings using K-Means with automatic k selection.
///
/// Runs K-Means for k = 2..min(sqrt(n), max_k, 20), computes silhouette
/// scores, and selects the k with the highest score. Uses K-Means++
/// initialization for robust centroid seeding.
///
/// Returns a `Vec<Cluster>` with members sorted by ascending distance to
/// their cluster centroid. Each cluster's `representative_symbols` field
/// contains the 5 members closest to the centroid.
///
/// # Edge cases
///
/// - Empty input: returns empty Vec
/// - Fewer than 3 embeddings: returns a single cluster
/// - All identical embeddings: returns a single cluster
pub fn cluster_embeddings(embeddings: &[(i64, Vec<f32>)], max_k: usize) -> Vec<Cluster> {
    if embeddings.is_empty() {
        return Vec::new();
    }

    let n = embeddings.len();

    // Edge case: fewer than 3 points -- can't meaningfully cluster
    if n < 3 {
        return make_single_cluster(embeddings);
    }

    // Edge case: all identical vectors
    if all_identical(embeddings) {
        return make_single_cluster(embeddings);
    }

    let dim = embeddings[0].1.len();

    // Build ndarray matrix from embeddings
    let flat: Vec<f32> = embeddings
        .iter()
        .flat_map(|(_, v)| v.iter().copied())
        .collect();
    let data = Array2::from_shape_vec((n, dim), flat).expect("shape mismatch building data matrix");

    // Determine k range: 2..=min(sqrt(n), max_k, ABSOLUTE_MAX_K)
    let sqrt_n = (n as f64).sqrt().ceil() as usize;
    let effective_max_k = sqrt_n.min(max_k).min(ABSOLUTE_MAX_K);

    if effective_max_k < 2 {
        return make_single_cluster(embeddings);
    }

    let dataset = linfa::DatasetBase::from(data.clone());

    let mut best_k = 2;
    let mut best_score = f32::NEG_INFINITY;
    let mut best_assignments: Option<Vec<usize>> = None;
    let mut best_centroids: Option<Array2<f32>> = None;

    let rng = StdRng::seed_from_u64(42);

    for k in 2..=effective_max_k {
        let model = match KMeans::params_with_rng(k, rng.clone())
            .tolerance(1e-4)
            .max_n_iterations(100)
            .n_runs(3)
            .fit(&dataset)
        {
            Ok(m) => m,
            Err(_) => continue,
        };

        let predictions = model.predict(&dataset);
        let assignments: Vec<usize> = predictions.as_targets().to_vec();

        let score = silhouette_score(&data, &assignments, k);

        if score > best_score {
            best_score = score;
            best_k = k;
            best_assignments = Some(assignments);
            best_centroids = Some(model.centroids().clone());
        }
    }

    match (best_assignments, best_centroids) {
        (Some(assignments), Some(centroids)) => {
            build_clusters(embeddings, &assignments, &centroids, best_k)
        }
        _ => make_single_cluster(embeddings),
    }
}

/// Populate cluster members with symbol metadata from the database.
///
/// For each `ClusterMember` across all clusters, looks up `symbol_name`,
/// `symbol_kind`, `file`, and `line` from the `symbols` table. Members
/// whose `symbol_id` is not found in the database are left unchanged.
///
/// Also resolves metadata for `representative_symbols`.
pub fn resolve_cluster_members(
    conn: &Connection,
    clusters: &mut [Cluster],
) -> Result<(), EmbeddingError> {
    // Collect all unique symbol IDs across all clusters.
    let mut all_ids: Vec<i64> = Vec::new();
    for cluster in clusters.iter() {
        for member in &cluster.members {
            all_ids.push(member.symbol_id);
        }
    }
    all_ids.sort_unstable();
    all_ids.dedup();

    if all_ids.is_empty() {
        return Ok(());
    }

    // Build an IN (...) clause.
    let placeholders: Vec<String> = (1..=all_ids.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "SELECT id, name, kind, file, line FROM symbols WHERE id IN ({})",
        placeholders.join(", ")
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    let params: Vec<&dyn rusqlite::types::ToSql> = all_ids
        .iter()
        .map(|id| id as &dyn rusqlite::types::ToSql)
        .collect();

    struct SymbolFields {
        name: String,
        kind: SymbolKind,
        file: String,
        line: usize,
    }

    let rows = stmt
        .query_map(params.as_slice(), |row| {
            let id: i64 = row.get(0)?;
            let name: String = row.get(1)?;
            let kind_str: String = row.get(2)?;
            let file: String = row.get(3)?;
            let line: usize = row.get::<_, i64>(4)? as usize;
            Ok((id, name, kind_str, file, line))
        })
        .map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;

    let mut by_id: HashMap<i64, SymbolFields> = HashMap::with_capacity(all_ids.len());
    for r in rows {
        let (id, name, kind_str, file, line) =
            r.map_err(|e| EmbeddingError::StorageFailed(e.to_string()))?;
        let kind = kind_str
            .parse::<SymbolKind>()
            .unwrap_or(SymbolKind::Function);
        by_id.insert(
            id,
            SymbolFields {
                name,
                kind,
                file,
                line,
            },
        );
    }

    // Apply metadata to all members and representatives.
    fn apply_metadata(member: &mut ClusterMember, by_id: &HashMap<i64, SymbolFields>) {
        if let Some(sym) = by_id.get(&member.symbol_id) {
            member.symbol_name = sym.name.clone();
            member.symbol_kind = sym.kind;
            member.file = sym.file.clone();
            member.line = sym.line;
        }
    }

    for cluster in clusters.iter_mut() {
        for member in &mut cluster.members {
            apply_metadata(member, &by_id);
        }
        for rep in &mut cluster.representative_symbols {
            apply_metadata(rep, &by_id);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use ndarray::Array2;

    #[test]
    fn euclidean_distance_basic() {
        // distance between (0,0) and (3,4) should be 5.0
        let a = [0.0_f32, 0.0];
        let b = [3.0_f32, 4.0];
        let dist = super::euclidean_distance(&a, &b);
        assert!((dist - 5.0).abs() < 1e-6);
    }

    #[test]
    fn euclidean_distance_same_point() {
        let a = [1.0_f32, 2.0, 3.0];
        let dist = super::euclidean_distance(&a, &a);
        assert!((dist - 0.0).abs() < 1e-6);
    }

    #[test]
    fn silhouette_score_perfect_clusters() {
        // Two well-separated clusters: (0,0),(1,0) and (100,0),(101,0)
        // Silhouette score should be close to 1.0
        let data =
            Array2::from_shape_vec((4, 2), vec![0.0_f32, 0.0, 1.0, 0.0, 100.0, 0.0, 101.0, 0.0])
                .unwrap();
        let assignments = [0, 0, 1, 1];
        let score = super::silhouette_score(&data, &assignments, 2);
        assert!(score > 0.9, "expected score > 0.9, got {score}");
    }

    #[test]
    fn silhouette_score_single_point_per_cluster() {
        // With one point per cluster, a=0 so silhouette is 0 by convention
        let data = Array2::from_shape_vec((2, 2), vec![0.0_f32, 0.0, 10.0, 0.0]).unwrap();
        let assignments = [0, 1];
        let score = super::silhouette_score(&data, &assignments, 2);
        assert!(
            score.abs() < 1e-6,
            "expected score ~0.0 for singleton clusters, got {score}"
        );
    }

    // -----------------------------------------------------------------------
    // cluster_embeddings tests
    // -----------------------------------------------------------------------

    #[test]
    fn cluster_empty_input() {
        let embeddings: Vec<(i64, Vec<f32>)> = vec![];
        let clusters = super::cluster_embeddings(&embeddings, 10);
        assert!(clusters.is_empty());
    }

    #[test]
    fn cluster_single_embedding() {
        let embeddings = vec![(1_i64, vec![1.0_f32, 2.0, 3.0])];
        let clusters = super::cluster_embeddings(&embeddings, 10);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].cluster_id, 0);
        assert_eq!(clusters[0].members.len(), 1);
        assert_eq!(clusters[0].members[0].symbol_id, 1);
        assert!((clusters[0].members[0].distance_to_centroid - 0.0).abs() < 1e-6);
    }

    #[test]
    fn cluster_two_embeddings() {
        // Two points -> n < 3 -> single cluster
        let embeddings = vec![(1_i64, vec![0.0_f32, 0.0]), (2_i64, vec![10.0_f32, 0.0])];
        let clusters = super::cluster_embeddings(&embeddings, 10);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members.len(), 2);
    }

    #[test]
    fn cluster_identical_embeddings() {
        // All identical vectors -> single cluster
        let embeddings = vec![
            (1_i64, vec![5.0_f32, 5.0]),
            (2_i64, vec![5.0_f32, 5.0]),
            (3_i64, vec![5.0_f32, 5.0]),
            (4_i64, vec![5.0_f32, 5.0]),
        ];
        let clusters = super::cluster_embeddings(&embeddings, 10);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].members.len(), 4);
    }

    #[test]
    fn cluster_two_groups() {
        // Two well-separated groups: should produce 2 clusters
        let mut embeddings = Vec::new();
        for i in 0..20 {
            embeddings.push((i as i64, vec![0.0_f32 + (i as f32) * 0.1, 0.0]));
        }
        for i in 20..40 {
            embeddings.push((i as i64, vec![100.0_f32 + (i as f32) * 0.1, 0.0]));
        }
        let clusters = super::cluster_embeddings(&embeddings, 10);
        assert_eq!(
            clusters.len(),
            2,
            "expected 2 clusters for 2 well-separated groups"
        );
        // Each cluster should have 20 members
        let mut sizes: Vec<usize> = clusters.iter().map(|c| c.members.len()).collect();
        sizes.sort();
        assert_eq!(sizes, vec![20, 20]);
    }

    #[test]
    fn cluster_three_groups() {
        // Three well-separated groups
        let mut embeddings = Vec::new();
        for i in 0..15 {
            embeddings.push((i as i64, vec![0.0_f32 + (i as f32) * 0.1, 0.0]));
        }
        for i in 15..30 {
            embeddings.push((i as i64, vec![100.0_f32 + (i as f32) * 0.1, 0.0]));
        }
        for i in 30..45 {
            embeddings.push((i as i64, vec![200.0_f32 + (i as f32) * 0.1, 0.0]));
        }
        let clusters = super::cluster_embeddings(&embeddings, 10);
        assert!(
            clusters.len() >= 2 && clusters.len() <= 4,
            "expected 2-4 clusters for 3 well-separated groups, got {}",
            clusters.len()
        );
    }

    #[test]
    fn cluster_respects_max_k() {
        // With max_k = 2, should never produce more than 2 clusters
        let mut embeddings = Vec::new();
        for i in 0..10 {
            embeddings.push((i as i64, vec![0.0_f32, 0.0]));
        }
        for i in 10..20 {
            embeddings.push((i as i64, vec![100.0_f32, 0.0]));
        }
        for i in 20..30 {
            embeddings.push((i as i64, vec![200.0_f32, 0.0]));
        }
        let clusters = super::cluster_embeddings(&embeddings, 2);
        assert!(
            clusters.len() <= 2,
            "max_k=2 but got {} clusters",
            clusters.len()
        );
    }

    #[test]
    fn cluster_cap_at_20() {
        // Even if max_k > 20, it should cap at 20
        let mut embeddings = Vec::new();
        for i in 0..500 {
            embeddings.push((i as i64, vec![i as f32, 0.0]));
        }
        // With 500 points, sqrt(500) ~ 22, but cap should be 20
        let clusters = super::cluster_embeddings(&embeddings, 50);
        assert!(
            clusters.len() <= 20,
            "cap at 20 but got {} clusters",
            clusters.len()
        );
    }

    #[test]
    fn cluster_representative_symbols() {
        // Representatives should be the top 5 closest to centroid
        let mut embeddings = Vec::new();
        for i in 0..20 {
            embeddings.push((i as i64, vec![0.0_f32 + (i as f32) * 0.1, 0.0]));
        }
        for i in 20..40 {
            embeddings.push((i as i64, vec![100.0_f32 + (i as f32) * 0.1, 0.0]));
        }
        let clusters = super::cluster_embeddings(&embeddings, 10);
        for cluster in &clusters {
            assert!(
                cluster.representative_symbols.len() <= 5,
                "representatives should be at most 5, got {}",
                cluster.representative_symbols.len()
            );
            // Representatives should be a prefix of sorted members
            for (i, rep) in cluster.representative_symbols.iter().enumerate() {
                assert_eq!(rep.symbol_id, cluster.members[i].symbol_id);
            }
        }
    }

    // -----------------------------------------------------------------------
    // resolve_cluster_members tests
    // -----------------------------------------------------------------------

    fn setup_test_db() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE symbols (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                file TEXT NOT NULL,
                line INTEGER NOT NULL,
                col INTEGER NOT NULL,
                end_line INTEGER,
                scope TEXT,
                signature TEXT,
                language TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn insert_symbol(
        conn: &rusqlite::Connection,
        id: i64,
        name: &str,
        kind: &str,
        file: &str,
        line: i64,
    ) {
        conn.execute(
            "INSERT INTO symbols (id, name, kind, file, line, col, signature, language)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, '', 'Rust')",
            rusqlite::params![id, name, kind, file, line],
        )
        .unwrap();
    }

    #[test]
    fn resolve_cluster_members_populates_metadata() {
        use crate::types::SymbolKind;

        let conn = setup_test_db();
        insert_symbol(&conn, 1, "foo", "function", "src/lib.rs", 42);
        insert_symbol(&conn, 2, "Bar", "struct", "src/types.rs", 10);

        let mut clusters = vec![crate::types::Cluster {
            cluster_id: 0,
            centroid: vec![0.0],
            members: vec![
                crate::types::ClusterMember {
                    symbol_id: 1,
                    symbol_name: String::new(),
                    symbol_kind: SymbolKind::Function,
                    file: String::new(),
                    line: 0,
                    distance_to_centroid: 0.5,
                },
                crate::types::ClusterMember {
                    symbol_id: 2,
                    symbol_name: String::new(),
                    symbol_kind: SymbolKind::Function,
                    file: String::new(),
                    line: 0,
                    distance_to_centroid: 1.0,
                },
            ],
            representative_symbols: vec![crate::types::ClusterMember {
                symbol_id: 1,
                symbol_name: String::new(),
                symbol_kind: SymbolKind::Function,
                file: String::new(),
                line: 0,
                distance_to_centroid: 0.5,
            }],
        }];

        super::resolve_cluster_members(&conn, &mut clusters).unwrap();

        assert_eq!(clusters[0].members[0].symbol_name, "foo");
        assert_eq!(clusters[0].members[0].symbol_kind, SymbolKind::Function);
        assert_eq!(clusters[0].members[0].file, "src/lib.rs");
        assert_eq!(clusters[0].members[0].line, 42);

        assert_eq!(clusters[0].members[1].symbol_name, "Bar");
        assert_eq!(clusters[0].members[1].symbol_kind, SymbolKind::Struct);
        assert_eq!(clusters[0].members[1].file, "src/types.rs");
        assert_eq!(clusters[0].members[1].line, 10);

        // Representatives should also be resolved
        assert_eq!(clusters[0].representative_symbols[0].symbol_name, "foo");
    }

    #[test]
    fn resolve_cluster_members_skips_missing_symbols() {
        let conn = setup_test_db();
        insert_symbol(&conn, 1, "foo", "function", "src/lib.rs", 42);
        // symbol_id 999 doesn't exist

        let mut clusters = vec![crate::types::Cluster {
            cluster_id: 0,
            centroid: vec![0.0],
            members: vec![
                crate::types::ClusterMember {
                    symbol_id: 1,
                    symbol_name: String::new(),
                    symbol_kind: crate::types::SymbolKind::Function,
                    file: String::new(),
                    line: 0,
                    distance_to_centroid: 0.5,
                },
                crate::types::ClusterMember {
                    symbol_id: 999,
                    symbol_name: String::new(),
                    symbol_kind: crate::types::SymbolKind::Function,
                    file: String::new(),
                    line: 0,
                    distance_to_centroid: 1.0,
                },
            ],
            representative_symbols: vec![],
        }];

        super::resolve_cluster_members(&conn, &mut clusters).unwrap();

        // Symbol 1 should be resolved, 999 stays with defaults
        assert_eq!(clusters[0].members[0].symbol_name, "foo");
        assert_eq!(clusters[0].members[1].symbol_name, "");
    }

    /// Performance test: clustering 5000 symbols should complete in < 5s.
    /// Only applies to release builds where optimizations are enabled.
    #[test]
    #[cfg(not(debug_assertions))]
    fn cluster_5000_symbols_performance() {
        // 5000 embeddings of dimension 128, arranged in ~5 groups
        let dim = 128;
        let n = 5000;
        let mut embeddings = Vec::with_capacity(n);
        let group_size = n / 5;
        for group in 0..5 {
            let offset = group as f32 * 100.0;
            for i in 0..group_size {
                let mut vec = vec![0.0_f32; dim];
                for d in 0..dim {
                    vec[d] = offset + (i as f32 * 0.01) + (d as f32 * 0.001);
                }
                embeddings.push(((group * group_size + i) as i64, vec));
            }
        }

        let start = std::time::Instant::now();
        let clusters = super::cluster_embeddings(&embeddings, 10);
        let elapsed = start.elapsed();

        assert!(
            elapsed.as_secs() < 5,
            "clustering 5000 symbols took {:?}, expected < 5s",
            elapsed
        );
        assert!(!clusters.is_empty(), "should produce at least 1 cluster");
    }

    #[test]
    fn cluster_members_sorted_by_distance() {
        let mut embeddings = Vec::new();
        for i in 0..20 {
            embeddings.push((i as i64, vec![0.0_f32 + (i as f32) * 0.5, 0.0]));
        }
        for i in 20..40 {
            embeddings.push((i as i64, vec![100.0_f32 + (i as f32) * 0.5, 0.0]));
        }
        let clusters = super::cluster_embeddings(&embeddings, 10);
        for cluster in &clusters {
            for w in cluster.members.windows(2) {
                assert!(
                    w[0].distance_to_centroid <= w[1].distance_to_centroid,
                    "members should be sorted by ascending distance: {} > {}",
                    w[0].distance_to_centroid,
                    w[1].distance_to_centroid
                );
            }
        }
    }
}
