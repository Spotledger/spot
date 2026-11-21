Part of [Running Spot](../running-spot.md).

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
extra_chains = ["ai", "quarantine"]  # the default: also verify the AI and quarantine trails; [] turns it off
```

Unknown keys are refused, and so is an unknown name in `extra_chains` (the error names it). `anchor_path` is required when `enabled = true` and must be **outside the database role's reach**: a file the server process can append to but a database login cannot touch. The service needs two ports from the module that owns the audit keys: `AuditKeys` (the signer and the key ring) and `AuditAccess` (the permission check); a site that has not provided them refuses to start with the service enabled. The service is **optional**: a failure degrades the site and never stops it.

## What it does

| Pass | When | What |
| --- | --- | --- |
| incremental | hourly | every lane (and every extra chain) from its verified head; then the newest anchor against the stored heads; then exactly-once (the newest entries of each lane against the appender's event index) |
| full | in slices, resumable | the whole chain from entry 1 (and each extra chain's own walk); the cursor (lane, next entry) is saved after every slice, so a restart continues; only a whole clean cycle clears a degraded state |
| anchor | hourly | the signed head of every lane appended as one JSON line to `anchor_path`; never written while degraded |

## The AI and quarantine trails (extra chains)

Two more tamper-evident chains live on the same store as the document lanes, and the watchman verifies them on the same schedule:

| Name | Scope | Written by |
| --- | --- | --- |
| `ai` | `ai:<site>` | the AI audit service ([`ai_audit`](ai_audit.md)) |
| `quarantine` | `quarantine:<site>` | the quarantine decisions (an item released, retried or discarded) |

The incremental pass verifies each from its own verified head and the nightly full pass walks each from entry 1 in slices of `full_slice_entries`, with its own saved cursor (the state file gains `extra_verified`, `extra_cursor` and `extra_cycle_failed`; a state file from before them reads as "not started"). A chain with no entries is clean: a site that never used it has nothing to verify. A mismatch degrades the service exactly like a lane's, with the readiness text `degraded: <kind> in the <name> chain entry <i>` (never entry content), the same `spot::audit_verify` error event (it also carries `chain`) and the chain's verified head stops advancing. Only that chain's own whole clean full walk clears it; a document-lane cycle ending clean does not. While the service is degraded for any reason no anchor is written.

**What this does and does not prove.** These two chains have no signer, so there is **no anchor and no signed head** for them. They are verified for **internal consistency** only: each entry's hash, its link to the entry before it and the sequence. Without an anchor outside the database, a wholesale rewrite of the tail of one of these chains, or a rollback to an earlier head that agrees with itself, is **not detectable by this service**. The audit routes (`verify_now`, the entry views and the export) stay on the document lanes.

## A mismatch

The service goes **degraded**: the site stays ready, the readiness text reads `degraded: <kind> at lane <n> entry <i>`, and one `error` event on the `spot::audit_verify` target (the high-priority alert) carries the lane, the entry index and the kind, never entry content. The verified head stops advancing. Kinds: `audit.hash_mismatch`, `audit.link_broken`, `audit.seq_gap`, `audit.duplicate`, `audit.checkpoint_ahead_of_chain`, `audit.bad_checkpoint_signature`, `audit.unknown_key`, `audit_verify.anchor_mismatch` (a rolled-back head, a rewritten entry, a forged anchor) and `audit_verify.exactly_once`.

What to do: keep the database as it is, compare the anchor file with the chain, find the first entry that differs and restore it from a backup. Never delete or rewrite entries by hand.

## The read model

`verify_now`, `entries` (a page of at most 100 masked entries, by time window and cursor) and `export` (a signed bundle: one masked entry per line, then a trailer with the key id and the signature over every byte before it). Each call asks `AuditAccess` first and fails closed; each is bounded and writes an access record, refusals included. A view holds the lane, index, kind, actor, subject DocType and time, never a payload. The routes `security_audit_chain_verify`, `security_audit_entries` and `security_audit_export` call them once a route port exists.
