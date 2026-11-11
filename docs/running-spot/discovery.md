Part of [Running Spot](../running-spot.md).

# LAN discovery: advertising this server and finding one

Spotledger servers can find each other (and be found by a client) on a LAN without anyone typing
an address: an optional mDNS advertisement under `_spotledger._tcp.local.`, and `spot discover`
on the other side. Neither one touches the database or authenticates anyone (ADR-070): discovery
only answers "which server, at which address, speaking which protocol" — nothing about who may
sign in.

## What the server advertises

With `[services.discovery] enabled = true`, `spot serve` advertises a **deliberately minimal**
record, and nothing else:

| Field | Meaning |
| --- | --- |
| instance | the administrator's chosen name (`instance_name`) — this, never the site name, is what a client remembers |
| port | the application port |
| pv | the discovery protocol version |
| tls | `off`, `self-signed` or `provided` |
| fp | the TLS key fingerprint prefix, only when `tls` is not `off` |

There is no field for a company name, a list of the sites this host serves, or a user count, and
there cannot be one by accident: `Advertisement::to_txt` (`spot-discovery`) only ever emits the
keys above (plus an always-empty `ver` key that `spot-discovery` still emits; the product version is deliberately not advertised), and `tests/discovery.rs`'s `only_the_allowed_fields_are_ever_advertised` checks the
wire bytes against that exact list on every change.

```toml
[services.discovery]
enabled = true
instance_name = "shop-floor-1"   # 1-63 ASCII letters, digits or hyphens
port = 8443                      # defaults to the discovery crate's documented default port
tls = "off"                      # the default; see "TLS is optional" below
# tls = "self-signed"
# fingerprint = "0123456789abcdef0123456789abcdef"   # required when tls is not "off"
# interfaces = ["192.168.1.10"]  # defaults to the private-range filter on every interface
```

* **Off by default.** A desktop package turns it on through an installer property; a server a
  company runs for itself has no reason to be found on a LAN and simply leaves this table out (or
  `enabled = false`). When it is off the host registers no discovery service at all, so
  `/api/health/ready` does not show a default site as degraded.
* **Level: optional.** A multicast failure (no network, a firewall, a VPN adapter) degrades the
  `discovery` entry on `/api/health/ready`; it never stops the site (ADR-068 rule A1) and never
  retries into a loop. A manual `host:port` address is always the documented fallback for a
  network with no multicast.
* **Stops on shutdown.** The advertisement is withdrawn (an mDNS goodbye) when `spot serve` stops,
  before the process exits — the record never outlives the server that owns it.
* A bad setting (a missing `instance_name`, a `fingerprint` with `tls` off or missing with `tls`
  on, an unparsable `interfaces` entry) is a start-up error naming `services.discovery.<key>`,
  never a value.

## TLS is optional on a LAN

`tls = "off"` (the default) means plaintext, and the client-side tooling must show that
plainly — it is a warning a user sees, not a log line. `self-signed` means the first connection
pairs on the server's key fingerprint (trust-on-first-use); `provided` means a certificate the
client's system already trusts. This module only publishes what is configured; the TLS stack
itself, and the pairing exchange, are `spot-discovery`'s (see its own module docs) and later
tasks' to wire into an actual connection.

## Finding a server: `spot discover`

```bash
spot discover                  # the default 3-second scan
spot discover --timeout 500ms  # a quicker one
```

```
NAME                             ADDRESS           TLS    FINGERPRINT         TRUST
shop-floor-1                     192.168.1.42      off    -                   new
back-office                      192.168.1.7       self-signed ab12-cd34-ef56-7890 pinned
warehouse-old-key                192.168.1.9       self-signed 0f1e-2d3c-4b5a-6978 changed-warning
```

Trust state, read from a per-host trust store (`<root>/discovery-trust.json`, never shared with a
site's own data):

* **`new`** — nothing is pinned for this server yet (or it advertises `tls = off`, so there is
  nothing to pin at all).
* **`pinned`** — the stored fingerprint matches the one just advertised.
* **`changed-warning`** — a fingerprint *is* stored and it does **not** match. This is shown, it
  is never auto-trusted, and a client must not connect as if it were still the same server.

## Pinning (trust-on-first-use)

```bash
spot discover pin shop-floor-1
```

scans, prints the fingerprint the server advertises (first 16 hex characters, in groups of four)
and stores **nothing** until you confirm. Compare the fingerprint out of band (read it aloud, or
check it on the server) first. Confirm by typing `y` at the prompt, or pass `--yes` (required
when there is no terminal; without a terminal and without `--yes` the command refuses and writes
nothing). The command then looks the server up again and stores the fingerprint you were shown
only if the server still advertises exactly that one; otherwise it refuses. Pinning an
already-pinned, unchanged fingerprint is a no-op.

A **changed** fingerprint is refused:

```
discovery.fingerprint_changed: `warehouse-old-key`'s fingerprint changed since it was pinned;
verify the new fingerprint out of band, then re-run with --force to re-pin deliberately
```

Only `spot discover pin warehouse-old-key --force`, after verifying the new fingerprint through
some other channel (a phone call, the server's own console), re-pins it. A plain re-run of the
same command is never treated as the deliberate confirmation the brief requires — only `--force`
is.

## Exit codes

0 done; 1 usage (a bad `--timeout`, a missing `ID`); 2 refused (no matching server found, nothing
to pin on a `tls = off` server, or a changed fingerprint without `--force` — nothing is changed);
3 failed (multicast is unavailable on this machine, or the trust store could not be read or
written).

## What is never in a log, an error or the trust store

A TLS private key, a site name beyond the administrator's chosen instance name (unless a later
task turns on `reveal_site_names`, which this module's defaults never do), or a user count. The
trust store holds only an instance id, a fingerprint and the Unix second it was pinned.
