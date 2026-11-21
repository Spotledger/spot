Part of [Running Spot](../running-spot.md).

# User administration (`[services.identity_admin]`)

Serves the user part of `/api/v1/identity/*`: users (list, create, get, patch, disable, enable,
anonymise, recover) and invitations (list, revoke, resend). Roles, assignments, keys and sessions
answer `501` until the modules that provide `dyn RoleAdmin`, `dyn AssignmentAdmin`, `dyn KeyAdmin`
and `dyn SessionAdmin` are present.

| Setting | Default | Meaning |
| --- | --- | --- |
| `enabled` | true | Off: nothing is mounted. |
| `invitation_ttl_hours` | 72 | How long an invitation token works (1 to 720). |
| `step_up_window_secs` | 900 | A write needs a step-up this recent (30 to 86400): see "Recent" below. |

- Every operation is one transaction. A disable ends the user's sessions, revokes their API keys,
  cancels pending invitations and queues `identity_user_disabled` together with the state change.
- The last tenant administrator cannot be disabled or anonymised; the administrators' rows are locked
  in the transaction, so two concurrent disables cannot both succeed.
- Permissions: `Tenant Administrator` holds all but impersonation; `User Manager` holds read and manage.
- Invitations need a platform DocType `invitation` and a module providing `InvitationMail`; without them
  `invite: true` and the invitation routes answer `503` and nothing is written (readiness: degraded).
- Audit: target `spot::audit::identity` (who, operation, target id, outcome; never a value).

## Recent (the step-up window)

A write that needs a recent step-up (`403 auth.step_up_required` otherwise) asks whether the caller's
session has **stepped up within `step_up_window_secs`**: its `step_up_at` marker, written by
`POST /api/v1/auth/step-up` (see [step-up](step_up.md)), is no older than the window. The window is
also how long the step-up route says the step-up lasts (`step_up_until`).

**Behaviour change (WP-IAM-12.T8).** Until then "recent" meant "the session was created within the
window", so a fresh sign-in counted as a step-up. It no longer does: a session that never stepped up
is not recent, **including a session that was just created**. The marker belongs to one session: a
second session of the same user has to step up on its own. A marker a few seconds ahead of the
node's clock (another node wrote it) still counts; one far in the future does not.

**One reader (WP-IAM-12.T9).** The check is the shared function `services::step_up::session_is_current`,
also used by the [single sign-on administration](sso.md) and the [support access](support_access.md)
caller, with this setting's window: no second copy of the window logic exists and no reader looks at
the session's age. The step-up is also a record of the identity audit chain; see [step-up](step_up.md).
