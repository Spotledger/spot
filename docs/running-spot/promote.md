Part of [Running Spot](../running-spot.md).

# spot promote

A customisation bundle goes from one environment to the next (dev to test, test to prod) inside one
environment set (the host root). Sites are linked to an environment first (`spot bundle env link`),
and the hand-over file is the signed one `spot bundle export` writes.

```
spot promote request --from SITE --to SITE --bundle FILE --as USER
spot promote approve ID --as USER [--role ROLE]... [--comment TEXT]
spot promote status ID
spot promote apply ID [--max-rehearsal-age-secs N]
spot promote abort ID
```

Exit codes: 0 done, 1 usage, 2 refused (nothing changed), 3 failed.

## What each step does

* **request** checks the environments (same set, target is the next rung, never the same site),
  evaluates the hand-over file against the target exactly as `spot bundle import` does (signature,
  direction, key policy, plan, conflicts with the target and the platform) and records the promotion
  under `promotions/ID.json` with the kept file `promotions/ID.handover`. Any conflict stops the
  request: nothing is recorded.
* **approve** records an approver's identity, roles and time after the same guards: the requester
  never approves production, one approver once. Test and dev need no approver for an additive bundle;
  production needs two, role `owner`.
* **apply** replays the record through the promotion service with the real ports: the target's
  approval store (plan authors, approvals checked again with the approver's recorded time), a
  rehearsal on a `pg_dump` clone of the **target** (never the live database; the scratch is dropped on
  every exit path), the freshness guard (`promote.rehearsal_stale` when the rehearsal is older than
  `--max-rehearsal-age-secs`, default 3600, for production), the write to the target's customisation
  records, and verification (a second plan is empty; otherwise the pre-image is written back).
  Every transition is written to the migration audit chain of **both** sites.
* **abort** closes a promotion that is not applied and drops the clones its rehearsals left on the
  target (only databases that carry the scratch marker). An applied promotion is not aborted:
  promote a corrected bundle.

## Things to know

* A write is `pending` customisation records (custom fields and property overrides); the site finishes
  them at its next start. Views, scripts, rules and fixtures have no live store yet: a bundle that
  changes one is refused at apply (`custom_bundle.apply_failed`) and nothing is written.
* The pipeline needs `pg_dump` (`SPOT_PG_BIN`) and the sites' audit tables prepared (start each site once).
* The environment identity compared on both ends is `spot_environments::EnvRef`: the set is the host root
  directory's name, the tenant is derived from the site name.
* The operator's approvals are kept as intent and written to the stores when `apply` runs; a run that
  fails can be retried with `apply` again.
