Part of [Running Spot](../running-spot.md).

# The side-effect hold: `spot migrate-hold`

While a migration runs, or a restored copy is being verified, **nothing may leave the process**:
no mail, no webhook, no bank API call, no connector request, no external job. The hold stops those
effects at the door, keeps them in a durable ledger and lets you replay or discard them
afterwards. Fiscal transmissions of issued documents are never held (ADR-43).

## How it works

`[services.effect_hold]` (on by default) hosts the hold for the site. It provides:

- the **effect gate**: every producer asks `try_pass(class, key)` immediately before the
  irrevocable act. `Pass` means go ahead; `Held` means the effect was recorded and parked, not
  failed, and its retry counter does not move. A producer that gets no answer treats it as
  `Held`.
- the **performers**: each producer registers how to perform one effect of its class, so a
  release can replay held effects in their original order, each with its original idempotency
  key, at most `replay_per_class_per_second` (default 20) per class.

If the hold state cannot be read (the ledger is down or missing), the gate **holds every
external effect** and `/api/health/ready` reports `degraded: effect_hold (...)`.

| setting | default | meaning |
| --- | --- | --- |
| `enabled` | `true` | `false` removes the gate; producers behave as before |
| `refresh_millis` | 1000 | how often the gate re-reads the ledger (100 to 60000) |
| `replay_per_class_per_second` | 20 | release rate limit per class (1 to 10000) |

## Commands

`spot migrate-hold` takes `--site S` and one of:

- `hold --reason "why"`: stop external effects and record them.
- `status`: open, or held (by which migration, since when, why).
- `release --replay`: perform the held effects once, in order, then open the site. Replay needs
  the performers the serving process registers, so run it through the management operation
  `site.migrate.release`; the command line refuses (exit 3) when no performer exists and the hold
  stays.
- `release --discard --approved-by USER [--reason "why"]`: drop the held effects. A discard is an
  explicit decision: a **second approver** who is not you and holds the migration right must be
  named, and the hold service audits it with the effect classes and key digests.

Exit codes: 0 done, 1 usage, 2 refused by policy or permission, 3 failed.

A hold taken by a running migration carries that migration's id and is released by the
migration; `release` here refuses it (`migration.hold_active`).

## Held payloads

A held effect can reference personal data. It stays in the site database with the same
protection as the outbox and is purged on release or after the retention setting.
