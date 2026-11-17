# Applying a migration: `spot migrate-apply`

The lifecycle as one worked example (the verbs are `spot migrate-plan`, `migrate-approve`,
`migrate-rehearse`, `migrate-apply`; `spot migrate rescue` is the way back):

```
spot migrate-plan plan --site S                 # prints HASH and the next command
spot migrate-approve approve HASH --site S --as alice   # a second person for a destructive plan
spot migrate-rehearse run HASH --site S         # on a clone; required for destructive plans
spot migrate-apply apply HASH --site S          # the dangerous verb
```

## What apply does

1. Reloads the stored plan, recomputes its hash and compares it with the approved one
   (`migration.plan_changed`); checks the state (`approved`, `rehearsed` when required; a
   destructive plan always needs `rehearsed`).
2. **The plan-state compare-and-set is the lock**: `approved/rehearsed` to `applying`; a second
   concurrent apply is `migration.site_busy`.
3. The approval service issues the apply ticket (audited, bound to the plan digest).
4. The engine closes the **maintenance gate** (writes get `503` with `Retry-After`, in-flight work
   is drained), starts the **effect hold**, takes the **named pre-apply snapshot**, runs the steps
   with a checkpoint after each, **verifies**, and opens the gate. A step never runs without a
   restore point.
5. On success the plan is `applied`. On failure it is `failed`, the gate stays closed unless the step
   was transactional and rolled back, and the command prints the one next action and exits 3.

## Resume, rollback, rescue

* `spot migrate-apply apply HASH --site S --resume`: continue from the last checkpoint after a crash
  or a failed step (same result as a clean run).
* `spot migrate rescue HASH`: go back (see `migrate_rescue.md`).

## Maintenance gate by hand

```
spot migrate-apply maintenance enter --site S --reason "disk swap"
spot migrate-apply maintenance leave --site S
```

The gate is a row in the control schema every node polls (default 2 s). If it cannot be proven
closed, nothing starts.

## `spot migrate --apply` and `require_plan`

The older `spot migrate --apply` keeps working for sites that have not turned the lifecycle on.
`sites/S/migrations.toml`:

```toml
require_plan = true        # --apply is refused with migration.not_approved and points here
require_rehearsal = true   # every plan, not only destructive ones, must be rehearsed first
```

Absent means `false`. A file that does not parse counts as `true`.
