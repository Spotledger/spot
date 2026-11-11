# Release plan: metadata 0 -> 0

- **Risk:** destructive
- **Estimate:** 4s expected (2s low, 8s high), blocking: maintenance window
- **Changes:** 2 changes (schema 0, metadata 2, patches 0, behaviour 0)

## Destructive (1)
- [metadata] party.old_name — Field `party.old_name` is removed. (Text -> absent)
  - why: removing a field discards its values and may be a rename without a hint
  - instead: if this is a rename, add a rename hint; otherwise stop using the field first and drop it later

## Additive (1)
- [metadata] party.new_name — Field `party.new_name` is added. (absent -> Text)
