Part of [Running Spot](../running-spot.md).

# The sandbox reaper

`spot sandbox create --ttl 14d` records an expiry and `spot sandbox reap` destroys what has
expired. Nothing ran the command. A sandbox holds a masked copy of production and a database; a
forgotten one is a cost and, if a masking rule is later found weak, an exposure that outlives its
purpose. The reaper is the same pass, hosted: `spot serve` runs it on an interval.

## Turning it on

It is **off by default**. The reaper can drop databases, so only a host that is meant to own
sandboxes turns it on, on purpose:

```toml
[services.sandbox_reaper]
enabled = true
interval_seconds = 300   # 30 to 86400
max_per_pass = 5         # 1 to 50
```

Without it, `spot sandbox reap` keeps working by hand. The service is optional: it never stops a
site.

## What a pass does

1. Takes the lease `sandbox-reaper:<site>` (the cluster profile; a single node has no coordinator
   and runs without a lease, and its readiness text says `no lease (single node)`). One node of a
   cluster reaps. The lease is renewed and its fence checked before every destruction; a lost
   lease stops the pass before the next one. It is released at the end, also after an error.
2. Reads the registry: `sites/<name>/sandbox.toml` of the sandboxes **cloned from this site**. A
   site's reaper never touches another site's sandboxes.
3. Takes the expired ones (expiry strictly before now; a creation unfinished for an hour counts),
   oldest first, at most `max_per_pass`. A pass with nothing expired does one listing and no
   database work.
4. Destroys each one: its database, then its site folder (the record goes with it). It is
   **idempotent**: a half-destroyed sandbox is finished; a sandbox already gone is a success.
5. Records one result per sandbox: its id, `destroyed`, `already_gone` or `failed`, and on failure
   a reason code (`drop_failed`, `folder_not_removed`, `database_unreachable`, ...). Never a
   database name, path or connection string.

A failure does not stop the pass. A sandbox that fails **three passes in a row** is skipped for
one hour and counted as `stuck` in the readiness text.

## What it never destroys

- a database that no sandbox record names, or one that does not start `sandbox_`;
- the source site of any registered sandbox, this site, or the database of a real site;
- a sandbox whose record says `keep`, has no expiry (unless its creation never finished), or
  expires in the future;
- anything while the clock reads earlier than it did by more than the interval: a pass on a
  suspicious clock is skipped with a warning.

## Readiness

The text is `sandbox_reaper: next expiry in 3d 4h, 0 stuck`; `overdue` when something is past its
expiry and not yet reaped, and `no sandbox` when there is none. (`spot serve` logs it at start;
the readiness endpoint shows the service's level and state.)

## Extending a TTL before expiry

There is no command for it yet. Edit `expires_at` in `sites/<name>/sandbox.toml`, or create a new
sandbox. (A `keep` flag in the record is honoured by the reaper, but `spot sandbox create` does
not write it.)
