Part of [Running Spot](../running-spot.md).

# Names and numbers of documents

This page has two readers. The **app author** writes `naming = "INV-.YYYY-.#####"` in a DocType
and wants to know exactly what comes out. The **operator or auditor** wants to know exactly when a
number can be lost, reused or skipped. It is checked against the code by
`tools/docs-check/check_naming_guide.py`: every `naming.*` code, the grammar keywords and every
example name below are verified by that script, and the example names were computed by it.

## For app authors

A DocType's `naming` property decides how a new document gets its name. The name is written into
the DocType's `title_field` inside the create transaction. There are four strategies:

| Pattern | Strategy | The name |
| --- | --- | --- |
| a pattern with a counter, such as `INV-.YYYY-.#####` | series | prefix, date parts and a zero-padded number from a series |
| a lone `field:<name>` | field | the value of that field, which must be a legal name |
| a lone `hash` | hash | a short id derived from a new UUIDv7 (Crockford base32) |
| the text `prompt` | prompt | the name the caller supplies (`naming.prompt_required` when none) |

### The grammar

<!-- grammar: the parts of a series pattern; checked against crates/framework/spot-plan/src/naming.rs -->

| Part | Syntax | Meaning | Notes |
| --- | --- | --- | --- |
| year | `.YYYY` or `.YY` (also `.Y`, `.YYY`) | the year of the business date | `YYYY` prints the year, a shorter part prints its low digits (`.YY` of 2026 is `26`) <!-- part:Year --> |
| month | `.MM` (or `.M`) | the month, always 2 digits | <!-- part:Month --> |
| day | `.DD` (or `.D`) | the day of the month, always 2 digits | <!-- part:Day --> |
| counter | `.#####` | the number from the series, zero-padded to the number of `#` | 1 to 20 `#`; a number wider than the pattern **widens**, it is never cut <!-- part:Counter --> |
| field | `field:<name>` | the value of a field | only as the whole pattern (strategy field); letters, digits and `_` in the name <!-- part:Field --> |
| hash | `hash` | a content-free short id | only as the whole pattern, as a whole word <!-- part:Hash --> |
| literal | any other text | printed as it is | a `.` that is not followed by `Y`, `M`, `D` or `#` is literal text <!-- part:Literal --> |

Five worked examples. Each line is `pattern @ business date counter N -> name`.

```naming-example
INV-.YYYY-.##### @ 2026-03-01 counter 42 -> INV-2026-00042
SO-.YY.MM-.#### @ 2026-11-05 counter 7 -> SO-2611-0007
REC-.YYYY.MM.DD-.### @ 2026-02-03 counter 5 -> REC-20260203-005
JV-.##### @ 2026-01-01 counter 123456 -> JV-123456
.#### @ 2026-05-05 counter 1 -> 0001
```

The fourth line shows widening: a 5 digit counter that reaches 123456 prints 6 digits.

### The series key and when a series resets

A series is addressed by its **resolved prefix**, date parts included, plus an optional scope key
(a company, a branch, a till). So `INV-.YYYY-.#####` has the key `INV-2026-` in 2026 and starts
again at 1 on the first create of 2027. A pattern with **no** date part has one key for ever and
never resets. A pattern that is only a counter has the key `n`. A key is at most 140 bytes; a
longer derived key is replaced by a stable hash with the prefix `h:`, so a long pattern still
numbers from one counter (`naming.series_key_invalid` when the key holds a control character).

The date parts come from the invocation's instant in the **site's business zone** (ADR-077), never
from the server clock or the server's local zone. A site without a configured zone is dated in UTC.

### What a pattern cannot say today

- **No fiscal year part.** `.FY` is not in the pattern grammar (the text would be kept as a literal),
  so a fiscal-year series cannot be written. The Foundation fiscal calendar comes later, in the
  build order.
- A pattern that mixes `field:` or `hash` with other text, or literal text with no counter, is not
  one of the four strategies and is refused (`naming.pattern_invalid`).
- A name is 1 to 140 bytes of printable text with no leading or trailing whitespace and no control
  character (`naming.invalid_name`).

## What is guaranteed

The number is taken from `spot_sequence`, one row per series, **inside the document's own
transaction**. So:

- committed numbers of a series are exactly `1..=n`: no gaps and no duplicates;
- a create that rolls back (a validation refusal, a storage error, or the rerun of a retryable
  transaction) returns its number to the series;
- two concurrent creates never get the same number, and different series never wait for each other.

Where this is proved: `crates/framework/spot-naming/tests/gapless_pg.rs` (200 sessions on
PostgreSQL), the engine's naming tests (`crates/framework/spot-document/tests/naming.rs`), and
the L7 invariant `naming-gapless` of the engine harness (`harness/rust/l7-engine`), which checks
that the committed allocations of a run are consecutive. What they do not prove: that nobody
deleted a document afterwards, or edited the database by hand.

## What can still happen

- **A deleted document leaves a gap in the visible names**, although the allocation was gapless.
  The lifecycle deletes drafts (and documents of DocTypes that are not posted), so a posted
  document is never deleted; the number of a deleted draft is not given out again.
- **A cancelled document keeps its name and number.** Cancelling changes the state, not the name.
- **A rename of a series- or hash-named document is refused** (`document.name_not_renameable`). A
  name that a caller or a field supplied is renamed through the engine, which refuses a name another
  document of the DocType holds (`document.duplicate`) and takes a lock on the name so two writers
  cannot both pass the check.
- **Restoring a backup** restores the series with the data, because the series is a table of the
  same database. Restoring only some tables would not. See the backup guide when it lands (there is
  no backup page on this base yet).
- **Copying a site's database to another site** shares no series: the copy continues from the
  numbers it holds.

## Gap policies

The allocator is gapless under rollback whatever the policy. The policy decides what a caller may
deliberately skip:

| Policy | Meaning |
| --- | --- |
| `Tolerant` | numbers may be skipped (`skip`) and reserved |
| `GaplessStrict` | `skip` and `reserve` are refused; the name is committed last, through a ticket |
| `Reserved(n)` | a block of `n` numbers is taken for a bulk operation |

The document engine names every document under **`GaplessStrict`**. The refusals: `skip` on a strict
series is `naming.skip_refused`, `reserve` on a strict series is `naming.reserve_refused`, a policy
that does not fit the operation is `naming.policy_unsupported`, and consuming more numbers than a
reserved block holds is `naming.block_exhausted`.

## Operator view

The allocator and the namer can read a series (`Namer::peek`, which allocates and rolls back) and
move it forward (`Namer::set_next`), but **there is no supported command** that shows or sets a
series yet. Do not edit the `spot_sequence` table by hand: it is not an interface. The platform
refuses to set a series back (`naming.series_backwards`): a number already handed out, committed or
not, would be handed out again, and two documents would share a number.

## Errors

| Code | Cause | What the author or operator does |
| --- | --- | --- |
| `naming.unknown_doctype` | the registry holds no compiled naming plan for the DocType | give the DocType a naming property |
| `naming.invalid_name` | a value is not a legal record name | 1 to 140 printable bytes, no leading or trailing whitespace |
| `naming.missing_field` | the pattern reads a field the document has not set | set the field before naming |
| `naming.duplicate` | the generated name is used by another document (users see `document.duplicate`) | choose another name, or make the field unique |
| `naming.prompt_required` | the DocType is named by its caller and no name was given | supply the name |
| `naming.pattern_invalid` | the plan is not one of the four strategies | use a series pattern, `field:<name>`, `hash` or `prompt` |
| `naming.series_key_invalid` | the derived series key is not legal | remove control characters from the pattern text |
| `naming.series_backwards` | the next number would hand a number out again | set the next number at or past the last allocated one |
| `naming.skip_refused` | a strict series refuses to skip | do not skip; cancel the document |
| `naming.reserve_refused` | a strict series refuses a reserved block | allocate one at a time, or use the tolerant policy |
| `naming.policy_unsupported` | the gap policy does not fit the operation | reserved policy with reserve, other policies with name_for |
| `naming.block_exhausted` | a reserved block was used past its end | reserve enough numbers for the bulk operation |
| `naming.storage` | the storage allocator failed | read the wrapped storage error; retry when it is retryable |
