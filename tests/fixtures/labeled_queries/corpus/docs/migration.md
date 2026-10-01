# Migration from 1.x to 2.x

The legacy flow (`src/compat/legacy_auth.rs`) is the compatibility layer.
The migration is staged:

1. Stop calling `open_session` from `src/compat/legacy_auth.rs`.
2. Move validation to the new module (`src/auth/token.rs`).
3. Rotate credentials per the policy in `src/auth/credentials.rs`.
4. Delete the compat layer.

Error handling follows the taxonomy in `src/errors.rs`.
