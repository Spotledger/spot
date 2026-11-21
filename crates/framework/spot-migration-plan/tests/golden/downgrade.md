# Release plan: metadata 0 -> 0

- **Risk:** destructive
- **Estimate:** 60s expected (30s low, 120s high), blocking: maintenance window
- **Changes:** 2 changes (schema 0, metadata 0, patches 2, behaviour 0)

## Destructive (1)
- [patches] 2026_01_rename_price_list — Patch `2026_01_rename_price_list` is applied but absent from the new system (a downgrade). (applied -> absent)
  - why: a downgrade must be requested explicitly and may need a down-step
  - instead: keep the patch in the new system, or provide a down-step before removing it

## Additive (1)
- [patches] 2026_02_add_note — Run patch `2026_02_add_note` (intent: schema). (not applied -> applied)
