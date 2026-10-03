# Unworked Review Issues

**Run:** 2026-09-30 09:34:20
**Task:** TASK-089
**Total:** 21 (0 critical, 1 major, 20 minor)

## Major

1. [x] **housekeeper** | `docs/commands.md:366` | documentation-stale
   *Addressed (majors sweep, 2026-10-02):* fixed (flag table + suppress tree documented).
   The Review section of docs/commands.md — which self-describes as the 'Full flag and example reference for every wonk command' — does not document any of the user-facing surface TASK-089 adds: (a) the flag table (lines 366-371) omits --min-confidence, --min-severity, --kind, and --max-findings; (b) the `wonk review suppress list|add|remove` subcommand tree (PRD-REV-REQ-014's headline CLI surface, confirmed user-facing by tests/review_cli_integration.rs lines 406-502) is entirely absent, including its `wonk init` prerequisite and bulk --rule removal; (c) the NDJSON examples are stale against the new wire contract: the finding-line example (line 389) lacks the now-always-present `confidence` field and the verdict-line example (line 390, `{"scope":"unstaged","verdict":"BLOCK","finding_count":1}`) lacks the always-serialized `drops` object with its five counters (src/output.rs ReviewDropsOutput, 'Always serialized in full — zeros included'); (d) the grep-mode stderr drop summary ('review dropped N finding(s): reason=n ...', printed in src/router.rs dispatch_review via DropCounts::summary_line) is undocumented. No deferral note exists anywhere: TASK-089's action items in specs/tasks.md (lines 3763-3771) contain no docs item, the deferred-work table has no docs entry, and no commit message defers it. The drops omission matters most: PRD-REV-REQ-015's core promise ('suppression is never silent') is a wire guarantee an NDJSON consumer cannot discover from the documented example.
   *Recommendation:* Update docs/commands.md's Review section: add the four filter flags to the flag table (noting defaults-off keeps today's report); add a `wonk review suppress` subsection covering list (--rule), add (identity + --rule/--file/--note, upsert semantics), and remove (identities and/or --rule), plus the shared no-auto-init/`wonk init` prerequisite; extend the NDJSON examples to show `confidence` on finding lines and the always-present `drops` object on the verdict line (zeros included); mention the stderr drop-summary line and that -q silences it. Per the coordinator's note, TASK-086's docs wave established the section, so a small follow-up docs commit within this task's merge (or an explicit deferral note recorded in the task) closes it.

## Minor

2. [ ] **architecture-alignment-checker** | `src/cli.rs:483` | interface-contract
   The `--scope` help text was narrowed from "unstaged (default), staged, all, or compare" to "unstaged (default), staged, or compare" in this diff, but parse_change_scope (router.rs:1896) and ChangeScope::from_str (types.rs:815) still accept `all` — the V4 ChangeScope enum verbatim that DR-035/§4.28 mandate review reuse. The documented CLI surface now under-reports a supported scope, and the edit is unrelated to TASK-089's goal.
   *Recommendation:* Restore `all` to the ReviewArgs scope help string (or, if `all` is genuinely unwanted for review, remove it from the accepted scopes deliberately and document that deviation — but the architecture says the enum is reused verbatim).

3. [ ] **architecture-alignment-checker** | `src/db.rs:190` | interface-contract
   The review_suppressions DDL drifts from the documented schema (specs/architecture.md:1245-1251): the implementation adds a `note TEXT` column, changes `file TEXT` (nullable in the doc) to `file TEXT NOT NULL DEFAULT ''`, and adds idx_review_suppressions_rule. None of these appear in the architecture doc's DDL, and `note` is not in the TASK-089 action items. The drift is additive and display-only — identity remains the sole primary key and the documented columns (identity, rule, file, created_at) are all present with the same meanings — so the AR-031 contract (listable, removable, keyed by identity) is unaffected.
   *Recommendation:* Sync the architecture doc's review_suppressions DDL (add note, the NOT NULL DEFAULT '' on file, and the rule index) next time the architecture is sourced from code, or drop the note column if doc fidelity is preferred; do not change the identity key space.

4. [ ] **code-quality-reviewer** | `src/cli.rs:483` | readability
   Help-text regression unrelated to the REQ-014/015 flags: the `--scope` doc dropped `all` (was "unstaged (default), staged, all, or compare"), but `ChangeScope::All` still parses (src/types.rs:822) and `detect_changes_detail` handles it, so `wonk review --scope all` keeps working while being no longer advertised in `--help`.
   *Recommendation:* Restore `all` to the scope doc string, or restrict ReviewArgs' scope values to unstaged/staged/compare deliberately (with a rejection test) if `all` is meant to be unsupported for review.

5. [ ] **code-quality-reviewer** | `src/review.rs:398` | elegance
   The stamp seam is convention-only. All three rules currently push `identity: String::new()` and every push site goes through `stamp_identity`, but nothing mechanically enforces it: a future rule (or a refactor moving a `findings.push`) that bypasses the stamp would ship findings with empty identities that can never be suppressed and would be silently kept forever.
   *Recommendation:* Add a cheap guard — e.g. `debug_assert!(!finding.identity.is_empty(), ...)` at the top of `rank_filter_cap` or where findings are collected in `run_review` — so a bypassed stamp fails loudly in debug builds instead of producing unsuppressable findings.

6. [ ] **code-quality-reviewer** | `src/review.rs:478` | error-handling
   `remove_suppressions` builds `identity IN (?, ?, ...)` with one bound parameter per identity. SQLite's compiled-in bound-parameter ceiling (999 on older builds, 32766 on modern bundled ones) makes a bulk remove with a very large ID list fail at runtime with an opaque 'too many SQL variables' error rather than a clear message.
   *Recommendation:* Chunk the identity list into batches under the parameter limit inside remove_suppressions, or catch the error and surface a actionable hint. Low practical risk — typical usage pastes a handful of IDs from a review.

7. [ ] **code-quality-reviewer** | `src/review.rs:936` | correctness
   For tier-1/3 anchors, `std::fs::read_to_string(...).ok()` yields None for a non-UTF-8 (or unreadable) file, so every finding's identity in that file silently degrades to symbol-grain (no fifth component). This is self-consistent — tree-sitter symbol parsing fails on the same file too, so anchors would mostly be Unresolved anyway — but the degradation is undocumented on `anchored_line_text`/`finding_identity`, and symbol-grain identities are coarser than the REQ-013 contract for the affected file.
   *Recommendation:* Document the fallback on `finding_identity`/`anchored_line_text` (one line: unreadable/non-UTF-8 current file contributes no anchor text), or use `String::from_utf8_lossy` if preserving per-line identity on non-UTF-8 files matters.

8. [x] **code-quality-reviewer** | `src/router.rs:2004` | correctness
   *Addressed (minors sweep, 2026-10-02):* fixed (clamped like every sibling flag).
   `--min-confidence` is passed raw into ReviewOptions with no sanitization, breaking the codebase's established pattern: every sibling command that takes the same flag clamps it via a `sanitize_confidence` helper (src/callgraph.rs:21, src/blast.rs:65, src/flows.rs:240, src/context.rs:31). `wonk review --min-confidence 5` silently drops every finding, `-1` silently keeps everything, and `NaN` (accepted by clap's f64 parser) behaves as no filter because `confidence < NaN` is always false.
   *Recommendation:* Apply the same clamp/reject semantics before building ReviewOptions in dispatch_review (or inside rank_filter_cap), so review's confidence floor is validated exactly like blast/flows/context.

9. [ ] **code-simplifier** | `src/review.rs:265` | code-structure
   The five drop reasons are enumerated twice beyond the struct fields themselves: DropCounts::total() sums five named fields (review.rs:265-271) and summary_line() rebuilds the same five (name, count) pairs in its reasons array (review.rs:279-285). Adding a sixth reason requires coordinated edits in both places (plus the output.rs mirror), inviting drift.
   *Recommendation:* Add a single accessor, e.g. pub fn reasons(&self) -> [(&'static str, usize); 5], and derive both total() (sum over the array) and summary_line()'s listing from it, so the reason list lives in exactly one place per module.

10. [ ] **code-simplifier** | `src/review.rs:491` | code-structure
   In remove_suppressions, the placeholder strings are built by a map closure that mutates the captured params Vec as a side effect (params.push inside .map at review.rs:492-498); readers expect map to be pure, and the placeholder numbering depends on the push order happening before the format! reads params.len().
   *Recommendation:* Replace the map-with-side-effect with a plain for loop that pushes the param and then pushes the formatted placeholder, so the two effects read sequentially and the ?N numbering is obviously correct. The Box<dyn ToSql> accumulation itself is the idiomatic rusqlite shape for a dynamic OR query and can stay.

11. [ ] **code-simplifier** | `src/router.rs:2048` | duplication
   dispatch_review_suppress duplicates dispatch_review's repo-root resolution and index-open block (~14 lines verbatim at router.rs:1975-1988 vs 2048-2061); only the error-message tail differs.
   *Recommendation:* Extract a small helper local to the two review dispatchers, e.g. fn open_review_index() -> Result<(PathBuf, Connection)>, parameterized by (or centralizing) the error tail. Scope it to the review pair only - the same block shape recurs elsewhere in router.rs as a pre-existing pattern, so a file-wide refactor is out of scope for this task.

12. [ ] **code-simplifier** | `tests/review_cli_integration.rs:296` | duplication
   indexed_repo_three_fns (tests/review_cli_integration.rs:296-318) is a near-verbatim copy of the pre-existing indexed_repo (line 64), differing only in the lib.rs fixture content; ~20 lines of git-init/config/init boilerplate are duplicated within one file.
   *Recommendation:* Parameterize one builder (e.g. fn indexed_repo_with(lib_rs: &str) -> TempDir) and have both fixtures delegate. DAMP favors some test repetition, so this is a judgment call - the shared part is pure boilerplate (git config, wonk init), not test meaning, which makes the collapse safe for readability.

13. [ ] **housekeeper** | `src/cli.rs:483` | documentation-stale
   This diff narrows the review --scope help text from 'unstaged (default), staged, all, or compare' to 'unstaged (default), staged, or compare', but `wonk review --scope all` still parses and runs: parse_change_scope (src/router.rs:1896) falls through to ChangeScope::from_str which accepts "all" (src/types.rs:822), and no review-specific rejection exists. The in-binary help now under-reports accepted input, inconsistently with `wonk changes --help` (src/cli.rs:460, still lists `all`) and docs/commands.md:368 (still lists `all` for review).
   *Recommendation:* Either restore `all` to the review --scope help string (matching runtime, changes --help, and docs/commands.md), or — if review genuinely should not offer `all` — reject it in dispatch_review and update docs/commands.md:368 to match. Do not ship help text that hides a value the parser accepts.

14. [ ] **performance-reviewer** | `src/impact.rs:423` | memory-allocation
   parse_all_diff_hunks_sides now executes result.entry(file.clone()).or_default() for every removed body line (src/impact.rs:423-430), cloning and re-hashing the file path per removed line even though the hunk-header branch already resolved the entry. The map is still built exactly once per run (via detect_changes_detail, src/review.rs:849), and typical diffs make this invisible, but a pathological vendored-file rewrite (100k+ removed lines) pays 100k+ path clones plus re-hashes in one pass.
   *Recommendation:* Capture the &mut FileDiffHunks once per hunk (or track the current file's entry alongside old_line) and insert into removed_lines without re-cloning the key; alternatively use get_mut on the known-present entry. Micro-win; safe to defer.

15. [ ] **performance-reviewer** | `src/review.rs:387` | memory-allocation
   finding_identity hex-encodes via 32 per-byte format!("{b:02x}") allocations per identity (src/review.rs:387-391), and fold_whitespace builds an intermediate Vec<&str> before join (src/review.rs:356-358); both run once per produced finding (up to 3 per changed symbol via stamp_identity). Negligible at typical finding volumes — the reproduced bench runs the full pipeline at p95 88ms — but it is ~33 small heap allocations per identity that a single-pass encoder removes.
   *Recommendation:* Build the hex string with String::with_capacity(64) and a nibble lookup or core::write! fold, and fold whitespace with a single-pass String build instead of split_whitespace().collect::<Vec<_>>().join(" "). Cold-ish path; only worth touching if the identity path is ever moved into a loop over many findings — benchmark before/after if changed.

16. [ ] **performance-reviewer** | `src/review.rs:935` | missing-caching
   Each candidate file is read from disk twice per run: parse_current_symbols does its own read_to_string (src/impact.rs:594-595, cached in current_cache) and the new current_lines_cache re-reads the same path (src/review.rs:935-940), then retains every line as an individually allocated String for the whole run. Bounded by distinct changed files (~10 in the AC's typical diff) and cached per file — never per symbol — so the cost is microseconds against the <2s AC, but the second read and duplicate buffer are avoidable.
   *Recommendation:* Read each changed file once into a String, pass &str to both parse_file_to_symbols and the lines split, or fold both caches over a single per-file source map. Optional given the measured headroom; do not complicate the fail-soft None-on-missing-file semantics (deleted files must stay cheap no-op misses).

17. [ ] **security-reviewer** | `src/cli.rs:527` | input-validation
   The 'review suppress add' identity argument accepts any string: empty strings, arbitrary unicode, or mistyped values are stored as-is with no 64-lowercase-hex validation, creating inert suppression rows that can never match an engine-stamped identity (all stamped identities are sha256 hex). There is no injection or cross-finding impact (exact-match lookup against hex-only keys), but dead rows accumulate and 'remove' of a mistyped id silently reports 0 removed. CWE-20.
   *Recommendation:* Validate the identity positional argument in ReviewSuppressAction::Add against ^[0-9a-f]{64}$ (or at minimum reject empty), with a clear error pointing at the identity printed on the finding line. Alternatively warn when a stored identity is not 64-hex in list output.

18. [ ] **security-reviewer** | `src/impact.rs:425` | resource-limits
   parse_all_diff_hunks_sides now retains the text of every removed line of the entire diff in the per-file removed_lines maps (src/types.rs FileDiffHunks.removed_lines), changing memory cost from O(hunks) to O(total removed bytes) per review run. On adversarial or merely large diffs (whole-file deletions, --scope=compare against an old base, deleted vendored/minified files) this roughly doubles peak memory versus the already-resident diff string. There is no cap, but the cost is bounded by the diff size the tool already buffers, and the actor is the local user's own repo per the frozen local-CLI baseline, so severity is minor (CWE-400, low).
   *Recommendation:* If hardening is desired: cap the retained text per file (e.g. first N KB of removed text; anchors beyond the cap fall back to unresolved-text identity, which is already a supported state), or store removed text only for lines that tier-2 anchoring can actually consult.

19. [ ] **security-reviewer** | `src/output.rs:1710` | output-encoding
   render_suppression writes rule=, file=, and note= values unescaped into the line-oriented text/grep output. A note or file containing a newline can forge additional output lines (CWE-117), and terminal control characters can inject escapes into the reviewer's terminal (CWE-150 adjacent). Sources are the user's own --note/--file flags and, because suppressions live in .wonk/index.db which is preferred over the central index, rows originating from a repo-shipped index DB when reviewing an untrusted clone. JSON/structured mode escapes correctly; only the text/grep path is affected.
   *Recommendation:* In render_suppression's non-structured branch, escape or strip control characters (at minimum \n, \r, and other C0 controls) from rule/file/note before writing, mirroring the neutralization the structured path gets from serde. Severity is minor under the local-CLI baseline (self-inflicted for flags; repo-shipped DB is an edge of the frozen trust model).

20. [ ] **security-reviewer** | `src/review.rs:370` | data-integrity
   finding_identity concatenates rule/kind/file/symbol/anchor_text with single 0x1f separators and no length prefix. If a component itself contains 0x1f (achievable in unix file paths, conceivable in symbol names depending on parser), two distinct findings hash identically, so suppressing one silently suppresses the other (suppression-confusion; CWE-133/relative-message-confusion class). Practical exploitability is low because stamped rule/kind are fixed constants and tree-sitter identifier alphabets exclude 0x1f, but the identity is the security-relevant key for the whole suppression mechanism and should be collision-proof by construction.
   *Recommendation:* Length-prefix each component (e.g. hash each part as len-bytes then the bytes) or serialize the component tuple as JSON before hashing, making the encoding injective. Add a unit test asserting that a 0x1f inside one component cannot be reproduced by shifting the boundary between two components.

21. [ ] **security-reviewer** | `src/review.rs:488` | resource-limits
   remove_suppressions builds a single identity IN (...) clause over every supplied identity. Beyond SQLite's SQLITE_MAX_VARIABLE_NUMBER (999 on older builds, 32766 on modern bundled ones) the DELETE fails with 'too many SQL variables'. The failure is atomic and returns an error (fails safe, no partial delete, no panic), so this is robustness, not a vulnerability.
   *Recommendation:* Chunk identities into batches of ~900 (below both limits) inside remove_suppressions and sum the deleted counts, so bulk removal scales past the parameter limit.
