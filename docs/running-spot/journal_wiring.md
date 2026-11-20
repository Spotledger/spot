Part of [Running Spot](../running-spot.md).

# Journal wiring: what connects the journal services

Four hosted services (the journal access routes, the journal audit, the journal and sync
management executors) are written to ask for their inputs as ports. The **journal wiring** module
(`[services.journal_wiring]`, `crates/framework/spot-cli/src/services/journal_wiring.rs`) provides
them, and only for a site whose journal is on: `[services.journal]` has a `target`, and the site has
`[services.secrets]`. A site without a journal provides nothing: the three access routes answer
`501 service.not_configured`, the audit is not registered and the executors are not registered.

| Port provided | Used by | What it is |
| --- | --- | --- |
| `JournalStore` | journal access, audit, report | The journal folder, **read only**: every write and delete is refused (`journal_wiring.read_only`), and the folder is checked on every use the way the shipper checks it (it exists, is a directory, is outside the site's data and code directories). |
| `ManifestKeys` | journal access | The public manifest keys derived from the site's key provider. They verify what the shipper signed and nothing else. |
| `AuditWiring` | journal audit | The audit's journal data (store, keys, segments and forks read from the **verified** manifests, derived entities, the tier's recovery time objective), the replay policies (every tenant weekly) and the recovery report's facts. |
| `JournalOps` | management executors | The journal operations, see below. |
| `SyncOps` | management executors | Delegates to the sync host's own implementation (`SyncService`); refuses with `journal_wiring.sync_service_absent` while none runs. |

## Start order

The module starts at order 1105: after `journal` (1100), whose settings and key provider it reads,
and before `journal_access` (1210), `journal_audit` (1220) and `mgmt_exec` (1230), which ask for the
ports while they register. The shipper's handle, the audit service and the sync service exist only
after every module registered, so the providers pick them up afterwards; until then they answer
`Unavailable` (`journal_wiring.journal_not_ready`, `journal_wiring.audit_not_running`).

## Settings

```toml
[services.journal_wiring]
enabled = true     # false: provide nothing
rto_hours = 4      # the recovery time objective an audit judges a rebuild against (1 to 720)
```

Unknown keys are refused.

## What each journal operation does

| Operation | Answer |
| --- | --- |
| `journal.status` | From the shipper's progress: state, shipped position, policy. The local head is not shown. |
| `journal.recovery.report` | From the recovery report service of the audit. A key backup that cannot be shown to be current reads as missing. |
| `journal.snapshot.list` | Snapshots with a valid index, newest first. The size is not read (0). |
| `journal.verify` | The deep verification of the chain; "ok" only when nothing was found and nothing was left unexamined. |
| `journal.audit.last` / `journal.audit.run` | From the audit history and the audit service (one run-now per hour unless the last failed). |
| `journal.policy.get` | The policy in the host's configuration. |
| `journal.policy.set` | Refused (`journal_wiring.policy_is_site_configuration`): edit the configuration and restart. |
| `journal.snapshot.take` | Refused (`journal_wiring.snapshot_take_not_hosted`): the snapshot service takes them on its schedule. |
| `journal.reship` | Refused (`journal_wiring.reship_not_hosted`): the shipper resumes by itself. |
| `journal.rebuild.plan` / `journal.rebuild.run` | Refused (`journal_wiring.rebuild_is_offline`): a rebuild runs offline on the node. |
| `journal.reader.*` | Refused (`journal_wiring.reader_grants_not_wired`): grant readers through the access routes. |
| `journal.subject.erase` | Refused (`journal_wiring.erase_needs_caller`): use the CLI on the node. |

Every refusal is a code of `journal_wiring.*` in the error catalogue. Results carry counts,
positions and codes, never a key, a path, a token or a record.
