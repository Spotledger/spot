# Release plan: metadata 0 -> 0

- **Risk:** additive
- **Estimate:** 5s expected (2s low, 10s high), blocking: online
- **Changes:** 1 change (schema 0, metadata 0, patches 0, behaviour 1)

## Additive (1)
- [behaviour] party validate_save erp_selling:check_credit_limit — Step `erp_selling:check_credit_limit` is added at `validate_save` of `party`. (absent -> registered)
