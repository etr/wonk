# Unworked Review Issues

**Run:** 2026-09-28 23:32:19
**Task:** TASK-078
**Total:** 29 (0 critical, 2 major, 27 minor)

## Major

1. [x] **code-simplifier** | `src/pipeline.rs:529` | code-structure
   *Addressed (majors sweep, 2026-10-02):* fixed (delete_file_rows).
   upsert_file_data (lines 529-548) and delete_file_data (lines 485-504) now contain five identical DELETE statements (type_edges, symbols, references, file_imports, term_stats). This diff added the term_stats DELETE to both blocks, extending the duplication so every future per-file table must be edited in two places in lockstep.
   *Recommendation:* Extract a helper such as `fn delete_file_rows(tx: &rusqlite::Transaction, rel_path: &str) -> Result<()>` holding the five shared DELETEs; delete_file_data calls it then deletes the files row, upsert_file_data calls it then does the INSERT OR REPLACE. Behavior-preserving and keeps each new table a one-place edit.

2. [x] **test-quality-reviewer** | `/Users/etr/progs/wonk/.worktrees/TASK-078/src/pipeline.rs:1995` | missing-test
   *Addressed (majors sweep, 2026-10-02):* fixed (trigger-based atomicity test).
   test_term_stats_and_files_row_same_transaction claims to pin the acceptance criterion 'stats written in same transaction as symbol rows', but the injected failure (invalid UTF-8) triggers at fs::read_to_string in reindex_file (pipeline.rs:318), before any transaction starts. The test would still pass if insert_term_stats were moved outside the upsert_file_data transaction into its own commit, so the criterion and the AR-024 mitigation ('statistics updated inside the same transaction as the file's symbol/reference rows') are not actually enforced by any test.
   *Recommendation:* Inject a failure inside the transaction instead: create a temp trigger (CREATE TRIGGER boom BEFORE INSERT ON term_stats BEGIN SELECT RAISE(ABORT, 'boom'); END), call reindex_file on a modified file, and assert that files/symbols/term_stats for that file are all unchanged (rollback, not partial write). Drop the trigger and verify a normal reindex then succeeds. Keep the UTF-8 test as a pre-write-failure case if desired.

## Minor

3. [ ] **architecture-alignment-checker** | `src/db.rs:110` | interface-contract
   The implemented index set for term_stats diverges from the architecture doc's section 5.2 index listing (line 1333 declares idx_term_stats_term ON term_stats(term)); the code instead relies on the PK(term,file) leftmost prefix for the term lookup and adds idx_term_stats_file ON term_stats(file) (src/db.rs:118), which the doc does not list. The deviation is functionally equivalent-or-better (a standalone term index would duplicate the PK prefix; the file index is required by the delete-by-file incremental path) and is declared in the frozen baseline assumptions, but the documented schema is now out of sync with the code.
   *Recommendation:* No code change needed. When the architecture document is next sourced from code (groundwork-source-architecture-from-code), update section 5.2's index listing to reflect the PK-prefix term lookup plus idx_term_stats_file so the documented schema matches the shipped one.

4. [ ] **architecture-alignment-checker** | `src/tokenizer.rs:1` | component-boundary
   The new tokenizer.rs module is not reflected in the architecture doc's component map: section 4.26 (line 908) and the component summary (line 190) describe the V5 Lexical Scorer as extending only search.rs + ranker.rs, and the V5 module plan (line 3059) says V5 extends existing modules rather than adding new ones without naming tokenizer.rs. The module itself is correctly structured (flat, std-only, single-responsibility, shared by TASK-078 index time and TASK-079 query time), so this is a documentation gap, not a code defect.
   *Recommendation:* No code change needed. In the next architecture doc sync, add tokenizer.rs to the V5 module/component map (e.g. as the canonical lexical tokenizer consumed by pipeline.rs at index time and search.rs at query time) so the doc's module inventory stays authoritative.

5. [ ] **code-quality-reviewer** | `src/db.rs:111` | consistency
   TERM_STATS_SQL is declared with the raw-string delimiter r"..." while every sibling schema constant (SCHEMA_SQL, TYPE_EDGES_SQL, EMBEDDINGS_SQL, SUMMARIES_SQL, FTS_SQL, TRIGGERS_SQL) uses r#"..."#.
   *Recommendation:* Use r#"..."# for TERM_STATS_SQL to match the established style in the file.

6. [ ] **code-quality-reviewer** | `src/db.rs:304` | diff-hygiene
   The diff rewraps the unchanged doc comment on ensure_type_edges_table ('Safe to call on databases that already...' line break moved), adding noise to a task diff that should only touch term_stats concerns.
   *Recommendation:* Revert the gratuitous reflow of the ensure_type_edges_table doc comment so the diff stays minimal.

7. [ ] **code-quality-reviewer** | `src/pipeline.rs:1995` | test-coverage
   test_term_stats_and_files_row_same_transaction exercises failure-before-write (invalid UTF-8 read fails before any transaction opens), so mid-transaction rollback of a failed insert_term_stats alongside symbol rows is guaranteed only structurally, not by a test.
   *Recommendation:* Either rename/annotate the test to reflect that it verifies failure-before-write atomicity, or accept the structural guarantee (both writes share one unchecked_transaction in upsert_file_data); injecting a mid-tx failure would require a refactor for little marginal assurance.

8. [ ] **code-quality-reviewer** | `src/tokenizer.rs:45` | elegance
   term_frequencies materializes the full Vec<String> of every token via tokenize() before counting, allocating a String per token occurrence only to then allocate HashMap keys; counting directly into the map would avoid the intermediate Vec and roughly halve per-token allocations on full builds.
   *Recommendation:* Optional: inline the scan loop and increment *freqs.entry(...).or_insert(0) directly per token, keeping tokenize() for callers that need the sequence. The bench harness already exists to confirm the win.

9. [ ] **code-simplifier** | `src/db.rs:1911` | patterns
   test_open_creates_term_stats_table opens via open(), discards that connection, then opens a second raw Connection::open to query sqlite_master, whereas the adjacent test_new_db_has_type_edges_table queries the connection returned by open() directly. Needless indirection that diverges from the sibling convention it was modeled on.
   *Recommendation:* Query the `open()` connection directly and drop the reopen (no pragma is needed to read sqlite_master).

10. [ ] **code-simplifier** | `src/pipeline.rs:1376` | patterns
   Contradictory SQLite bind-parameter limits documented in one file: the new insert_term_stats comment says 'bundled SQLite allows 32766 variables' (ROWS_PER_STMT=1000, i.e. 3000 params per statement) while the pre-existing type-edge resolution ~line 892 says 'SQLite variable limit is 999; chunk to stay under it' and chunks at 900.
   *Recommendation:* Reconcile the two comments (state the actual bundled limit once and why each site chose its chunk size), so a future editor does not 'fix' one to match the other incorrectly. No code change needed — both chunks are safe under their stated limits.

11. [ ] **code-simplifier** | `src/pipeline.rs:1636` | patterns
   The tokenizer-oracle comparison is implemented twice: assert_stats_match_disk's per-file body (read -> term_frequencies -> collect actual -> compare len -> per-term loop) is re-implemented inline in test_term_stats_and_files_row_same_transaction (lines 2019-2039), and both use a manual len-then-per-key loop instead of direct map equality.
   *Recommendation:* Extract a per-file helper `fn assert_file_stats_match(conn: &Connection, root: &Path, rel: &str)` used by both, and compare via `assert_eq!(actual, expected_i64_map)` after converting u32->i64 — shorter code and a full diff on failure instead of first-mismatch-only.

12. [x] **code-simplifier** | `src/pipeline.rs:3676` | dead-code
   *Addressed (minors sweep, 2026-10-02):* fixed (helper returns (), median printed internally).
   bench_three_fresh_builds returns a sorted Vec<Duration> that no caller consumes (both #[ignore] bench tests discard the value); only the internal println of the median is used.
   *Recommendation:* Return () and keep the median println, or have a caller actually assert on the durations — an unused return value on a measurement helper is YAGNI.

13. [ ] **code-simplifier** | `src/pipeline.rs:526` | comments
   Stale comment introduced by the diff: upsert_file_data's comment 'Delete old type edges, symbols, references, and imports for this file.' now also deletes term_stats, and delete_file_data's doc (line 477) still says '(symbols, references, file row)' although its body handles type_edges, file_imports, and term_stats too.
   *Recommendation:* Update both lists to include term_stats (or reword to 'all per-file rows' once finding 1's helper lands, which removes the need to enumerate).

14. [ ] **code-simplifier** | `src/pipeline.rs:633` | code-structure
   The term-row collection is duplicated between upsert_file_data (lines 634-639, iterator map/collect style) and batch_insert (lines 862-868, explicit for-loop style). Same logic, two styles, in the same diff.
   *Recommendation:* Move collection inside the helper: `fn insert_term_stats_for(tx: &rusqlite::Transaction, results: &[FileResult]) -> Result<()>` that builds, sorts, and chunks internally. upsert_file_data passes `std::slice::from_ref(result)`, batch_insert passes `results`. Removes both collection blocks and the style mismatch.

15. [ ] **code-simplifier** | `src/tokenizer.rs:26` | code-structure
   Emptiness guard duplicated: the tokenize loop only calls push_token on a separator when `!current.is_empty()`, but push_token itself already handles the empty case (its `!current.is_empty()` check plus clear() being a no-op on an empty String). The invariant is owned in two places, which obscures which one is load-bearing (the helper's check is what makes the final flush-after-loop safe).
   *Recommendation:* Let the helper own the invariant: call push_token unconditionally on separators (`} else { push_token(&mut tokens, &mut current); }`) — behavior is identical since push_token on an empty current is a no-op — and keep a one-line comment that the emptiness check exists for the trailing flush.

16. [ ] **code-simplifier** | `src/tokenizer.rs:59` | dependencies
   Redundant `use std::collections::HashMap;` inside the tests module: `use super::*;` on line 58 already brings the parent module's HashMap import into scope.
   *Recommendation:* Delete line 59; the existing `HashMap::new()` uses resolve through the glob import.

17. [ ] **conventions-reviewer** | `src/tokenizer.rs:1` | project-structure
   The new src/tokenizer.rs module (exported via 'pub mod tokenizer;' in src/lib.rs) has no corresponding row in the Module Responsibilities table in /Users/etr/progs/wonk/.worktrees/TASK-078/CLAUDE.md (lines 32-60), leaving the module's documented role — canonical lexical tokenizer shared by index-time term statistics (TASK-078) and query-time BM25 scoring (TASK-079) — absent from the architecture documentation. This is NOT a violation of an explicit CLAUDE.md rule: the file contains no directive language requiring the table to be exhaustive or updated on module addition, and the table already omits five pre-existing modules (bundled_embedding.rs, color.rs, errors.rs, progress.rs, types.rs) that predate this task, so no exhaustive-table invariant exists to break. Flagged as minor documentation drift introduced by this diff only.
   *Recommendation:* Add a tokenizer.rs row to the Module Responsibilities table in CLAUDE.md (e.g., '| `tokenizer.rs` | Canonical lexical tokenizer — lowercased alphanumeric tokens with separators at all non-alphanumeric chars, MAX_TERM_LEN filter, per-file term frequencies; shared by index-time BM25 term statistics and query-time scoring |') so the cross-task (TASK-078/TASK-079) contract of the module is documented; optionally backfill the five pre-existing missing modules in a separate change.

18. [ ] **housekeeper** | `/Users/etr/progs/wonk/.worktrees/TASK-078/CLAUDE.md:34` | documentation-stale
   The 'Module Responsibilities' table in CLAUDE.md does not include the new tokenizer.rs module added by this task (src/tokenizer.rs, 108 lines, registered as `pub mod tokenizer` in src/lib.rs). The table's convention does not promise exhaustiveness — types.rs, errors.rs, color.rs, progress.rs, and bundled_embedding.rs are also unlisted — and it does list comparably small modules such as budget.rs, and tokenizer.rs is a cross-cutting canonical component that TASK-079's query-side scoring depends on, so an agent using CLAUDE.md to locate tokenization logic will not find it.
   *Recommendation:* Add one row to the Module Responsibilities table, e.g. `| tokenizer.rs | Canonical lexical tokenizer — lowercased Unicode-alphanumeric runs capped at MAX_TERM_LEN, shared by index-time term statistics (TASK-078) and query-time BM25 scoring (TASK-079) |`. This is a one-line CLAUDE.md edit, independent of the deferred specs sync.

19. [ ] **housekeeper** | `:null` | documentation-stale
   Acceptance criterion 'Index build time increase is measured and within budget' is only partially satisfied durably. The measurement harness is committed (src/pipeline.rs: bench_build_index_term_stats_overhead and bench_real_repo_build_index, both #[ignore] with a documented manual invocation), and commit 0e2648e records one relative measurement ('measured insert-phase cost drops roughly in half on the synthetic benchmark corpus'). However, no absolute numbers — baseline t0 vs final t1 median build times, term_stats row counts, or DB size delta from the harness's printed output — are recorded in any durable artifact, and no explicit within-budget verdict was written anywhere (no numeric build-time budget is even defined for BM25; DR-033's <10ms figure is TASK-079 query latency). The bench/ directory has precedent for durable results files (bench/bundled-embedding-results.md, bench/semantic-quality.md) that this task did not follow.
   *Recommendation:* Capture the benchmark output (synthetic and real-repo medians before/after the term_stats write path, row counts, DB size) into a bench/ results file such as bench/term-stats-overhead.md, or append the measured numbers and the budget verdict to the task's final commit message, so the acceptance criterion's 'measured and within budget' claim is auditable after the worktree is merged.

20. [ ] **performance-reviewer** | `src/pipeline.rs:1373` | missing-caching
   insert_term_stats re-prepares a fresh SQL statement for every 1000-row chunk: all full chunks produce a byte-identical ~11KB statement (1000x '(?, ?, ?)') but tx.prepare() (not prepare_cached) re-parses and re-plans each one — 19 redundant preparations on wonk's 18,968 rows per full build, scaling linearly with repo size (a 1M-row repo pays ~1000 preparations). Additionally rows.to_vec() at line 1373 copies the entire row slice (40 bytes/row) before sorting, and the term_rows Vec in batch_insert (lines 862-867) grows from zero capacity. Statement re-parse cost is of the same order as the 2ms sort the executor measured, so removing it is a measurable fraction of the remaining overhead.
   *Recommendation:* Prepare the full-chunk INSERT once outside the loop (or use tx.prepare_cached, which keys the connection cache on the identical SQL) and execute it per chunk, preparing one additional statement only for the final short remainder; pre-size term_rows with Vec::with_capacity over the FileResults' term_freqs lens; avoid the sorted copy where practical (sort in place or build sorted).

21. [ ] **performance-reviewer** | `src/pipeline.rs:3676` | bench-methodology
   The committed bench harness times only end-to-end build_index (bench_three_fresh_builds, lines 3676-3697), but the ≤15% budget claim rests on component numbers (≈17ms insert, ≈2ms sort) that came from uncommitted ad-hoc instrumentation, and the third component — tokenization — was never directly measured by the executor at all (it was only inferred by subtraction from noisy end-to-end deltas). I measured it independently: ~12.7ms CPU per full-repo pass, parallelized by rayon in the real build, giving a defensible total of ~8-11% of the ~235ms build. The conclusion holds, but the evidence chain is not reproducible from the repository, so TASK-079 (query side) and any future write-path change cannot mechanically re-verify the budget.
   *Recommendation:* Extend the existing #[ignore] bench to print per-component timings — cumulative term_frequencies time (summed across parse_one_file/reindex_file) and insert_term_stats time (including sort) — alongside the end-to-end median, so the budget arithmetic is a one-command check. Also consider reporting min-of-N in addition to median-of-3: my re-run showed a 425ms spike under external load (run 2 vs 233/239ms), confirming minima are the more stable statistic on a loaded machine.

22. [ ] **performance-reviewer** | `src/tokenizer.rs:45` | memory-allocation
   term_frequencies allocates one String per token OCCURRENCE via tokenize (tokenizer.rs:18-32): each occurrence is allocated, SipHash-hashed as an owned key, and dropped when already present in the map. Measured on this repo's own files, occurrences outnumber distinct terms 14-19x (src/pipeline.rs: 14,724 tokens vs 1,007 distinct; src/indexer.rs: 18,808 vs 950), so ~14-19x more short-lived allocations than the map requires. I measured the full-repo term_frequencies pass standalone at ~12.7ms CPU (33 .rs files, 1.74MB) per build; the allocation churn is a significant slice of that. Wall-clock impact on the build is dampened because this runs inside the rayon parallel parse, so this stays minor, but it is the cheapest available win on the hot path.
   *Recommendation:* Count occurrences against &str slices during a single scan of the text (char-boundary walks, HashMap<&str, u32> updated via get_mut on the slice) and materialize one owned String per DISTINCT term only, either by re-keying at the end or by interning into the map on first sight; optionally swap SipHash for a faster short-key hash (fxhash/ahash). Keep the existing tokenize() for query-side use (TASK-079). Expected effect: allocation count drops ~14-19x and a few ms of CPU leave the parallel parse phase.

23. [ ] **security-reviewer** | `src/tokenizer.rs:18` | resource-exhaustion
   MAX_TERM_LEN bounds per-term length but nothing bounds term cardinality per file, and tokenize() materializes a Vec<String> of ALL tokens (not just distinct ones) before counting, so peak transient memory is roughly 2-4x file size per file, multiplied by rayon's parallel workers (walker.rs has no file-size cap). A repo legitimately containing a large minified/generated bundle (the case MAX_TERM_LEN's own doc comment anticipates) yields millions of tokens and high-distinct-term maps in RAM, and correspondingly millions of term_stats rows on disk (db.rs TERM_STATS_SQL) plus a full-build-wide term_rows Vec (pipeline.rs:862-868) — index bloat and memory pressure, not attacker-reachable per the frozen trusted-content baseline.
   *Recommendation:* Harden for realistic large generated files: count frequencies in a streaming pass over chars instead of materializing the full token Vec, and/or cap distinct terms per file (dropping stats or the excess) so one 100MB hexdump cannot balloon memory and the index DB.

24. [ ] **security-reviewer** | `src/tokenizer.rs:48` | resource-exhaustion
   term_frequencies counts into u32 with a plain `+=` (`*freqs.entry(token).or_insert(0u32) += 1;`). A single file containing one term ~2^32 times (a >=~8.6GB file of `a a a ...`, since one 64+ char run collapses to a single dropped token, separators are required) overflows: in release builds tf wraps silently, corrupting BM25 statistics; in debug builds it panics (CWE-190). The value is later cast `*tf as i64` (pipeline.rs:637, :865), so the storage column could have held u64 counts. Practically unreachable under the frozen baseline (trusted local repos, and read_to_string already loads the whole file into memory at that size), hence minor.
   *Recommendation:* Use `tf = tf.saturating_add(1)` on u32 (or count in u64/usize) so pathological files degrade stats gracefully instead of wrapping or panicking.

25. [ ] **spec-alignment-checker** | `src/pipeline.rs:3729` | acceptance-criteria
   Acceptance criterion 'Index build time increase is measured and within budget' is substantively met (seeded 300-file synthetic harness with 3-run median at line 3729, real-repo harness at line 3744, sorted multi-row insert optimization whose effect is claimed measured in commit 0e2648e, and the frozen baseline declares real-repo component overhead of ~8-12% against a <=15% budget), but the actual measured numbers (t0 baseline vs t1 medians for both harnesses) are not persisted anywhere in the repository — bench/ contains results documents for other benchmarks (bundled-embedding-results.md, semantic-quality.md) and specs/todo_notes.md has no TASK-078 entry, so a future reader cannot reproduce or audit the 'within budget' claim from the repo alone.
   *Recommendation:* Commit a short bench/term-stats-overhead.md (or a todo_notes entry) recording the t0/t1 medians from bench_build_index_term_stats_overhead and bench_real_repo_build_index, the row counts/DB sizes printed by print_bench_db_stats, and the budget conclusion, matching the existing bench/ documentation convention.

26. [ ] **test-quality-reviewer** | `/Users/etr/progs/wonk/.worktrees/TASK-078/src/db.rs:1906` | redundant-test
   test_open_creates_term_stats_table is fully subsumed by the updated test_open_creates_all_tables (db.rs:612 now asserts term_stats in the table list); the broad test passing implies this test always passes.
   *Recommendation:* Either drop the focused test or keep it consciously for failure localization, consistent with the codebase's existing mix of broad and focused schema tests -- non-blocking either way.

27. [ ] **test-quality-reviewer** | `/Users/etr/progs/wonk/.worktrees/TASK-078/src/db.rs:1923` | missing-test
   The pre-V5 migration tests exercise ensure_term_stats_table, which has no production caller (open() migrates via apply_schema, db.rs:217). The migration guarantee is therefore pinned on a code path users never hit; nothing asserts that open() on a legacy database lacking term_stats creates the table.
   *Recommendation:* Extend the existing legacy-open migration test (test_open_migrates_legacy_db_without_caller_id_or_confidence) or this test to build a legacy-shaped DB and open() it, asserting term_stats exists afterwards -- keeping the guarantee on the real path. (Mirrors the pre-existing uncalled ensure_type_edges_table pattern, so low urgency.)

28. [ ] **test-quality-reviewer** | `/Users/etr/progs/wonk/.worktrees/TASK-078/src/pipeline.rs:1369` | missing-test
   No pipeline-level test covers a file that yields zero terms (empty file or punctuation/whitespace-only content). The insert_term_stats early-return on empty rows and the resulting state -- a files row with line_count set but no term_stats rows -- are unverified; assert_stats_match_disk's document-length invariant only inspects files that appear in term_stats, so this combination is silently unchecked. (The tokenizer unit tests cover empty input, but not the DB round-trip.)
   *Recommendation:* Add an empty .rs file (and optionally a punctuation-only file) to one of the fixture repos or the AR-024 sequence test and assert the files row exists with zero term_stats rows after build and after reindex.

29. [ ] **test-quality-reviewer** | `/Users/etr/progs/wonk/.worktrees/TASK-078/src/pipeline.rs:1378` | missing-test
   insert_term_stats chunks rows at ROWS_PER_STMT = 1000 (3000 bound parameters per statement), but no test ever produces more than 1000 distinct terms in one transaction, so the multi-chunk path -- placeholder string assembly joined with per-chunk params_from_iter -- is never executed. Real repositories routinely have files with >1000 distinct terms; a chunk-boundary bug would only surface there.
   *Recommendation:* Add a test that indexes a generated file with ~1500 distinct terms (e.g., w0..w1499 in one .rs file) through reindex_file and verifies the distinct-term count and a sample of tf values against the oracle.
