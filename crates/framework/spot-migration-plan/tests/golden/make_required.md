# Release plan: metadata 0 -> 0

- **Risk:** needs review
- **Estimate:** 2s expected (1s low, 4s high), blocking: brief lock
- **Changes:** 1 change (schema 0, metadata 1, patches 0, behaviour 0)

## Needs review (1)
- [metadata] party.name — Field `party.name` becomes required. (false -> true)
  - why: existing records may have no value for it
  - instead: add it optional, backfill it, then make it required in a later release
