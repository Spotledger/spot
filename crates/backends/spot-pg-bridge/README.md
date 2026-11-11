# spot-pg-bridge

The one shared, bounded way for **synchronous** code to reach PostgreSQL (ADR-082 addendum 1,
WP-CON-01.T5). No `*-pg` crate writes its own `block_on`.

## When a bridge is legitimate, and when it is a defect

- **Defect: sync-over-async on a request path.** Code that runs inside a request, a hook, a
  lifecycle step, a job handler or a realtime/SSE task never blocks on a database future. The
  async layer loads the data first (the engine for documents) and the synchronous code receives
  it as an argument. **Rule authors:** a rule that must read mid-step goes through
  `HookContext`; it does not open a bridge, and it does not call this crate. If it does, the
  call is refused with `pg_bridge.wrong_thread` on any tokio runtime thread.
- **Legitimate:** control-plane processes, CLI commands, migrations, and the class C stores of
  ADR-082 (a legacy synchronous trait with no async alternative). There is **one bridge per
  process**, constructed at start-up and shared by `Arc<Bridge>`. A second bridge in the same
  process is a second runtime and a second set of limits: do not.

ERP and app modules do not use the bridge (ADR-082 A4).

## Use

```rust,ignore
let bridge = Arc::new(Bridge::for_backend(BridgeConfig::default(), &backend)?);

let row_count = bridge.run(Duration::from_secs(2), |conn| {
    Box::pin(async move {
        let row = conn.query_one("SELECT count(*) FROM t", &[]).await?;
        Ok::<_, tokio_postgres::Error>(row.get::<_, i64>(0))
    })
})?;                                             // BridgeError<tokio_postgres::Error>
```

`Bridge::for_backend(config, &backend)` is the normal construction: it takes `PgBackend::pool()`
and, when that pool connects with TLS, the same connector for the cancel request
(`PgBackend::cancel_tls()`), so the cancel key never travels in clear and a host cannot forget it.
A caller that has only a pool uses `Bridge::new(config, pool)` for a plain pool, or
`Bridge::with_cancel_tls(config, pool, connector)` with its own connector.

The closure receives a `PooledConn` and returns a `'static` future. The store's own error is
returned as `BridgeError::Store(e)`, unchanged. Every other variant is a refusal with a stable
code (`pg_bridge.*`, catalogue file `pg_bridge.toml`): `wrong_thread`, `busy`, `timeout`,
`closed`, `connect`, `config`.

- A `timeout` means the deadline passed (queue wait and pool wait count). The statement was
  cancelled with the driver's cancel request and the connection was **closed, not pooled**, so a
  late cancel can never hit the next user's statement. A timed-out write may or may not have
  been applied: the caller decides whether a retry is safe.
- `busy` is returned at once, with nothing queued.
- A deadline above `max_deadline` is clamped.

## Session state

The pool is built with `RecyclingMethod::Fast` (`spot-postgres/src/conn.rs`), which hands a
returned connection to the next caller without touching it. **The bridge resets nothing between
calls**, so whatever one closure does to the session outlives its call and is inherited by the
next caller — of this bridge, or of the same shared pool from anywhere else. That covers:

- a session-level `SET`: `search_path`, `ROLE`, `statement_timeout`, `timezone`, any other GUC;
- temporary tables, with their rows, indexes and `ON COMMIT` options;
- session-level advisory locks (`pg_advisory_lock`): the connection goes back to the pool with the
  lock still held, so a lock taken in a closure outlives the call and can block later callers;
- named prepared statements (`PREPARE`, `prepare_cached` with a name).

A caller that needs a setting must therefore scope it to a transaction with `SET LOCAL` (or
`set_config(name, value, true)`), and must not create temp tables or take session locks here:

```rust,ignore
bridge.run_default(|mut conn| {
    Box::pin(async move {
        let tx = conn.transaction().await?;
        tx.batch_execute("SET LOCAL search_path = tenant_a, public").await?;
        // ... the work; the setting ends with the transaction, on commit or rollback.
        tx.commit().await
    })
})?;
```

A `SET LOCAL` outside a transaction block has no effect (the server only warns), and a plain `SET`
inside one still commits with it. `tests/postgres.rs` pins both halves against a real database: a
plain `SET application_name` is visible in the next call on the same connection, and a `SET LOCAL`
inside a transaction is not.

## Sizing

| Setting | Meaning | How to choose |
| --- | --- | --- |
| pool size | connections in the shared `deadpool` pool | peak concurrent bridge calls plus the other users of the pool plus 2 |
| `max_in_flight` | calls running at once | the number of statements the database should run for this process at once; at most `pool - 2` (the reserve is 1 and one more stays free) |
| `queue_capacity` | calls allowed to wait for a slot | the burst you are willing to absorb: waiting calls hold a caller thread each, so keep it near the number of caller threads |
| `workers` | threads of the bridge's own runtime | 2 is enough for I/O-bound database work; raise only if the runtime itself is busy (CPU in the futures) |
| `default_deadline`, `max_deadline` | per-call time budget and its ceiling | longer than your slowest healthy statement, shorter than a caller's patience |

`Bridge::new` refuses an inconsistent set (`pg_bridge.config`, naming the setting). The largest
`max_in_flight` for a pool is `BridgeConfig::largest_in_flight(pool_size)`.

## Observability

Through the recorder `spot-telemetry` installs (label `bridge="<name>"` only; never SQL or
data): `spot_pg_bridge_queue_depth`, `spot_pg_bridge_in_flight` (gauges),
`spot_pg_bridge_wait_seconds`, `spot_pg_bridge_run_seconds` (histograms),
`spot_pg_bridge_busy_total`, `spot_pg_bridge_timeout_total`,
`spot_pg_bridge_wrong_thread_total`, `spot_pg_bridge_cancelled_total` (counters). A queue depth
that stays above zero or a rising `busy_total` means the limits are too low or the database is
slow, before it becomes an outage.

## Shutdown

`Bridge::shutdown(grace)` stops accepting, waits up to `grace` for calls in flight, cancels the
rest (their callers get `closed`) and releases the runtime without waiting. `Drop` cancels and
returns without blocking.

## Tests

Policy tests run over a fake connection source with no database. Tests against a real
PostgreSQL need `SPOT_TEST_PG_URL` (admin URL; see `spot-postgres` testing) and are skipped
with a message when it is unset, unless `SPOT_REQUIRE_PG=1`.
