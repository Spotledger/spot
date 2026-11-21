Part of [Running Spot](../running-spot.md).

# Your own sessions

A signed-in user can see the sessions they hold and end any of them. `GET /api/v1/auth/sessions`
lists them (`auth_session_list`); `DELETE /api/v1/auth/sessions/{id}` ends one
(`auth_session_end`). The contract is `contracts/native-api/openapi.yaml`; the rules (the store,
the idle and absolute timeouts, the revoke) live in `spot-sessions`, and the sign-in service in
`spot-login` sits on top of it. This page is what the host adds and what an operator can turn.

## What a user sees

The list is the caller's own **live** sessions: the session the request is made under first
(`current: true`), then the others, newest first. Each entry carries `id`, `created_at`,
`expires_at`, `last_used_at`, `method`, `source` and, when one was recorded, `device`. The session
token and its digest are never in the answer; the id is an opaque handle, not a credential.

Two things differ from what the contract's example may suggest:

- `method` is always `password` today. A session is started only by a password sign-in; an API
  key is a bearer credential with no session of its own, and SSO has not landed (ADR-070 section 7).
- `source` is the site's salted digest of the client address, not the address: the session store
  keeps the digest only.

## Ending a session

`DELETE` answers `204` when the session belongs to the caller. The token stops working on the
very next request on the node that served the call (its authenticator cache is dropped);
ending the current session is allowed and equals signing out.

An id that is unknown, that belongs to another user, or that is already ended is **one answer**,
`404 auth.session_not_found`, so an id alone tells nothing about other users. An id that is not 1 to
64 letters, digits, underscores or hyphens is `400 api.invalid_request` for everyone.

## Who may use it

Only a person with a session. An **API key** bearer is refused with `403 permission.denied`, on
both routes: a key is not a person and has no sessions to manage. A session that has not chosen its
password yet (the one-time first sign-in secret) is refused the same way; the request gate already
allows such a session nothing but the password change, sign-out and reading the session. A request
without a credential is `401 auth.required`. The routes are authenticated (never public) and
answer `Cache-Control: private, no-store`.

The password-change rule is unchanged: changing the password ends every **other** session of the
user and keeps the current one.

## Audit

Every ending writes one identity audit record, kind `session_ended_by_user`, with the user's record
id, the session id and the reason `user_ended`. The record is written **before** the session is
revoked and the ending is refused (`503 server.unavailable`, retry hint) when the record cannot be
written, so an ending that is not on the audit trail does not happen. The sign-in service then
records the revoke itself (`session_revoked`). No record carries a token, a digest or an address.
A refused attempt (another user's id, an unknown id) ends nothing and writes no ending record.

## The service

| | |
| --- | --- |
| Name | `session` (`services/session.rs`, start order 1030) |
| Level | optional |
| Setting | `[services.session]` `enabled` (default `true`; unknown keys are refused) |
| Start-up check | the site's sign-in service is present |

If the sign-in service is not available the check fails, readiness reports the site degraded and
the two routes answer `503 server.unavailable` with a retry hint. They never answer an empty list
for a service that cannot answer. Sign-in, sign-out and the data routes are unaffected. With
`enabled = false` the routes are not mounted and answer as an unserved operation does.

## Limits

Sessions are held in the process (see `LoginStores::LIMITATION`), so a restart ends every session
and the list is per node. A user holds at most the session policy's `max_per_user` sessions, so the
list is short and is not paged.
