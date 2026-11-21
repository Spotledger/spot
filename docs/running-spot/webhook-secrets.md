Part of [Running Spot](../running-spot.md).

# Webhook signing secrets

A webhook signs every delivery with an HMAC-SHA256 key, its **signing secret**. The secret is the
sealed `secret` field of the `webhook` document. It is **write-only**: you can set it, rotate it and
read back a fingerprint, but no API, export, report, search, webhook payload, backup text dump or log
can show the value. Only the delivery handler can open it, to sign. The module is
`crates/framework/spot-cli/src/services/webhook_secret.rs`.

It needs the secrets vault ([Secrets](secrets.md)): `[services.secrets]` in `common.toml`, with a key
file for the site. Without one, a webhook cannot be signed (see "No vault").

## Set or rotate a secret

```
PUT /api/v1/webhooks/{name}/signing-secret
Content-Type: application/json

{ "secret": "<16 to 256 bytes of text>" }        a value you chose
{ "generate": true }                             a value the server makes
```

`{name}` is the webhook's `name` field (percent-encoded; at most 200 characters). The body is exactly
one of the two forms, never both and never anything else.

- A **supplied** secret must be 16 to 256 bytes. The answer never contains it.
- A **generated** secret is 32 random bytes, base64url without padding (43 characters). It is in the
  answer **once**, as `data.secret`, and nowhere else. **The client that receives it must not log it,
  cache it or put it in a URL**; copy it to the receiver and drop it. If it is lost, rotate.
- Every `PUT` answer, like every answer of these routes, carries `Cache-Control: private, no-store`.

The answer is `{"data": {"key_version", "fingerprint"}}` (plus `secret` for a generated value). The
**fingerprint** is twelve hex characters of a SHA-256 over the site, the webhook name and the value:
enough to tell whether two copies of a secret match, not enough to recover it. The **key version** is
the vault key version in force when the value was set.

A rotation takes effect on the next delivery attempt. The old value stops signing at once; this
version keeps no previous value for a receiver that is still switching over.

## Read the fingerprint

```
GET /api/v1/webhooks/{name}/signing-secret
```

Answers `{"data": {"set", "key_version", "fingerprint"}}`. `set` is `false`, with a key version of 0 and
an empty fingerprint, when the webhook has no secret yet. A document read, a list or a `GET` of the
`webhook` document never contains a `secret` field: it is absent, not blank, for every role.

## Who may call it

The caller signs in as usual (a session that has not chosen its password yet may do nothing here) and
must hold, on the permission snapshot served now, **both** `update` on the `webhook` DocType and
`update` on the `secret` protection group of `webhook`. `Tenant Administrator` holds both by
definition. A caller without them is answered `403 permission.denied`, and the answer is the same
whether or not the webhook exists: the right is checked before the webhook is looked up. A caller who
holds the right and names a webhook that does not exist gets `404 document.not_found`.

The shipped grants give `System Manager` write on the `webhook` document but no grant on the `secret`
group, so today only `Tenant Administrator` passes the second check; give another role the group
`update` grant in the site's permissions to let it manage secrets.

## No vault

With no secrets vault the module is **degraded** (listed on `/api/health/ready` as `webhook_secret`),
the routes answer `501 service.not_configured`, and the delivery handler's key port answers an error:
a webhook that cannot be signed is **not delivered**. There is no plaintext fallback and no default key.

## What is audited

- Every call that reaches the engine is journalled by the engine as a `sys_secret_audit` event: the
  operation (`set`, `open`), the DocType and field, the caller kind `webhooks`, the site, the outcome
  and the error code, with the actor in the event's metadata. A `set` is journalled in the transaction
  of the write, so it commits with it or not at all; a refused call is journalled on its own.
- A call refused before the engine (no right, no vault, wrong site, unknown webhook) writes one line to
  the log target `spot::audit::webhook_secret`: the site, the actor, the operation and the reason.

Neither the journal nor the log line holds a value, a sealed blob, the input of a fingerprint or a key.
Verify the journal with the secrets tooling in [Secrets](secrets.md).

## For the delivery handler

`spot_cli::services::webhook_secret::WebhookSigningKeys` is a port of its own: `signing_key(site,
webhook_name)` opens the value as the platform itself (journalled as an `open` by the caller
`webhooks`) and returns a redacted, zero-on-drop `SecretString`. It is an error, never an empty key,
for an unset secret, an unknown webhook, a site that is not the service's own and a site with no vault.
The adapter onto the webhook crate's `SecretResolver` is the delivery wiring's.
