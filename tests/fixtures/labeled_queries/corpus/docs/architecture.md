# Architecture

The walkthrough below is the canonical map of the tree.

- Tokens: `src/auth/token.rs` validates and issues; the hub every caller
  funnels through is `issue_token`.
- Sessions: `src/auth/session.rs` opens and closes; storage notes live in
  `src/notes/storage_notes.rs`.
- Cache: `src/cache/engine.rs` owns the eviction strategy described in
  its module docs.
- Queue: `src/queue/engine.rs` drains on shutdown.
- Parsing: `src/parser/engine.rs` normalizes request headers.
- Compat: the pre-2.0 flow under `src/compat/legacy_auth.rs` is described
  in `docs/migration.md`.
- Sharding: `src/shard/shard.rs` selects shards by consistent hashing.
- Bindings: `src/generated/user.g.dart` regenerates from `src/user.dart`;
  ambient TypeScript types sit in `src/types.d.ts`.
- The demo wiring (`src/main.rs`) and the tour (`examples/basic.rs`) show
  every module meeting the token hub.
- The token regression suite is `tests/token_test.rs`; cache eviction
  tests live in `tests/cache_engine_test.rs`.
