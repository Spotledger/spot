Part of [Running Spot](../running-spot.md).

# PostgreSQL storage: settings, failures and what to do

Every service of a site stands on one PostgreSQL 18 database. This page says what is in it,
where each setting lives, what a failure looks like and what to do about it. A script
(`tools/docs-check/check_postgres_runbook.py`) reads this page and the code and fails when a
code, a key or a default here is not in the source, so what is written here is what the code
does. The server must be PostgreSQL 18 with the ICU collation provider; the backend refuses
anything else when it connects (`storage.unsupported`).

## What the database holds

One tenant is one database (ADR-34). `spot new-site` creates it with `CREATE DATABASE ...
ENCODING 'UTF8' LOCALE 'C' LOCALE_PROVIDER icu` from `template0`, using the role in
`[postgres] admin_url`; a database name is 1 to 63 lowercase letters, digits or underscores.

- **Document tables** are named after the DocType's storage name and carry the system columns the
  catalogue defines (`id`, `version`, and the lifecycle columns). A child table has `parent`
  columns and no `version` of its own.
- **Everything the platform adds itself carries the `spot_` prefix**: `spot_meta` (catalogue
  version and collation, the feed generation and the feed retention), `spot_outbox` and
  `spot_change` with their `_pub` position tables and `spot_feed_state` (the outbox and the
  change feed), `spot_sequence` (gapless number series), and the tables of the service crates
  (jobs, coordinator, graph edges and so on). The `spot_` rule is a CI gate
  (`tools/lints/table-prefix`) because `spot migrate` treats any unprefixed table it does not
  know as someone else's and refuses a plan that would touch it.
- Document tables get a capture trigger that writes the change feed in the same transaction.

## Connecting

The URL form is `postgres://user:password@host:port/dbname?...`. Example (never put a real
password in a ticket): `postgres://spot:***@db.example:5432/postgres`.

| Setting | Where | Default | Effect | How to change |
| --- | --- | --- | --- | --- |
| `[postgres] admin_url` | `common.toml` | none, required | the URL of a role that can create databases; per-site URLs are derived from it by changing the database name | edit `common.toml`, restart |
| `[postgres] pool_max_size` | `common.toml` | 16 <!-- default:cli_pool_max_size=16 --> | most connections the pool of one site opens | edit `common.toml`, restart |
| pool size in `PgConfig` | library default | 16 <!-- default:pool_max_size=16 --> | the same default for code that builds a `PgConfig` itself | in code |
| connect timeout | URL parameter `connect_timeout` (seconds) | 5 s <!-- default:connect_timeout_s=5 --> | longest time to open one connection | URL |
| pool wait | `PgConfig::pool_wait_timeout` | 5 s <!-- default:pool_wait_timeout_s=5 --> | longest wait for a free pooled connection before the call fails as `storage.unavailable` | in code only; there is no `common.toml` key |
| `application_name` | URL parameter | `spot` | what `pg_stat_activity` shows; each transaction appends its label | URL |
| TLS | URL parameter `sslmode` | off | **only** `sslmode=require` turns TLS on (server certificate verified against the platform roots); every other value, and no value, means plain TCP | URL |

Rules the URL parser enforces: exactly one TCP host (a Unix socket path or several hosts are
refused as an invalid value), the port defaults to 5432, the database defaults to `postgres`.
The URL is never printed back in an error. Plain TCP is for loopback and test servers only; use
`sslmode=require` for anything else.

## The pool

`pool_max_size` is per site. Connections are recycled without a reset (`RecyclingMethod::Fast`),
so session state set by one user of a connection outlives its call: that is why the rest of the
platform scopes every setting to a transaction (`SET LOCAL`). A caller that waits longer than the
pool wait gets `storage.unavailable` ("the database is not available right now"); the backend's
health check reports the pool `Degraded` once a waiter has queued for more than one second.

How big should the pool be? The code gives no per-tier rule, so this page does not invent one:
measure with `harness/rust/l9-scale` and read the L9 baseline notes for the
reference volume. Raising the size is not the first fix for `storage.unavailable`: first look for
long transactions holding connections (`pg_stat_activity`, rows with `state = 'idle in
transaction'` or a long `query_start`), then for a lock or a slow statement, and only then add
connections. Keep the total over all sites and nodes below the server's `max_connections`. Some
services need a few connections of their own: the partition tables need `pool_max_size` of at
least 3, and the audit chain opens its own pool capped at 4.

## Transactions

- Isolation is declared per transaction type and the document engine uses **read committed** for
  its writes and reads; stricter levels exist in the backend (and the number allocator and lock
  tests run on all three), but the engine does not use them today.
- A transaction runs with a lock timeout of 5 s <!-- default:tx_lock_timeout_s=5 -->, a statement
  timeout of 30 s <!-- default:tx_statement_timeout_s=30 --> and at most 10,000 writes
  <!-- default:tx_max_writes=10000 --> (children and outbox events count), all set with
  `SET LOCAL` through `set_config`. An idle-in-transaction timeout of the statement timeout plus
  10 s kills a transaction that holds locks and does nothing.
- A retryable failure (`storage.serialization_failure`, `storage.deadlock`) reruns the whole
  transaction. The engine's `RetryPolicy` default is 3 <!-- default:retry_max_retries=3 -->
  retries, waiting 10 ms, 20 ms and 40 ms (never more than 200 ms). A storm of these in the log
  means two kinds of work collide on the same rows: find the two transaction labels in the
  `storage.tx` spans.
- After a database error the transaction is unusable: some kinds (a unique violation, an invalid
  value) allow a savepoint rollback, all others poison it. Nothing is sent to the server once it
  is poisoned.
- `storage.commit_outcome_unknown` means the connection broke while `COMMIT` was in flight: the
  server may have committed. The caller checks the outcome (for example by reading the document
  or by repeating the command id) and never retries blindly.
- Gapless numbering lives in `spot_sequence`, one row per series, updated inside the document's
  own transaction. A rollback returns the number, two transactions never get the same number,
  and different series never wait for each other. The load tests that prove it run 200 sessions
  (`crates/backends/spot-postgres/tests/concurrency_load.rs`; give the test server
  `max_connections` of at least 250 for the full version). A series can be moved forward, never
  back.

## Schema changes: the migration timeouts

`spot migrate` (`spot-pg-migrate`) uses its own defaults: lock timeout 5 s
<!-- default:migrate_lock_timeout_s=5 -->, statement timeout 60 s
<!-- default:migrate_statement_timeout_s=60 --> and, for `CREATE INDEX CONCURRENTLY`, 900 s
<!-- default:migrate_index_statement_timeout_s=900 --> (15 minutes). A table that stays locked by
other work longer than the lock timeout fails the step with `migrate.lock_timeout`; nothing is
lost, the next run continues. The safe response is to wait for the long transaction to end (find
it in `pg_stat_activity`) and run the migration again, never to raise the timeout on a busy site.
A stopped run is finished by running it again (`migrate.interrupted`); a destructive plan is
refused (`migrate.refused`) and belongs to expand and contract; a plan that needs a missing
extension refuses with `migrate.extension_unavailable` (see Extensions).

## The blocking bridge

Synchronous code outside Tokio (control-plane commands, migrations, a few legacy stores) reaches
PostgreSQL through `spot-pg-bridge`, one bridge per process. It never runs on a request path.

| Code | Meaning |
| --- | --- |
| `pg_bridge.wrong_thread` | called from inside an async runtime: a defect; load the data in the async layer instead |
| `pg_bridge.busy` | the bounded queue is full; nothing was queued; retry later or shed work |
| `pg_bridge.timeout` | the deadline passed, the statement was cancelled and the connection closed; the work may or may not have been applied |
| `pg_bridge.closed` | the bridge is shut down |
| `pg_bridge.connect` | no connection could be taken from the pool |
| `pg_bridge.config` | the bridge configuration is not usable (`max_in_flight` must leave its reserve free in the pool) |

## The outbox and the change feed

Events written by document transactions go to `spot_outbox`, and a trigger on every document
table writes one net row per document per transaction to `spot_change`. Readers get positions
`(generation, seq)` from the publisher. Entries are kept for at least the retention, **7 days**
<!-- default:feed_retention_days=7 --> unless `spot_meta` holds another `feed_retention_secs`
(a retention of zero is refused); `cleanup`, called by the job engine, prunes older published
entries.

`storage.resync_required` means a consumer's bookmark can no longer be continued: it is below the
retained floor, from another generation, or beyond the head. After a **restore** the operator
starts a new generation (`start_new_generation`), so every old bookmark fails this way on purpose
and every consumer rebuilds from the start and reconciles its effects. A consumer that was stopped
for more than the retention gets the same answer.

How to see lag: the backend exposes no feed-lag metric of its own. Look at the consumers'
positions against the head in the store, and at the `storage.tx` and `storage.query` spans (see
Telemetry). Do not invent a metric name; none is exported by the storage layer.

## Extensions

The closed list is `postgis`, `btree_gist`, `pg_trgm` and `unaccent`. Nothing installs one
implicitly: `prepare` and `spot migrate` never create an extension. A catalogue that needs one that
is missing fails with `storage.invalid_schema` or `migrate.extension_unavailable` naming it and the
operation `extension.install` of `spot-pg-migrate`, which is audited and has a dry run with a plan
hash. An extension outside the list is refused (`migrate.extension_not_allowed`); a role without
the privilege gets `migrate.extension_privilege_missing` with the statement a DBA can run. There is
no separate extensions page on this base yet.

## Telemetry

Every transaction is a `storage.tx` span (label only) and every storage operation a child
`storage.query` span (kind and label only). The per-request cost accumulator sees each statement's
time and rows. A test (`crates/backends/spot-postgres/tests/telemetry.rs`) proves that no SQL text
and no parameter value reaches a span or a log event. Driver errors are logged at debug level only;
the error that reaches the caller is a fixed plain-language sentence.

## Symptoms and actions

| Symptom or code | Probable cause | What to check | What to do |
| --- | --- | --- | --- |
| `storage.unavailable`, requests slow, pool `Degraded` | pool exhausted by long transactions | `pg_stat_activity`: `idle in transaction`, long queries; pool size against `max_connections` | end or fix the long work first; add connections last |
| connect refused, `storage.unavailable` at start, `postgres.admin_url: cannot reach PostgreSQL` | server down, wrong host or port, `pg_hba.conf` | `pg_isready -h HOST -p PORT` | fix the URL or the server; the URL is never echoed |
| connection fails only with `sslmode=require` | the server certificate is not trusted by the platform roots, or the server has no TLS | the server log; the certificate chain | fix the certificate; do not drop TLS on a network you do not own |
| `migrate.lock_timeout` | another transaction holds the table | `pg_stat_activity` and `pg_locks` for the table | wait for it to end and run `spot migrate` again; do not raise the timeout |
| many `storage.serialization_failure` or `storage.deadlock` | two kinds of work on the same rows | the transaction labels in the `storage.tx` spans | reduce the overlap; the engine already retries 3 times |
| `storage.lock_timeout` | a row or series lock held longer than 5 s | long transactions; a hot number series | find the holder; keep transactions short |
| `storage.statement_timeout` | a statement ran longer than 30 s (a report on a transactional path) | the query and its plan | move reports to the report path, add an index, or chunk the work |
| `storage.transaction_too_large` | more than 10,000 writes in one transaction | the operation that bulk-writes | chunk the work into several transactions |
| `storage.resync_required` | consumer behind the retention, or a restore started a new generation | the consumer's last position | let the consumer rebuild; this is by design after a restore |
| `migrate.extension_unavailable` or `storage.invalid_schema` naming an extension | an extension the catalogue needs is not installed | `SELECT extname FROM pg_extension` | run the audited `extension.install` with a role that has the privilege, or have the DBA create it |
| text sorts wrongly for a tenant | the database was created with another ICU collation | `spot_meta` collation and the database's ICU locale | a collation cannot be changed in place: create the database again with the right collation and move the data |
| a table without the `spot_` prefix is refused by `spot migrate` | the table was created by hand or by an older crate | `\dt` in `psql` | move your own table to another schema; never rename a platform table |
| `storage.unavailable` while writes fail and the server log says "No space left on device" | disk full (SQLSTATE class 53, insufficient resources, maps to unavailable) | the server log and `df` | free or add disk space first; do not restart the server until there is room |
| `spot new-site` cannot create the database | the `admin_url` role cannot create databases | `\du` for the role | grant `CREATEDB` to the role, or use another role in `admin_url` |
| `pg_bridge.timeout` or `pg_bridge.busy` in a control command | slow statement or too many callers | `pg_stat_activity` | retry later; the work may have been applied after a timeout, check before repeating |

## Every storage code

The storage codes are the layer's vocabulary; hosts report their own codes to users. They appear
in logs and spans.

| Code | Meaning |
| --- | --- |
| `storage.not_found` | the record does not exist |
| `storage.already_exists` | a record with this id already exists |
| `storage.stale_version` | the stored version is not the expected one |
| `storage.unique_violation` | a unique index would hold two equal keys |
| `storage.reference_violation` | a database-checked link points at a missing record, or the record is still linked |
| `storage.invalid_value` | a value does not fit its field |
| `storage.invalid_name` | a name is not a valid storage identifier |
| `storage.invalid_schema` | the catalog is inconsistent, or needs a missing extension |
| `storage.unknown_entity` | the entity is not in the transaction's catalog |
| `storage.unknown_field` | the field is not defined on the entity |
| `storage.invalid_query` | the query is not valid against the catalog |
| `storage.serialization_failure` | could not be serialized with concurrent transactions; retried |
| `storage.deadlock` | chosen as a deadlock victim; retried |
| `storage.lock_timeout` | a lock was not granted within the lock timeout |
| `storage.lock_not_available` | a no-wait lock is held by another transaction |
| `storage.statement_timeout` | a statement exceeded its timeout |
| `storage.transaction_too_large` | the transaction exceeded its write limit |
| `storage.read_only_transaction` | a write was attempted in a read-only transaction |
| `storage.traversal_too_large` | a graph traversal reached more nodes than its limit |
| `storage.resync_required` | a feed position can no longer be continued |
| `storage.unsupported` | the backend does not offer this capability (also: wrong server version) |
| `storage.unavailable` | the database cannot be reached or the pool wait expired; retry is safe |
| `storage.commit_outcome_unknown` | the connection failed during commit; the outcome is unknown |
| `storage.internal` | an unexpected backend failure (the SQLSTATE is in the text) |
| `storage.exclusion_violation` | a write conflicts with an exclusion rule |
