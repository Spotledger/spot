Part of [Running Spot](../running-spot.md).

# Journal and sync operations on a running site

The management operations of the journal (`journal.*`, `spot-mgmt-journal`) and of sync (`sync.*`,
`spot-mgmt-sync`) are executed on the site by two wrappers, `JournalExec` and `SyncExec`, over the
services that do the work (the snapshot service, the audit, the grant store, the enrolment store,
the quarantine and so on). The wrappers add what every operation needs and no service should repeat.

## Which site, and who

The **site is the signed command's audience**, never a parameter: the executors are looked up by
audience, so a command for site A can only reach site A's services, and an audience that is not a
site of this host is "not found" without saying whether another site exists. The **principal** is the
principal of the signed command; operations never run as `system`. A replayed command is refused by
the management API's nonce store before the executor is reached.

## Permissions (default deny)

| Family | Operations | Permission |
| --- | --- | --- |
| journal, read | status, recovery report, snapshot list, verify, audit last, policy get, rebuild plan | `journal.view` or `journal.admin` |
| journal, change | snapshot take, audit run, policy set, reship, rebuild run, reader list, grant and revoke | `journal.admin` |
| journal, erasure | `journal.subject.erase` | `journal.erase_subject` (its own) |
| sync, read | health, node list and show, conditions, quarantine list, policy, bootstrap status | `sync.view` or `sync.admin` |
| sync, change | resume, suspend, revoke, rotate certificate, resend, notify contact, quarantine retry and discard, role change | `sync.admin` |
| sync, enrolment | `sync.node.enrol` | `sync.node.enrol` or `sync.admin` |

The Tenant Administrator role holds every permission except `journal.erase_subject`. A deployment can
provide a finer permission source as a port. The families are separate: `sync.admin` cannot grant
`journal_reader`. An **external** reader's grant of more than seven days needs the administrator role.

## Rules on every call

- `journal.rebuild.run` re-plans and refuses (`plan_hash_differs`) unless the plan's hash is the one
  the person confirmed.
- `journal.rebuild.run` and `sync.role.change` need the site **stopped**: the executor never stops
  it and refuses with `mgmt_exec.site_running`. Use the offline path on the node itself (stop the site,
  then the CLI's rebuild or role entry point, the way `spot migrate` runs).
- **One destructive operation per site at a time** (`mgmt_exec.busy`).
- **An audit record for every outcome** of a state-changing operation, refusals and failures
  included (`sys_journal_admin`, `sys_sync_node_admin`, `sys_sync_decision`): who, which operation, the
  outcome, a code; never content or a secret. A destructive operation writes its record **first**: no
  record, no action. A change that happened but could not be recorded is reported as unaudited, never as
  a quiet success. Reads are not recorded when they succeed; their refusals and failures are logged
  and counted. Grants are audited by the grant service itself (`sys_journal_grant`).
- **Idempotent retry**: the same command id answers the first result for 10 minutes
  (`result_window_seconds`). An enrolment code is delivered once; the stored copy never holds it.
- **Rate limit**: 30 commands a minute per principal (`rate_per_minute`); the limiter's refusals are
  counted (`Shared::rate_limited`), not audited one by one.
- Lists are capped at 500.

## Configuration

```toml
[services.mgmt_exec]
enabled = true
rate_per_minute = 30
result_window_seconds = 600
```

The executors register when the journal and sync services provide their implementations of the
operations; otherwise nothing is registered and the management API answers "not found" for them.
