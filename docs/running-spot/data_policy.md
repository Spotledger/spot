Part of [Running Spot](../running-spot.md).

# Retention (`[services.data_policy]`)

The retention job turns a tenant's retention rules into deletions, **dry run first**, with legal holds checked for every item at the moment it is processed, hard caps, a kill switch and an audit trail. `spot-data-policy` decides; this service hosts it.

## Settings

```toml
[services.data_policy]
enabled = false            # off by default
policy_dir = "policy/acme" # relative to the configuration root; required when enabled
enforcement = "off"        # off | manual | automatic
interval_seconds = 3600    # between passes; at least 60
automatic_after_days = 7   # `automatic` runs only after this many days of unchanged dry runs
batch_size = 100           # 1 to 10,000; the checkpoint is saved after each batch
slice_seconds = 300        # a run yields after this long
max_deletions_per_run = 10000
```

`policy_dir` holds `rules.toml` (required), and `holds.toml`, `statutory.toml`, `classes.toml` (optional). Their formats are those of `spot-data-policy`. The files are read again **before every batch**, so a hold added during a run protects its records from the next batch on. An invalid file stops planning and running and the service reports `degraded (data_policy_host.policy_invalid)`.

## The modes

| mode | what a pass does |
|---|---|
| `off` | builds a plan and keeps it pending with its digest; never executes |
| `manual` | executes the pending plan only if an administrator confirmed **its digest**; the person who requested the plan cannot confirm it |
| `automatic` | executes only after the same policy files were observed in dry runs for `automatic_after_days` days; a changed policy restarts the observation |

## A run

The audit chain (scope `retention:<site>`) gets `retention.plan_built`, `retention.plan_confirmed`, `retention.run_started` (**written before the first batch**; if it cannot be written nothing is deleted), `retention.run_finished`, and one `retention.proof` per deleted record. Every deletion is one store transaction that carries the item's outcome, audit record and signed deletion proof.

A run stops after the current batch when: the kill switch is set (`kill_switch`), a hold with scope `all` exists (`tenant_hold`), the time slice ended (`time_slice`) or `max_deletions_per_run` items were applied (`deletion_cap`). The plan stays pending in this process and the next run **continues** it from the checkpoint.

**After a restart the pending plan is gone** (it is not persisted): the next pass plans again, the digest differs and must be confirmed again. Records already deleted are simply not in the new plan.

## The kill switch

`enabled = false` removes the service on the next reload. A stopped run, or a hold with scope `all`, stops a run within one batch.

## Backups

When a backup catalogue is provided, each pass lists the backup sets expired by their rule (`plan_backup_expiry`). The backup service executes the expiry; this service only lists it.

## The store provider

The service needs a `StoreProvider` port (the PostgreSQL ledger of `spot-data-policy-pg` plus a record source that deletes through the document lifecycle). **Until a module provides one the service starts degraded** and says so; it never touches a record.

## Errors

Stable codes in `crates/framework/spot-errors/catalogue/data_policy_host.toml`. A message never carries a record or a value.
