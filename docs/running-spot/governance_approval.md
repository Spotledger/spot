Part of [Running Spot](../running-spot.md).

# Grant approval (`[services.governance_approval]`)

Connects the grant gate (`governance`) to the process engine: a grant that needs approval starts an approval
Case and waits for the named approver instead of being refused.

| Setting | Default | Meaning |
| --- | --- | --- |
| `enabled` | true | Off: no launcher is set and grants that need approval are refused. |

- The Case is keyed by the grant request id, so a retried launch resumes the same Case.
- Without the gate or the process host the launcher stays unset: the gate refuses with
  `iam_governance.approval_unavailable` and readiness is degraded. Nothing is auto-approved.
- The Decision node's completion calls `governance_approval::complete` with the authenticated user: the named
  approver or escalation target only, never the grantee; a request is decided once; the gate runs again
  before the grant, so an approval does not override a conflict.
