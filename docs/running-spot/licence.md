Part of [Running Spot](../running-spot.md).

# The licence client (`[services.licence]`, `spot licence`)

Every site can carry a signed licence. The client loads and verifies it **offline** (the only inputs
are the licence file, the issuer's public keys and the clock), keeps an anti-rewind clock guard,
renews from the control plane when one is configured and reachable, and answers "may this happen?"
to the rest of the host through the `LicenceGate` port.

```toml
[services.licence]
required          = false                         # true: a site without a valid licence is closed
trusted_keys_file = "/etc/spot/issuer-keys.txt"   # the issuer's public keys (below)
tenant            = ""                            # the tenant the licence must name; empty = the site name
# Online renewal (optional; leave out for a fully offline desktop):
control_plane     = "https://cp.example.net:8443"
host_cert         = "/etc/spot/host.crt"
host_key          = "/etc/spot/host.key"          # mode 0600
ca_file           = "/etc/spot/control-plane-ca.crt"
renew_check_hours = 6
```

Files live in `<root>/licence/<site>/`: `licence.txt` (the signed text form), `clock.state` (the clock
guard) and `clock.state.attempt` (the last renewal attempt and its result code). They are written
atomically, never writable by group or others (the site refuses a file that is), and are not secrets.

The trusted keys file has one issuer key per line: `<key id> <64 hex digits> <valid_from> <valid_to
or ->` (Unix seconds; `#` starts a comment). The issuer rotates by adding a key with a window;
removing a key revokes everything signed with it as far as this host is concerned.

## Activating an offline desktop from a file

```
spot licence install --site acme licence.txt
spot licence status  --site acme
```

`install` verifies the file against the trusted keys, refuses a licence for another tenant and a
**lower or equal serial of the same licence** (no downgrade), and places it atomically. A
different licence id (a new purchase) is accepted. Nothing needs the network.

## What each standing lets the user do

| Standing | Meaning | May do |
| --- | --- | --- |
| `unlicensed` | no licence file | everything when `required = false`; only renewal when `required = true` |
| `invalid` | a file is there but failed verification | as `unlicensed`, and the service reports degraded |
| `not_yet_valid` | before the licence's start | nothing but renewal |
| `active` | valid | everything the entitlements allow |
| `renewal_due` | past `renew_after` | everything, with a renewal prompt |
| `grace` | expired but inside the grace period | everything, with a warning |
| `clock_suspect` | the clock moved back more than a day | as grace, with a warning, until a renewal |
| `restricted` | past the grace (a subscription or perpetual) | read-only: open and export; no new documents, no app installs |
| `expired` | an ended trial | nothing |

`required = false` (the default) is for developer sites and open-source installs: the licence can
never block anything there. A `restricted` or `expired` licence never stops the site from answering:
the gate says "read-only" or "nothing"; the site stays up so the user can export, install a licence
or renew.

## Clocks

The clock guard remembers the highest time this install has ever seen (persisted in `clock.state`).
A clock set back by more than a day is `clock_suspect` (behaves like grace, with a warning) and
stays so across restarts until a renewal is applied and the clock passes the remembered time again.
A correction inside a day is absorbed.

## Renewal

With `control_plane` set, when the standing is `renewal_due`, `grace`, `restricted` or
`clock_suspect`, the site fetches `GET /cp/v1/host/licence` over mutual TLS every
`renew_check_hours` (the control plane must be on the site's egress allow-list; the start-up
message names a missing entry). The licence is verified against the **local** keys; one that is
older or equal in serial, for another tenant, for another licence id, or reported revoked is refused
with its stable code and changes nothing. Success replaces the file atomically and resets the
clock-guard anchor. `spot licence renew --site acme` runs one fetch now.

If the control plane is unreachable, nothing is shown to anyone: the attempt and its result code are
recorded (`spot licence status` prints them) and the site tries again later. Offline for the whole
grace period is normal.

## `spot licence status`

Prints the edition, kind, validity window, renew-after, grace end, today's standing, entitlements and
the last renewal attempt; `--json` prints one object. Exit codes follow the `spot` convention: 0
done, 1 usage, 2 refused, 3 failed. A `restricted` or `expired` licence makes `status` exit **2**, so
a script can alert; a refused `install` also exits 2 and changes nothing; an operational failure
(unreadable file, `renew` offline or unreachable) exits 3.

## The heartbeat

The `LicenceStateSource` port feeds the `licence_state` signal of the fleet heartbeat: 0 for `active`
and `renewal_due`, 1 for `grace` and `clock_suspect`, 2 for `restricted`, `expired`, `not_yet_valid`
and `invalid`; an unlicensed site sends no value.
