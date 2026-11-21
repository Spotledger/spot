Part of [Running Spot](../running-spot.md).

# Roles on a running site

`spot-iam-roles` holds the rules for roles (duties, inheritance, copy on customise, the upgrade
diff, the snapshot build). `crates/framework/spot-cli/src/services/roles.rs` hosts them: it
serves the role operations of the identity API and keeps one published snapshot of roles and
assignments up to date. Level: **optional**.

## The snapshot

- One immutable snapshot is published at a time. A rebuild loads the roles and assignments from the
  site's `RoleSource`, builds the new snapshot off to the side and swaps it in one step. A request
  that took the snapshot before the swap finishes under it.
- A rebuild that fails (the records cannot be read, an assignment is invalid, a role is unknown) keeps
  the previous snapshot, degrades the site (`roles_host.source_unavailable` or
  `roles_host.snapshot_build_failed` in readiness) and never publishes an empty or partial one.
  Access is never widened by a failed build.
- A rebuild runs on a change notice (`RoleChangeSink::changed`, called by the outbox subscriber of a
  role or assignment event and by this module after its own write), at the next second an assignment
  starts or ends, and every `refresh_seconds` as the cross-node bound. Another node's write is in the
  shared stored records, so it is in force on every node within `refresh_seconds` (default 5).

## The administration

`identity_role_list|get|customise|upgrade_diff` answer from the snapshot (`permissions` is the
flattened list `doctype.right`, `:owner` for owner-only, plus the app permissions). Customise copies
a built-in role; the copy records the version it came from. A pack upgrade never changes a copy; the
upgrade diff is read only. A role id of another tenant is the same `404 identity.not_found` as an
unknown one. Before the first snapshot exists the routes answer `503 server.unavailable`.

## Settings

```toml
[services.roles]
enabled = true          # default
refresh_seconds = 5     # 1 to 300
```

Unknown keys are refused. Without a module that provides the `RoleSource` port the service reports
`roles_host.no_source` and the site is degraded.

## What the engine reads

The engine's own authoriser (`spot-cli/src/permissions.rs`) resolves a user's assignments from the
stored `role_assignment` records per request and takes its grants from the installed apps' model;
it does not yet read the published snapshot. Wiring the engine to `RoleSnapshots::current()` is the
follow-up named in the task report; until then this module serves the administration and owns the
snapshot, and the engine keeps deciding as before.
