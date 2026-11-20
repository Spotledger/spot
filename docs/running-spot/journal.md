Part of [Running Spot](../running-spot.md).

# The journal service (`[services.journal]`)

Ships a site's outbox to an off-site journal: signed manifest, sealed segments and, under `remote-sync`, a tail chunk for every batch. The journal is the outbox stream, sealed; there is no second pipeline (ADR-061 section 1). One shipper task runs per site and only on the node that holds the site's writer lease.

For the design, the outbox barrier and how to recover from a broken chain, read journal hosting (the project process notes). This page is what an operator configures and sees.

## Turning it on

The journal is **off** until the host has a target. With no `[services.journal]` table (or no `target`) the site serves, readiness says `journal: off (no off-site history)` and the platform's standing warning applies: there is no history outside the database.

```toml
# common.toml (the whole host); every site of the host ships to its own prefix of the target.
[services.secrets]
kek_file = "/etc/spot/keys/{site}.json"   # the journal keys are derived from this key provider

[services.journal]
policy = "remote-sync"        # local | replica-sync | remote-sync (default remote-sync)
on_unreachable = "degrade"    # degrade (default) | halt
region = "eu-1"               # recorded for the operator
seal_max_mib = 64             # seal at this size (default 64)
seal_max_seconds = 300        # or when the open segment is this old (default 300)
batch_max_entries = 2000
ship_interval_ms = 250
rpo_grace_seconds = 900       # outage length before RPO_BREACH is raised (default 900)
verify_every_seconds = 600    # chain verification when nothing sealed (default 600)
lease_seconds = 30            # writer lease time to live (default 30)

[services.journal.target]
kind = "folder"               # the only provider today; JournalStore is the seam for S3 or Azure
path = "/srv/spot-journal"    # must exist, be writable, and be outside <root>/sites and the app directories

# Optional, per site:
[services.journal.sites.acme]
policy = "local"
# enabled = false             # turns the journal off for this one site
```

Unknown keys are refused at start. Run `spot migrate` first: the shipper's state table (`spot_journal_ship_state`) is created by it, never by the serving host. Without it the journal service starts degraded and says to run `spot migrate`.

## Policies

| `policy` | Durability is | Watermark moves to |
| --- | --- | --- |
| `remote-sync` | a tail chunk in the target, written before the batch is accepted | the batch's last position |
| `replica-sync` | a synchronous standby in the database layer | the last sealed segment |
| `local` | the local commit | the last sealed segment |

`on_unreachable` decides what a target outage means. `degrade` keeps trading, retries with backoff (never more than 30 seconds), keeps every entry in the outbox, and raises `RPO_BREACH` once the outage outlasts `rpo_grace_seconds`. `halt` (with `remote-sync`) makes the service **required** and, with the acknowledgement gate (WP-RTB-01.T4), refuses writes until the target is back. Under every other combination the service is optional: it must not stop trading.

## What you see

Readiness text of the journal (`JournalHandle::readiness_text`):

- `journal: remote-sync ok (lag 0s, shipped g1:s20931)`
- `journal: degraded (not shipping for 41s, RPO risk)`
- `journal: remote-sync standby (another node ships)`
- `journal: stopped (<code>), needs a person` (a broken chain: the shipper never writes on top of one)
- `journal: off (no off-site history)`

Conditions: `RPO_BREACH` (red) when the shipper has been failing longer than the grace, `JOURNAL_GAP` (red) for a damaged chain or a hole in the outbox, `KEY_BACKUP_MISSING` (red) when the key provider reports no verified backup of the journal key.

Error codes are `journal_ship.*` (see `crates/framework/spot-errors/catalogue/journal_ship.toml`).

## Safety rules the host enforces

- The target is canonicalised once at start. It must exist, be a writable directory and not be inside the site data (`<root>/sites`) or an application directory (`journal_ship.target_unsafe`).
- Every object key comes from the journal layout and starts with the site's own prefix; the store handed to a site refuses any other prefix, so one site on a shared folder cannot read, list or overwrite another's.
- The manifest signing key and the tail-chunk keys come from the site's key provider, never from a file of the journal's own, and never appear in a log, an error, readiness or a metric label.
- Personal fields are sealed before the relay sees a line. Without a personal-field protector (WP-RTB-01.T8) lines ship as they are and the host logs a warning at start.
