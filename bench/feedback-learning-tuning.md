# Feedback learning-rule tuning (TASK-102, OQ-019)

Date: 2026-10-02
Environment: Apple M4, macOS (darwin 25.3.0, arm64), rustc 1.97.1, debug
test profile (the experiment is a deterministic trace simulation, not a
wall-clock benchmark).

Closes OQ-019: the update rule and its constants. Rule (fixed by
TASK-102's design, DR-042/DR-043): per qualifying event, every feature
observable on the useful result or its alternatives takes one bounded
step along its contrastive advantage,

```
A(k)   = v_useful(k) − mean_alternatives(v(k))        ∈ [−1, 1]
next   = clamp( decayed(k) + step · A(k),  lo(k), hi(k) )
decayed = default + 0.5^(age / half_life) · (stored − default)
```

with `lo/hi = default·(1 ∓ dev)` for signal keys (a zero-default signal
is pinned at `[0, 0]` — enabling a criterion stays a human decision) and
`±dev` for descriptive keys. Tuned here: `step` and `half_life_days`.

## Method

Real usage traces do not exist yet (the feature has no users), so the
honest method is a deterministic simulation over synthetic-but-shaped
event streams derived from the real integration fixture's slate shapes,
driven by a fixed xorshift seeded per scenario — no new dependencies.
Harness: `tests/feedback_learning.rs`, module `tuning` (the sweep test
prints the table below under `--nocapture`; the pinning test asserts the
chosen cell's numbers verbatim so this document cannot drift).

Scenarios (2000 events each, 4 sessions round-robin in the gate sense,
`now` advancing one event per simulated hour):

- **consistent** — 85% of events favor the true direction, |A| ≈ 0.8
  (the path-character-vs-tests shape).
- **noisy** — 60% +0.3 / 40% −0.3: a weak true direction, mean advantage
  +0.06 — the shape of mixed real feedback.
- **adversarial** — 100% one direction at full magnitude.
- **stale** — a 50-event consistent burst, then silence; read-time decay
  does the rest.
- **flip** — consistent, with the true direction reversing at event 1000.

Grid: `step ∈ {0.01, 0.02, 0.05}` × `half_life ∈ {14, 30, 60}` days, over
a `path_character`-shaped signal (default 0.6, dev 0.5, band [0.3, 0.9],
band width 0.3).

## Grid

| step | half_life | (a) events to 50% of band | (b) noisy residual | (c) noisy sign-flips | (d) stale fall-back days | (e) bound violations |
|------|-----------|---------------------------|--------------------|----------------------|--------------------------|----------------------|
| 0.01 | 14 | 26 | 0.290 | 23 | 28.0 | 0 |
| 0.01 | 30 | 23 | 0.292 | 23 | 60.0 | 0 |
| 0.01 | 60 | 23 | 0.293 | 23 | 120.0 | 0 |
| 0.02 | 14 | 12 | 0.284 | 23 | 28.0 | 0 |
| **0.02** | **30** | **12** | **0.286** | **23** | **60.0** | **0** |
| 0.02 | 60 | 12 | 0.287 | 23 | 120.0 | 0 |
| 0.05 | 14 | 5 | 0.267 | 23 | 28.0 | 0 |
| 0.05 | 30 | 5 | 0.268 | 23 | 60.0 | 0 |
| 0.05 | 60 | 5 | 0.269 | 23 | 120.0 | 0 |

Metrics: (a) events past the start to reach 50% of the deviation band in
the consistent stream (want ≤ ~60 — "tens of events matter"); (b)
residual |effective − default| in the noisy stream after 2000 events
(want ≤ 25% of band); (c) sign flips of effective − default in the noisy
stream (want ≤ 6); (d) days for the stale stream to fall back under 25%
of band (want ≤ ~2 half-lives); (e) bound violations across every stream
(must be 0).

## Choice and rationale

**Chosen: `learn_step = 0.02`, `learn_half_life_days = 30` — the
defaults, retained.**

- (e) is zero in every cell: the clamp is the safety property, and it
  holds under full adversarial repetition for every candidate constant —
  AR-043 does not depend on the tuning choice.
- (d) is exactly two half-lives in every cell (the stale burst saturates
  the band, then halves back in the read-time decay), so half-life
  controls fall-back speed exactly as specified; 30d sits between the
  14d and 60d edges.
- The plan's aspirational wants for (b) and (c) are **unsatisfiable at
  the hourly event rate for every cell**: i.i.d. evidence with any drift
  saturates the band (the 60/40 noisy stream reaches the +bound and
  crosses default 23 times under every step). That is not a tuning
  failure — it is the clamp doing its job, and it is why influence is
  additionally gated on observations and distinct sessions
  (PRD-FB-REQ-025) rather than on the step size alone. With sparser real
  feedback (events days apart, not hours), the decay mean-reverts the
  walk and the residuals shrink; re-run this sweep when real traces
  exist.
- (a) is the only metric that separates the cells: 23 / 12 / 5 events.
  Every step already satisfies "tens of events matter". 0.05 buys
  convergence 2.4× faster at 2.5× the per-event jolt; 0.01 is the most
  conservative but the slowest to demonstrate value. 0.02 is the mildest
  step that still converges in ~a dozen events — and it is the value the
  acceptance fixtures were built against.

The remaining defaults were not swept (they answer different questions):
`learn_max_deviation = 0.5` is the AR-043 bound, `learn_min_observations
= 10` / `learn_min_sessions = 3` are the PRD-FB-REQ-025 gate, and
`[rank.weights] feedback = 0.35` sits below the structural signals.
