Part of [Running Spot](../running-spot.md).

# The node role model: `[services.node_role]`

## What it is

`spot serve` builds this site's role from `[services.node_role]` of `common.toml`
(WP-RTB-03.T1). This table is host-wide, like every `[services.<name>]` table: it is the same
for every site this process serves. See `## Open questions` in the task's PR for why this is
not the per-site `[sync]` table a design sketch might suggest — `crates/framework/spot-cli/src/
config.rs` is a different task's owned path and carries `#[serde(deny_unknown_fields)]` with no
room for a bespoke top-level table, so this module uses the registry's own seam instead
(`[services.<module file stem>]`; the module's file is `node_role.rs`).

```toml
[services.node_role]
role = "standalone"              # "standalone" (default), "hub" or "edge"
node = "branch-07"                # edge only; must equal [partition] node
hub_url = "https://hub.example.internal"       # edge only
hub_fingerprint = "sha256:ab12..."             # edge only; set after enrolment
enrol_code_file = "/etc/spot/enrol.code"       # edge only, first start only
listen = "0.0.0.0:7443"           # hub only
host_roles = ["branch-edge"]      # the host role set; an edge needs it (or SPOT_HOST_ROLES)
audit_scratch_rebuild = false     # edge only: true runs the audit job's scratch rebuild on an edge
```

Unknown keys are refused (the table is `deny_unknown_fields`, as every registry table is).

## The three roles

- `standalone` (the default): no sync service starts and nothing dials out. Today's behaviour,
  unchanged.
- `hub`: requires `listen`. The side edges enrol against and sync with.
- `edge`: requires a `[partition]` profile, `node` equal to `[partition] node`, `hub_url`, and
  either a prior enrolment (a recorded `hub_fingerprint`) or a readable, non-empty
  `enrol_code_file`.

## The host's own role set

The host (not the site) may run a narrower set of roles than this table asks for — the
`runtime.node.roles` setting a host agent manages (`web`, `jobs`, `realtime`, `database`,
`branch-edge`). `spot-cli` has no dependency on `spot-agent-runtime` and no reachable settings
store for that value, so it is mirrored through the `SPOT_HOST_ROLES` environment variable: a
comma-separated list of the same role names, read once at start-up.

- `role = "edge"` requires `branch-edge` in `SPOT_HOST_ROLES` and refuses `database` there.
- `role = "hub"` or `role = "standalone"` refuses `branch-edge` in `SPOT_HOST_ROLES`.
- The set is `host_roles` in this table, else `SPOT_HOST_ROLES`. When neither is given the set is
  unknown: a hub or standalone is accepted (nothing contradicts it) but an **edge is refused**
  (`node_role.role_conflict`), because an edge must positively know it runs as `branch-edge`
  without `database`.

A mismatch is `node_role.role_conflict`.

## What each role runs

The registry runs a module only in the roles its `pub const ROLES: &[&str]` names; a module
without one runs in every role. The table (`SERVICE_SETS` in `services/node_role.rs`):

| Module | standalone | hub | edge |
| --- | --- | --- | --- |
| every module without `ROLES` (the local trading stack) | yes | yes | yes |
| `sync_hub`, `enrolment_admin`, `journal_access` | no | yes | no |
| `sync_node`, `sync_loops` | no | no | yes |

- A skipped module's `register` never runs: no connection, listener, setting read or readiness
  entry.
- A module with `ROLES` must start after `node_role` (an `ORDER` above 1005); otherwise the site
  start is refused. A `ROLES` entry that is not `standalone`, `hub` or `edge` is refused too, so a
  typo never enables a service.
- The audit job's scratch rebuild runs on a standalone and a hub; on an edge it runs only when
  `audit_scratch_rebuild = true`.
- On an edge or a standalone the hub-only routes (`/api/v1/sync/hub`, `/api/v1/journal`,
  `/api/v1/enrolment`) answer `501 service.not_configured`. No hub listener binds and no hub key
  material is read.
- The role is read once from the validated configuration at assembly; nothing on the request
  path can change it.

## The refusals

Every refusal below stops the site before it starts anything for it: no listener opens, no job
loop starts, no write happens (the module's own `register` fails before the services registry
ever calls `start` on any service). On a reload the site keeps answering `503`. The message
never echoes a secret, a key, a code or a fingerprint.

| Code | When | Remedy |
| --- | --- | --- |
| `node_role.unknown_role` | `role` is not `standalone`, `hub` or `edge`. | Fix the value. |
| `node_role.edge_without_partition` | An edge names no `[partition]` profile. | Add one, naming this node. |
| `node_role.edge_without_hub` | An edge names no `hub_url`. | Set it. |
| `node_role.edge_not_enrolled_and_no_code` | An edge that has never enrolled names no readable, non-empty `enrol_code_file`. | Set `enrol_code_file` to a one-time code the hub issued. |
| `node_role.hub_without_listener` | A hub names no `listen`. | Set it. |
| `node_role.role_conflict` | The host role set disagrees, or a hub-only and an edge-only setting are both present. | Match `host_roles` or `SPOT_HOST_ROLES` (an edge needs one of them to say `branch-edge`); keep `listen` off an edge and `hub_url`/`hub_fingerprint`/`enrol_code_file` off a hub or standalone. |
| `node_role.node_id_mismatch` | `node` is absent or differs from `[partition] node`. | Make them equal. |
| `node_role.role_changed_unsafely` | This site already recorded a different role. | Run the role-change operation (WP-RTB-04.T2, not yet built), or stop the site and delete its row of `spot_node_role_state` (see below) if this really is a fresh bootstrap, not a production role change. |
| `node_role.table_missing` | `spot_node_role_state` does not exist. | Run `spot migrate`. |

`node_role.partition_scope_uncovered` is not this module's: it stays `participants.rs`'s own
`partition.scope_uncovered`.

## The small table

`spot_node_role_state` (one row per site: `site`, `role`, `hub_fingerprint`, `updated_at`)
records the role a site started with, so a later start under a different role is a refusal, not
a silent switch. It is created by `spot migrate`, never by `spot serve` itself; a site whose
database does not have it yet is refused with `node_role.table_missing` and told to migrate.

There is no `spot sync change-role` command yet (WP-RTB-04.T2, out of scope here). Until it
exists, intentionally moving a site to a different role (not a production incident — a genuine
re-bootstrap, for example a test fixture) is:

```sql
DELETE FROM spot_node_role_state WHERE site = 'the-site-name';
```

then restart with the new `[services.node_role]`. Doing this in production bypasses the audit
the future command will add; it is for deliberate re-provisioning, not routine role changes.

## Offline-safe readiness

The module's own readiness contribution is `Level::Optional` (the site's readiness check never
treats it as required) and performs no network I/O: it never fails `/api/health/ready` for a
hub-reachability reason. The actual hub-link probe (reported as `sync_node: offline 3h`, never
as a readiness failure) is `services.sync_node`'s own, built in WP-RTB-02.

## Looking at it

```bash
spot serve
curl -s http://127.0.0.1:PORT/api/health/ready
```

A refused role never reaches the HTTP readiness detail (it is logged at start-up, to the
operator, never to an unauthenticated caller); see the host's own log line, which names the
stable code.
