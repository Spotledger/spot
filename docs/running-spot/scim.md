Part of [Running Spot](../running-spot.md).

# SCIM provisioning

SCIM 2.0 (RFC 7643 and 7644) is how your identity provider (IdP) creates, changes and removes people
in Spot. The server is `/scim/v2` with `Users`, `Groups`, `ServiceProviderConfig`, `ResourceTypes` and
`Schemas`. It is served by `crates/framework/spot-cli/src/services/scim.rs` over the `spot-iam-scim`
core, and it is **off** until you turn it on.

It needs the secrets vault ([Secrets](secrets.md)): the SCIM credential's digest is sealed by it.
Without a vault every SCIM call answers `503` (it never falls back to anything weaker).

## Turn it on

```toml
# common.toml
[services.secrets]
kek_file = "keys/{site}.json"

[services.scim]
enabled = true
# optional, with their defaults
max_body_bytes      = 1048576   # a larger body is 413 before it is read
requests_per_window = 600       # per credential, per minute; over it is 429 with Retry-After
default_count       = 100       # page size when the client names none
max_count           = 1000      # the largest page
max_filter_chars    = 1024
```

Unknown keys are refused at start-up. With `enabled` absent or false, `/scim/v2` is a 404.

## The credential

SCIM is a privileged provisioning channel, so it has its own credential and nothing else works on it.

- It is an **API key** (`spk_<id>_<secret>`) owned by a **service user**, with the scope list
  **exactly** `scim`. It is not a session. A session token, a native API key (any other scope), an
  expired or revoked key, a key whose owner is disabled, and an unknown key are all the same answer:
  `401` in the RFC 7644 error form.
- It belongs to one site. It cannot read or change another site's directory.
- The reverse holds too: a SCIM key is refused by the native API. (Today the host authenticates
  native API keys from the site file, where a SCIM key does not exist. When the storage-backed key
  store is wired, it must refuse any key whose scope list contains `scim`.)
- The key text is shown **once**, when it is issued, and never stored: only its digest is, sealed by
  the vault. Put it in the IdP's secret store and nowhere else. To rotate, issue a new one, switch the
  IdP, and revoke the old (`state = revoked` on its `api_key` record).

Create the service user (any name; `user_type = service`), then issue the key through the hosting
module's `ScimKeyIssuer` port (`services::scim::ScimKeyIssuer::issue(owner, label, expires_at)`). An
administration command or API in front of it is a follow-up task; until then the call is made by the
operator tooling that holds the port.

## What SCIM can see

Only users of type **internal**. Service, agent and portal users, and so the credential's own owner,
are not listed, read, changed or deactivated by SCIM. Everything it changes is written as the system
principal, in one `Serializable` transaction per request: a refused request writes nothing.

## Users

| Request | Does |
| --- | --- |
| `POST /Users` | creates a user (type `internal`, no password: it signs in through your IdP) |
| `GET /Users/{id}`, `GET /Users?filter=&startIndex=&count=` | reads, filters (`eq`, `co`, `sw`, `and`, `or`, `pr`, ...), pages; `ETag` on one resource |
| `PUT /Users/{id}` | replaces; `If-Match` with a stale ETag is `412` |
| `PATCH /Users/{id}` | RFC 7644 patch operations |
| `DELETE /Users/{id}` | **deactivates**, never deletes: the id stays in audit |

### Deactivation

`PATCH active=false`, a `PUT` that clears `active`, and `DELETE` all do the same: in **one transaction
with the user change**, the user is disabled and the user's API keys are marked revoked. Every sign-in
path re-reads the user's state on each request (the host keeps no session cache), so the user's
sessions, tokens and first-sign-in invitation stop at the moment of commit, not at some later expiry.
`identity_user_disabled` goes to the outbox in the same transaction.

## Groups and roles

A SCIM `Group` grants nothing by itself. The **group-to-role table** does: one document per IdP group
name in the `group_mapping` DocType (System Manager edits it; User Manager reads it), with the role
and an optional scope. Membership of a mapped group gives that role, with **source `idp`**, through the
assignment rules of `spot-iam-assign`; a manual assignment of the same role is never touched, and a
group edit changes no role record. A group with no role grants nothing (the admin view flags it).
Members are written by SCIM only. Portal users are refused desk roles.

## Errors and limits

Every answer is `application/scim+json`, errors included, in the RFC 7644 form (`schemas`, `status` as
text, `scimType` where the RFC has one, `detail`). A detail names the rule, never a value: a malformed
body is not echoed. `429` carries `Retry-After`. `503` with `Retry-After` means nothing was written
(the store was busy or a concurrent change won): the IdP's own retry is the remedy.

## Audit

Every change writes the `identity_*` events of the identity contract to the outbox, in the same
transaction, with the key's owner as the actor, and one line per request to the log target
`spot::audit::scim` (the key id, the operation, the status). No token, body or secret is in either.

## Known limits of this version

- The whole directory of internal users is read for each request (fine to tens of thousands of users;
  a store with filter push-down is a follow-up).
- Session, refresh-token and invitation counts in the deactivation event are reported as zero: they
  are ended by the user's state, not counted.
- Stored roles carry no portal kind, so every role is treated as a desk role.
