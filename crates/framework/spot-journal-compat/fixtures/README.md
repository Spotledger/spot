# The frozen journal fixtures

`release-1/` is a journal as the previous release wrote it: two sealed segments in generation 1,
their signed manifest, one signed snapshot at position 5 (with the signed marker that makes it a
verified base), the public half of the key that signed them (`key.pub`), and
`golden-state.json`, the state today's code must rebuild it into.

Nothing here is read by a running system. The crate `spot-journal-compat` loads these bytes into
the journal's in-memory store and rebuilds them with today's planner, executor and upcaster
registry; `tests/rebuild_previous_release.rs` compares the result with `golden-state.json` at the
end of the journal and at every position.

## The bytes are committed on purpose

The fixture stands in for a journal an operator still has and a release that no longer exists. If
the fixture were generated during the test run, it would follow the code: a format change that
made old journals unreadable would regenerate a fixture that reads fine, and the test would stay
green while every real journal written by the previous release broke.

So the bytes are frozen, and `tests/fixture_determinism.rs` asks the generator to produce them
again and compares it with the committed files, byte for byte and file for file. A red determinism
test means one of two things:

* a format change altered the bytes an old journal decodes to — that is a compatibility break,
  and the fix is to keep reading the old form, not to regenerate the fixture; or
* the fixture was edited by hand — it never should be.

## Regenerating

```console
$ cargo run -q -p spot-journal-compat --bin gen_fixture
```

It writes only the files whose bytes differ and prints how many it wrote, so a run against a
fixture that is already current writes nothing.

Regenerate only when the *harness* changes on purpose — a new entry in the script, a new shape
change the releases declare — and say so in the commit. The generator is deterministic: no clock,
no random source, no filesystem read decides anything, so the same code always writes the same
bytes. Time is a constant (`ISSUED_AT_MILLIS`, `TAKEN_AT_MILLIS`), ids and timestamps come from the
entry's position, and the segments and snapshot parts are compressed with the same zstd level every
time.

`cargo run -p spot-journal-compat --bin gen_fixture -- <dir>` writes into another directory, which
is the easiest way to diff a regeneration.

## The signing key is a throwaway

`key.pub` is the public half of the TEST-ONLY Ed25519 key hard-coded in
`src/generator/key.rs`. It signs invented documents in a test fixture; it is never a tenant's
journal key, never leaves this repository, and nothing logs it. There is no secret in the fixture:
`key.pub` is a public key by definition, and publishing the private half would only mean that
anyone could regenerate the same test fixture.

The documents are invented too (`acme`, one account, an invoice, two sales orders). A personal
field appears only as a sealed envelope — opaque JSON with a ciphertext and a key id, no key and no
plaintext — so the fixture also proves a rebuild carries a sealed field through untouched.

## What the journal holds

| position | entry |
| --- | --- |
| 1 | `account` a1 inserted, schema 1 |
| 2 | `invoice` i1 inserted, schema 2 |
| 3 | `sales_order` o1 inserted, schema 1 (`warehouse` in the header) |
| 4 | `account` a2 inserted, schema 1 |
| 5 | `account` a1 updated, version 2 — the snapshot's position |
| 6 | `sales_order` o2 inserted |
| 7 | `account` a1 updated, version 3 |
| 8 | `invoice` i1 updated, version 2 |
| 9 | `sales_order` o1 updated, version 2 |
| 10 | `account` a2 deleted (a tombstone) |
| 11 | `account` a1 updated, version 4 |
| 12 | a domain event (`doc_posted`): counted, not replayed |
| 13 | an audit entry (`sys_role_changed`): replayed |

Positions 1–5 are the first segment; 6–13 the second. The three entities cover the three kinds of
shape change a release can make: a renamed field (`account`, 1 → 2), a dropped field with a
default added (`invoice`, 2 → 3), and a field moved into a child table (`sales_order`, 1 → 2).

## Layout

```text
release-1/
  key.pub                       the TEST-ONLY public key, 64 hex digits
  golden-state.json             the state per position, canonical JSON
  objects/acme/manifest/…       the signed manifest of generation 1
  objects/acme/segments/…       the two sealed segments
  objects/acme/snapshots/…      one signed snapshot: parts, index.json and its verified marker
```

`objects/` mirrors the journal store's key space exactly: the file's path below `objects/` is the
object's key, built by `spot-journal-store`'s `Layout`. The loader refuses a file there that is not
a canonical journal key, so the fixture cannot drift from the layout.
