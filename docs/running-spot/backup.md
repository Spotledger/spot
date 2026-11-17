Part of [Running Spot](../running-spot.md).

# `spot backup`: run, list, verify and prune a site's backups

**A backup you have not verified is a hope.** `spot backup run` writes and checks a backup;
`spot backup verify --full` proves it can be restored. Restoring is `spot restore`
(WP-PRD-04.T3); this page ends at the pointer to it.

```
spot backup run    --site S [--target NAME] [--kind logical] [--full] [--allow-unencrypted]
spot backup list   --site S [--target NAME]
spot backup verify --site S [--target NAME] [--id ID] [--full]
spot backup prune  --site S [--target NAME] [--apply]
```

Exit codes: `0` done, `1` usage, `2` refused (nothing changed), `3` failed.

## Targets

A target is a named table in `common.toml`. The command line never carries a path: it carries
only the target's name.

```toml
[backup]
default_target = "usb"        # used when --target is not given (or the only target)
verify = "quick"              # "full" makes every run restore into a scratch database

[backup.retention]
keep_daily = 7                # the grandfather-father-son window
keep_weekly = 4
keep_monthly = 12
keep_last = 1                 # the newest N backups are never removed (at least 1)

[backup.targets.usb]
kind = "folder"
path = "/mnt/backup/spot"     # absolute; the directory must exist
```

`kind = "cloud"` parses but `spot backup` refuses it until WP-PRD-04.T5.

A target is **refused** (`backup.target_unsafe`, exit 2) when its real path is, or holds, the
site data (`<root>/sites`), an app directory, or the PostgreSQL data directory (`PGDATA`, and the
server's own `data_directory` when the database role may read that setting), or when the final
component is a symbolic link. A target that does not exist or that this user cannot write to is
refused with `backup.target_unavailable` before anything is written. Put the folder on another
disk than the live data and make it owner-only (`chmod 700`): a backup holds the tenant's data.

## What a run does

1. Takes the audit chain (`backup_run_started`; without the chain nothing runs) and the target's
   lock (`ledger.lock`, so a second run is refused with `backup.ledger_locked`).
2. Dumps the site's database with `pg_dump`. Where the client tools are is decided by one order:
   `[backup] pg_bin_dir`, then the `SPOT_PG_BIN` environment variable, then the first `pg_dump` on
   `PATH` (an empty or relative `PATH` entry is skipped, never read as the current directory).
   The first source that is set but unusable is an error; it never falls through to the next, and
   the environment never overrides the config silently. The run prints which source won on stderr
   (`pg_dump: <dir> (config)`). The directory must be absolute, exist, hold `pg_dump`, and (Unix)
   neither it, after following any link, nor `pg_dump` may be world-writable or owned by anyone but
   root or the user running the command (refused with `backup.target_unsafe`). The key is checked
   when the configuration loads: not empty, absolute, no `..`, no NUL. The password reaches
   `pg_dump` through its environment, never its command line.
3. Seals the dump with the site's key provider (see below), writes the package under a temporary
   name, syncs and renames it, so an interrupted run leaves no half package, and sets its mode to
   `0600`.
4. Verifies it. `quick` checks the stored bytes' digest, size and header. `--full` also restores
   the dump into a scratch database, runs the snapshot checks (catalogue version, system-table
   counts) and **drops the scratch database again**; a scratch that cannot be dropped fails the
   run and names the database, because it holds a copy of the data.
5. Records the result in `ledger.toml` beside the packages, applies retention, and prints one line:

```
backup ok id=acme-main-3 bytes=1048576 digest=<sha-256> duration_ms=812 verify=quick encrypted target=usb checks=header,digest,size,file_count
```

A failure exits `3`, prints the stable code (`backup.verify_failed`, `backup.io`,
`backup.ledger_write_failed`, ...) and never deletes an earlier backup. **A ledger write failure
is a failed backup** (the package may exist, but nothing counts it as verified). A package that
fails `--full` verification is kept, marked unverified, so you can look at it.

## Stopped-instance backups (`--kind stopped`)

`spot backup run --kind stopped --site SITE` copies the **data directory of the bundled
PostgreSQL** file by file (with a SHA-256 for every file) instead of dumping the database.

- **When to use it.** A very large site where a dump is too slow to take or restore, or when you
  want the exact files. The copy restores quickly because it is the data directory itself. For
  everything else use the logical kind.
- **Downtime.** The instance must be stopped for the copy, so the site is unavailable for the whole
  run. The command prints `stopped-kind backup: service unavailable for the duration` before it
  starts, and the provider starts the instance again when the copy ends, even when the copy fails.
  It never copies the files of a running instance: when the bundled server is up the command
  refuses (`backup.instance_running`, exit 2) and writes nothing; stop the service, then run it.
- **Only the bundled server.** The data directory is never taken from the command line. The
  command uses the bundled layout under the host root (`pgsql/bin`, `pgsql/data`, `logs`) and only
  when the site's `admin_url` is that server (loopback address, port 5433). A site on an external
  PostgreSQL server is refused with `backup.unsupported` ("this site does not use the bundled
  PostgreSQL server"); use the logical kind.
- **Target safety.** The rules of a folder target apply, and a target inside the data, binaries or
  log directory of the bundled server (or holding one of them) is refused with
  `backup.target_unsafe`.
- **Encryption, ledger, atomic write, mode 0600.** The same as the logical kind; a site without a
  key provider needs `--allow-unencrypted`.
- **Verification.** Quick always (digest, size and header), and with `--full` every copied file is
  checked against its recorded checksum. A data-directory copy is not restored into a scratch
  database or started, and the verdict says only that. `spot backup verify --full` of such a backup
  does the same.
- **Audit.** The audit chain lives in the site's database, which is down during the copy. The run is
  recorded in the chain when the instance is back; if that is not possible the run reports
  `backup.audit_unavailable` and exits 3 although the package was written.

**Known limit:** the `schema_version` in a manifest is a placeholder (1), not read from the site
(WP-PRD-04.T10 will). A restore preflight (WP-PRD-04.T3) must treat it as unverified.

**Known limit:** `keep_last` is enforced by `prune` only; `run`'s own retention applies the
daily/weekly/monthly numbers (WP-PRD-04.T11 adds `keep_last` to the library policy).

## What is and is not encrypted

If the host has `[services.secrets]` (see [the secrets service](secrets.md)), the payload is
encrypted with XChaCha20-Poly1305 under a fresh data key per package, and that key is wrapped
under the site's current key version. The output says `encrypted`. **Back the key file up apart
from the backups: a backup without its key file cannot be opened.**

Without `[services.secrets]` (or with an unusable key file) `run` is **refused**
(`backup.encryption_unavailable`, exit 2, nothing written). `--allow-unencrypted` overrides this
explicitly: the package is clear text, the output line says `unencrypted`, the manifest records no
key, and the audit chain gets a `backup_run_unencrypted_override` record. The manifest (tenant, versions, sizes, digest, the key
id and version) is never encrypted. No key, password or connection string appears in any output,
error or audit record.

## `list`, `verify` and `prune`

`list` shows the site's backups, newest first, with `verified` or `unverified`. `verify` checks one
backup (the newest unless `--id`), records the verdict in the ledger and the audit chain, and on a
failure prints an `ALERT` line and exits `3`. A pass never prints a bare "verified": `--full` prints and records `verified: snapshot checks + L7 invariants (shipped_checks,capture_invariant,identity_chain)` (a stopped copy, which is not restored into a database, keeps `verified: snapshot checks only (L7 invariants not run)`), a quick check `verified: digest, size and header only (nothing was restored)`; an id is a plain token (`[A-Za-z0-9._-]`), never a
path, and a symbolic link is never followed. `--full` needs the PostgreSQL client tools and a
writable target (the scratch restore unpacks into a private directory inside it); an encrypted
backup also needs the site's key file.

`prune` prints every backup retention keeps (with the reason) and every one it would remove, and
removes them only with `--apply`. It never removes the newest `keep_last` backups or the last
verified one, and refuses to remove anything while no backup is verified.

## Restoring

`spot restore` (WP-PRD-04.T3) restores a package into a site. Verify a backup with
`spot backup verify --full` before you need it.

## What `--full` checks (L7 invariants)

`--full` restores the whole backup into a scratch database, so it needs as much free disk as the data and takes about as long as a restore. On the scratch copy only, read-only, it runs:

- the shipped snapshot checks (catalogue version, system-table counts);
- the **journal capture invariant**: every change in the change feed has its journal event, from the start, with no lag; any settled finding fails the verification;
- the **identity audit chain**: every entry links to the one before it, in sequence, as `spot audit verify`.

The report names checks, counts and stable codes, never a row or a secret. A check that cannot run is a failure, not a pass. **If an invariant fails the backup must not be used for a restore**: it is kept for inspection, marked unverified in the ledger (so it is never counted as the last good backup), the run exits `3` and an alert is raised. The scratch database is dropped on every path. The other L7 checks (journal chain, gapless naming, ledger balance, idempotency) have no entry point that runs on a database alone yet and are not part of `--full`.

## Targets: folder, removable drive, cloud

Name a target in `[backup.targets.<name>]` and select it with `--target <name>` (or `default_target`).

```toml
[backup.targets.main]
kind = "folder"
path = "/srv/backups"

[backup.targets.usb]
kind = "removable"
volume = "label:SPOT-BACKUP"     # or "uuid:ABCD-1234"; never a drive letter or a path
subdir = "spot-backups"          # optional, relative; this is the default

[backup.targets.offsite]
kind = "cloud"
endpoint = "https://s3.example.com"
key_id = "AKIA..."               # an identifier, never key material
credential_ref = "secret:offsite-key"   # a secret reference, never the secret
prefix = "acme"                  # optional
max_kbps = 2048                  # optional pacing
quota_bytes = 107374182400       # optional ceiling
```

- **removable.** The drive is found by label or UUID through the platform's device list. It must be removable and must not share a disk with the system (`backup.target_unsafe`); two drives with one label are `backup.target_ambiguous` (name it by UUID). If the drive is **not plugged in or not mounted**, the run is refused with `backup.target_absent`, nothing is written, and the next scheduled slot tries again. After a successful run the command prints how to eject the drive. There is no `insecure` option and no unknown key is accepted.
- **cloud.** Needs an S3-compatible connector and the tenant key that the host supplies; a host without them refuses the target with `backup.target_unavailable` (the message names no endpoint, key or credential). A real connector is a separate decision (HTTP stack); until then the adapter is exercised with an in-memory store in tests.
