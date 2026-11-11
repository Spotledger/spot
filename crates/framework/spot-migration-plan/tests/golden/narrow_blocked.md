# Release plan: metadata 0 -> 0

- **Risk:** destructive
- **Estimate:** 60s expected (30s low, 120s high), blocking: maintenance window
- **Changes:** 1 change (schema 1, metadata 0, patches 0, behaviour 0)

## Destructive (1)
- [schema] party.amount — The backend refuses a schema change: `party.amount` would narrow from numeric(18,4) to numeric(18,2); narrowing can lose digits (present -> refused)
  - why: `party.amount` would narrow from numeric(18,4) to numeric(18,2); narrowing can lose digits
