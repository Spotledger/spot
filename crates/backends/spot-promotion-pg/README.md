# spot-promotion-pg

The PostgreSQL `PromotionStore` of `spot-promotion` (WP-ALM-04.T6, ADR-082 class C). A promotion
is kept between runs as a versioned `Snapshot` (`spot_promotion::Snapshot`) in
`spot_control.spot_promotion`: `tenant` and `id` (primary key), `revision bigint`, `state`,
`doc jsonb`. No extension is needed.

## Tenant scope

A store is built for one tenant, the **target environment's** tenant. The tenant leads the
primary key and every statement filters on it, so the same promotion id in two tenants is two
rows that never meet. A promotion whose target is another tenant is refused with
`promotion.environment_mismatch`, and a row found under the wrong tenant is refused on read.

## Where it may be used

The trait is synchronous, so the store blocks on the shared `spot-pg-bridge`: use it from **a
control-plane process, a CLI command or a job worker**, never from a request handler (the bridge
refuses on any tokio runtime thread and the store returns `promotion.store_failed` naming
`pg_bridge.wrong_thread`) and never inside a transaction an async store holds.

## Use

```rust,ignore
let store = PgPromotionStore::new(bridge.clone(), &backend.pool(), target_tenant)?; // plain thread
store.ensure_schema()?; // once at start-up; idempotent, under an advisory lock
let service = PromotionService::new(Arc::new(store), approvals, approval_audit, audit, applier, policy)?;
```

## Semantics

`insert` (revision 0, `promotion.duplicate`), `load` (`None` when unknown), `save` (a
compare-and-swap on the revision in one transaction; `promotion.not_found` and
`promotion_pg.stale_save` told apart exactly; a stale save writes nothing), `list` (by id, pages of
`LIST_PAGE`=100 in one snapshot; above `LIST_CEILING`=5000 it fails with `promotion.store_failed`,
never truncating). A row that no longer restores is refused with `promotion_pg.snapshot_invalid`
or `promotion_pg.snapshot_version`, never repaired. Deadlines: 5 s read, 10 s write, 30 s list.
Errors carry only a bridge code or SQLSTATE.

## What a snapshot does not hold

The typed changes of the bundle's apply plan have no persisted form. A promotion loaded from this
store has `plan().apply_plan() == None`; the service re-derives it against the target before a
rehearsal or a verification and refuses a plan whose hash is not the approved one.

## Sizing

Promotions are few and written by operators: `max_in_flight = 2`, `queue_capacity = 8` and a pool
of 4 (the bridge needs `max_in_flight <= pool - 2`); add the other class C stores' `max_in_flight`
of the process to the pool rather than raising this one.

## Tests

Need `SPOT_TEST_PG_URL` (admin URL); skipped with a printed `SKIPPED:` line when unset.
