# Key and session administration (`[services.key_admin]`)

Provides the key and session areas of `/api/v1/identity/*` to the group mounted by `identity_admin`.

| Setting | Default | Meaning |
| --- | --- | --- |
| `enabled` | true | Off: the areas answer `501`. |
| `max_key_days` | 365 | The longest key lifetime (1 to 3650); an expiry is always set. |

- A key is made `revoked`, its digest sealed in the vault, then activated; the secret is shown once.
- Owners: the caller, or a `service` user. Scope subset (fail closed): only an owner holding
  `Tenant Administrator` may hold scopes; for any other owner every scope is an excess.
- SCIM credential: needs `[services.scim] enabled = true`; owned by the service user `scim-provisioner`.
- Admin session control ends sessions by `state = ended`; audited on `spot::audit::identity`.
- Impersonation is refused (default deny): no support grant exists on this host.
