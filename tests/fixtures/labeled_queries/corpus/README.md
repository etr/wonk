# gatehouse

A session and access-control library used as the labeled ranking fixture.

Module map (see `docs/architecture.md` for the full walkthrough):

- `src/auth/token.rs` — token validation and issuance
- `src/auth/session.rs` — session lifecycle
- `src/auth/credentials.rs` — credential rotation
- `src/cache/engine.rs` — the cache engine and eviction
- `src/retry/engine.rs` — retry with exponential backoff
- `src/parser/engine.rs` — request header parsing
- `src/queue/engine.rs` — the queue worker
- `src/compat/legacy_auth.rs` — pre-2.0 auth flow (deprecated)
- `src/generated/user.g.dart` — generated Dart bindings
- `examples/basic.rs` — a runnable tour

Run the regression suite with `cargo test --test token_test`.
