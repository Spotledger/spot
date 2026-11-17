# Invitations (`[services.invitations]`)

An administrator invites a person (`POST /api/v1/identity/users` with `invite: true`, or
`resend` on an invitation). The person receives a token by mail and exchanges it once for a
password with `POST /api/v1/auth/invitations/accept`; they then sign in with `auth_login`.

| Setting | Default | Meaning |
| --- | --- | --- |
| `enabled` | true | Off: no invitation mail and no acceptance route. |
| `accept_rate_per_minute` | 60 | The site's open-path limit for the acceptance route (1 to 6000). On top of it every client address has the `auth` rate class limit. |

## The record

The platform DocType `invitation` holds `user`, `email`, `state`, `expires_at`, `sent_at`,
`accepted_at` and `digest` (the lowercase hex of the SHA-256 digest of the token). The token is never
stored. States: `pending`, then one of `accepted`, `expired` (the clock) or `revoked` (an administrator,
or disabling the user). Nothing leaves a final state.

## The mail

The invitation and its mail are written in the same transaction (`issue_with_mail`): a kill between
the two leaves both or neither. The mail is a `platform:send_mail` job (an outbox event of the
transaction), sent by the mail service; with `services.mail.enabled = false` no mail is provided and
`invite: true` answers `503` (readiness: degraded). The mail names the site and the expiry and holds
the token in its text; until the job is sent, the job arguments hold the token too, so restrict
access to the job and outbox tables like any other credential store.

## Accepting

- One answer for an unknown, malformed, used, revoked or expired token:
  `400 identity.invitation_invalid`. There is no way to learn which.
- A password below the tenant's policy is `422 auth.password_policy` and does not use the token up.
  The policy is judged only after the token proved valid.
- A success sets the password verifier through the sign-in service's credential store (the module
  that provides `InvitationCredentials`), makes the user `active` and the invitation `accepted` in one
  transaction, and adds no role: the user has exactly the assignments the administrator made when
  inviting.
- Two accepts of one token serialise on the invitation row: one succeeds, the other is refused.
- Throttle: the per-address `auth` rate class, then the site's `accept_rate_per_minute`.
- Audit: target `spot::audit::identity`, operation `invitation_accept`, the outcome and (on success)
  the user id; never the token, the password or an address. The durable record is the pair of
  identity events (`identity_user_enabled`, `identity_password_changed`) written in the transaction.

Readiness (`invitations`, optional) is degraded when the `invitation` DocType, mail, or the sign-in
service's password store is missing.
