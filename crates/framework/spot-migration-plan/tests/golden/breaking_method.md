# Release plan: metadata 0 -> 0

- **Risk:** destructive
- **Estimate:** 5s expected (2s low, 10s high), blocking: online
- **Changes:** 1 change (schema 0, metadata 0, patches 0, behaviour 1)

## Destructive (1)
- [behaviour] party.make_payment — Method `party.make_payment` changes its argument schema. (amount: int -> amount: text)
  - why: the change removes, retypes or newly requires an argument
  - instead: add the new argument as optional, or keep the old one
