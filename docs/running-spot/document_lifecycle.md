Part of [Running Spot](../running-spot.md).

# The document lifecycle port

The `document_lifecycle` module (`ORDER` 1262) lets a remedy view, cancel, amend, recompute or
repost a document through the document engine, **as the remedy's own actor**. It provides the port
the world's documents part (`world_access/documents.rs`, see [World access](world-access.md)) is
written over, so the `document.*` remedies run on a live site instead of answering `no documents`.
It starts below the world composer (1265) and the remedy host (1270), which read the port.

The module never writes a document row. Every change goes through the document engine behind the
same guarded wrapper the native API and the MCP server use, so the engine's permission check,
validation, field guard, link checks and audit apply exactly as for that user acting directly.

## Settings

`[services.document_lifecycle]`

| Key       | Default | Notes                                                                  |
|-----------|---------|-------------------------------------------------------------------------|
| `enabled` | `true`  | `false` provides no port; the documents part then reads `no documents`. |

Unknown keys are refused.

## A document reference

The world names a document by one text, `<doctype>/<record id>`, for example
`sales_invoice/0190a7c2-5b1e-7c3a-9d4e-1f2a3b4c5d6e`. A reference that does not parse is "no such
document".

## Who acts

The actor is a user record id, with or without an `agent:` or `user:` prefix: an agent is a user
record whose assignments are its role set. The invocation carries that identity, so the actor's own
permission decides. An actor that is not a record id is refused (`the actor is not a known user`).
The platform's system principal looks at a document (state and the financial flag, never a value)
but never changes one.

## What it reads

`view` answers two facts, never a field value: the lifecycle state (`draft`, `submitted` for a
posted document, `cancelled`; a DocType with no lifecycle reads `draft`) and whether the record is
**financial** (the DocType has a field classified `financial`). A document that does not exist on
this site, or whose DocType is unknown, is "no such document"; the engine is the site's own, so
another site's document is never visible.

## Operations and what each refuses

| Operation      | What it does | Refuses |
|----------------|--------------|---------|
| `Cancel`       | The engine's cancel of a posted document. The remedy's reason is not stored by the engine. | A draft or a cancelled document (`the document is not in a state that allows this`); a user who may not cancel (`the actor may not change this document`). |
| `Amend`        | The engine's correct: cancels the document and creates its amended draft. The answer is the original's state, `cancelled`. | As cancel. |
| `Recompute`    | Runs the DocType's `field_changed:<field>` steps on the document the actor loads; if the field's value changed, saves **only that field** through a normal update. A value that did not change writes nothing, so a second recompute is a no-op. | A field the type does not have; a **protected** field (one in a protection group), because a value computed from masked inputs must never be written; the hard floor. |
| `Repost`       | For a **non-financial** document, an update with no change, which runs the save steps (validation, link checks, hooks) again. | A financial record, always (`a financial record is never reposted`); the hard floor. |
| `Relink`       | Not supported: the engine validates links on every save but has no operation that re-resolves one. | Always `not supported by the document lifecycle`. |
| `JournalEntry` | Not supported: a journal entry is not a document lifecycle call. | Always `not supported by the document lifecycle`. |

**The hard floor.** A submitted financial record is never edited: only cancel and amend pass. The
world part refuses first; the module refuses again with the same text (`a submitted financial
record changes only through cancel, amend or a journal entry`).

## Refusals carry no data

Every refusal is a fixed text chosen from the engine error's class alone (`permission denied`,
`not found`, `stale version`, `invalid state`, `validation`, `linked`, anything else): no field
value, engine message or document name beyond the id appears in an error. A log line carries the
actor id, the document reference and the operation's stable name.

## Tests

`tests/document_lifecycle.rs` runs the module over an in-memory document service that applies a
user's rights and a field mask like the guarded engine does, and covers the operations above, the
fixed refusals, an unknown actor, another site's document and the documents part running a remedy
as a rehearsal and for real. It needs no PostgreSQL.

## What is proven where

The module's own logic (the hard floor, the fixed refusals, the actor mapping, which engine call each
operation makes) is proven in `tests/document_lifecycle.rs` over the fake service, without
PostgreSQL. What the **engine** enforces is proven in `tests/document_lifecycle_engine.rs`, which
needs PostgreSQL (`SPOT_TEST_PG_URL`): on a running site, over the real `Engine` and `GuardedEngine`,
a user with the right cancels and one without it is refused (so the actor reaches the engine as that
user and never as the platform), a draft cannot be cancelled, amend creates the amended draft and
answers the original's state (`cancelled`), the hard floor, `relink` and `journal entry` answer their
fixed texts, a document of another site is invisible, a protected field is never recomputed and is
neither returned nor written for an actor the field guard hides it from, a repeated recompute writes
nothing, and the documents part's rehearsal leaves the engine untouched while its real run changes
it. Not proven on the real engine: a recompute that **writes** (it needs a DocType with a
`field_changed:<field>` step that changes its own field, and no shipped app declares one); the fake
service covers the write path.

Amend answers `cancelled` (the original's state), while the world fixture's own amend leaves the
document `submitted`; the real-engine test asserts what the engine does.
