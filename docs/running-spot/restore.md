Part of [Running Spot](../running-spot.md).

# `spot restore`: put a backup back, safely

**Restoring over the wrong site is the failure this command exists to prevent.** A restore never
overwrites in place: the backup is restored into a *scratch* database first, checked there, and
only then swapped for the live database in one atomic step. What it replaces is kept.

```
spot restore preflight --site S --from BACKUP_ID [--target NAME] [--as-new-site NAME]
spot restore apply     --site S --from BACKUP_ID --confirm TOKEN [--target NAME]
                       [--as-new-site NAME] [--accept-unverified-schema] [--migrate-forward]
spot restore purge-quarantine --site S --older-than 7d [--apply]
```

Exit codes: `0` done, `1` usage, `2` refused (nothing changed), `3` failed. A failure after the
maintenance gate closed puts the previous database back and opens the gate again, and the single
failure line says so; if it cannot, the line says that instead.

The backup comes from a named folder target of `common.toml` (see [backup](backup.md)); the
command line never carries a path. `spot backup list` shows the ids.

## 1. Preflight

`spot restore preflight --site S --from ID` changes nothing. It prints:

- the backup id, when it was taken and its size, and whether the package verifies (digest, size
  and header: nothing is restored by this check);
- whether the target exists and exactly what would be replaced;
- the PostgreSQL major version of the backup, and an estimate of free disk (the volume that holds
  the host root; `unknown` where the platform cannot measure it);
- the **schema version** comparison (below);
- a **confirm token**, valid for 10 minutes.

The token is 256 random bits. Only its digest is stored, in `sites/<site>/state/`
(`restore-confirm.toml`, mode `0600`), together with the backup id, the target and the OS user
that asked. `apply` refuses a token that is wrong, expired, for another backup or target, issued
to another OS user, or already used. A second `preflight` replaces the first token.

Preflight refuses (exit 2, no token) when the backup belongs to another site, its package is
damaged, the PostgreSQL major differs, the apps it needs are not installed, the free disk is
short, the site is being migrated or its maintenance gate is closed, or the command runs as root
while the host root belongs to another user. It says which.

## 2. Apply

`spot restore apply --site S --from ID --confirm TOKEN` runs, in this order, and nothing else:

1. **Checks again**: the token, the package (a tampered package is refused here, before the gate
   is touched), the site's gate and journal, and the lock (`sites/<site>/state/restore.lock`: one
   restore at a time).
2. **Decrypts** a sealed backup with the site's key provider into a private file next to the
   package; it is removed on every way out.
3. **Closes the maintenance gate** (the site's gate row, so a running `spot serve` answers
   `server.maintenance`), asks the host to reload, waits for in-flight work, then **bars new
   connections to the live database and ends the rest**. The command proceeds only when it has
   proven that no other session holds the database; otherwise it opens the gate again and exits 3.
4. **Restores into a scratch database** (`spot_scratch_rs_<time>_<pid>`) with `pg_restore`, runs
   the snapshot checks and compares schema versions, **resets node-local state** on the scratch
   (the keys the backup marks node-local, leases, coordination nodes and rate rows, and a new
   outbox generation), all before the live database is touched.
5. **Swaps**: in one transaction the live database is renamed to `<db>_replaced_<seconds>` (the
   *quarantine*; connections to it are allowed so you can inspect it) and the scratch database
   takes the live name. Then the gate is opened on the restored database and the host is told to
   reload (its in-memory sessions end with the reload, so a session from before the restore does
   not work afterwards).

The audit chain records `restore_started` (before anything changes, and the restore does not run
without it) and `restore_succeeded` or `restore_failed`/`restore_refused`. No key, token,
connection string or password is written to the output or the audit record.

The restored data is checked by the snapshot checks (catalogue version, system-table counts).
**The L7 invariants are not run by this command yet**; the output says what ran.

### Schema versions

A backup records the site's schema version (`spot_meta.catalog_version`). Apply compares it with
what this build expects, and again with the version read from the **restored scratch database
itself** (a manifest that disagrees with its data is refused: `restore.schema_mismatch`).

| Backup is | Result |
|---|---|
| newer than this build | refused: `restore.too_new` |
| older | refused unless `--migrate-forward`, which migrates the restored copy (the scratch, never the live database) and reports the steps |
| `1` or `unrecorded` (a backup made before the version was recorded) | `unverified`: refused unless `--accept-unverified-schema`, which is audited; the data's own version is then compared with this build |

### Restoring into a new site

`--as-new-site NAME` restores the backup into a new database and a new site `NAME`, and needs no
existing target. It is refused when a site directory or database called `NAME` exists. The source
site is untouched. The new site has **no credentials**: set a password with `spot set-password`.

## The desktop path (USB drive or folder)

1. Plug in the drive; it is the `[backup.targets.usb]` folder of `common.toml`.
2. `spot restore preflight --site S --from <id>`, read what it replaces, copy the token.
3. `spot restore apply --site S --from <id> --confirm <token>`.
4. Open the site. The previous data is in the quarantine database until you purge it.

## The new-PC path

1. Install Spot and create the host root with the same `common.toml` (same apps, and a backup
   target that points at the drive) and a PostgreSQL of the same major version.
2. Create an empty site with the **same name** as the one in the backup and the same apps:
   `spot new-site S` (a bare site needs `spot install-app` for each app the backup had). A backup
   is only ever restored into the site it was taken from, so the name matters.
3. Plug in the drive and run the restore: `spot restore preflight --site S --from <id>`, then
   `apply`. The empty database is quarantined and the backup takes its place.
4. Choose a password with `spot set-password` if the backup predates your accounts, then start
   `spot serve`.

## What is quarantined, and for how long

The replaced database is kept as `<db>_replaced_<seconds>` (the database name is cut to 40
characters) and is marked as this site's quarantine. It keeps its data and its modes. Nothing
removes it by itself:

```
spot restore purge-quarantine --site S --older-than 7d            # lists
spot restore purge-quarantine --site S --older-than 7d --apply    # drops
```

`--older-than` is at least `24h` (`1d`): a younger quarantine is never dropped, and the command
refuses (exit 2). Only databases marked as this site's quarantine are considered.

## If a restore is killed

A kill leaves the previous database intact. While the gate is closed the live database refuses
connections, so the next `spot restore preflight` or `apply` first notices the dead restore (its
lock names a process that is gone), lets connections in again, opens the gate and says so. A
leftover scratch database (`spot_scratch_rs_*`) from the killed run holds a copy of the site data:
drop it with `DROP DATABASE`.

## Limits, stated

- Approved migration plans: the platform keeps no approved-plan store yet, so the preflight checks
  a running migration and a closed gate only.
- The target directory must be writable by the restoring user for an encrypted backup (the
  decrypted package is written next to it).
- A restore run as root is refused when the host root belongs to another user.
- Cloud targets are refused until WP-PRD-04.T5.
