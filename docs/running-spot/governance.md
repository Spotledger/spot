Part of [Running Spot](../running-spot.md).

# Grant governance (`[services.governance]`)

Provides the port `GrantGuard`: the one function every role-grant path calls. It decides with
`spot-iam-governance` over the live directory.

| Setting | Default | Meaning |
| --- | --- | --- |
| `enabled` | true | Off: no port is provided. |
| `delegation` | crate default | Whether a grantor is limited to the roles and scopes they hold. |
| `approval` | none | `roles`, `duties`, `approver`, `escalate_to`, `sla_secs`: what needs approval and by whom. |

- Rules and catalogue: each installed app may ship `sod_rules.toml` and `role_catalogue.toml`. A file that
  does not parse makes every decision `iam_governance.snapshot_failed` (fail closed) and degrades readiness.
- Outcomes: allowed, blocked (codes and rule ids), needs override, pending approval, unavailable.
- Approval needs a provided launcher (WP-IAM-08.T11); without one a grant that needs approval is refused
  with `iam_governance.approval_unavailable`.
- Audit target `spot::audit::governance`: request id, outcome and codes; never a user name.
- Pending requests and overrides are held in memory in this version.
