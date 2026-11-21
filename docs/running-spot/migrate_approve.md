Part of [Running Spot](../running-spot.md).

# Approving a migration plan: `spot migrate-approve`

```
spot migrate-approve approve HASH --site S --as USER [--role ROLE] [--comment TEXT]
spot migrate-approve reject  HASH --site S --as USER [--role ROLE] [--comment TEXT]
```

An approval is a signed-off decision on **one plan, by its hash**. It cannot be moved to another
plan, and a plan changed in the store (`migration.plan_changed`) can be neither approved nor
rejected.

## Rules

- **Separation of duties.** The approver, and the rejecter, must not be the plan's author
  (`migration.self_approval`). A plan whose author is unknown cannot be approved.
- **The role.** The decision is recorded under the role the site policy requires for the plan's risk
  class (`release_manager` by default). The person must hold it.
- **How many.** The policy decides. With no policy set the strictest one applies: two different
  approvers for every plan, and a hardware signature for destructive ones. A site that wants
  looser rules sets its approval policy explicitly (`additive` may auto-approve; `needs_review` one
  approver; `destructive` two).
- **Who.** A person, never the system. On the command line `--as` is stated by the operator at the
  host; the management API authenticates the approver with a signed command, which is what makes
  separation of duties hold against a dishonest operator.
- **Audit.** Every decision is recorded in the audit before it is stored; if it cannot be
  audited, it is not taken.

When enough distinct approvers have approved, the plan becomes `approved` and can be rehearsed.
A rejection closes the plan (`rejected`); plan again to propose a new one.

Exit codes: 0 done, 1 usage, 2 refused by policy or permission, 3 failed.
