# Tail chunk fixtures (ADR-061 Appendix A)

One tail chunk of each format version, with the decode it must produce. A second
implementation, and any future change of `spot-journal-relay`, tests against these bytes.

| File | What it is |
|---|---|
| `v1-gen1-batch1.chunk` | A version 1 (`SPOTJTL1`) chunk: plaintext lines and a SHA-256 checksum. Read-only format: the relay never writes it again. Needs no key. |
| `v1-gen1-batch1.expected.txt` | The object key, version and the three entries the chunk decodes to. |
| `v2-gen1-batch1.chunk` | A version 2 (`SPOTJTL2`) chunk: AEAD lines and one MAC over the bytes and the object key. Sealed under the public test keys of `TailKeys::for_tests("acme")` (never a real tenant key). The three entries are the same as in version 1. |
| `v2-gen1-batch1.expected.txt` | The object key, tenant, generation, batch id, entry count, first and last sequence, the SHA-256 of the chunk (`content_sha256`) and the three entries. |

`tests/fixtures_current.rs` fails when a file is missing, when the v2 chunk does not match the
digest in its expected file (one of them was edited by hand), or when either chunk stops decoding
to the entries listed.

## When regeneration is legitimate

Only after a deliberate change of the chunk format, which is a change of ADR-061 Appendix A and a
decision for the architecture owner. The version 2 sealed form carries random nonces, so the bytes
differ on every run. Regenerate with

```
cargo test -p spot-journal-relay --test fixtures_current write_v2_fixture -- --ignored
```

and commit the chunk and its expected file together, in the same change that explains the format
change. Never regenerate to make a failing test pass: a v2 fixture that stops decoding means a
reader change broke the bytes already written.

The version 1 files never change: that format is frozen.
