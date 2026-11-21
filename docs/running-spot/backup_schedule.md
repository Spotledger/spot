Part of [Running Spot](../running-spot.md).

# Scheduled backups: `[services.backup_schedule]`

`spot serve` takes a backup on a schedule when you turn this service on. It does what
`spot backup run` does (see [backup](backup.md)) and adds the schedule, the lease, the alerts and
the metrics around it. It is **off by default** and **optional**: a failing backup is loud, but it
never stops the site.

```toml
[services.backup_schedule]
enabled = true
cron = "0 0 2 * * *"          # six fields: sec min hour day month weekday; daily at 02:00
timezone = "Asia/Karachi"     # the zone the cron is read in (default UTC)
target = "usb"                # a name from [backup.targets]; absent = default_target, or the only one
kind = "logical"              # the only kind; "stopped" is refused
verify_full_every = 7         # restore every 7th backup into a scratch database (0 = never)
allow_unencrypted = false     # a site without [services.secrets] writes nothing unless this is true
tick_seconds = 30             # how often the schedule is looked at (at least 5)

[services.backup_schedule.keep]   # optional overrides of [backup.retention]
keep_daily = 7
keep_weekly = 4
keep_monthly = 12
```

An unknown key, a cron that does not parse or a zone that does not exist stops the site from
starting with the key named. The target comes from `common.toml`, never from this table's own
path, and the same safety rules apply as for the command (not inside the site data, not a
symbolic link, not the PostgreSQL data directory).

## How a slot becomes one backup

1. On every tick the service asks the schedule for the **latest due slot of the last 25 hours**
   and submits it as a `spot_backup:backup` job. The job's idempotency key is the slot's
   deduplication key, so a second tick, a restart or a second node submits the same slot and gets
   the same job: **one slot is one backup**.
2. A node that was down at 02:00 and starts at 07:00 runs the missed backup once, not once per
   missed slot.
3. The job carries the tenant, the schedule name and the slot. **Never a path or a credential.** A
   job that names another tenant is refused (`backup.tenant_mismatch`): the service is built for
   one site.
4. The job handler runs on the node the job worker gives the job to (the worker itself runs only
   on the node that holds the jobs lease), and takes the site's **backup lease**
   (`outbox:<site>:backup`) while it works. A node that cannot take it (another node is backing
   up) retries the job later.
5. The backup is produced and verified in a private work directory inside the target, then written
   with a temporary name and an atomic rename. The package is `0600`.

A node that **loses the lease** mid-backup, or sees the site **stop**, abandons the run within one
renew interval (5 seconds): the work directory removes itself, so no partial package is left, and
the job is retried. Stopping the site waits for that, within the site's shutdown grace.

## Failure and readiness

A failed run (the drive is absent, the target is unwritable, the key is missing, verification
failed) raises a health condition and an alert, counts a failure, and leaves every earlier backup
alone: the last verified backup is never removed by a failing run. An absent drive or a held lock
retries; a damaged package or a missing key does not.

After **three failures in a row** the service reports `degraded`
(`BackupScheduleService::readiness_text`, a port a later module or tool can read); the first
success resets it. Retention runs inside every successful run.

## Metrics

| Metric | Meaning |
|---|---|
| `spot_backup_last_success_timestamp_seconds` | Unix time of the last successful backup |
| `spot_backup_last_duration_seconds` | how long the last run took |
| `spot_backup_failures_total` | failed runs since the process started |
| `spot_backup_newest_verified_age_seconds` | age of the newest verified backup this process made (absent until there is one) |

Alert on the age, not only on the failure count: a schedule that stopped firing shows as an age
that keeps growing.

## What recovery point this gives

The recovery point is how much work you can lose, and it is the schedule's interval, nothing
better:

| Tier | Schedule | Recovery point |
|---|---|---|
| S (desktop, one machine) | daily to a local folder or drive | **24 hours** |
| S with a cloud target and a connection | hourly (`cron = "0 0 * * * *"`) | **1 hour** (cloud targets arrive with WP-PRD-04.T5) |
| M | every 15 minutes | **15 minutes needs WAL archiving, which this service does not provide.** A logical dump every 15 minutes is possible but heavy; do not promise a recovery point this service cannot keep |

## Limits, stated

- Only **logical** backups are scheduled; a stopped-instance backup needs the bundled PostgreSQL
  layout.
- `verify_full_every` restores into a scratch database and runs the shipped snapshot checks; the
  L7 invariants are not run (the same limit as `spot backup verify --full`).
- A cloud target is refused until WP-PRD-04.T5 (the service starts degraded and says so).
- The readiness text is read through the service's port; the host's `/api/health/ready` does not
  list it yet.
- The schema version in the manifest is read from `spot_meta.catalog_version`; a site that never
  recorded one writes `0`, which a restore treats as unverified.
