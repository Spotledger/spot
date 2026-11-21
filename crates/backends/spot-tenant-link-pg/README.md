# spot-tenant-link-pg

PostgreSQL link store for the tenant link (ADR-058, ADR-080). See the crate docs for the data
layout and the administration operations.

## Handshake ports: use the durable ones

A host must not wire `MemoryNonceLog` or `MemoryPeerKeyDirectory` in production: a restart would
forget every used handshake nonce (a captured message replays) and every pinned peer key.
Take both ports from the store in one call:

```rust
let store = PgLinkStore::new(pool, runtime, tenant);
let (nonces, keys) = store.handshake_ports();
let policy = HandshakePolicy { keys: &keys, trust, consent, gate: &gate, nonces: &nonces };
```

- `PgNonceLog::record` is `Ok(false)` for a nonce seen before and `Err` for a database failure,
  never `false`. It never prunes by itself; the job engine calls `PgLinkStore::prune_nonces`.
- `PgPeerKeyDirectory::pinned` returns `None` on a failed read (the trait cannot say more) and
  records the failure: check `last_error()` before offering `Trust::FirstUseConfirmed`, and prefer
  `pinned_checked()` where a failure must be an error. Two different keys recorded for one party
  are a failure, not a choice.
