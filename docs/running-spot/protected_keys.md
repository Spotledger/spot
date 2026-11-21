Part of [Running Spot](../running-spot.md).

# protected_keys

Hosts the key hierarchy on the hub: tenant root key, key release to nodes under lease, revocation
with data-key rotation, and the protected-group sync-class table. Off unless
`[services.protected_keys]` is configured.

```toml
[services.protected_keys]
groups = [{ name = "payroll", class = "sealed" }, { name = "prices", class = "encrypted" }]
```

Classes: `everywhere`, `encrypted`, `sealed`, `home_only`. An unknown class or a duplicate group
refuses the start; a group with no mapping is fail closed. The service is required when any group
is not `everywhere`. Release goes only to an enrolled, non-revoked node under the tier's lease; a
revoked node's next release is refused and every group it held rotates to a new epoch.
