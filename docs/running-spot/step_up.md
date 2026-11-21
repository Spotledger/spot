Part of [Running Spot](../running-spot.md).

# Step-up

Some operations are sensitive enough that being signed in is not enough: API keys, the single
sign-on provider administration, access governance, the data policy and the security audit answer
`403 auth.step_up_required` unless the caller re-authenticated recently. `POST /api/v1/auth/step-up`
(`auth_step_up`, ADR-070 section 3) is how a person does that. The contract is
`contracts/native-api/openapi.yaml` (operation `auth_step_up`, schemas `AuthStepUpRequest` and
`AuthStepUpResponse`); this page says what the route does and how it is wired.

## The route

A signed-in person (staff or partner; never a consumer, never an API key) sends one proof family:

| Body | Meaning |
| --- | --- |
| `{"password": "..."}` | A password step-up. |
| `{"password": "...", "totp": "123456"}` | The same, with the current second-factor code (needed when the user is enrolled). |
| `{"sso": true}` | A single sign-on session asks to re-authenticate at its provider. |

The body is strict: only `password` (1 to 1,024 bytes), `totp` (1 to 16 bytes) and `sso` are known,
an unknown field, a wrong type, an empty object, a `totp` without a `password` or a mixed family is
`400 api.invalid_request`, and it is refused **before** anything is checked, so a malformed call
never counts against the throttle. The answer never echoes the body.

The answer is `200` with `{"data": {"step_up_until": ..., "redirect_url": ...}}`:

- a password step-up answers `step_up_until` (a UTC instant) and a `null` `redirect_url`;
- a single sign-on step-up answers the `redirect_url` of the provider's forced re-authentication
  and a `null` `step_up_until` (the step-up is not done until the person comes back).

Every answer carries `Cache-Control: no-store`. The operation takes no `Idempotency-Key`: a repeat
is a repeat of the action, and the router never stores or replays its answer.

A wrong password is `401 auth.invalid_credentials`, with the same body whether the account has a
second factor or not (the answer never tells which part was wrong); a missing code is `401
auth.totp_required`; a wrong code is `401 auth.totp_invalid`; a throttled call is `429
api.rate_limited` with `Retry-After`; a consumer credential is `403` from the bearer layer before
the route runs; no credential is `401 auth.required`.

No password, code or redirect is ever written to a log line, a metric label, an error or an audit
record.

## The port

`spot-http` holds no credentials. The route is the group `step_up` (`src/api/router/routes/step_up.rs`);
it needs one port, `StepUpPort` (`src/api/ports/step_up.rs`):

```text
step_up(ctx, proof) -> Result<StepUpOut { step_up_until, redirect_url }, StepUpError>
```

The route passes no user id. Who the caller is comes from `ctx.credential` only, and everything
that decides is the port's: the audience, the proof, the throttle, the marker that makes the
session "recently stepped up" and the audit of a success and of a refusal. `StepUpError::Api` passes
a registry error through with its code; `StepUpError::NotConfigured` is `501 service.not_configured`
for a branch the site cannot serve.

The group is mounted only when the port is provided (a group with a required port absent is not
mounted at all, and the operation then answers as an unserved one does: `404`). The reading side is
`IdentityStepUpPort`, which only asks whether the caller's step-up is recent; it is not changed by
this route.

The host adapter (`spot-cli`, WP-IAM-12.T8) checks the password and the code, writes the marker and
audits; see "Host side" below.

## Host side

The host adapter is the module `step_up` (`crates/framework/spot-cli/src/services/step_up.rs`,
start order 1042). It provides `StepUpPort` and mounts the route itself; it is optional and is on
unless `[services.step_up] enabled = false`.

**Who may step up.** A person with a session of the staff or partner audience that has chosen its
password. An API key, a consumer, a session still restricted to choosing a password and another
site's request are `403 permission.denied`. The adapter is never given a user id: the user and
the session are the caller's own.

**What is checked, in order.**

1. The caller's session record: it is active, it belongs to the caller, and it says how it was
   established. A password proof is for a password session; `sso: true` is for a single sign-on
   session. The other way round is `400 api.invalid_request`.
2. The sign-in throttle, before the credential is looked at: the password door, counted under the
   user's own name and the client address, the **same buckets as the sign-in** (settings
   `[services.signin_throttle]`). A throttled caller is `429 api.rate_limited` with `Retry-After`
   and the password is not checked, so a right and a wrong password are answered alike; a throttle
   that cannot count refuses with `503` (fail closed).
3. The password, with the sign-in's own hasher against the stored hash (never a second hashing
   implementation), then, when the user is enrolled, the code: a one-time code once per time step
   (the step is recorded with a compare-and-set, so a code cannot be replayed here or at sign-in)
   or a recovery code (consumed).

**What it writes.** Two consumers, side by side; they are different consumers and both stay.

- *The identity audit chain* (WP-IAM-12.T9; the chain `spot audit verify` checks, the same one
  sign-ins are written to). A success is one `step_up_succeeded` record, a refusal one
  `step_up_failed` record with a reason class: `bad_credentials`, `totp_required`, `totp_invalid`,
  `throttled`, `not_a_person` (an API key, a restricted session or another audience, which is
  recorded for the user the credential names) or `marker_not_written`. A record names the user, the
  session id (an opaque id, never the token), the salted source digest the session stored (never
  the address) and the channel `api`. There is no field for a password, a code or free text.
- *The event stream* (WP-IAM-12.T8). On success `session.step_up_at` is set to now on the caller's
  own session (an optional datetime field of the platform app's `session` DocType, additive; sites
  pick the column up with the next schema sync), in one transaction with one
  `identity_sign_in_succeeded` event with `step_up: true`. A refusal after the credential was
  identified (a wrong password, a missing or wrong code, a throttled call) is one
  `identity_sign_in_failed` event with the reason class `bad_credentials` or `locked`. The events
  carry the user, the method, and for a success the session id; never a password, a code, a token
  or a digest.

Every outcome is also one line on the `spot::audit::identity` log target with the outcome and the
registry code. Nothing is written for a malformed body.

**Fail closed.** The order of a success is: the proof, then the chain record, then the marker with
its event. The chain record is written with `Recorder::required` (retried once): a record that
cannot be written **refuses the step-up** (`503 server.unavailable`, logged as
`step_up_host.chain_unavailable`) and **no marker is written**, so an unauditable step-up is never
granted. A refusal's record is written with `Recorder::failure`: it counts against the per-minute
failure budget of the audit and never changes the answer the caller sees (an audit outage is not an
oracle for a wrong password).

**A marker that fails after a recorded success.** The chain record stands before the marker is
written (an audit record cannot be taken back). If the marker write then fails, the answer is a
`503`, the step-up has no effect, and the module writes a `step_up_failed` record with the reason
`marker_not_written` for the same user and session. A reader of the chain therefore tells a step-up
that took effect (a `step_up_succeeded` with no `marker_not_written` after it for that session)
from one that did not.

**The answer** is `step_up_until = now + window`. The window is `[services.identity_admin]
step_up_window_secs` (900 by default, 30 to 86,400): the same value the operations that need a
recent step-up use, so the two cannot disagree. See [identity administration](identity_admin.md)
for what "recent" now means.

**Single sign-on.** A forced re-authentication at the provider is not offered by the single
sign-on service yet, so `sso: true` on a single sign-on session answers `501 service.not_configured`.
A password step-up works for password sessions only.

**Not mounted.** The route needs the sign-in stores and the sign-in throttle on the site (both are
required services, so this is only a misconfiguration). Without them the host logs
`step_up_host.not_mounted` and the route answers as an unserved operation does. With the route
absent no session has a marker, so the operations that need a recent step-up stay refused.

## A behaviour change

Before this adapter, "a recent step-up" meant "the session was created within the window", so a
**fresh sign-in counted as a step-up** and the operations that ask for one were open to anyone who
had just signed in. It no longer does: a step-up is the marker `session.step_up_at`, which only this
route writes. A session that never stepped up is not recent, however new it is. A client that
relied on the old behaviour calls `POST /api/v1/auth/step-up` first and repeats the operation
(`403 auth.step_up_required` says so).

Since WP-IAM-12.T9 **every** reader of "recent" reads the marker, through one function
(`services::step_up::session_is_current`, which uses `within_window`), with the one window
`[services.identity_admin] step_up_window_secs`. There is no second copy of the window logic and no
use of the session's age:

| Reader | Where | Reads the marker |
| --- | --- | --- |
| Identity administration (`IdentityStepUpPort::is_recent`) | `services/identity_admin.rs` | yes (T8, now through the shared function) |
| Single sign-on administration (`SsoAdminPort::step_up_current`) | `services/sso_admin.rs` | yes (T9). It used to be "created in the last 15 minutes" (900 s, a constant; the default window is also 900 s, so a default site changes only in that a fresh sign-in no longer counts) |
| Support access (`SupportAccessPort::step_up_current`, through `dyn SupportCaller`) | `services/support_access.rs`, `support_access_wiring.rs` | yes (T9): the host now provides the real `SupportCaller` |
| `GovernancePort::step_up_current` | `spot-http` trait, `routes/governance.rs` | no host implementation in `spot-cli` at this commit: nothing decides recency on its own |
| `DataPolicyPort` and `ProtectedGroupPort::step_up_current` | `spot-http` traits, `routes/data_policy.rs` | no host implementation in `spot-cli` at this commit (the `DataPolicyHost` of `services/data_policy.rs` implements neither) |
| `AuditPort::step_up_current` (the audit export) | `spot-http` trait, `routes/security_audit.rs` | no host implementation in `spot-cli` at this commit |

A host implementation of the last three, when one is added, must ask `session_is_current` (or
delegate to `IdentityStepUpPort`) and decide nothing from a session's age.
