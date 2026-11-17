Part of [Running Spot](../running-spot.md).

# `spot update`: check, download, apply and roll back a signed update

An update is only ever installed when it is **signed by a key you trust, inside that key's
window, not revoked, not older than what you run, and byte-for-byte what the signed manifest
says**. None of these checks can be switched off by a setting or a flag. Before anything is
changed the data is backed up; if the new version does not come up, the old one is put back.

```
spot update check    [--channel stable|beta] [--repo DIR]
spot update download [--channel stable|beta] [--repo DIR]
spot update apply    [--channel stable|beta] [--repo DIR] [--apply]
spot update rollback [--apply]
spot update status
```

Exit codes: `0` done, `1` usage, `2` refused (nothing changed), `3` failed. Every refusal prints a
stable `updater.*` code (see the error catalogue for the remedy of each).

## What you need on disk

The updater works on a versioned install under the install root (`services.updater.install_root`,
by default the host root):

```
<root>/versions/<version>/   program files of one version (never data)
<root>/current               the active version
<root>/previous              the last good version
<root>/update-trust.toml     the trust anchor, installed with the package
```

The **trust anchor** lists the update signing keys. It is installed with the package and is never
read from the update server, so a compromised server cannot add a key:

```toml
format = "spot-update-trust"

[[keys]]
public = "<base64 Ed25519 public key>"
valid_from = 0                 # Unix seconds
valid_to = 1900000000          # optional
```

## Where updates come from

An update source has one layout, over HTTPS and in an offline folder:

```
<base>/<channel>/update.toml        the signed manifest
<base>/<channel>/update.sig         the detached signature
<base>/<channel>/revocations.toml   the signed list of revoked update keys
<base>/<channel>/<archive>          the archive the manifest names
```

- **HTTPS only.** The URL must be `https`, the certificate and name are verified against the
  platform's trusted roots, and a redirect is an error: it is never followed, so an update cannot be
  sent to plain `http` or elsewhere.
- **Offline repository.** `--repo DIR` reads the same layout from a folder (a USB stick, a share)
  and never touches the network. See the offline repository guide for how to build one.
- The manifest, signature and revocation list are verified **before** the archive is requested.
  The archive is then fetched with the size the signed manifest states as the limit, and its
  SHA-256 is checked before anything is unpacked.

## Policy: `[services.updater]`

```toml
[services.updater]
mode = "notify"                 # off | notify | auto
channel = "stable"              # stable | beta
url = "https://updates.example.com/spot"
window = "02:00-05:00"          # UTC; may cross midnight (22:00-02:00)
```

| Key | Meaning |
| --- | --- |
| `mode` | `off`: nothing. `notify` (default): look and record, change nothing. `auto`: also apply, inside `window`, when the host is idle. |
| `channel` | The channel looked at; `--channel` overrides it for one command. |
| `url` | The `https` base URL. Without it only `--repo` works. |
| `window` | The maintenance window. `spot update apply --apply` and `auto` run only inside it. Absent means any time. |
| `install_root` | The install root (default: the host root). |
| `product` | The product name a manifest must carry (default `spotledger`). |
| `schema_epoch` | The database schema epoch, until the installed version records its own. |
| `unit` | The service manager's name for the host (default `spot`). |
| `ready_addr` | `host:port` polled for `/api/health/ready` after a restart (default: the first `server.bind`). |
| `health_timeout_secs` | How long the new version has to become ready (default 60). |
| `probation_secs` | How long after a commit a failing new version still rolls back (default 300). |
| `telemetry` | `false` by default. On, a check carries only the installed version, the platform and the channel as a query; nothing else, ever. |

The service looks at most **once a day** (a failed look is retried after an hour), starting a few
minutes after the host starts at an offset that is stable per installation. The result is kept in
`update-status.toml` and shown by `spot update status`. In `auto` mode the service does not apply
the update itself, because the update restarts the very service that would run it: it starts a
separate `spot update apply --apply` process (under `systemd-run` when the host runs under
systemd). It waits while a file called `update.hold` exists under the install root, so a job or a
migration that must not be interrupted can ask for that.

## What `apply` does

Without `--apply` it prints the plan and changes nothing. With it, inside the window:

1. **Check:** verify the offer and decide (a downgrade, another product or channel, or a schema
   the installed database cannot reach is refused).
2. **Back up** every site with the same code as `spot backup run`, before anything is staged. A
   failed backup stops the update; nothing has changed.
3. **Stage** the verified files beside the running version and verify them on disk.
4. **Check the migration plan** of the new version (`spot migrate --site S`, plan only) when the
   schema changes. A plan the new version refuses (a destructive change without an approved plan)
   stops the update. Applying the migration is a separate, approved step (`spot migrate --apply`).
5. **Switch** the `current` pointer, restart the service through the platform's service manager
   (`systemctl` on Linux, the Windows Service Control Manager on Windows) and wait for readiness.
6. **Roll back automatically** if any step fails: the previous version is put back and restarted.

A power cut at any point is resumed by running `spot update apply --apply` again: the journal ends
in exactly one of the old version running and intact, or the new version running and committed.

## How a failed update looks

| You see | It means |
| --- | --- |
| `refused: updater.signature` (exit 2) | The signature does not match the manifest. Nothing was changed. |
| `updater.key_unknown`, `updater.key_expired`, `updater.key_revoked` | The signer is not in the anchor, outside its window, or revoked. |
| `updater.downgrade` | The offer is older than what runs. Use `rollback` to go back. |
| `updater.payload_digest` | The archive is not what the signed manifest says. |
| `updater.archive_escape` | The archive holds a path that would leave the staging folder. |
| `refused: outside the maintenance window` | Wait for the window, or change `window`. |
| `refused: the backup failed` | Fix the backup (`spot backup run`) first. |
| `failed at <step>: ...; rolled back` (exit 3) | The new version did not come up; the previous one serves. |
| `needs attention` | Neither the update nor its rollback finished; the journal says where things are. Run `spot update status`. |

## Rollback

`spot update rollback` switches `current` back to the version in `previous` (restart and readiness
check as above) and, if that does not become ready, puts things back as they were. **It does not
restore data.** If the update changed the schema, restore the backup the update took instead.

## What to configure at the service manager

`systemctl` must be able to stop and start the unit named by `unit`, and the unit's start command
must run the version `current` names. Under systemd, give the unit `KillMode=process`, or let
`auto` use the `systemd-run` it starts the apply with, so that stopping the unit does not stop the
update that is restarting it.
