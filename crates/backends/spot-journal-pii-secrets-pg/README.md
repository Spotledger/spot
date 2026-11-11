# spot-journal-pii-secrets-pg

The PostgreSQL `WrappedKeyStore` for the per-subject journal keys (WP-JRN-08.T6, ADR-061 s6 and
s9). `spot-journal-pii-secrets` keeps each data subject's key wrapped by the tenant's key
provider; this crate makes the place it keeps them durable and shared, so a restart keeps every
key (every personal field in the journal stays readable) and a destroy on one node reaches the
others.

## Wiring

```rust,ignore
// Once per process: the shared bridge (spot-pg-bridge README).
let bridge = Arc::new(Bridge::for_backend(BridgeConfig::default(), &backend)?);
// The table is made by the migrator: hand it this crate's extra table with the other options
// (a planned, reported, locked, additive step `extra_table:spot_journal_subject_key`).
let options = PlanOptions {
    extra_tables: vec![spot_journal_pii_secrets_pg::extra_table()?],
    ..PlanOptions::default()
};
// ... dry_run / apply with `options`, then, once per tenant database at start-up:
spot_journal_pii_secrets_pg::ensure_present(&bridge)?; // reads the catalogue only; creates nothing
let store = PgWrappedKeyStore::new(bridge.clone());
let keys = ProviderSubjectKeys::new(config, provider, store, audit, rng, clock)?;
```

`ensure_present` fails with `journal_pii_secrets_pg.table_missing` when the table or one of its
seven columns (name and type) is absent, so a node that starts before the migration refuses to
serve instead of failing on the first key. There is no run-time `CREATE TABLE`: the crate needs no
database role that may create tables.

A host builds one `PgWrappedKeyStore` per tenant database, shares the bridge, and passes the store
to `ProviderSubjectKeys`.

## The table

The migrator owns the DDL (`DDL` is one `CREATE TABLE IF NOT EXISTS` statement without a
semicolon, as the hook requires). `spot_journal_subject_key` is the **only** place wrapped subject keys live. One row per entry id:
`entry` (primary key), `state` (`live` or `destroyed`), `key_id` and `key_version` (the
key-encryption key that wrapped it), `wrapped` (null once destroyed), `created_at`, `destroyed_at`
(the database's `now()`; the crate reads no clock). Every constraint is named after the table, and
a check constraint ties the columns together: a destroyed row has no `wrapped`, no `key_id` and no
`key_version`.

A tombstone is kept for good: it is what keeps an old ciphertext from getting a second key.
Nothing in this crate deletes a row, and nothing overwrites a tombstone or a live key.

## Where the tenant comes from

The tenant is the database. The crate has no tenant column and no SQL names a tenant; the table is
unqualified, so it lands in the connection's `search_path`. A host that keeps several tenants in
one database gives each a schema and a pool whose `search_path` is set for it (a role-level
`search_path`, not a session `SET`: the bridge resets nothing between calls, see the bridge README,
"Session state"). The entry id the adapter builds also names the tenant, and a key is wrapped with
the tenant in its associated data, so a row copied between tenants does not open.

## What a failure means

Every failure is a `PortError` with a fixed sentence and a code (`journal_pii_secrets_pg.*`), never
"absent": a missing database, a full bridge queue and a timeout all fail the call. A timed-out
write may or may not have been applied. The message carries the bridge code or the SQLSTATE and
never a value, a wrapped byte or an entry id (an id is a salted subject hash; it is still not
logged).

## Tests

Tests against a real PostgreSQL need `SPOT_TEST_PG_URL` and are skipped with a message when it is
unset (`SPOT_REQUIRE_PG=1` makes that a failure).
