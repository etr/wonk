//! Shingle signatures and the novelty/near-duplicate machinery
//! (TASK-100, DR-041, PRD-DUP-REQ-001/002/003/006).
//!
//! A signature is a bottom-k shingle sketch: the body is tokenized with the
//! canonical [`crate::tokenizer::tokenize`], hashed over every 5-token
//! window with xxh3_64, and the 64 smallest distinct (low-32-truncated)
//! hashes are stored sorted ascending as a little-endian blob — at most
//! 256 bytes per symbol. Sketches are written at index time into
//! `symbol_shingles` and are the ONLY thing similarity ever reads: no
//! symbol body is fetched at query time (PRD-DUP-REQ-002).
//!
//! Determinism: tokenize → truncate-to-u32 → sort → dedup → bottom-k is
//! order-independent and xxh3_64 is a fixed spec, so the same body yields
//! the identical blob on every run, Rust version, and platform.
//!
//! REQ-003 "record near-duplicates" reading: the requirement's trigger is
//! the system *determining* that two symbols exceed the threshold.
//! DR-041's index-time consequence is the SIGNATURE table only, so pairs
//! are recorded by the two cheap deterministic writers here — the
//! query-time memo ([`record_near_duplicate_pairs`], fed by the novelty
//! pass which compares candidates anyway) and the `wonk duplicates`
//! sweep ([`sweep_near_duplicates`], hash-bucketed so it is never
//! all-pairs). Nothing quadratic ever runs at index time.

use std::collections::HashMap;
use std::collections::HashSet;

use rusqlite::Connection;

/// Tokens per shingle window. At 5, unrelated functions share almost no
/// 5-grams while common short idioms (`fn get(&self) -> &str`) stay below
/// the window, so boilerplate does not inflate similarity (AR-041).
pub const SHINGLE_K: usize = 5;

/// Sketch size: the k smallest distinct shingle hashes kept per symbol.
/// Bottom-k Jaccard error ~ sqrt(J(1−J)/64) ≈ 0.045 at J=0.85 — adequate
/// for a bounded demotion (not a deletion) decision.
pub const SKETCH_SIZE: usize = 64;

/// Bodies longer than this many tokens are truncated before shingling,
/// bounding index-time cost (storage is fixed at SKETCH_SIZE regardless).
pub const MAX_BODY_TOKENS: usize = 8192;

/// How many of a sketch's smallest hashes the sweep buckets a signature
/// under. Two near-duplicates share at least one of their 4 smallest
/// hashes with probability ≈ 1−(1−J)^4 ≈ 0.999 at J=0.85, so a hash
/// bucket preselects comparison candidates cheaply and deterministically.
pub const BUCKET_KEY_COUNT: usize = 4;

/// Oversized buckets (mass-generated boilerplate) compare only their
/// first this-many members in (file, line) order — bounded work, honest
/// output (the report notes the truncation).
pub const MAX_BUCKET_MEMBERS: usize = 1024;

/// One symbol's non-empty sketch, correlated to its symbol by start line
/// (unique within a file). Written into `symbol_shingles` at index time.
#[derive(Debug, Clone, PartialEq)]
pub struct ShingleSignature {
    /// 1-based symbol start line (the `symbols.line` value).
    pub line: usize,
    /// Sorted ascending distinct bottom-k shingle hashes.
    pub sketch: Vec<u32>,
}

/// A pair of positions whose sketch Jaccard exceeds the duplicate
/// threshold, surfaced by the novelty pass (and recorded by
/// [`record_near_duplicate_pairs`]).
#[derive(Debug, Clone, PartialEq)]
pub struct NearDuplicatePair {
    /// First position, in pre-novelty rank order.
    pub a: (String, u64),
    /// Second position, in pre-novelty rank order.
    pub b: (String, u64),
    /// Sketch Jaccard at detection time.
    pub similarity: f32,
}

/// One member of a reported duplicate group.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupMember {
    /// Path relative to repo root.
    pub file: String,
    /// 1-based definition line.
    pub line: u64,
    /// Symbol kind as indexed (e.g. "function").
    pub kind: String,
    /// Symbol name as indexed.
    pub name: String,
}

/// A connected component (size >= 2) of the `sim > threshold` graph over
/// the sweep's candidate pairs, with the mean internal pair similarity.
#[derive(Debug, Clone, PartialEq)]
pub struct DuplicateGroup {
    /// Members ordered by (file, line).
    pub members: Vec<GroupMember>,
    /// Mean similarity over the qualifying pairs internal to the group.
    pub mean_similarity: f32,
}

/// The `wonk duplicates` sweep result: groups ordered by size descending,
/// then first member (file, line) ascending.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DuplicatesReport {
    /// Duplicate groups (singletons dropped).
    pub groups: Vec<DuplicateGroup>,
    /// Number of buckets truncated to MAX_BUCKET_MEMBERS — nonzero means
    /// the sweep compared a subset and more duplicates may exist.
    pub truncated_buckets: usize,
}

/// Hash every shingle of `body` in body order: tokenize (canonical
/// lowercase alphanumeric tokenizer), truncate to the first
/// [`MAX_BODY_TOKENS`] tokens, hash each [`SHINGLE_K`]-token window joined
/// by a single space with xxh3_64.
pub fn shingle_hashes(body: &str) -> Vec<u64> {
    let mut tokens = crate::tokenizer::tokenize(body);
    tokens.truncate(MAX_BODY_TOKENS);
    if tokens.len() < SHINGLE_K {
        return Vec::new();
    }
    tokens
        .windows(SHINGLE_K)
        .map(|window| xxhash_rust::xxh3::xxh3_64(window.join(" ").as_bytes()))
        .collect()
}

/// The bottom-k sketch of `body`: the [`SKETCH_SIZE`] smallest distinct
/// low-32 bits of its shingle hashes, sorted ascending. Bodies with
/// fewer than [`SHINGLE_K`] tokens yield an empty sketch (no row).
pub fn body_signature(body: &str) -> Vec<u32> {
    let mut truncated: Vec<u32> = shingle_hashes(body).iter().map(|h| *h as u32).collect();
    truncated.sort_unstable();
    truncated.dedup();
    truncated.truncate(SKETCH_SIZE);
    truncated
}

/// The bucket keys of a sketch: its [`BUCKET_KEY_COUNT`] smallest hashes
/// (the sketch is stored sorted, so its first entries).
pub fn bucket_keys(sketch: &[u32]) -> Vec<u32> {
    sketch.iter().copied().take(BUCKET_KEY_COUNT).collect()
}

/// Encode a sketch as a little-endian u32 blob (sorted as given).
pub fn encode_sketch(sketch: &[u32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(sketch.len() * 4);
    for hash in sketch {
        blob.extend_from_slice(&hash.to_le_bytes());
    }
    blob
}

/// Decode a little-endian u32 blob back into a sketch. A trailing partial
/// word (corrupt row) is ignored rather than propagated.
pub fn decode_sketch(blob: &[u8]) -> Vec<u32> {
    blob.chunks_exact(4)
        .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
        .collect()
}

/// Exact Jaccard similarity `|A∩B| / |A∪B|` of two sorted sketches via
/// one merge — O(len a + len b), no hashing, no body reads.
pub fn sketch_jaccard(a: &[u32], b: &[u32]) -> f32 {
    let (mut i, mut j) = (0usize, 0usize);
    let (mut intersection, mut union) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => {
                union += 1;
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                union += 1;
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                union += 1;
                intersection += 1;
                i += 1;
                j += 1;
            }
        }
    }
    union += (a.len() - i) + (b.len() - j);
    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

/// The novelty redundancy ramp (PRD-DUP-REQ-004, AR-041): exactly 0 at or
/// below the threshold, rising continuously to 1 at identity, so a
/// borderline boilerplate pair just over the threshold demotes only
/// slightly. `clamp01((max_sim - threshold) / (1 - threshold))`.
pub fn novelty_redundancy(max_sim: f32, threshold: f32) -> f32 {
    if !max_sim.is_finite() || !threshold.is_finite() {
        return 0.0;
    }
    if max_sim >= 1.0 {
        // At or past identity the ramp saturates; a degenerate
        // threshold of 1.0 leaves only identity redundant.
        return 1.0;
    }
    if max_sim <= threshold {
        return 0.0;
    }
    ((max_sim - threshold) / (1.0 - threshold)).clamp(0.0, 1.0)
}

/// Persist qualifying pairs into `near_duplicates`, resolving each
/// (file, line) to its smallest symbol id, canonicalizing
/// `symbol_id_a < symbol_id_b`, one transaction, INSERT OR REPLACE (the
/// memo is a cache; re-recording refreshes the similarity).
///
/// Positions that no longer resolve to a symbol (edited away between
/// ranking and recording) are skipped silently — a memo must never fail
/// a search.
pub fn record_near_duplicate_pairs(
    conn: &Connection,
    pairs: &[NearDuplicatePair],
) -> anyhow::Result<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    {
        let mut resolve = tx.prepare(
            "SELECT id FROM symbols WHERE file = ?1 AND line = ?2 \
             ORDER BY id ASC LIMIT 1",
        )?;
        let mut insert = tx.prepare(
            "INSERT OR REPLACE INTO near_duplicates (symbol_id_a, symbol_id_b, similarity) \
             VALUES (?1, ?2, ?3)",
        )?;
        for pair in pairs {
            let a: Option<i64> = resolve
                .query_row(rusqlite::params![pair.a.0, pair.a.1 as i64], |row| {
                    row.get(0)
                })
                .ok();
            let b: Option<i64> = resolve
                .query_row(rusqlite::params![pair.b.0, pair.b.1 as i64], |row| {
                    row.get(0)
                })
                .ok();
            let (Some(a), Some(b)) = (a, b) else {
                continue;
            };
            if a == b {
                continue;
            }
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            insert.execute(rusqlite::params![lo, hi, pair.similarity])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// The dispatch-layer memo policy (REQ-003): persist the pairs the
/// novelty pass surfaced, best-effort — a missing connection (grep
/// fallback search) is a no-op and a write failure degrades with a
/// stderr warning, never failing the search.
pub fn record_pairs_best_effort(conn: Option<&Connection>, pairs: &[NearDuplicatePair]) {
    let Some(conn) = conn else {
        return;
    };
    if pairs.is_empty() {
        return;
    }
    if let Err(e) = record_near_duplicate_pairs(conn, pairs) {
        eprintln!("warn: could not record near-duplicate pairs: {e:#}");
    }
}

/// One indexed signature joined to its symbol metadata.
struct SweepEntry {
    symbol_id: i64,
    member: GroupMember,
    sketch: Vec<u32>,
}

/// Union-find `find` with path halving over entry indices.
fn uf_find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

/// Sweep every indexed signature, bucket by the smallest hashes, compare
/// within buckets (capped at [`MAX_BUCKET_MEMBERS`]), record qualifying
/// pairs into `near_duplicates`, and return the union-find groups with
/// singletons dropped.
///
/// Deterministic end to end: signatures load ordered by symbol id, bucket
/// keys walk in sorted order, bucket members compare in (file, line,
/// symbol id) order, and groups emit sorted — no HashMap iteration order
/// feeds the output.
pub fn sweep_near_duplicates(
    conn: &Connection,
    threshold: f32,
) -> anyhow::Result<DuplicatesReport> {
    let mut entries: Vec<SweepEntry> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT ss.symbol_id, s.file, s.line, s.kind, s.name, ss.signature \
             FROM symbol_shingles ss JOIN symbols s ON s.id = ss.symbol_id \
             ORDER BY ss.symbol_id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Vec<u8>>(5)?,
            ))
        })?;
        for (symbol_id, file, line, kind, name, blob) in rows.flatten() {
            let sketch = decode_sketch(&blob);
            if sketch.is_empty() {
                continue;
            }
            entries.push(SweepEntry {
                symbol_id,
                member: GroupMember {
                    file,
                    line: line as u64,
                    kind,
                    name,
                },
                sketch,
            });
        }
    }

    // Bucket every signature under each of its smallest hashes.
    let mut buckets: HashMap<u32, Vec<usize>> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        for key in bucket_keys(&entry.sketch) {
            buckets.entry(key).or_default().push(idx);
        }
    }

    // Compare within buckets, capped; a pair is compared at most once
    // across the buckets it shares.
    let mut compared: HashSet<(usize, usize)> = HashSet::new();
    let mut qualifying: HashMap<(usize, usize), f32> = HashMap::new();
    let mut truncated_buckets = 0usize;
    let mut keys: Vec<u32> = buckets.keys().copied().collect();
    keys.sort_unstable();
    for key in &keys {
        let mut members = buckets.remove(key).unwrap_or_default();
        members.sort_by(|&a, &b| {
            let (ma, mb) = (&entries[a].member, &entries[b].member);
            ma.file
                .cmp(&mb.file)
                .then(ma.line.cmp(&mb.line))
                .then(entries[a].symbol_id.cmp(&entries[b].symbol_id))
        });
        members.dedup();
        if members.len() > MAX_BUCKET_MEMBERS {
            members.truncate(MAX_BUCKET_MEMBERS);
            truncated_buckets += 1;
        }
        for w in 0..members.len() {
            for v in w + 1..members.len() {
                let (i, j) = (members[w], members[v]);
                let key = (i.min(j), i.max(j));
                if !compared.insert(key) {
                    continue;
                }
                let sim = sketch_jaccard(&entries[i].sketch, &entries[j].sketch);
                if sim > threshold {
                    qualifying.insert(key, sim);
                }
            }
        }
    }

    // Union-find over the qualifying pairs; components with >= 2 members
    // become groups.
    let mut parent: Vec<usize> = (0..entries.len()).collect();
    for &(i, j) in qualifying.keys() {
        let (ri, rj) = (uf_find(&mut parent, i), uf_find(&mut parent, j));
        if ri != rj {
            parent[ri] = rj;
        }
    }
    let mut component_members: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut component_sims: HashMap<usize, Vec<f32>> = HashMap::new();
    for idx in 0..entries.len() {
        let root = uf_find(&mut parent, idx);
        component_members.entry(root).or_default().push(idx);
    }
    for (&(i, _), &sim) in &qualifying {
        let root = uf_find(&mut parent, i);
        component_sims.entry(root).or_default().push(sim);
    }

    let mut groups: Vec<DuplicateGroup> = component_members
        .into_iter()
        .filter(|(_, members)| members.len() >= 2)
        .map(|(root, members)| {
            let mut members: Vec<GroupMember> = members
                .into_iter()
                .map(|idx| entries[idx].member.clone())
                .collect();
            members.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
            let sims = component_sims.get(&root);
            let mean_similarity =
                sims.map_or(0.0, |sims| sims.iter().sum::<f32>() / sims.len() as f32);
            DuplicateGroup {
                members,
                mean_similarity,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| a.members[0].file.cmp(&b.members[0].file))
            .then_with(|| a.members[0].line.cmp(&b.members[0].line))
    });

    // Record the qualifying pairs (REQ-003), canonical and deterministic.
    let mut pair_rows: Vec<(i64, i64, f32)> = qualifying
        .iter()
        .map(|(&(i, j), &sim)| {
            let (a, b) = (entries[i].symbol_id, entries[j].symbol_id);
            if a < b { (a, b, sim) } else { (b, a, sim) }
        })
        .collect();
    pair_rows.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    if !pair_rows.is_empty() {
        let tx = conn.unchecked_transaction()?;
        {
            let mut insert = tx.prepare(
                "INSERT OR REPLACE INTO near_duplicates (symbol_id_a, symbol_id_b, similarity) \
                 VALUES (?1, ?2, ?3)",
            )?;
            for (a, b, sim) in &pair_rows {
                insert.execute(rusqlite::params![a, b, sim])?;
            }
        }
        tx.commit()?;
    }

    Ok(DuplicatesReport {
        groups,
        truncated_buckets,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shingle_count_is_tokens_minus_k_plus_one() {
        let body = "alpha beta gamma delta epsilon zeta eta";
        let tokens = crate::tokenizer::tokenize(body).len();
        assert_eq!(tokens, 7);
        assert_eq!(shingle_hashes(body).len(), tokens - SHINGLE_K + 1);
        assert_eq!(shingle_hashes(body).len(), 3);
    }

    #[test]
    fn tokens_normalized_lowercase() {
        // Case folding is the canonical tokenizer's; the sketch over
        // differently-cased but lexically identical bodies is identical.
        let upper = "FN ParseHTTPRequest(Payload: &Input) -> Output";
        let lower = "fn parsehttprequest(payload: &input) -> output";
        assert_eq!(shingle_hashes(upper), shingle_hashes(lower));
        assert_eq!(body_signature(upper), body_signature(lower));
    }

    #[test]
    fn identical_bodies_identical_sketches() {
        let body = handler_body();
        assert_eq!(body_signature(&body), body_signature(&body));
        let sketch = body_signature(&body);
        assert!((sketch_jaccard(&sketch, &sketch) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn renamed_copy_stays_above_threshold() {
        // The PRD case: a copy-paste identical modulo the function name in
        // the header must clear the 0.85 default threshold.
        let original = handler_body();
        let renamed = original.replace("handle_user_created", "handle_account_created");
        assert_ne!(original, renamed);
        let a = body_signature(&original);
        let b = body_signature(&renamed);
        let sim = sketch_jaccard(&a, &b);
        assert!(
            sim > 0.85,
            "renamed copy must stay above 0.85, got {sim:.4}"
        );
    }

    #[test]
    fn unrelated_bodies_below_threshold() {
        let a = body_signature(&handler_body());
        let b = body_signature(
            "fn sort_records(items: &mut [Record]) {\n    items.sort_by_key(|r| r.priority);\n    items.dedup_by(|x, y| x.id == y.id);\n}\n",
        );
        let sim = sketch_jaccard(&a, &b);
        assert!(
            sim < 0.85,
            "unrelated bodies must sit well below 0.85, got {sim:.4}"
        );
    }

    #[test]
    fn sketch_capped_sorted_distinct() {
        // A body with far more than SKETCH_SIZE shingles yields exactly
        // SKETCH_SIZE entries, strictly ascending, no duplicates.
        let long: String = (0..500)
            .map(|i| format!("token{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let sketch = body_signature(&long);
        assert_eq!(sketch.len(), SKETCH_SIZE);
        assert!(sketch.windows(2).all(|w| w[0] < w[1]), "strictly ascending");
    }

    #[test]
    fn body_shorter_than_k_no_sketch() {
        assert!(body_signature("fn get() -> &str").is_empty());
        assert!(body_signature("").is_empty());
        // Exactly K tokens produce exactly one shingle.
        assert_eq!(body_signature("alpha beta gamma delta epsilon").len(), 1);
    }

    #[test]
    fn body_over_token_cap_truncated() {
        let filler: Vec<String> = (0..MAX_BODY_TOKENS).map(|i| format!("cap{i}")).collect();
        let prefix: String = filler.join(" ");
        let long = format!("{prefix} beyond the cap these tokens must be ignored");
        assert_eq!(body_signature(&long), body_signature(&prefix));
    }

    #[test]
    fn blob_roundtrip() {
        let sketch = body_signature(&handler_body());
        let blob = encode_sketch(&sketch);
        assert_eq!(blob.len(), sketch.len() * 4);
        assert_eq!(decode_sketch(&blob), sketch);
        // A trailing partial word (corrupt row) is dropped, not fatal.
        let mut corrupt = blob.clone();
        corrupt.push(0xAB);
        assert_eq!(decode_sketch(&corrupt), sketch);
    }

    #[test]
    fn jaccard_identity_zero_disjoint_symmetric() {
        let a: Vec<u32> = vec![1, 3, 5, 7];
        let b: Vec<u32> = vec![1, 3, 5, 7];
        assert!((sketch_jaccard(&a, &b) - 1.0).abs() < 1e-6);
        let c: Vec<u32> = vec![2, 4, 6, 8];
        assert_eq!(sketch_jaccard(&a, &c), 0.0);
        // Symmetric on partial overlap: {1,3,5,7} vs {3,7,9,11} shares 2 of 6.
        let d: Vec<u32> = vec![3, 7, 9, 11];
        let ab = sketch_jaccard(&a, &d);
        let ba = sketch_jaccard(&d, &a);
        assert!((ab - 2.0 / 6.0).abs() < 1e-6);
        assert_eq!(ab, ba);
        // Empty inputs carry no evidence.
        assert_eq!(sketch_jaccard(&[], &a), 0.0);
        assert_eq!(sketch_jaccard(&[], &[]), 0.0);
    }

    #[test]
    fn ramp_zero_at_and_below_threshold() {
        assert_eq!(novelty_redundancy(0.85, 0.85), 0.0);
        assert_eq!(novelty_redundancy(0.70, 0.85), 0.0);
        assert_eq!(novelty_redundancy(0.0, 0.85), 0.0);
    }

    #[test]
    fn ramp_continuous_midpoint() {
        // Halfway between threshold 0.85 and identity: redundancy 0.5.
        let midpoint = 0.85 + (1.0 - 0.85) / 2.0;
        assert!((novelty_redundancy(midpoint, 0.85) - 0.5).abs() < 1e-6);
        // A quarter of the way: 0.25 — continuous, not a cliff.
        let quarter = 0.85 + (1.0 - 0.85) / 4.0;
        assert!((novelty_redundancy(quarter, 0.85) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn ramp_one_at_identity() {
        assert_eq!(novelty_redundancy(1.0, 0.85), 1.0);
        assert_eq!(novelty_redundancy(1.5, 0.85), 1.0);
        // Degenerate threshold 1.0: only identity is redundant.
        assert_eq!(novelty_redundancy(1.0, 1.0), 1.0);
        assert_eq!(novelty_redundancy(0.99, 1.0), 0.0);
    }

    #[test]
    fn xxh3_pinned_value() {
        // Guards an accidental algorithm swap: the 5-token shingle
        // "fn handle request payload validate" hashes to this exact u64
        // under xxh3_64.
        let hashes = shingle_hashes("fn handle request payload validate");
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], 11_047_260_377_780_550_550_u64);
    }

    #[test]
    fn bucket_prefilter_recall_fixture() {
        // Two near-duplicate sketches must share at least one of their 4
        // smallest hashes, or the sweep's bucket prefilter would miss them.
        let original = handler_body();
        let renamed = original.replace("handle_user_created", "handle_account_created");
        let a = body_signature(&original);
        let b = body_signature(&renamed);
        let keys_a: HashSet<u32> = bucket_keys(&a).into_iter().collect();
        let keys_b: HashSet<u32> = bucket_keys(&b).into_iter().collect();
        assert!(keys_a.intersection(&keys_b).count() >= 1);
    }

    // -- persistence: record + sweep over a seeded connection -------------

    fn seeded_conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::TempDir::new().unwrap();
        let conn = crate::db::open(&dir.path().join("index.db")).unwrap();
        (dir, conn)
    }

    fn insert_symbol_with_sketch(
        conn: &Connection,
        name: &str,
        file: &str,
        line: i64,
        body: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO symbols (name, kind, file, line, col, language) \
             VALUES (?1, 'function', ?2, ?3, 0, 'rust')",
            rusqlite::params![name, file, line],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        let blob = encode_sketch(&body_signature(body));
        conn.execute(
            "INSERT INTO symbol_shingles (symbol_id, signature) VALUES (?1, ?2)",
            rusqlite::params![id, blob],
        )
        .unwrap();
        id
    }

    fn recorded_pairs(conn: &Connection) -> Vec<(i64, i64, f32)> {
        let mut stmt = conn
            .prepare(
                "SELECT symbol_id_a, symbol_id_b, similarity FROM near_duplicates \
                      ORDER BY symbol_id_a, symbol_id_b",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, f32>(2)?,
            ))
        })
        .unwrap()
        .flatten()
        .collect()
    }

    #[test]
    fn record_pairs_persists_canonical() {
        let (_dir, conn) = seeded_conn();
        let a = insert_symbol_with_sketch(&conn, "handler_a", "a.rs", 1, &handler_body());
        let b = insert_symbol_with_sketch(&conn, "handler_b", "b.rs", 1, &handler_body());

        // Positions given in either order canonicalize to (min, max).
        record_near_duplicate_pairs(
            &conn,
            &[NearDuplicatePair {
                a: ("b.rs".to_string(), 1),
                b: ("a.rs".to_string(), 1),
                similarity: 1.0,
            }],
        )
        .unwrap();
        assert_eq!(recorded_pairs(&conn), vec![(a.min(b), a.max(b), 1.0)]);

        // Re-recording replaces (refreshes) rather than duplicating.
        record_near_duplicate_pairs(
            &conn,
            &[NearDuplicatePair {
                a: ("a.rs".to_string(), 1),
                b: ("b.rs".to_string(), 1),
                similarity: 0.95,
            }],
        )
        .unwrap();
        assert_eq!(recorded_pairs(&conn).len(), 1);
        assert_eq!(recorded_pairs(&conn)[0].2, 0.95);
    }

    #[test]
    fn sweep_finds_group_and_records_pairs() {
        let (_dir, conn) = seeded_conn();
        let body = handler_body();
        let renamed = body.replace("handle_user_created", "handle_account_created");
        insert_symbol_with_sketch(&conn, "handler_a", "a.rs", 1, &body);
        insert_symbol_with_sketch(&conn, "handler_b", "b.rs", 1, &renamed);
        insert_symbol_with_sketch(
            &conn,
            "sort_records",
            "c.rs",
            1,
            "fn sort_records(items: &mut [Record]) {\n    items.sort_by_key(|r| r.priority);\n}\n",
        );

        let report = sweep_near_duplicates(&conn, 0.85).unwrap();
        assert_eq!(report.truncated_buckets, 0);
        assert_eq!(report.groups.len(), 1, "one group, singleton dropped");
        let group = &report.groups[0];
        assert_eq!(group.members.len(), 2);
        assert_eq!(group.members[0].file, "a.rs");
        assert_eq!(group.members[1].file, "b.rs");
        assert!(group.mean_similarity > 0.85);
        // The sweep records REQ-003 rows itself.
        assert_eq!(recorded_pairs(&conn).len(), 1);
    }

    #[test]
    fn sweep_bucket_cap_truncates_and_notes() {
        let (_dir, conn) = seeded_conn();
        // MAX_BUCKET_MEMBERS + 1 identical bodies: the bucket they all
        // share is truncated, the note is set, and pairs are still
        // recorded among the first MAX_BUCKET_MEMBERS members.
        let body = handler_body();
        for i in 0..=MAX_BUCKET_MEMBERS {
            insert_symbol_with_sketch(&conn, "clone", &format!("f{i}.rs"), 1, &body);
        }
        let report = sweep_near_duplicates(&conn, 0.85).unwrap();
        assert!(report.truncated_buckets >= 1);
        assert_eq!(report.groups.len(), 1);
        assert_eq!(report.groups[0].members.len(), MAX_BUCKET_MEMBERS);
        // Capped bucket: C(1024, 2) qualifying pairs, bounded.
        assert_eq!(
            recorded_pairs(&conn).len(),
            MAX_BUCKET_MEMBERS * (MAX_BUCKET_MEMBERS - 1) / 2
        );
    }

    #[test]
    fn sweep_empty_index_is_empty() {
        let (_dir, conn) = seeded_conn();
        let report = sweep_near_duplicates(&conn, 0.85).unwrap();
        assert_eq!(report.groups.len(), 0);
        assert_eq!(report.truncated_buckets, 0);
    }

    // A realistic ~90-token handler body: validation, dedup, persistence,
    // metrics, notification, audit — the copy-paste archetype.
    fn handler_body() -> String {
        [
            "pub fn handle_user_created(event: &CreateEvent, store: &mut Store) -> Result<(), Error> {",
            "    let user = event.payload_user();",
            "    if user.email.is_empty() {",
            "        return Err(Error::Validation(\"email required\"));",
            "    }",
            "    let existing = store.find_by_email(&user.email)?;",
            "    if existing.is_some() {",
            "        return Err(Error::Conflict(\"email already registered\"));",
            "    }",
            "    let record = store.insert(&user)?;",
            "    let quota = store.quota_for(record.plan)?;",
            "    billing::reserve(&record.id, quota.remaining)?;",
            "    metrics::count(\"user_created\", 1);",
            "    notifier::welcome(&record.email)?;",
            "    audit::log(\"user_created\", record.id);",
            "    Ok(())",
            "}",
        ]
        .join("\n")
    }
}
