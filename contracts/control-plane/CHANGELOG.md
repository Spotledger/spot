# Changelog: control-plane API

Every change is an ADR plus a semantic version bump. Additive changes bump the minor version and keep the `/cp/v1` prefix.

## 0.1.0 (draft, awaiting architecture-owner approval)

First version (WP-CON-03.T61; the architecture owner gave the lead authority over contract changes on 31 Oct 2026 and for 12 Nov 2026). Thirty-four operations and fifteen `cp.*` error codes.
- **Operator** operations (`/cp/v1/...`, mutual TLS plus the signed command envelope for mutations): tenants (create via provisioning workflow, get, list, suspend, resume, two-step deprovision), workflows, placements, clusters, moves, licences, entitlements, health, usage, quotas, support grants (need the customer-approved record), rollouts.
- **Tenant host** operations (`/cp/v1/host/...`, mutual TLS with the enrolled host certificate): `cp_host_heartbeat`, `cp_host_usage` (idempotent by event id), `cp_host_licence_get`.
- **Rules:** the control plane never reads tenant business data; scope is not an oracle (`cp.not_found`); idempotency keys on every create; resumable workflows; deprovisioning is two-step and respects the licence retention period; telemetry is bounded.
- `check.py` (rules 1 to 10, with self-tests). The operations are not served until the control-plane service tasks land.
- **Decisions taken here:** the signed envelope rides in the `X-Spot-Command` header (it binds the body by digest) so the body stays the operation's own; `cp.limit_exceeded` is `403`; `cp.cardinality_rejected` is `422`; the agent poll and result report are in the agent-channel contract (T62).
- **compatibility.** New contract; nothing older exists.
