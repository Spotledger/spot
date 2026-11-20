Part of [Running Spot](../running-spot.md).

# The fleet-loop test

`cargo test -p spot-cli --test fleet_loop` runs the real control-plane modules (health, licensing and,
with PostgreSQL, usage) and the real tenant-host emitters (heartbeat, licence client, metering)
against each other. The usage case and the usage half of the drift guard need `SPOT_TEST_PG_URL`
and print why they skip without it.

## How the rig works (`tests/fleet_loop/rig.rs`)

- `Rig` builds the control plane with `ControlPlane::build` over in-memory rows (PostgreSQL for
  usage) and one `ManualClock` shared by both sides. Operator calls are signed commands as the
  middleware requires.
- A `Link` is one host's connection. It sends requests to the host router carrying the certificate
  identity `CN=spot-host/<tenant>/<host>` and a fingerprint, as the host listener's middleware reads
  them. `down` cuts it like an unreachable network. The TLS handshake itself is not in this rig
  (control-plane `skeleton` tests and `spot-egress` `tls_options` tests cover it).
- Emitters run unchanged; only their shippers and fetcher are the link.

## What each case proves

a. a seeded failure shows amber or red within 60 injected seconds, recovery is green, a silent host
   goes stale and a restarted emitter is green again;
b. ten intervals offline keep the newest batches and a gap marker, then ship in order with no repeat;
c. a licence activates, the site works through Active and Grace offline, becomes Restricted (create
   denied, export allowed), then the operator's renewal is applied without a restart; a revoked
   licence is no longer served;
d. usage events arrive once, totals match, the quota answer flips, a resent batch counts nothing;
e. another tenant's host cannot name or read a tenant, a missing tenant answers like a foreign one,
   and a revoked certificate is refused;
drift guard: every endpoint an emitter calls is served with the same method.
