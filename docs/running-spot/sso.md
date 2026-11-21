Part of [Running Spot](../running-spot.md).

# Single sign-on

`spot serve` signs people in through an OpenID Connect (OIDC) or SAML 2.0 identity provider when the
site enables `[services.sso]`. The sign-in decision (state, nonce, PKCE, signatures, audience, the
clock) is made by the `spot-sso` library; this module hosts it. It is **optional**: when it cannot
serve, the site keeps serving passwords and API keys and the readiness page names the cause.

```toml
[services.sso]
enabled = true
store = "postgres"            # the default; "memory" is accepted only in the single-node profile
fetch_timeout_secs = 5
max_fetch_bytes = 262144
jwks_max_age_secs = 3600
jwks_refetch_min_secs = 60
jwks_max_keys = 32

# An identity provider on a private network is reachable only when named here, with its addresses.
[[services.sso.private_providers]]
host = "idp.internal.example.com"
addresses = ["10.20.0.0/24"]
```

Unknown keys are refused. `[services.secrets]` and `[services.signin_throttle]` must be configured:
the pending sign-in record is sealed with a key derived from the secrets service, and every attempt
is counted by the sign-in throttle.

## What the server does

- **Pending state is durable.** The `state`, `nonce` and PKCE verifier between "begin" and "finish" live in
  the site database (`spot_sso_pending`), sealed, found by a keyed digest of the browser's binding cookie,
  and removed by a single statement, so a restart does not lose a sign-in and two nodes cannot both finish
  one. Assertion ids and logout-token ids are claimed in `spot_sso_replay` the same way. A site that would
  use the in-memory versions anywhere but the single-node profile does not start.
- **Outbound calls are policed.** Discovery, the key set, the token endpoint and SAML metadata are fetched
  through the egress policy: HTTPS only, a 5 second limit, a 256 KiB cap, no redirects, no private, loopback
  or link-local address unless `private_providers` names the host. The key set is cached (bounded in size
  and age) and refetched at most once a minute per provider, so an unknown `kid` cannot make the server
  fetch in a loop.
- **Every attempt is throttled first**, charged to the provider and the source address, and every outcome,
  success or refusal, is audited with the provider id, a stable code and the correlation id. No token,
  assertion, claim or client secret reaches a log, an error or the audit.
- **The provider's audience decides who can sign in.** A consumer provider cannot sign a staff user in; a
  partner provider signs in portal users only. The session records the audience and `method = sso`.
- **An e-mail address alone never links an account.** A known identity signs in; an open invitation links;
  a just-in-time rule creates a user with no roles; a matching local account requires the user to prove
  control of it first (`sso.link_requires_proof`).

## A generic OIDC provider

In the identity provider, register a **web application** that uses the authorization code flow and:

| Enter in the provider | Value |
| --- | --- |
| Redirect URI | `https://<site>/api/v1/auth/sso/<provider id>/callback`, exactly as configured in Spot |
| Scopes | `openid`, and `email` and `profile` for the e-mail and the name |
| Token endpoint authentication | client secret in the request body |
| PKCE | S256 (required) |

In Spot, the provider configuration holds the issuer (exactly the `issuer` of the discovery document),
the discovery URL (`.../.well-known/openid-configuration`), the client id, the **name** of the secrets
entry that holds the client secret (never the secret itself), the accepted signing algorithms (RS256 or
ES256), the claim mapping and the clock skew. A skew of 60 seconds is a sound default; raise it only for a
provider whose clock you cannot fix, and never above the maximum the loader accepts.

## A generic SAML identity provider

Give the identity provider the service provider metadata at
`https://<site>/api/v1/auth/sso/<provider id>/saml-metadata` (the entity id, the assertion consumer URL
`https://<site>/api/v1/auth/sso/<provider id>/acs` with the HTTP-POST binding, and the single-logout URL if
you use it). In Spot pin the provider's entity id, its single-sign-on URL and its signing certificates:
only a signature by a pinned certificate is accepted, and the signature must cover the element Spot
reads. The tolerated clock skew is 120 seconds by default.

## Ports other modules provide

The host needs the directory and the session (`SsoIdentityPort`), the client secret by name
(`SsoSecrets`), an audit sink (`SsoAudit`; the default is a log line of ids and codes), the SAML providers
(`SamlProviders`; none until a module provides them) and, for logout, a handler installed by
`sso_logout`. Until the identity port is provided, the service reports itself degraded and every single
sign-on attempt is refused.

## The providers behind the host (`[services.sso_identity]`)

`[services.sso]` moves bytes and decides nothing about your people; `[services.sso_identity]`
(WP-IAM-05.T16) provides what it hands every verified identity to, so a sign-in completes. Off by
default; turn it on together with `[services.sso]`:

```toml
[services.sso_identity]
enabled = true
base_url = "https://erp.acme.example"      # the site's public origin, for providers made through the API
handover_ttl_secs = 60                     # 10 to 300

[services.sso_identity.client_secrets]     # name -> where the value is read, at the moment of use
okta-prod = "env:SPOT_OKTA_SECRET"         # or "file:/etc/spot/secrets/okta" (mode 0600)
```

- **Directory and linking.** An identity reaches a user only through an already linked
  `(issuer, subject)` or a **pending, unexpired invitation** whose address the provider verified.
  An e-mail match alone never links; it is refused. Linking and the user's activation are one
  transaction; two sign-ins with one invitation serialise and the second is refused.
- **Just-in-time users** are created only by a rule of the tenant policy, `active`, **with no role**;
  a domain rule needs a verified address; a person who already has the address is never given a
  second account.
- **Consumers** are never users. A consumer-audience sign-in is refused until the consumer principal
  store exists.
- **One-time hand-over.** After the provider sign-in the browser carries a random value for
  `handover_ttl_secs`; the table `spot_sso_handover` keeps its digest and the redemption deletes
  and reads it in one statement, so it works once. The session is started by the ordinary session
  manager (the same token format the authenticator accepts) and marked `method = oidc` or `saml`,
  with the audience of the provider and the cap a SAML assertion set.
- **Client secrets** are names in the provider configuration; the value is read from an environment
  variable or a protected file when needed and is never logged, stored or returned. `private_key_jwt`
  is not read.
- **SAML providers** have their own table (`spot_sso_saml_provider`, versioned). The administration
  API does not create them yet (they come from signed IdP metadata); it can read, test and delete
  them.
- **Policy.** The tenant sign-in policy is `spot_sso_policy` (one row per tenant, versioned). No row
  means local. A row that cannot be read refuses every password. A change is reviewed against
  lock-out (`sso.would_lock_out`) and is a compare-and-set on the version (`If-Match`).
- **Single logout.** `spot_sso_session_index` records, per session, the provider it began through,
  the user and the IdP `sid`. A logout ends only the sessions of the provider that asked, with end
  reason `sso_logout`; open realtime sockets close within their session check interval; API keys are
  never touched.
- **Administration** (`/api/v1/admin/sso/*`): Tenant Administrators only, a **recent step-up**
  before every write (the session's `step_up_at` marker within `[services.identity_admin]
  step_up_window_secs`, read through the same function as the identity administration; see
  [step-up](step_up.md)), every outcome audited on `spot::audit::identity`. Until WP-IAM-12.T9 this
  was "the session was created in the last 15 minutes" (a constant of 900 s), so a fresh sign-in
  counted: it no longer does, and an administrator calls `POST /api/v1/auth/step-up` first (`403
  auth.step_up_required` says so). The default window is also 900 s.

Tables created by `prepare` at start (idempotent): `spot_sso_saml_provider`, `spot_sso_policy`,
`spot_sso_handover`, `spot_sso_session_index`.
