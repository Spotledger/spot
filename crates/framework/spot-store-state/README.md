# spot-store-state

The store server's own durable state (WP-STR-03.T4), as a synchronous port (`StoreStateStore`)
with a memory double (`MemoryStoreState`): hashed publisher keys, review records, the listing
state, idempotency keys, revoked keys and bundles with the revocation list generation, and the
append-only audit log. `spot-store-state-pg` is the PostgreSQL implementation.

- **The listing fails closed.** Only an explicit `listed` row lists a version; no row,
  `pending_review`, `rejected` and `withdrawn` do not. Transitions: none to `pending_review`,
  `pending_review` to `listed` or `rejected`, `listed` to `withdrawn`; everything else is
  `store_state.bad_transition`.
- **No secrets.** Keys are stored as SHA-256 (`KeyHash`, whose `Debug` prints no bytes); no error,
  `Debug` output or audit entry carries a key, a hash, a token, a bundle or a summary.
- **Bounded, never truncated.** Identifiers 128 bytes, reasons 1 KiB, review summaries 64 KiB,
  stored revocation lists 1 MiB; over the bound is `store_state.too_large`.
- **Time is an argument.** No implementation reads a clock; no floats.
- **Tests.** `tests/common/conformance.rs` holds the shared cases; the PostgreSQL crate runs the
  same file against a real server.

Errors are catalogued in `crates/framework/spot-errors/catalogue/store_state.toml`.
