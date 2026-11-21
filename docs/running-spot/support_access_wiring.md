Part of [Running Spot](../running-spot.md).

# Support access and break-glass: the wiring

The break-glass host (`services/break_glass`, order 1170) reads its parts as ports. This module (`services/support_access_wiring`, order 1165) provides each port from the real component of the site, or leaves it missing so break-glass stays closed. Nothing is permissive by default.

## What comes from where

| Port | Provided from | If the component is absent |
|---|---|---|
| audit chain | the site's hash-chained audit store, one chain of its own (scope `break_glass:<site>`), used only when the database reports the guard against rewrites complete | no chain: break-glass is closed and readiness names `audit chain` |
| object store and retire | the site's file store, private objects under `break_glass/`, written once, never overwritten | closed, names `object store` |
| sealer | the secrets service's vault (`[services.secrets]`) | closed, names `sealer` |
| duty check | the site's permission engine: the duties `security_administrator` and `security_auditor` are app permissions of the same name | closed, names `duty check` |
| agent bridge | nothing in this host reaches a host agent yet, so every kill is refused with `break_glass_host.not_wired` | (always provided as a refusal) |
| retention | `retention_days` below, when set | not provided: `[services.break_glass] retention_days` applies |

A revoke closes the session first, so it stops acting at once. Until a real agent bridge exists the kill is not delivered, the answer says so, and the break-glass sweep keeps retrying.

## Setting

```toml
[services.support_access_wiring]
enabled = true              # default
retention_days = 365        # optional, 1 to 3650
max_object_bytes = 268435456  # default 256 MiB, 1 to 1 GiB
```

Unknown keys and out-of-bounds values are refused at start (`support_access_wiring.settings_invalid`). A recording is stored as the hex of its bytes under the vault, so an object takes twice the recording's size: keep `max_object_bytes` at least twice `[services.break_glass] max_recording_bytes`.

## What an operator checks

- `/api/health/ready` says `support_access_wiring (support_access_wiring.part_missing: ... missing <parts>)` while a required part is missing. The service is optional: the rest of the site is unaffected.
- The permission engine must declare the two duties as app permissions and grant them to the customer's roles, or every duty check answers no (a refusal, not an allow).
- The sealer waits for the vault on the site's multi-thread runtime. On any other runtime a recording is refused, never stored unsealed.
- The audit store must be prepared (tables and rewrite guard installed) so its protection is complete.

## The support-access caller (WP-IAM-12.T9)

The wiring also provides `dyn SupportCaller` (read by `[services.support_access]`, which starts
later), when the site has its sign-in service and the permission engine. For a request it names:

- the **user** of the credential's own session (a person of the staff or partner audience with an
  unrestricted password session; an API key, a restricted session and another site's request name
  nobody), never a system principal;
- `is_security_admin`: the `security_administrator` duty, decided by the permission engine for that
  user (an engine error is "no caller");
- `step_up_current`: the session's `step_up_at` marker within `[services.identity_admin]
  step_up_window_secs`, read through the shared `services::step_up::session_is_current` (a marker
  that cannot be read is "no caller"). A session that never stepped up is not recent, however new it
  is, and another session's marker, even of the same user, does not count.

Without a site context or a permission engine no caller is provided and every support-access
operation is refused, as before.

## Not wired here

The approval verifier (control plane), the agent dispatcher (control plane) and the account provider (host agent) run in their own processes with their own start-up; this host cannot reach them. They stay fail-closed in those processes until their own wiring tasks provide them.
