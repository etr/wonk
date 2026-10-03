# Unworked Review Issues

**Run:** 2026-09-30 01:00:02
**Task:** TASK-085
**Total:** 29 (0 critical, 3 major, 26 minor)

## Major

1. [x] **code-quality-reviewer** | `src/review.rs:357` | correctness
   *Addressed (majors sweep, 2026-10-02):* fixed ((name,kind,file) key + test).
   Rule A's same-diff-removed-caller filter keys on (name, kind) only. The `removed` HashSet (lines 357-363) is matched against blast-radius callers at line 214 without file or scope, so a live caller that merely shares name+kind with ANY unrelated removed symbol in the diff is dropped as 'dead code'. With common method names (run/apply/new/fmt as Method across different types, since SymbolKind does not encode the receiver) this under-reports callers, and if it is the only surviving caller the BLOCK finding disappears entirely, flipping the verdict. The intended dead-code case (helper + its caller deleted together, test rule_a_removal_with_all_callers_removed_is_not_blocking) usually lives in the same or a known file, so the coarse key is not required to implement it.
   *Recommendation:* Key the removed set on (name, kind, file): ChangedSymbol.file and BlastAffectedSymbol.file are both old-side/indexed paths for removed code, so they compare cleanly, and the dead-code test still passes. Note the blast-output side already carries s.file. If cross-file dead-code removal (helper and caller in different files) must keep working, at least document the name-collision hazard next to the filter.

2. [x] **code-simplifier** | `src/review.rs:256` | patterns
   *Addressed (majors sweep, 2026-10-02):* fixed (From<&BlastAffectedSymbol>).
   The BlastAffectedSymbol-to-SymbolRef four-field mapping (name/kind/file/line clone) is hand-rolled four times in one file: rule_breaking_change's `related` (lines 256-264), rule_coverage_gap's `related` (lines 321-331), and twice in tests (symbol_refs_of at 1041-1053, and inlined again in ac4a_rule_a_related_equals_standalone_blast_output at 1095-1110 even though symbol_refs_of already exists beside it).
   *Recommendation:* Add `impl From<&BlastAffectedSymbol> for SymbolRef` in src/types.rs next to the existing `impl From<&ChangedSymbol> for SymbolRef` (types.rs line 273), then use `.map(SymbolRef::from)` at each site. This shares only the field mapping; the two rule functions stay fully separate, preserving the declared TASK-089 per-rule-suppression assumption.

3. [x] **test-quality-reviewer** | `/Users/etr/progs/wonk/.worktrees/TASK-085/src/review.rs:1254` | missing-test
   *Addressed (majors sweep, 2026-10-02):* fixed (rule-B disable + config-wiring tests).
   AR-022 requires each rule family to be independently disable-able, but only family A is verified: `rules_independently_disableable` disables `breaking_change` and asserts empty findings/approve. No test anywhere sets `ReviewOptions { coverage_gap: false, .. }` through `run_review`, and no test wires a `[review]` config file through `dispatch_review` into `ReviewOptions` (the CLI integration tests run with default config only, and `rules_independently_disableable` constructs options directly). If `dispatch_review` (src/router.rs, ReviewOptions construction) miswired `coverage_gap: config.review.breaking_change`, or the rule-B gate at src/review.rs:375 regressed, the entire suite would still pass.
   *Recommendation:* Add one fixture test mirroring `rules_independently_disableable` with `coverage_gap: false` (a would-be coverage-gap diff must yield no coverage findings and verdict APPROVE), and one CLI integration test that writes `<repo>/.wonk/config.toml` with `[review] coverage_gap = false` (and ideally `breaking_change = false`) and asserts the warnings disappear from the JSON output — that pins both the option gate and the config-to-options wiring.

## Minor

4. [ ] **architecture-alignment-checker** | `src/review.rs:165` | interface-contract
   Finding identity uses a provisional SHA-256 over rule+file+symbol only, which does not yet match the architecture 4.28 / PRD-REV-REQ-013 formula (rule, category, normalized file path, symbol, whitespace-folded anchored-line text, line number excluded). The deviation is documented in code and matches the task plan: specs/tasks.md:3754 assigns 'Finding identity, durable suppression, and drop accounting' to TASK-089, which depends on TASK-085, and the String field type keeps the swap non-breaking. Recorded so the ledger tracks the dependency rather than because TASK-085 misaligned.
   *Recommendation:* No change within TASK-085. Land the REQ-013 identity formula in TASK-089 as sequenced; until then treat Finding.identity as unstable across runs and do not persist it.

5. [ ] **code-quality-reviewer** | `src/cli.rs:486` | cli-ux
   `--base` is accepted and silently ignored when --scope is not `compare`: `wonk review --scope all --base main` parses fine, then parse_change_scope (src/router.rs:1896) only reads base when scope == "compare", so the user's requested base never takes effect with no diagnostic. The arg help says 'required when --scope=compare' but the inverse is unenforced. (Behavior inherited verbatim from the pre-existing `wonk changes` path through the shared helper; the new review surface now exposes it too.)
   *Recommendation:* Either clap `requires_if("compare", base)`-style validation on ReviewArgs/ChangesArgs, or have parse_change_scope error when base.is_some() && scope != "compare" so the silent drop becomes a usage error.

6. [x] **code-quality-reviewer** | `src/impact.rs:353` | dead-code
   *Addressed (minors sweep, 2026-10-02):* fixed (unreachable guard removed with justification).
   Unreachable guard in parse_hunk_header_sides: `if plus_pos <= minus_pos { return None; }`. plus_pos = minus_pos + line[minus_pos..].find('+')?, and the slice starts with '-' so the found offset is always >= 1; plus_pos is strictly greater than minus_pos whenever the ?-operator has not already returned None. The branch suggests a case that cannot occur.
   *Recommendation:* Delete the guard (the None-paths through find already reject non-hunk lines, covered by parse_hunk_header_sides_rejects_non_hunk_lines).

7. [ ] **code-quality-reviewer** | `src/review.rs:53` | readability
   ChangeType is spelled fully-qualified (crate::types::ChangeType::Removed/Modified/Added) six times across run_review, resolve_anchor, and rule_breaking_change (lines 53, 223, 361, 370-371, 377-378) while every other shared type (Finding, FindingSeverity, Symbol, SymbolRef, ...) is in the module's use list.
   *Recommendation:* Add ChangeType to the existing `use crate::types::{...}` import and use the short form; the rule-trigger conditions in run_review shrink to single lines and read much better.

8. [ ] **code-quality-reviewer** | `src/review.rs:69` | correctness
   resolve_anchor's tier re-resolution matches current-file symbols on (name, kind) only and takes min(line): for duplicate same-named symbols in one file (e.g. methods `work` in impl Foo and impl Bar, which impact.rs's own SymbolKey distinguishes by scope) the anchor can pin to the wrong duplicate after insertions shift lines. The choice is deliberate and documented by resolve_anchor_prefers_lowest_current_line_on_duplicate_names; the root cause is that ChangedSymbol carries no scope, so resolve_anchor cannot disambiguate.
   *Recommendation:* Acceptable to keep for 085 given the honest-anchor principle still emits a real line; when Finding/ChangedSymbol identity is revisited (TASK-089 owns the identity space), thread scope (or match candidates by nearest line to cs.line instead of global min) to remove the wrong-duplicate case.

9. [ ] **code-quality-reviewer** | `src/review.rs:95` | readability
   Redundant function-local `use crate::types::FindingSeverity;` inside derive_verdict; the type is already imported in the module-level use list (line 24). Leftover from an earlier draft.
   *Recommendation:* Remove the inner use statement.

10. [x] **code-simplifier** | `bench/review_bench.rs:134` | dead-code
   *Addressed (minors sweep, 2026-10-02):* fixed (clone dropped).
   `paired_deltas` is collected and then used exactly once — to clone into `paired` before percentiles; the original is never read again, so the intermediate vector and its clone are pure overhead in the bench harness.
   *Recommendation:* Collect directly into the mutable binding: `let mut paired: Vec<f64> = full_ms.iter().zip(&detect_ms).map(|(f, d)| f - d).collect();` and delete paired_deltas.

11. [ ] **code-simplifier** | `src/impact.rs:318` | dependencies
   `pub struct HunkSides` is newly public in this diff but has no consumer outside impact.rs: its only producer (parse_hunk_header_sides) and all its consumers (parse_hunk_header, parse_all_diff_hunks_sides, and the in-module tests) are module-private or same-module.
   *Recommendation:* Drop the `pub` (keep the struct and its doc comment as-is). The both-sides parser stays a module-internal detail and the public surface of wonk::impact does not grow (Hyrum's Law); the projection pattern assumption is unaffected.

12. [ ] **code-simplifier** | `src/review.rs:361` | naming
   ChangeType and SymbolKind are referenced fully qualified nine times (`crate::types::ChangeType::Removed/Modified/Added` at lines 53, 223, 361, 370-378; `crate::types::SymbolKind` at 203, 357) while fifteen sibling types are already imported at lines 22-25, so qualification style is inconsistent within the same file.
   *Recommendation:* Add ChangeType and SymbolKind to the existing `use crate::types::{...}` list and drop the `crate::types::` prefixes throughout the file.

13. [ ] **code-simplifier** | `src/review.rs:58` | code-structure
   The Removed branch of resolve_anchor uses `hunks.filter(|h| range_covers(...)).map(|_| cs.line)` — an Option-filter that discards the reference only to repackage cs.line, which reads as if the hunk were transformed when it is only being tested.
   *Recommendation:* Use the direct boolean form: `if hunks.is_some_and(|h| range_covers(&h.removed_ranges, cs.line)) { (Some(cs.line), AnchorMethod::OldSideLine) } else { (None, AnchorMethod::Unresolved) }` — matching the tier-1 check at line 79 that already uses is_some_and.

14. [x] **code-simplifier** | `src/review.rs:95` | dead-code
   *Addressed (minors sweep, 2026-10-02):* fixed (local use removed).
   derive_verdict opens with a function-local `use crate::types::FindingSeverity;` even though FindingSeverity is already imported in the module-level use at line 24.
   *Recommendation:* Delete the function-local use; the body already resolves through the module import.

15. [ ] **code-simplifier** | `src/router.rs:1984` | code-structure
   The --since sugar in dispatch_review builds an intermediate tuple of owned Strings ("compare".to_string(), since.clone(), args.scope.clone(), args.base.clone()) solely to call parse_change_scope, whose parameters are &str.
   *Recommendation:* Match on `args.since.as_deref()` and call parse_change_scope per arm: `let scope = match args.since.as_deref() { Some(since) => parse_change_scope("compare", Some(since))?, None => parse_change_scope(&args.scope, args.base.as_deref())? };` — same behavior, no allocations, no shadowing tuple.

16. [ ] **code-simplifier** | `src/router.rs:1991` | consistency
   Config-load failure handling differs between the two sibling dispatchers without a stated reason: dispatch_changes silently defaults (`.map(|c| c.reach.enabled).unwrap_or(true)`, lines 1925-1927) while dispatch_review hard-fails on the same call (`Config::load(Some(&repo_root))?`). For review the fail-loud choice is arguably deliberate (a broken config must not silently re-enable disabled rules), but nothing records that intent.
   *Recommendation:* Either keep the fail-loud path and add one comment stating why (unreadable config must not silently reset rule switches), or degrade to defaults symmetrically with dispatch_changes. No behavior change required if the comment route is taken.

17. [ ] **housekeeper** | `docs/commands.md:null` | documentation-stale
   This diff ships a user-facing `wonk review` command (src/cli.rs Command::Review with --scope/--base/--since, dispatched in src/router.rs dispatch_review) and a user-facing `[review]` config section (src/config.rs ReviewConfig with breaking_change/coverage_gap, wired through ConfigOverlay with tests), but docs/commands.md — whose header claims "Full flag and example reference for every wonk command" — and docs/configuration.md, which documents every config section except the pre-existing [reach] omission, were both left untouched. Judgment on the deferral, per the repo's own patterns: deferring to TASK-086 is acceptable. TASK-086 is the immediate next task (blocked only by TASK-085 + TASK-084, both done), owns the grep-compatible terminal output format per PRD-OUT and the NDJSON surface that a commands.md section would describe, and carries the explicit action item "Update README and MCP server instructions with the review workflow"; documenting 085's interim rendering (output.rs render_review notes "Toon falls back to grep lines here; its PRD-OUT treatment is TASK-086's") would churn immediately when 086 finalizes the format. Clap doc comments provide interim discoverability via `wonk review --help`. However, the deferral carries a concrete risk: no task's action items name docs/commands.md or docs/configuration.md — TASK-086 names only README and MCP server instructions — so a narrow execution of 086's doc wave leaves the gap persisting past 086, repeating the pre-existing [reach].depth/[reach].enabled omission (TASK-080) in configuration.md. After 085 merges, `review` is additionally the only live command missing from commands.md (the `wonk ls` section there is a pre-existing remnant of the summary merge).
   *Recommendation:* Do not block TASK-085 on this. When TASK-086 runs its documentation action item ("Update README and MCP server instructions with the review workflow"), explicitly extend that wave to include a `wonk review` section in docs/commands.md and a `[review]` keys entry (breaking_change, coverage_gap) in docs/configuration.md — ideally adding those two files to TASK-086's action-item text now so the deferral is owned rather than implied.

18. [ ] **performance-reviewer** | `src/review.rs:394` | missing-caching
   For a rule-B-only candidate (added or body-modified symbol) whose blast radius contains a test — the healthy, expected-common case — the context blast's result is discarded: rule_coverage_gap consumes it only via the finding's `related` field, which is built solely when the finding actually fires. With reach enabled the waste is one reach-table lookup per covered symbol; with the reach kill switch disabled it is a full second live BFS per covered symbol, doubling BFS work exactly in the already-slower mode. Measured blast+rules is 7.8ms p95 for 13 applicable symbols, so impact is small today, but the work is pure waste.
   *Recommendation:* In run_review, for symbols that are rule-B candidates but not rule-A candidates, run the with_tests blast first and fetch the context blast lazily only when the coverage finding will be emitted (a test is absent from the radius). Rule-A candidates keep context unconditionally (their finding and the verdict need it). This preserves AC4 byte-identity — `related` remains the canonical tests-excluded blast output — while skipping the context blast for covered symbols.

19. [ ] **performance-reviewer** | `src/review.rs:408` | missing-caching
   Each changed file with applicable symbols is read from disk and tree-sitter-parsed twice per review: once inside detect_changes_detail via detect_changed_symbols_with (src/impact.rs:461-478 already reads the file and calls parse_file_to_symbols) and again by impact::parse_current_symbols when seeding the tier-3 anchor cache. The duplication is linear in changed files (a 100-file diff pays 100 extra reads+parses) and is included in the measured p95, but is avoidable.
   *Recommendation:* Have detect_changes_detail (or a detail variant) return the parsed current-file symbols it already produces, and seed run_review's current_cache from them; this deletes one file read plus one tree-sitter parse per changed file without changing any behavior.

20. [ ] **performance-reviewer** | `src/review.rs:69` | algorithmic-complexity
   resolve_anchor rescans all current-file symbols (name+kind filter, min over matching lines) for every changed symbol in that file — O(changed_in_file x symbols_in_file) per file. Acceptable for realistic files (hundreds of symbols, e.g. the bench's 200-fn files); only a pathologically large file with many simultaneous changes would notice.
   *Recommendation:* When populating current_cache, additionally build a per-file HashMap<(String, SymbolKind), usize> mapping to the minimum current line, making anchor resolution O(1) per changed symbol instead of a linear rescan.

21. [ ] **security-reviewer** | `src/impact.rs:345` | input-validation
   parse_hunk_side computes `start + count - 1` on usize values parsed from hunk-header text; a crafted header such as `@@ -18446744073709551615 +18446744073709551615 @@` overflows and panics in debug builds (wraps in release), e.g. via the pub fn parse_diff_hunks. Within the frozen baseline this is unreachable from a hostile repository: git derives @@ line numbers from real file line counts, and diff body lines are '+'/'-'/' '-prefixed so file content cannot forge a header line; the risk only materializes if a future caller feeds non-git-generated text to the pub parser.
   *Recommendation:* Use checked arithmetic (start.checked_add(count - 1)) or saturating_add, returning None on overflow, so the parser is total over hostile input regardless of caller.

22. [ ] **security-reviewer** | `src/impact.rs:395` | input-validation
   Diff-output parsing does not handle git's path quoting or adversarial filenames: git quotes paths containing newlines/control characters (e.g. "a/fo\no") in both `diff --name-only` and `diff --git` lines, and detect_scoped_files (src/impact.rs:233-239) / parse_all_diff_hunks_sides (src/impact.rs:394-411) consume the raw text without unquoting, while the `rest.find(" b/")` split mis-attributes filenames containing the literal substring " b/". Result is silent degradation (hunk map keyed by a wrong/quoted name, lookup miss, findings for such files drop to unresolved anchors or get skipped) — no panic, no path traversal (quoted names cannot synthesize a '..' component, and validate_file_path guards the re-parse entry point).
   *Recommendation:* Run git with `-z` (NUL-terminated paths: `git diff --name-only -z`, `git diff --unified=0 -z`) and split on NUL, or unquote C-quoted paths; at minimum document the limitation so a later task does not assume hunk coverage is complete for exotic filenames.

23. [ ] **security-reviewer** | `src/impact.rs:451` | broken-access-control
   detect_changed_symbols_with joins `repo_root.join(file)` and reads the file without calling validate_file_path, unlike its public sibling detect_changed_symbols (src/impact.rs:93) and parse_current_symbols (src/impact.rs:567). Today its `file` argument derives exclusively from git `--name-only` output inside detect_changes_detail, which git guarantees is repo-relative with no '..' components, so traversal is not reachable via the review/changes path; the inconsistency is a defense-in-depth gap that a future caller of this fn-shaped helper could turn into a path-traversal read (CWE-22).
   *Recommendation:* Hoist the validate_file_path(file)? call into detect_changed_symbols_with (its callers already validated or pass git-derived names, so the added check is free), preserving the invariant that every repo_root.join(user-influenced-path) is guarded.

24. [ ] **security-reviewer** | `src/impact.rs:536` | injection
   validate_git_ref's allowlist includes '-' and '.', so refs beginning with '-' (reachable via the '=' CLI form, e.g. `wonk review --since=-Oevil` or `--scope=compare --base=-Oevil`, since clap without allow_hyphen_values only blocks them in the space-separated form) pass validation and are appended to git's argv where they are parsed as options: `git diff -Oevil` reorders diff output via an order file, and an `--output`-shaped ref in get_diff_hunks_for_file (src/impact.rs:294-297, argv `git diff --unified=0 <ref> -- <file>`) would consume the following `--` token as the output filename. Because the ref is self-supplied by the local user who can already run git directly, this is defense-in-depth, not an exploitable injection (CWE-88 residual; `=` and shell metacharacters are correctly rejected, and no shell is ever involved).
   *Recommendation:* Extend validate_git_ref to reject refs whose first character is '-' (git refs cannot start with '-'), and consider rejecting '..'/'...' range syntax which silently changes diff semantics (compares two refs instead of ref-vs-worktree).

25. [ ] **test-quality-reviewer** | `/Users/etr/progs/wonk/.worktrees/TASK-085/src/review.rs:184` | missing-test
   `format_caller_names` is only exercised with exactly one caller across the entire lib suite. The 2-3-name join and the `(+k more)` truncation branch (names.len() > 3) — part of AC1's 'callers listed' contract and user-visible message text — are untested; only the bench fixture has many callers and it asserts counts, not message content.
   *Recommendation:* Add a fixture with 4-5 indexed callers asserting the exact truncated message (e.g. 'a, b, c (+2 more)') and the full `related` list, or promote `format_caller_names` to a small table test.

26. [ ] **test-quality-reviewer** | `/Users/etr/progs/wonk/.worktrees/TASK-085/src/review.rs:371` | missing-test
   Every signature-change fixture alters a TYPE (`i32 -> i64`, param added). Whether a parameter RENAME (`fn f(x: i32)` -> `fn f(y: i32)`) counts as a signature change — and therefore BLOCKs when callers remain — is unpinned, yet it is a common real-world diff and drives a blocking verdict through `signature_changed`.
   *Recommendation:* Extend `detect_changes_detail_flags_signature_modified_not_body_only` or the rule-A fixture with a rename-only case, asserting the intended classification either way (flagged or not) so the BLOCK semantics for renames are contractual rather than incidental.

27. [ ] **test-quality-reviewer** | `/Users/etr/progs/wonk/.worktrees/TASK-085/src/review.rs:397` | missing-test
   The fail-soft contract — a per-symbol blast failure degrades to a `warnings` entry and skips the symbol rather than silently dropping or aborting (documented on `ReviewResult.warnings`) — is never exercised: no test asserts `result.warnings` is non-empty, and the CLI hint-printing of warnings (dispatch_review) is untested too.
   *Recommendation:* Either construct one reachable failure (e.g. an indexed symbol whose blast query errors) and assert the warning text plus the skipped symbol producing no finding, or note the channel as covered-by-inspection; as-is, a regression that drops the warning entirely would not fail any test.

28. [ ] **test-quality-reviewer** | `/Users/etr/progs/wonk/.worktrees/TASK-085/src/review.rs:447` | missing-test
   The findings sort (severity desc, then file/line/rule) has no test at any level, and no lib fixture produces two findings to order — every fixture asserts exactly one finding or filters by kind. The one-symbol-two-findings case (a signature-changed symbol that both blocks and warns) exists only in the bench self-check, which is not part of `cargo test`.
   *Recommendation:* Add a fixture with a signature-changed, caller-owning, uncovered symbol plus an uncovered body-edit elsewhere, asserting the full ordered findings vec (blocking first, then file/line order) — this also pins the both-rules-on-one-symbol path in the test suite proper.

29. [ ] **test-quality-reviewer** | `/Users/etr/progs/wonk/.worktrees/TASK-085/src/review.rs:758` | missing-test
   The AC1 end-to-end fixture is numerically ambiguous: `used()` lived at old line 1 and, after deletion, `caller()` occupies new line 1, so `assert_eq!(f.line, Some(1))` would pass even if the engine anchored to the wrong side. The `anchor_method == OldSideLine` assertion is what discriminates (as the task AC acknowledges), but no e2e test removes a symbol whose old-side line differs from every new-side line, so the line NUMBER flowing through real git output is never independently anti-mis-anchor (the table tests at lines 618-640 cover this only with synthetic hunks).
   *Recommendation:* Reorder the fixture (caller first, `used()` second, e.g. old line 3) so the expected old-side line cannot collide with any post-change line, making both the anchor_method and the numeric assertion independently discriminating. Same applies to the JSON smoke test's `v["findings"][0]["line"] == 1` in tests/review_cli_integration.rs:101.
