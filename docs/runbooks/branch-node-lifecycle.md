# Runbook: the lifecycle of a branch node

How to enrol a branch (edge) node, watch it catch up, suspend and resume it, what happens when a
node is restored from a backup, how to rotate or revoke its certificate, handle quarantined
records and retire it. Background: ADR-062 to ADR-066. Commands are exactly what the product
renders; `example-` names are placeholders. Related pages: [node roles](../running-spot/node_role.md),
[conditions](../running-spot/conditions.md), [journal recovery](journal-recovery.md),
[the runbook index](README.md).

The operations behind the steps are `sync.node.enrol`, `sync.node.suspend`, `sync.node.resume`,
`sync.node.revoke`, `sync.node.rotate_certificate`, `sync.quarantine.list` and
`sync.bootstrap.status`; the console offers the same ones.

**When to stop and call:** if a node's certificate may be in the wrong hands, revoke it first
(below), then contact the support contact in your agreement. If a red condition below does not
clear after the first action, stop and call.

## Enrol

```
spot ops sync node enrol --node example-branch-1 --role edge --partitions example_branch_a --subscription EXAMPLE-branch-preset --valid-hours 24
```

The result shows a **one-time enrolment code, once**. It is never part of the command and never in
a log. Give it to the person at the branch over a channel you trust; it expires after the valid
hours (1 to 168). On the branch machine the code goes into the node's enrolment step, which pins
the hub's certificate. Then follow the node:

```
spot ops sync node show --node example-branch-1
spot ops sync node list
```

The node is `commissioning`, then `bootstrapping`, then `active` (ADR-066). A node that never
appears after its code expired needs a new enrolment.

## Catch up

A new or long-offline node is bootstrapped from a snapshot over the network and is **stale for
every freshness class until it first confirms current** (ADR-066 section 3). There is no offline
horizon, only a cost: a branch gone for six months is bootstrapped, not replayed.

```
spot ops sync bootstrap status
spot ops sync health
```

A transfer that is cut resumes at the next chunk. If the condition `BOOTSTRAP_STALLED` (amber)
stays open, the system resumes it with back-off; look at the node's network and then at
`spot ops sync node show --node example-branch-1`.

## Suspend and resume

```
spot ops sync node suspend --node example-branch-1
spot ops sync node resume --node example-branch-1
```

A suspended node syncs nothing and keeps its data; on resume it catches up from where it stopped.
To push the stream again from its last acknowledged position:

```
spot ops sync node resend --node example-branch-1
```

To tell the node's contact person that it needs attention:

```
spot ops sync node notify_contact --node example-branch-1
```

## The node was restored from a backup

The hub detects it: the node's stream position or generation is behind what the hub recorded for
it (ADR-066 section 4). The node is **bootstrapped, never continued**: mirrors are re-sent, and the
node's stream starts a new generation, recorded by the hub as a fork. Its numbering series are
advanced beyond the highest number the hub has seen (`SERIES_ADVANCE`), so numbers are not reused.

What is lost: only what the node created **after the backup and never synced**. The hub cannot
know about it, so say so to the branch: those records must be re-entered. Check the node:

```
spot ops sync node show --node example-branch-1
spot ops sync bootstrap status
```

If instead the **hub** was restored from an older backup, a node may hold records newer than the
hub's. The hub never overwrites them: `NODE_AHEAD_OF_OWNER` (red) opens and a person decides. See
the table below.

## Rotate a certificate

Rotate before the old one expires (`CERT_EXPIRING` warns, and the system renews it automatically
when the node is connected). To rotate by hand:

```
spot ops sync node rotate_certificate --node example-branch-1
```

The old and the new certificate overlap for a window, so the node keeps syncing while it
installs the new one. Check the expiry with `sync node show`.

## Revoke a stolen node

Revoke at once. The node's certificate is refused immediately and it must be enrolled again; this
cannot be undone. The confirmation must equal the node's name.

```
spot ops sync node revoke --node example-branch-1 --confirm example-branch-1
```

The node can no longer send or receive. Documents it created offline and never sent are **not**
recoverable from the hub: decide with the branch whether they must be re-entered, and treat the
stolen machine's data as exposed.

## Quarantine

A record that cannot be applied is held for a person (`QUARANTINED`, red). The list shows reason
codes and no record content.

```
spot ops sync quarantine list
spot ops sync quarantine retry --item EXAMPLE-item-1
spot ops sync quarantine discard --item EXAMPLE-item-1 --reason-code duplicate_record --confirm EXAMPLE-item-1
```

Retry first. A discard is permanent and needs the discard permission, a reason code and the item
id as confirmation; the reason is recorded.

## Retire a node

A retired branch is revoked: use the revoke command above after its last data has synced
(`sync node show` shows the node current, and `sync quarantine list` is empty for it). There is no
separate retire operation.

## Conditions you may see

Read from the conditions catalogue (`spot-sync-conditions`); a test fails if a Red condition is
missing from this table or a name here is not in the catalogue. "First action" is the first step,
not the whole cure.

| Condition | Severity | Meaning | First action |
| --- | --- | --- | --- |
| `NODE_SILENT` | amber | a node has not been heard from; red at the staleness limit | `sync node show`, then `sync node notify_contact` |
| `LAG_HIGH` | amber | a node is behind; catch-up is prioritised automatically | wait; look only if it keeps growing |
| `OBLIGATION_OVERDUE` | amber | a mandatory obligation passed its deadline | `sync node notify_contact`, `sync node resend` |
| `MISSING_DEP` | amber | a record's dependency is missing; pull-on-miss is running | wait; `sync node resend` if it stays |
| `SCHEMA_HOLD` | amber | records held for version skew | upgrade the node; they release automatically |
| `NODE_TOO_OLD` | red | the node runs a version the hub no longer accepts | upgrade the branch; `sync node suspend` meanwhile |
| `SCHEMA_RETIRE_BLOCKED` | amber | retiring a version waits on nodes | upgrade the listed nodes |
| `QUARANTINED` | red | a record is held for a person | `sync quarantine list` |
| `PROJECTOR_FAILED` | red | a derived-data projector failed after a retry (an app defect) | call support with the condition's id |
| `DIVERGENCE` | amber | a mirror differs from its owner; repaired automatically | wait; red after 24 hours |
| `NODE_AHEAD_OF_OWNER` | red | a node holds newer data than the restored owner | a person reviews and approves recovery; do not overwrite |
| `CLOCK_SKEW` | amber | a node's clock is off | fix the node's clock |
| `STAMP_VIOLATION` | amber | a record's stamp is invalid; a finding is queued | review the finding |
| `ADOPTION_PENDING` | amber | an adoption request has waited too long | bring the branch online and approve |
| `EXTRAS_GROWTH` | info | extras are growing on a node | none |
| `CLOSURE_TOO_LARGE` | red | a record's dependency closure passed the hard limit and is refused | a metadata design problem: call support |
| `BOOTSTRAP_STALLED` | amber | a node's bootstrap stopped; it resumes with back-off | `sync bootstrap status` |
| `SERIES_LOW` | amber | a node's number range is running low | connect the node; the next range is allocated |
| `SERIES_ADVANCE` | info | numbering was advanced after a restore | none |
| `HEAL_BUDGET_EXHAUSTED` | red | automatic repairs stopped | a person must look; call support |
| `CERT_EXPIRING` | amber | a certificate expires soon; renewed when connected | connect the node, or `sync node rotate_certificate` |
| `RPO_BREACH` | red | the journal's durability policy is not being met | `journal reship`, then `journal status` |
| `JOURNAL_GAP` | red | a journal segment, chunk or manifest is missing or the chain is broken | `journal verify`; call support |
| `JOURNAL_AUDIT_MISMATCH` | red | a scratch rebuild differs from the live data | stop and call; do not rebuild |
| `CAPTURE_MISMATCH` | red | a change without a captured state: a write-path defect | call support |
| `KEY_BACKUP_MISSING` | red | the journal key backup is absent or unverified | restore the key backup; `journal recovery report` |
| `OBLIGATION_DUE_SOON` | info | an obligation is due soon | none |
| `OBLIGATION_RED` | red | an obligation is past its grace | `sync node notify_contact`, `sync node resend` |
