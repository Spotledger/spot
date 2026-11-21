Part of [Running Spot](../running-spot.md).

# The scheduled journal audit and the recovery report

A journal that has never been replayed is a hope (ADR-061 section 8). The audit rebuilds the latest
snapshot plus the journal into a **scratch database**, runs the invariants, compares the record
counts, digests and financial fingerprints with the live database, measures the rebuild against the
tier's recovery time objective and raises `JOURNAL_AUDIT_MISMATCH` naming the first differing
record. The recovery report is the page that answers "could I recover?" at any moment.

## What runs

- **Schedule.** Weekly; daily for tenants on the `always` replay policy. `check_seconds` (default
  3600) is how often the schedule is examined; a tenant that is not due is skipped. One pass at a
  time per host (`max_parallel`, default 1): a scratch rebuild is heavy. A pass runs on the blocking
  pool. The platform job `platform:journal_audit` does the same pass when a schedule entry or the
  management API enqueues it.
- **Run now** (management API): one per site per hour, unless the previous run failed.
- **Results.** Table `spot_journal_audit_result`, created by `spot migrate`: when, outcome, the
  verified position, record counts, the number of differences, the rebuild time and, for a
  mismatch, the **identity** of the first differing record (entity and id) and the kind of
  difference. Never a field value. 52 weeks are kept.
- **Conditions.** A clean run clears `JOURNAL_AUDIT_MISMATCH` and `JOURNAL_AUDIT_FAILED`; a
  mismatch raises the first, a rebuild that could not finish raises the second (red); a rebuild
  slower than the tier's objective (standard 4 h, premium 1 h, `[journal] rto_hours` in the site
  file) raises the amber `RTO_EXCEEDED`. Every pass writes `sys_journal_admin` (`audit_run`).
- **Verified snapshots.** A clean run marks the snapshot it started from verified, so the rebuild
  planner trusts it.
- **The live database is never written.** The live side is one read-only snapshot.

## The scratch database and its role

The scratch copy holds a full copy of a tenant's data, so it gets a database of its own on the same
PostgreSQL server: `spot_audit_<tenant>_<runid>`, created before the run and dropped after it on
every path, with `CONNECT` revoked from `PUBLIC`. It is owned by a dedicated role, named by
`services.journal_audit.scratch_role`, that holds `CREATEDB` and nothing else.

- **The installer creates the role**, never the service. The service checks that it exists and
  that it holds only `CREATEDB`: a superuser, or a role that can create roles, replicate or bypass
  row security, is refused (`journal_audit_host.role_too_powerful`) and the audit does not start.
- **The password** is read from the environment variable `scratch_password_env` names
  (`SPOT_AUDIT_SCRATCH_PASSWORD` by default). It is never in a file, a log or an error.
- **A crash** can leave a scratch database behind; the next start sweeps the ones more than a day
  old (their comment records the creation time and the tenant, so only this site's leaks go).

## The recovery report

`RecoveryReportService::report` builds the page from real facts: the journal target and policy, the
shipper's lag, the key service (is the journal key present, is its backup confirmed), the owners'
exclusions with their reasons (`Engine::capture_exclusions()`), the audit history (the table
above), the chain and manifest check and the snapshot listing. The verdict is `Recoverable`,
`NotRecoverable` or `CannotConfirm`. A fact that cannot be read is unreadable, never a default,
and a tenant whose blob cross-check did not run cannot be confirmed.

## Configuration

```toml
[services.journal_audit]
enabled = true
scratch_role = "spot_audit_scratch"
scratch_password_env = "SPOT_AUDIT_SCRATCH_PASSWORD"
max_parallel = 1
check_seconds = 3600
```

The audit is optional: a site whose audit cannot run still trades, and readiness names the reason
(`run spot migrate`, `the scratch role is too powerful`).
