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
| `step_up_window_secs` | 900 | A write needs a session created this recently (30 to 86400). |

- Every operation is one transaction. A disable ends the user's sessions, revokes their API keys,
  cancels pending invitations and queues `identity_user_disabled` together with the state change.
- The last tenant administrator cannot be disabled or anonymised; the administrators' rows are locked
  in the transaction, so two concurrent disables cannot both succeed.
- Permissions: `Tenant Administrator` holds all but impersonation; `User Manager` holds read and manage.
- Invitations need a platform DocType `invitation` and a module providing `InvitationMail`; without them
  `invite: true` and the invitation routes answer `503` and nothing is written (readiness: degraded).
- Audit: target `spot::audit::identity` (who, operation, target id, outcome; never a value).
