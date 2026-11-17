# Rehearsing a migration: `spot migrate-rehearse`

A rehearsal runs an **approved** plan on a copy of your site, so you learn what the apply will do
before it touches the real database. It is what makes the apply safe to approve.

```
spot migrate-rehearse run HASH --site S [--as USER] [--budget-minutes N] [--json]
spot migrate-rehearse clean --site S
```

## What happens

1. The plan must be approved (`migration.not_approved` otherwise). Nothing starts if the free
   space is below **2.5 times** the database size (`migration.insufficient_space`).
2. The site is snapshotted with `pg_dump` and restored into a **scratch database** named
   `spot_rehearsal_<site>_<plan>_<time>`, never the live name. Set `SPOT_PG_BIN` to the directory of
   the PostgreSQL client tools if `pg_dump` is not on the `PATH`; `SPOT_REHEARSAL_DIR` moves the
   snapshot directory (default: the system temporary directory).
3. The plan is applied to the scratch with the real apply engine, the financial fingerprint is
   measured before and after, the invariants run, and (when the site's replay policy says so) recent
   documents are replayed through the new version.
4. The scratch and the snapshot are removed on every exit path, including a failure and the time
   budget (default 30 minutes; `--budget-minutes`).
5. The report is stored by plan hash and time. The plan becomes `rehearsed` only if the rehearsal
   completed with no failed invariant and no difference the plan does not explain.

Exit 0 pass; 2 refused (nothing started); 3 the rehearsal failed or timed out (the report is stored
and its ledger key printed). A failed rehearsal leaves the plan `approved`.

## What is, and is not, proven

Proven: the plan applies to a faithful copy of your data, the schema invariants hold afterwards,
and every change in the fingerprint is one the plan declares. **Not** proven: the speed of the real
apply under live load, the behaviour of connected external systems, or anything the invariant sets
do not check. Until the ERP registers its fingerprint queries the fingerprint set is empty; that is
recorded as a pass with the sentence "no fingerprint sources registered".

## Reading a replay difference

The report names the document type, the operation and the field that behaved differently, never the
values. A difference the release metadata declares is "expected"; any other is "unexplained" and
fails the rehearsal. Fix the release or declare the change, plan again and rehearse the new hash.

## When a rehearsal is killed

A process killed mid-way can leave a scratch database. Run `spot migrate-rehearse clean --site S`:
it drops only databases of this site's rehearsals that carry the scratch marker, and never the live
database. The next rehearsal does not need it, because every name includes the time.

The scratch holds a full copy of the data. Treat it like the live database until it is dropped.
