Part of [Running Spot](../running-spot.md).

# Support access (the customer's side)

Support staff reach a tenant's data only through a request the customer approves with a signed
consent. The module `support_access` (order 2010, `crates/framework/spot-cli/src/services/support_access.rs`
and its `support_access/` directory) hosts the customer's side over `spot-deep-access` (the rules)
and `spot-deep-access-pg` (the state, in six `spot_deep_access_*` tables, tenant on every row).

## The flow

1. The platform operator's request arrives through the in-process `SupportIntake::submit` port (the
   transport is the management link). It is stored `pending` and conveys no authority. A request for
   another tenant, a malformed one, one whose window has ended or an id already used is refused.
2. The tenant's security administrators see it (`GET /api/v1/security/support-access/requests`):
   scope, tier, window, reason, ticket, who asked, approvals so far and needed.
3. An administrator approves with a **signed consent** (`POST .../requests/{id}/consent`, body
   `consent` and `signature`): the consent record names the request's hash, the approver (the
   signed-in administrator, never someone else), the scope (never wider than requested), the validity
   window, the authentication method and a single-use nonce, and is signed by a key the customer
   enrolled for that identity. It is verified on arrival; a wrong key, an unknown identity, another
   request, an expired window or a wider scope is `security.consent_invalid` with the
   `deep_access.*` reason. A request that may write (tier above T0) needs two different
   administrators; the same one twice is refused; the requester or the requesting operator can
   never approve.
4. Only then can the operator side call `SupportIntake::issue`. It refuses anything not approved,
   re-verifies the consent and the vendor hardware attestation, consumes the receipt nonce in
   PostgreSQL (one winner across nodes, kept across restarts) and writes requested, consented and
   issued to the tenant audit chain. The credential's secret is never stored, only its digest.
5. The customer revokes at any time (`POST .../requests/{id}/revoke`): the request and every
   session issued from it end at once, recorded first in the audit chain; the next `authorise` of
   the session refuses (nothing is cached). Expired requests are swept; credentials and
   revocations are history and stay.
6. The customer sees the sessions (`GET .../sessions`: active, scheduled, expired, revoked) and the
   actions taken in a session (`GET .../sessions/{id}/recording`: `at`, `action`, `target`), read
   from the tenant audit chain, never anything the operator could edit.

Consent, revoke and recordings need a recent step-up: the caller's session has `step_up_at` within
`[services.identity_admin] step_up_window_secs` of now (written by `POST /api/v1/auth/step-up`, read
through the shared `services::step_up::session_is_current`; see [step-up](step_up.md)). A session
that never stepped up is not recent, however new it is (`403 auth.step_up_required`). Every outcome, refusals included, writes an
audit record, and a decision whose record cannot be written is not carried out.

## Settings

`[services.support_access]` (unknown keys are refused): `enabled` (default true), `approvals_required`
(1 or 2, default 1), `two_person_for_write` (default true), `sweep_every_secs` (default 600, at least
60), `nonce_keep_days` (default 400, at least 90).

## Ports

This module takes, from modules with a smaller `ORDER`: `dyn SupportCaller` (who the credential
names, security administrator, step-up; provided by `support_access_wiring`), `dyn SupportAudit` (the tenant audit chain),
`dyn SupportRecordings` (a session's actions from the chain) and, for issuance, the shared
`spot_pg_bridge::Bridge`, `dyn EntropySource`, `dyn SessionLogOpener` and the vendor attestation
verifier. It provides `dyn SupportAccessPort` (the routes) and `dyn SupportIntake`.

## Level and failure

Optional. The start-up check creates the tables and requires the audit port: without it the site is
shown degraded and every operation answers `server.unavailable` (nothing is decided). Without the
issuance ports the customer can still decide, and `issue` answers unavailable.
