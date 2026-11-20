Part of [Running Spot](../running-spot.md).

# Changing the schema: `spot migrate`

This page is for the person who runs `spot migrate` at an upgrade and meets a refusal. A script
(`tools/docs-check/check_schema_migration_runbook.py`) reads it together with the code and fails
when a code, a default, a flag or a printed sentence on this page is not in the source.

**About the example output.** The console blocks below show the format and the fixed sentences
that the command prints. They were assembled from the strings in the code and from the
assertions of `crates/framework/spot-cli/tests/commands.rs`, not captured from a live run (no
PostgreSQL server was available when the page was written, and the code outputs that follow a
real run need one). The script checks every fixed sentence against the source; the plan hash and
the step counts are placeholders. When you run the walkthrough, your numbers differ and your
output is the truth.

## The model

The metadata of the installed apps is the source of truth. The catalogue (the tables, columns,
indexes and constraints) is derived from it. `spot migrate` reads the live database, diffs it
against the catalogue and prints a plan: an ordered list of steps. Safe changes run **online**.
Anything destructive or lossy is **refused**, with a named reason: dropping or renaming a table
or a column, narrowing a type, changing a type family, adding or lowering a length limit,
making an existing optional column required, removing a database-checked link, changing an
index or an exclusion, changing the collation. There is no force flag. Those changes belong to
the expand and contract path (`crates/framework/spot-expand-contract/` and
`crates/framework/spot-migration-plan/`), which this page does not explain.

What the tests of the migration engine prove (`crates/backends/spot-pg-migrate/tests/migrate.rs`):

- a plan with any refusal applies **nothing**, not even the safe part (`every_refused_class_is_refused_and_changes_nothing`);
- an interrupted run is finished by the next run (`a_crash_after_any_step_is_finished_by_the_next_run`): `apply` re-plans from the live schema, so it does only what is left;
- two nodes cannot apply at once (`two_concurrent_applies_on_one_site_run_once`): `apply` takes a session advisory lock on the database, and the node that waits then finds nothing to do;
- a second run on an up-to-date database plans nothing (`plan_of_an_unchanged_catalogue_is_empty_and_the_hash_is_stable`).

## Reading a plan

```text
spot migrate [--site SITE] [--dry-run | --apply [--plan-hash HASH]]
```

Without `--apply` the command only prints the plan (`--dry-run` says so and cannot be combined
with `--apply`). `--site` is needed when the root holds more than one site. The output has a
line `plan hash: <64 hex digits>`, then one line per step with its risk class in brackets, then
one line per refusal. The hash is a SHA-256 over the ordered steps. `--apply --plan-hash HASH`
applies only if the plan **now** has exactly that hash: review a plan, copy its hash, and the
apply is refused if anything changed in between ("the plan changed since it was reviewed"; nothing
is applied). Without `--plan-hash` the apply runs whatever the plan is.

The risk classes (`Risk::*`) as the command prints them:

| Printed | Variant | Meaning | Example |
| --- | --- | --- | --- |
| `[additive]` | `Risk::Additive` | safe and online | a new table, a new optional column, a new index |
| `[needs-backfill]` | `Risk::NeedsBackfill` | part of filling a new required column on a table that has rows | add the column as optional, fill it in batches, then make it required |
| `[needs-validation]` | `Risk::NeedsValidation` | checks every existing row inside one statement that blocks writes | an exclusion constraint added to a table that has rows |
| `[refused]` | (a refusal, not a step) | destructive or lossy | a column removed from the metadata |

How a step runs (`Run::*`): `Run::Tx` is a short transaction with the lock and statement timeouts;
`Run::Outside` is one statement outside a transaction (`CREATE INDEX CONCURRENTLY`, and the
exclusion constraint, with the long index timeout); `Run::Backfill` fills the NULLs of a new
required column in batches that each commit on their own (and are resumable); `Run::SetNotNull`
does a last sweep and then `SET NOT NULL`; `Run::Finalise` installs the change-feed capture on new
tables and records the catalogue version, and is the last step of every non-empty plan;
`Run::Extension` only **checks** that an extension is installed (it never runs `CREATE
EXTENSION`; the planner for schema changes never emits it).

## Walkthrough

Use the scratch site of [Running Spot](../running-spot.md) (`$SPOT` is the binary, the site is
`acme`, the root is `~/spot-root`). Every command below is `spot migrate`; the apps are the
repository's `apps/platform` and `apps/sample`.

**1. A clean site plans nothing.**

```console
$ "$SPOT" migrate --site acme
schema is up to date; nothing to do
```

**2. Install an app and add a field.** `install-app` runs the same plan and is a dry run until
`--apply`. After the sample app is installed, add an optional text field to the `task` DocType
(`apps/sample/doctypes/task.toml`):

```toml
[[fields]]
name = "review_note"
label = "Review Note"
type = "text"
max_chars = 80
```

```console
$ "$SPOT" migrate --site acme
plan hash: 3f1c... (64 hex digits)
  [additive] Add column `task.review_note`.
  [additive] Install the change feed capture on new tables and record the catalogue version.
dry run: nothing was changed; add --apply (and --plan-hash) to apply
$ "$SPOT" migrate --site acme --apply --plan-hash 3f1c...
plan hash: 3f1c... (64 hex digits)
  [additive] Add column `task.review_note`.
  [additive] Install the change feed capture on new tables and record the catalogue version.
applied 2 step(s)
```

**3. A required field on a table that has rows.** Add a required text field. The table holds
tasks, so the new column is added as optional, filled with the type's zero value (the empty text)
in batches, and only then made required:

```console
$ "$SPOT" migrate --site acme
plan hash: 9ab4... (64 hex digits)
  [needs-backfill] Add column `task.reviewer_note` as optional; it becomes required after the backfill.
  [needs-backfill] Fill the empty values of `task.reviewer_note` in batches.
  [needs-backfill] Make `task.reviewer_note` required.
  [additive] Install the change feed capture on new tables and record the catalogue version.
dry run: nothing was changed; add --apply (and --plan-hash) to apply
```

A required **link** has no neutral value to fill with, so the plan is refused (code
`migrate.no_backfill_default`, which the command does not print; it prints the sentence). The
command exits non-zero and changes nothing:

```console
$ "$SPOT" migrate --site acme
plan hash: 5d02... (64 hex digits)
  [refused] task.reviewer: `task.reviewer` is new and required, `task` holds records and the field has no default to fill them with
1 change(s) refused; nothing was applied (task.reviewer: `task.reviewer` is new and required, `task` holds records and the field has no default to fill them with). Destructive changes belong to the expand and contract migration path
```

The `spot migrate` command has no per-field default option (the planner takes one as a library
setting, `PlanOptions::defaults`, which the command leaves empty). The supported routes are to
make the field optional in the metadata, or to use expand and contract.

**4. A change the engine refuses.** Remove the `estimated_hours` field from `task.toml`, or change
its type family. The database still has the column, and a type change can lose values:

```console
$ "$SPOT" migrate --site acme
plan hash: 77e1... (64 hex digits)
  [refused] task.estimated_hours: column `task.estimated_hours` is in the database but not in the catalogue; dropping or renaming a column is not an online change
1 change(s) refused; nothing was applied (task.estimated_hours: column `task.estimated_hours` is in the database but not in the catalogue; dropping or renaming a column is not an online change). Destructive changes belong to the expand and contract migration path
```

Nothing is applied, even with `--apply`. Put the field back in the metadata, or plan the removal
as an expand and contract migration.

**5. Apply twice.** After step 2, run the apply again: it plans nothing.

```console
$ "$SPOT" migrate --site acme --apply
schema is up to date; nothing to do
```

## Refusals

Every refusal prints as `[refused] <table>.<column>: <reason>`. The command's exit code is
non-zero, and **no data is touched** by a refusal. The reasons in the code, with their fix:

| Reason (the fixed words) | Why | Fix |
| --- | --- | --- |
| the collation cannot change | existing text would no longer be in order | create a new database with the right collation and move the data |
| a table is in the database but not in the catalogue | a table was dropped or renamed in the metadata | restore the DocType, or plan the removal as expand and contract |
| a column is in the database but not in the catalogue | a field was removed or renamed | restore the field, or use expand and contract |
| a table lacks the system column | the table was not created by this backend | rebuild the table through the platform; do not edit it by hand |
| a column would narrow | narrowing can lose digits | widen, or use expand and contract |
| a column would change type | a change of type family can lose values | use expand and contract |
| a column is optional in the database but required in the catalogue | making an existing column required can fail on existing records | add it required from the start on a new table, or use expand and contract |
| a column gains a length limit, or lowers it | a new limit can fail on existing text | use expand and contract |
| a link is no longer checked by the database | removing a check is not an online change | restore the link |
| an index exists with a different definition | changing an index is not an online change | restore the definition; plan the change as expand and contract |
| an exclusion exists with a different definition, or its name is taken | changing an exclusion is not an online change | restore it |
| an exclusion is in the database but not in the catalogue | dropping an exclusion is not an online change | restore it |

The stable codes, as the engine reports them (`migrate.*`, in the error catalogue):

| Code | Meaning | What to do |
| --- | --- | --- |
| `migrate.refused` | a destructive or lossy change: the plan holds a refusal | expand and contract, or change the metadata back |
| `migrate.no_backfill_default` | a new required field on a table with rows and no value to fill it with | make it optional, or use expand and contract |
| `migrate.drift` | after the run the live schema still differs from the catalogue | plan and apply again; if it remains, report it as a defect |
| `migrate.lock_timeout` | a table stayed locked past the lock timeout, after the retries | run again when the long transaction has ended |
| `migrate.interrupted` | the run stopped after a step (raised only by the test hook that stops after a step) | run again; only the rest runs |
| `migrate.database` | the database refused a statement (the message carries the SQLSTATE, never the SQL) | read the database log, fix the cause, run again |
| `migrate.duplicate_values` | a unique index cannot be built because rows already share values | remove or merge the duplicate rows, then run again |
| `migrate.extension_unavailable` | the catalogue needs an extension that is not installed | install it with the audited `extension.install` operation, then plan again |
| `migrate.extension_version` | the extension is older than the catalogue needs | update it with `extension.install` |
| `migrate.extension_not_allowed` | the extension is outside the closed list | only `postgis`, `btree_gist`, `pg_trgm` and `unaccent` can be installed |
| `migrate.extension_privilege_missing` | the connecting role cannot create the extension | run it with a role that can, or have a DBA run the statement shown |
| `migrate.extension_plan_stale` | the database changed since the `extension.install` dry run | dry run again and use the new plan hash |
| `migrate.extra_table_invalid` | a platform table handed to the migrator breaks the rules of the hook | a defect in the owning crate; report it |

## When it stops

- **Lock timeout (`migrate.lock_timeout`).** A table was locked by other work for longer than the
  lock timeout, and the step was retried 5 <!-- default:max_lock_retries=5 --> times. Nothing is
  lost. Find the long transaction (`pg_stat_activity`), let it end and run again. The safe response
  is never to raise the timeout on a busy site.
- **Interrupted.** A killed process, a lost connection or a stopped node leaves what finished.
  Run `spot migrate --apply` again: it re-plans from the live schema and does only the rest.
  (The code `migrate.interrupted` itself is raised by the test hook that stops after a step.)
- **Drift (`migrate.drift`).** After the run the live schema still differs from the catalogue. Look
  at the first differences in the message, plan again and apply; if the difference remains it is a
  defect to report. Do not "fix" it by editing the schema by hand: a table or column the platform
  does not know is refused by the next plan.
- **The timeouts**, read from the defaults of the engine: lock timeout 5 s
  <!-- default:lock_timeout_s=5 -->, statement timeout 60 s
  <!-- default:statement_timeout_s=60 -->, index builds 900 s
  <!-- default:index_statement_timeout_s=900 --> (15 minutes), backfill batches of 5,000 rows
  <!-- default:batch_size=5000 -->. A step is in a transaction except the ones marked
  `Run::Outside` above.

## Extensions and extra tables

A plan that needs an extension (`postgis` for a geo field, `btree_gist` for an exclusion with
scope columns, `pg_trgm` for trigram search) **refuses** when the database lacks it, naming the
audited `extension.install` operation of `spot-pg-migrate`; a migration never creates an
extension. There is no separate extensions page on this base yet.

Tables that are not documents (edge sync, the audit chain, the mail sent log) are handed to the
migrator by their owning crate and appear in the plan as one step after all document steps:
`Create the platform object ...`, with the step key `extra_table:<name>`. They run under the same
advisory lock.

## Before an upgrade

1. Back up the database.
2. Run `spot migrate --site SITE` (a dry run) against a copy first.
3. Read the plan; a `[refused]` line stops the upgrade until it is resolved.
4. Apply the reviewed plan: `spot migrate --site SITE --apply --plan-hash HASH`.
5. Run `spot check`.
