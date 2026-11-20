Part of [Running Spot](../running-spot.md).

# Agent identity and tiers

When an AI agent connects through the MCP endpoint (`docs/running-spot/mcp.md`) it is held to a
**tier** and, if it belongs to the support organisation, to a **support session**. This guide is for the
operator. The rules are in `crates/framework/spot-agent-tiers` and `spot-support-session`; this module
hosts them and makes them durable.

Level: **optional**. If its three tables cannot be created, readiness says `degraded: agent_tiers (...)`
and, because an agent path without a tier source would be unguarded, `/mcp` answers `503` until the
site restarts healthy. With `[services.mcp] enabled = false` or `[services.agent_tiers] enabled = false`
the module does nothing.

## Settings

`[services.agent_tiers]`:

| Key | Default | Meaning |
| --- | --- | --- |
| `enabled` | `true` | Host the tier service. |
| `unattended_policy_open` | `false` | The M9 gate. While `false` **every agent runs at T0**: it explains and reads, and anything that changes data is refused, whatever tier the agent is registered for. Only the M9 gate task flips it. |

## Who is an agent

A signed-in user is an agent exactly when a row of `spot_agent_tier_registration` names its user id. The
tier is that row's `max_tier`, capped by the policy above; **a header, a tool argument or a token cannot
raise it**. A user with no row is a person, with a person's rights (the call is not tier-checked).
If the lookup itself fails, the caller is treated as an agent at T0, never as an unchecked person.

Registered agents are kept in three tables of the site's database, created at start:
`spot_agent_tier_registration` (agent id, owner, domain `support` or `operations`, `max_tier`, `deep`,
`status` `active|suspended|retired`), `spot_agent_tier_support_session` (the grant as JSON) and
`spot_agent_tier_event` (every registry change and every refusal). Everything is scoped by site.

## What is refused, and how you see it

Before every MCP tools/call request, the gate refuses, in this order, with a permission error and one stable code:

1. `agent.suspended` / `agent.retired`: the agent may not act at all, from its next call.
2. `agent.above_tier`: the tool needs more than the agent's tier. Tools are classified by name: reads
   (`search_*`, `get_*`, `query`, `job_status`) need T0, drafts and updates T2, function calls and
   anything unknown T3.
3. For a **support** agent only: no live session covers the call. `session.none` (no session at all),
   or the support session's own refusal: `session.wrong_tenant`, `session.not_yet_active`,
   `session.expired`, `session.revoked`, `session.above_ceiling`, `session.action_not_in_scope`,
   `session.doctype_not_in_scope`, `session.write_in_read_only_scope`. A session scope writes the DocType
   as named in metadata (`Sales Invoice` matches the tool `get_sales_invoice`) and the action as `read`,
   `create`, `update` or `call`.

Every refusal is appended to the site's `mcp-<site>` audit chain as `agent.refusal` (agent, the tier the
tool needed, the tool, the code; never the arguments) and to `spot_agent_tier_event` as `agent.refused`. If the
chain cannot be written the refusal stands and an error is logged.

## Support sessions

A session is scoped to a tenant, named DocTypes and actions, a tier ceiling and a window, approved by
someone other than the requester. The window is read from the stored times at each call (no timer to fail),
extension is a **new approval** (`TierService::extend_session`; the policy limits the count and the total
length), and revocation (`revoke_session`) is final and read by the next call. A session whose tenant is not
this site never opens this site.

## What this does not do

T1/T2 unattended execution (milestone M9) and break glass (cluster C) are not wired here. Registering and
revoking agents and sessions is the service API (`TierService`, offered as a port); an admin screen over it
is a later task.
