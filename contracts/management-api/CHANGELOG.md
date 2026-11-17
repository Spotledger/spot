# Changelog: management API

Every change is an ADR plus a semantic version bump (the contracts-first rule). Additive changes bump the minor version and keep the `/mgmt/v1` prefix.

## 0.2.0 (draft, awaiting architecture-owner approval)

Additive (WP-CON-03.T63). The site operations of the catalogue are named under `x-spot-operations`: `site.backup.run|list|verify`, `site.restore.preflight|apply`, `site.migrate.plan|show|approve|reject|rehearse|apply|hold|release|rescue|audit`, `site.maintenance.enter|leave`, `site.status` (eighteen), each with id, summary, risk, parameters with types and bounds, `requires` (`signature`, `approval:<kind>`), `idempotent`, the equivalent CLI pattern and the audit event.
- **Risks** are the five of `spot-mgmt-ops`: read for list, show, status and audit; `low` or `medium` for the changes; `critical` for restore apply, migrate apply, rescue and maintenance enter.
- **Parameter rules:** `site` is a registered site id; `plan_hash` (hex `PlanDigest`) is required by approve, rehearse, apply and rescue; a backup target is a configured name, never a path; `restore.apply` needs the single-use `confirm` token of `restore.preflight`. Approve and reject need an approver different from the plan author.
- **Result schemas:** `MgmtPlanSummary`, `MgmtRehearsalReport`, `MgmtApplyProgress`, `MgmtAuditRef`.
- `check.py` rule 11 (with three more self-tests). The check that every CLI pattern names a command of `spot-cli` waits for the command registry export of WP-PRD-01.T27 (it does not exist yet).
- **Decisions taken here:** the brief's `change`/`destructive` risk classes map onto the five of the crate (see above); `site.restore.preflight` is `low`, not `read`, because it issues a token; the approval kinds are `approval:restore` and `approval:migration`.
- **compatibility.** A 0.1.0 client is unaffected; the operations are new catalogue entries.

## 0.1.0 (draft, awaiting architecture-owner approval)

First version (WP-CON-03.T60; the architecture owner gave the lead authority over contract changes on 31 Oct 2026 and for 12 Nov 2026). Seven operations and fifteen `mgmt.*` error codes. The native API contract is not changed.
- **Operations:** `mgmt_operation_list`, `mgmt_operation_get`, `mgmt_operation_validate` (dry validation and the equivalent command line), `mgmt_command_invoke` (signed envelope, `202` and a command id), `mgmt_command_get`, `mgmt_command_cancel`, `mgmt_health` (unauthenticated liveness).
- **Transport:** mutual TLS 1.3 only; no bearer token, no password, no cookie; `401` before any body is read. **Signed commands** (`spot-signed-command`) for any risk above `read`; approvals for `high` and `critical`.
- **Audit:** every request that reaches authentication is audited with the reason code of a refusal. Limits: 64 KiB body, at most 4 concurrent commands per site, one exclusive lock for `critical` operations.
- `check.py` (rules 1 to 10, with self-tests). The operations are not served until WP-ADM-01.T4 to T6 land.
- **Decisions taken here:** the risk levels are the five of `spot-mgmt-ops` (`read`, `low`, `medium`, `high`, `critical`), not the three the brief names for the site operations (T63 maps them); three codes beyond the brief's twelve (`mgmt.unknown_command`, `mgmt.rate_limited`, `mgmt.body_too_large`) so every answer is a registered code; the catalogue-export check against `spot-mgmt-ops` waits for WP-ADM-01.T4 (there is no `catalogue_json` example yet).
- **compatibility.** New contract; nothing older exists.
