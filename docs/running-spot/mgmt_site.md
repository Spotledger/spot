Part of [Running Spot](../running-spot.md).

# Site operations of the management API (`[services.mgmt_site]`)

The management API (`mgmt_api.md`) serves eighteen site operations. This module hosts them: it adds
them to the API's catalogue when the API is enabled and has no listener, port or certificate of its
own.

| Operation | Risk | Equivalent command |
|---|---|---|
| `site.status` | read | `spot backup list` and `spot migrate-hold status` |
| `site.backup.run` / `list` / `verify` | medium / read / low | `spot backup run`, `list`, `verify` |
| `site.restore.preflight` / `apply` | low / critical | `spot restore preflight`, `apply` |
| `site.maintenance.enter` / `leave` | critical / medium | `spot migrate-apply maintenance enter`, `leave` |
| `site.migrate.plan` ... `audit` (ten) | read to critical | `spot migrate-plan`, `migrate-approve`, `migrate-rehearse`, `migrate-apply`, `migrate-hold` |

## One implementation

An operation **runs the same `spot` command** you would type: the host starts its own executable with
`--root` and the command's arguments (one `--flag=value` per argument, no shell). So the API result and
the CLI result come from the same code, and a refusal of the command (exit 2) is a refusal of the API
(`mgmt.refused_by_policy`). Exit 1 is `mgmt.invalid_parameter`, anything else `mgmt.internal`.

Every result carries the equivalent command line (a secret shows as `<prompt>`), the command's output
(at most 200 lines, 400 characters each; credentials in URLs and `password=`, `key=`, `token=` fields
are replaced by `***`) and `truncated: true` when something was cut.

## Rules

- **The site.** The `site` parameter must be this host's site. A command naming another site is
  `mgmt.forbidden` and nothing runs.
- **Targets.** A backup target is a name from `common.toml` `[backup.targets]`, never a path.
- **Restore needs two steps.** `site.restore.preflight` prints a single-use `confirm_token`
  (10 minutes, bound to the backup id, the target and the OS user). `site.restore.apply` takes it as
  `confirm`; without a token, or with a wrong, used or expired one, nothing is restored. The token is
  the only secret a result keeps, and only in the preflight's own answer. Restore needs the approval
  kind `approval:restore`.
- **Cancel.** Only reads and checks (`list`, `verify`, `status`, `migrate show`, `migrate audit`) stop on
  a cancel request. A backup, a restore, the maintenance gate and the changing migration verbs finish or
  fail on their own terms: they are never torn down half way.
- **Maintenance.** `site.maintenance.enter` closes the gate and waits for in-flight work to drain; it
  stays closed until `site.maintenance.leave`. A restore closes and reopens the gate itself.
- **Time.** A command that outlives its timeout is killed and the operation fails with `mgmt.internal`.

```toml
[services.mgmt_site]
short_timeout_seconds = 600      # reads, checks, preflight, maintenance leave (10 to 86400)
long_timeout_seconds  = 21600    # backup, restore, maintenance enter (up to 604800)
```

## Known limits

- `site.status` reports the maintenance state as `unknown` until the running server's gate can be read
  from this module; the newest backup and whether a migration is held come from the CLI.
- The migration verbs need the lifecycle context of the site (stores, approvals, gate), which another
  module provides. Until it does, they answer `mgmt.internal` (`migration.port_unavailable`) and change
  nothing.
- The confirm token travels as a command-line argument of the child process, visible to the same OS
  user only; it is single use and expires after ten minutes.
