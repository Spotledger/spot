# Runbook: recover a site from its journal

Use this when the site's database is lost or corrupted, or when someone asks "could we recover?".
The journal (ADR-061) holds every committed event of the tenant; this page tells you how to find
out whether it can restore the site, how to restore it, and what you will have lost.

Every command below is exactly what the product renders for that operation: run it as shown, or
use the same operation from the console. Names that start `example-` or `EXAMPLE-` are
placeholders. Related pages: [the commands](../running-spot/commands.md),
[conditions](../running-spot/conditions.md), [the runbook index](README.md).

## When to use this

- The site's database server or disk is gone and a backup (or nothing) is what remains.
- A bug or an operator error corrupted data and you need the state from before it.
- The `RPO_BREACH`, `JOURNAL_GAP` or `JOURNAL_AUDIT_MISMATCH` condition is open and you must know
  whether the journal can still be trusted.

The operations behind the steps are `journal.recovery.report`, `journal.verify`,
`journal.rebuild.plan`, `journal.rebuild.run`, `journal.audit.run` and `journal.reship`; the
console offers the same ones.

**When to stop and call:** if the recovery report says the site is not recoverable, or a step
below fails twice, stop and contact the support contact in your agreement. Do not improvise.

## Before you touch anything

Read the recovery report. It changes nothing and answers "could I recover?".

```
spot ops journal recovery report
```

Read the verdict and the blockers it lists:

| What the report shows | Meaning | What to do |
| --- | --- | --- |
| journal target not configured | there is no off-site history | Stop. Only a database backup can help; recovery from the journal is not possible. |
| key backup missing or stale | without the journal key nothing in the journal is readable (ADR-061 section 9) | Stop until the key provider's backup is restored; the condition `KEY_BACKUP_MISSING` is red for the same reason. |
| last verified position is old or none | the journal has not been proven recently | Run the verify step below first. |
| excluded entities listed | some entities are not in the journal; each has its owner's reason | Plan to restore those from their own backup. |

Also read the current state:

```
spot ops journal status
spot ops journal policy get
spot ops journal snapshot list --limit 20
```

If the journal itself is suspect, check it before you rely on it:

```
spot ops journal verify
```

**When the verdict is not recoverable: stop.** Do not run a rebuild.

## Choose the target

| You want | Mode | Target |
| --- | --- | --- |
| Everything committed (normal disaster path) | `full`, or `roll-forward` over a restored database backup | `latest` |
| The state before a corruption | `point-in-time` | `at <generation>:<position>` or `before <RFC 3339 time>` |

A point-in-time rebuild **forks history**: the stream gets a new generation and the old one stays
in the journal for ever (ADR-061 section 6). Consumers see `ResyncRequired` for positions in the
abandoned generation. Choose a position from the snapshot list or the audit result, never from a
guess. The plan (next step) says whether the target can be used; do not run a rebuild whose plan
refuses it.

## Plan, then run

Plan changes nothing. It shows what a rebuild would read and apply and gives you a hash.

```
spot ops journal rebuild plan --target latest --mode full
```

For a point in time:

```
spot ops journal rebuild plan --target 'at 3:48211' --mode point-in-time
```

Check the counts against what you expect (records, segments, the snapshot it starts from). The
**run** executes only the plan whose hash you confirm. Keep the site's maintenance gate closed
until the verify step below passes.

```
spot ops journal rebuild run --target latest --mode full --confirm-plan-hash 00000000000000000000000000000000
```

(The hash above is a placeholder: use the one your plan printed.) A rebuild is a critical
operation: it needs the matching permission and is audited. If the plan hash no longer matches
(the journal moved), plan again; never copy an old hash.

## Verify

```
spot ops journal verify
spot ops journal audit run
spot ops journal audit last
```

The audit rebuilds a scratch copy from the latest snapshot plus the journal and compares it with
the live data, including the financial fingerprint and record counts. Reopen the site only when
both are clean. If the audit shows a mismatch, the condition `JOURNAL_AUDIT_MISMATCH` opens and
names the first differing record: stop and call.

## What you lose

The loss window depends on the durability policy the tenant ran (ADR-061 section 4). Check it
with `spot ops journal policy get`.

| Policy | Commit acknowledged when | What can be lost |
| --- | --- | --- |
| `local` | the local database commit is durable | the journal shipping lag, if the machine and disk are lost |
| `replica-sync` | a synchronous standby has the commit | nothing for loss of one zone, node or disk; only the journal lag for loss of a whole region or a logical disaster |
| `remote-sync` with `halt` | the entry is also durable in the journal's object store; commits are refused when the store is unreachable | nothing: zero |
| `remote-sync` with `degrade` | as above, but commits continue locally when the store is unreachable (an `RPO_BREACH` alarm is raised) | nothing, except what was committed during an unreachable period that was not yet re-shipped |

What the journal alone does not recover: key material, attachment blobs (they need the file
provider's own store), control-plane data and external effects already sent, such as e-mails and
bank files. A rebuild never sends them again (ADR-061 section 9).

## After recovery

1. Queue anything still unshipped and check the shipping state:

   ```
   spot ops journal reship
   spot ops journal status
   ```

2. **Edge nodes that were ahead of the journal** (they hold records newer than the restored
   site) are restored-from-the-future. The hub never overwrites a newer copy: the condition
   `NODE_AHEAD_OF_OWNER` opens (red) and a person reviews and approves recovery (ADR-066). See
   the [branch node runbook](branch-node-lifecycle.md).
3. Re-issue reader grants that were lost, and list what exists now:

   ```
   spot ops journal reader list
   spot ops journal reader grant --grantee EXAMPLE-auditor --days 30 --no-external
   ```

4. Check the open conditions (`spot ops sync condition list`) and close each with its own
   procedure.
