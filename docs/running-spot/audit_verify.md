# Audit verification (`services/audit_verify.rs`)

The hosted watchman of the audit chain (WP-SEC-06.T8). It verifies the chain of every lane of the site on a schedule, anchors the signed heads outside the database, degrades the audit service on any mismatch and says so loudly. It never fixes, rewrites or skips an entry.

## Turn it on

`common.toml`, one table:

```toml
[services.audit_verify]
enabled = true
lanes = 4                    # the document audit's lane count
epoch = 1                    # its lane epoch
incremental_seconds = 3600   # new entries since the verified head, hourly
full_seconds = 600           # one slice of the nightly full pass every ten minutes
full_slice_entries = 10000   # entries per slice
anchor_seconds = 3600        # a signed head every hour
anchor_path = "/var/lib/spot/audit/anchors.jsonl"
# state_path = "..."         # defaults to <anchor_path>.state
```

Unknown keys are refused. `anchor_path` is required when `enabled = true` and must be **outside the database role's reach**: a file the server process can append to but a database login cannot touch. The service needs two ports from the module that owns the audit keys: `AuditKeys` (the signer and the key ring) and `AuditAccess` (the permission check); a site that has not provided them refuses to start with the service enabled. The service is **optional**: a failure degrades the site and never stops it.

## What it does

| Pass | When | What |
| --- | --- | --- |
| incremental | hourly | every lane from its verified head; then the newest anchor against the stored heads; then exactly-once (the newest entries of each lane against the appender's event index) |
| full | in slices, resumable | the whole chain from entry 1; the cursor (lane, next entry) is saved after every slice, so a restart continues; only a whole clean cycle clears a degraded state |
| anchor | hourly | the signed head of every lane appended as one JSON line to `anchor_path`; never written while degraded |

## A mismatch

The service goes **degraded**: the site stays ready, the readiness text reads `degraded: <kind> at lane <n> entry <i>`, and one `error` event on the `spot::audit_verify` target (the high-priority alert) carries the lane, the entry index and the kind, never entry content. The verified head stops advancing. Kinds: `audit.hash_mismatch`, `audit.link_broken`, `audit.seq_gap`, `audit.duplicate`, `audit.checkpoint_ahead_of_chain`, `audit.bad_checkpoint_signature`, `audit.unknown_key`, `audit_verify.anchor_mismatch` (a rolled-back head, a rewritten entry, a forged anchor) and `audit_verify.exactly_once`.

What to do: keep the database as it is, compare the anchor file with the chain, find the first entry that differs and restore it from a backup. Never delete or rewrite entries by hand.

## The read model

`verify_now`, `entries` (a page of at most 100 masked entries, by time window and cursor) and `export` (a signed bundle: one masked entry per line, then a trailer with the key id and the signature over every byte before it). Each call asks `AuditAccess` first and fails closed; each is bounded and writes an access record, refusals included. A view holds the lane, index, kind, actor, subject DocType and time, never a payload. The routes `security_audit_chain_verify`, `security_audit_entries` and `security_audit_export` call them once a route port exists.
