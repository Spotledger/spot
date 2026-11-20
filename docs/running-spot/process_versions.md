Part of [Running Spot](../running-spot.md).

# Process versions

The process versions module (`crates/framework/spot-cli/src/services/process_versions.rs`,
`ORDER = 1160`) is the production adapter behind the version operations of native API 0.12.0:
list, get, publish and retire a version of a process, and plan, start and follow the migration of
running Cases to another version. It runs over the process engine (`process`, 1100), the trigger
intake (`process_triggers`, 1130) and the documents module (`process_documents`, which provides
the service-principal publish guard). It is an **optional** service: a broken one stops
publishing and migrating and degrades readiness; Cases and documents go on.

## Permissions

All four are platform permissions, granted to roles in the usual way.

| Permission | Allows |
| --- | --- |
| `process.publish` | publish, retire, list, get |
| `process.read_versions` | list and get only |
| `process.publish_service_principal` | publish a version whose Document node acts as a service principal |
| `process.migrate` | plan, start and follow a migration (separate from publish) |

A caller without the permission, an unknown process and an unknown version are one answer
(`404 process.not_found`; `404 process.migration_not_found` for a migration), so nobody learns
which processes exist. A permission source that cannot answer **denies**. Every refusal is audited.

## Publishing

In this order, the first failing class answers:

1. The text must parse and name the process in the path; the model compiles it (cycles,
   unreachable nodes and the other model rules: node ids and rule codes only).
2. A definition with a service-principal Document node needs `process.publish_service_principal`
   and the site's publish guard (`403 process.publish_denied`, naming no principal).
3. Every node kind needs a handler on this site (`handler_missing`, with the node id). A kind no
   module registered is refused here, never failed mid-Run.
4. The effect checks (`spot-bpm-effect-checks`) over the platform's effect classes.
5. Every webhook trigger needs a configured secret (`[services.process_triggers.webhooks]`),
   named by kind `webhook_secret_missing`.

Then the version is stored with the expected head (`expected_previous`; two concurrent publishes
make one version and one `409`), an audit row is written, and the engine's registry and the trigger
index are rebuilt **before the call returns**: a publish (or a retire) reloads the triggers at once
through the trigger intake's `TriggerReload` port, so the next event or webhook already sees the new
trigger set and the 15 s pass of `process_triggers` is only the backstop. The first publish of a
name creates the process. If the rebuild fails the version is stored all the same (it is live in
the engine), the old trigger set stays in force, an audit row says which step failed
(`registry_reload_failed` or `trigger_reload_failed`, never a text), and the worker retries every
tick. If the trigger intake is off on the site (no `TriggerReload` port), the publish succeeds and
the intake's own pass picks the change up.

A problem carries a node id and a closed kind token, never text from the document.

## Retiring and pinning

A **running Case stays on its version**: it pins the version and content hash it started on, and
nothing here writes a Case on publish or retire. Retiring replaces the live registry entry by its
retired form (a Case pinned to it keeps running; no new Case starts on it) and reloads the
triggers at once, as a publish does. The trigger index only holds published versions, so retiring the only published version
turns off the process's triggers until a new version is published.

## Migration

`plan` is a read-only scan (at most 20 000 Cases per plan): counts per node pair, the Cases that
cannot move with a stable reason code, at most 100 of them, each by id only. `start` writes one
durable record (`spot_bpm_migration`) and wakes the worker. A repeated `Idempotency-Key` returns
the first migration id and starts nothing.

The worker claims a migration under a database lease (`lease_secs`), so two nodes never migrate
the same Cases, and moves at most `batch` Cases per pass from the record's cursor. **Each Case moves
in one transaction** that also counts the move and writes its audit row (`spot_bpm_migration_audit`,
the `sys_bpm_case_migrated` payload), so a restart at any point finishes exactly once. A Case that
is mid-node (`bpm_versioning.case_mid_node`), waiting on a timer, message, person or child Case
(`bpm_versioning.case_waiting`), whose mapping does not fit, or that changed after it was planned,
is **skipped with its reason and counted, never forced**. A migration whose target is retired
after the start fails (it never moves a Case onto a retired version).

State: `running` while planned or in progress (and resumable), `completed` when every Case has an
outcome and none failed, `failed` when a Case failed or the target is gone.

### Skipped Runs

A migration that ends with `skipped = 40` means forty Runs were left where they were. The count is
all the worker keeps: **skips are counted, not listed per Run**, and the reason of each skip is not
stored with the migration. To see which Runs and why, use the plan, which lists up to 100 Runs that
cannot move, each by id with its reason code. Nothing is forced and nothing is lost: a skipped Run
keeps running on its pinned version.

| Reason | What it means | Will a later migration of the same version pair move it? | What to do |
| --- | --- | --- | --- |
| `bpm_versioning.case_not_found` | The Run no longer existed when the move was attempted (deleted or archived between the plan and the move). | No: there is nothing left to move. | Nothing, unless you expected the Run to exist. |
| `bpm_versioning.case_not_running` | The Run had already completed, failed or been cancelled. A finished Run keeps its pinned version. | No: only running Runs are listed for a migration. | Nothing. |
| `bpm_versioning.case_not_on_version` | The Run was no longer on the source version (another migration or a rollback moved it first). | No, not from this pair: it is on another version now. | Check which version it is on; no action if that is the one you wanted. |
| `bpm_versioning.case_changed` | The Run advanced after the plan was made (its revision moved), so the plan was stale. | Yes, if it is still on the source version: it is planned afresh. | Start the migration again. |
| `bpm_versioning.case_mid_node` | A node of the Run was executing at that moment (a running node, or an attempt started or being retried). | Yes, once the node settles. | Run the migration again later. |
| `bpm_versioning.case_waiting` | The Run was waiting on a timer, an event, a person's task or a child Run. The host does not migrate waiting Runs in this version: it skips them and keeps them on their version. | Yes, once the wait has ended and the Run is again at rest on the source version; a Run that waits for days is skipped on every pass until then. | Run the migration again after the wait ends, or let the Run finish on its pinned version. |

A later migration of the same pair starts from the beginning of the source version's running Runs,
so a repeat is the way to pick up the Runs that were busy. Other skips, whose reasons are the
planner's own (`bpm_versioning.version_mismatch`, `unmapped_node`, `unknown_target_node`,
`mapping_collision`, `variable_missing`), say the mapping does not fit that Run; correct the mapping
and migrate again.

## Configuration

```toml
[services.process_versions]
enabled = true      # false makes the module inert
batch = 100         # Cases per migration pass (1 to 1000)
tick_secs = 5       # seconds between worker passes (1 to 60)
lease_secs = 60     # seconds a worker holds a migration (10 to 600)
```

Unknown keys are refused. Tables: `spot_bpm_version_audit`, `spot_bpm_migration`,
`spot_bpm_migration_audit`, all tenant-scoped and created at start.

## Audit and logs

Target `spot::audit::process_versions` and the `spot_bpm_version_audit` table record who, which
process and version, the operation and the rule that refused (or `ok`), plus the content hash and
the note length of a publish. Never the definition text, the note, an idempotency key, a mapping or
a document value. Metric labels never carry ids.
