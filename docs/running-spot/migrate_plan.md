# Planning a migration: `spot migrate-plan`

A plan says what an upgrade will do, how risky it is and how long it will take, **before**
anything changes. Planning and showing change nothing in the site. Every later step (approve,
rehearse, apply, rescue) names the plan by its hash and runs exactly that plan.

## Commands

```
spot migrate-plan plan --site S [--as USER] [--json]
spot migrate-plan show HASH --site S
```

`plan` reads the installed apps' metadata and the live schema, works out the changes, classifies
the risk (`additive`, `needs_review`, `destructive`), estimates the duration and the kind of lock
(`online`, `brief_lock`, `maintenance_window`), applies the gates below, stores the plan by its
hash with its author and prints the exact next command. The same inputs give the same hash and no
second copy: running `plan` again (even by someone else) returns the stored plan.

`show` prints the stored plan, its state, who authored it, and who approved it. If the installed
apps changed after the plan was made, the plan is reported (and marked) `superseded`: plan again.

The existing `spot migrate [--apply]` keeps working untouched and keeps using the schema engine
until the apply step of the lifecycle is wired.

## The gates

- **Permission.** The person needs the migration right. Without it the command exits 2 and nothing
  is stored.
- **Expand and contract.** A change that removes or tightens something (a drop, a rename, a
  `NOT NULL`) must ship one release after the code stopped using it. If it does not, the plan is
  refused and the object and releases are named. A destructive plan with no usage manifests to
  prove that is refused too.
- **Conflicts.** Findings about the site's own customisations (a custom field that now collides, a
  script that no longer type-checks) are printed under the plan. They are information for the
  approver; they are not part of the hash.

## Who acts

`--as USER` names the person (default: the system). On the command line the operator at the host
states it; the management API operation `site.migrate.plan` authenticates it. The plan's author is
recorded, and the person who approves must be someone else (see `migrate_approve.md`).

Exit codes: 0 done, 1 usage, 2 refused by policy or permission, 3 failed.
