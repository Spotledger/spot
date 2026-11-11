# Storage semantics: the parity rules

Status: WP-STO-10.T1. These are the rules that the in-memory fake (`spot-storage-fake`) and
PostgreSQL (`spot-postgres`) must both follow, written once, each with a stable id. The matrix
in `crates/backends/spot-storage-parity/cases/` proves every `decided` rule on both backends.

## How to read this

- A rule has an **id** (`SEM-<GROUP>-<NN>`), a **status**, one sentence, and its **source**.
- `decided`: an ADR, the contract's doc comments, the expression language document or the task
  brief states it. Every decided rule has at least one case, and a decided rule with no case
  fails the build.
- `open`: the sources do not say. The sentence is the question for the lead. An open rule has
  cases that record what the backends do today (`same_as_reference`: the answer is whatever
  PostgreSQL gives), so a change in either backend is noticed, but **nobody has decided the
  answer**. The lead decides; the status then becomes `decided` and the case gets a fixed
  expectation.
- A known, accepted difference between the two backends is listed in
  `cases/divergences.toml` with the rule id, a reason and an owner. See "Divergences" below.
- Rows are single lines; a cell never contains a vertical bar.

Sources: `value.rs`, `query.rs`, `schema.rs` are the storage contract's files in
`contracts/storage/src/`. ADR-053 is the storage contract, ADR-069 the (proposed) query
limits, ADR-077 the time model. `expr-lang` is `docs/expression-language.md`. `brief` is the
the task brief.

## Text

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-TXT-01 | decided | Text equality (`=`, `<>`, `In`, `Equals`, and unique indexes) is exact: case-sensitive and accent-sensitive, character by character. | value.rs; ADR-053 text order |
| SEM-TXT-02 | decided | A case-insensitive match (`case_insensitive`) uses Unicode simple case folding: `A` matches `a`, but `ß` does not match `SS` and `İ` is not `i`. | query.rs TextMatch |
| SEM-TXT-03 | decided | Accents are never ignored: `é` and `e` differ in every comparison, case-insensitive ones included. | value.rs; ADR-053 |
| SEM-TXT-04 | decided | Sorting follows the tenant collation, with ties broken by code point so the order is total. | query.rs; schema.rs Collation; ADR-053 |
| SEM-TXT-05 | decided | Range comparisons (`<`, `<=`, `>`, `>=`) on text follow the tenant collation, not byte order. | query.rs; ADR-053 |
| SEM-TXT-06 | decided | Text `Min` and `Max` follow the tenant collation. | query.rs |
| SEM-TXT-07 | decided | Text patterns match literally: `%`, `_` and `\` have no special meaning, for every mode and for case-insensitive matches. | query.rs; ADR-053 |
| SEM-TXT-08 | decided | Text is stored as given: no Unicode normalisation. `é` as one character and `e` plus a combining accent are different values and different unique keys. | expr-lang section on text equality; brief |
| SEM-TXT-09 | decided | Text round-trips exactly: empty, only combining marks, leading and trailing spaces, tabs and line breaks, characters outside the BMP, and long text. | value.rs; brief |
| SEM-TXT-10 | decided | Grouping, `distinct` and unique indexes use exact equality: `Ali` and `ali` are different keys and different groups. | query.rs; schema.rs Collation |
| SEM-TXT-11 | open | Text containing U+0000 (NUL): PostgreSQL text cannot hold it. Should every backend refuse it with `InvalidValue`? | none |
| SEM-TXT-12 | decided | A text length limit counts characters (Unicode scalar values), not bytes; the limit itself is accepted and one more is refused with `InvalidValue`. | schema.rs FieldType::check |
| SEM-TXT-13 | decided | Collation order is by the tenant's locale: `en` and `und` order accented and capital letters differently from code points, and a different locale (`sv`) changes the order of `ä`. | schema.rs Collation; ADR-053 |

## NULL, empty and absent

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-NUL-01 | decided | `Null` and empty text are different values; storage never turns one into the other, and a filter for one does not find the other. | value.rs; ADR-053 |
| SEM-NUL-02 | decided | A comparison with a null operand is not true: the row is filtered out, for `=`, `<>`, `<` and the rest. | query.rs |
| SEM-NUL-03 | decided | Comparing with a literal null is refused with `InvalidQuery`; `IsNull` is the only test for null. | query.rs; ADR-053 shared validation |
| SEM-NUL-04 | decided | `In` never matches a null operand, an `In` list containing null is refused with `InvalidQuery`, and an empty `In` list matches nothing. | query.rs |
| SEM-NUL-05 | decided | Three-valued logic: `Not` of an unknown is unknown, so `Not(In)` and `Not(=)` over a null operand do not return the row. | query.rs |
| SEM-NUL-06 | decided | `And` is true when empty and `Or` is false when empty; `Or(true, unknown)` is true and `And(false, unknown)` is false. | query.rs Expr |
| SEM-NUL-07 | decided | Null placement in a sort is always explicit (`First` or `Last`), in both directions, never backend-dependent. | query.rs Nulls |
| SEM-NUL-08 | open | Do null keys form one group in `group by`, and is a null counted by `count distinct`? SQL groups all nulls together and `count distinct` skips them; the contract says grouping is exact but not how null is grouped. | none |
| SEM-NUL-09 | decided | `Sum`, `Min` and `Max` over no rows give null and `Count` gives 0. | query.rs |
| SEM-NUL-10 | decided | `Count(x)`, `Sum`, `Min` and `Max` ignore null values; `Count()` counts rows. | query.rs AggFunc |
| SEM-NUL-11 | decided | `Coalesce` gives the first non-null operand and null when all are null. | query.rs Expr |
| SEM-NUL-12 | decided | A unique index never conflicts for rows with a null in any key field. | schema.rs IndexDef |
| SEM-NUL-13 | decided | Writes are full: a declared field that is absent from a write is refused with `InvalidValue`; there are no storage-side defaults. | ADR-053 documents |
| SEM-NUL-14 | decided | A null in a non-nullable field is refused with `InvalidValue`. | schema.rs FieldType::check |
| SEM-NUL-15 | decided | In a left join the fields of a missing right row read as null. | query.rs JoinKind |
| SEM-NUL-16 | open | Arithmetic with a null operand: is the result null (SQL)? The contract says arithmetic is on numbers and does not mention null. | none |
| SEM-NUL-17 | decided | A nullable field of every type stores and returns null unchanged. | value.rs |

## Numbers

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-NUM-01 | decided | A decimal written at the column's scale is stored exactly and reads back with the same digits: `1.50` reads `1.50`, `0.00` stays `0.00`, trailing zeros are kept. | value.rs; ADR-053; query_values.rs |
| SEM-NUM-02 | decided | A decimal with more digits after the point than the column's scale is refused with `InvalidValue`, never rounded (`1.005` in scale 2); trailing zeros beyond the scale (`1.500`) are not extra digits and are accepted. | value.rs; schema.rs; ADR-12 |
| SEM-NUM-03 | decided | A decimal that does not fit the column's precision is refused with `InvalidValue`; the largest and smallest values that fit are accepted and returned exactly. | schema.rs FieldType::check |
| SEM-NUM-04 | decided | Decimal comparison is by numeric value, not scale: `1.5` equals `1.50`; ordering is numeric, negatives included. | expr-lang section 1 |
| SEM-NUM-05 | decided | An integer is a full 64-bit signed value: the minimum and maximum round-trip and compare correctly. | value.rs |
| SEM-NUM-06 | decided | An integer and a decimal can be compared and added with each other. | query.rs comparable |
| SEM-NUM-07 | decided | `Sum`, addition, subtraction and multiplication of decimals are exact: no floating point, so `0.1 + 0.2` is `0.3` and a product keeps every digit. | ADR-053; ADR-12; query_values.rs |
| SEM-NUM-08 | open | Integer overflow in `Sum` or arithmetic (`i64::MAX + 1`): which error kind, and must every backend refuse it? | none |
| SEM-NUM-09 | open | What scale does the result of `Sum` and of multiplication have (the sum of the operands' scales for a product)? Only the numeric value is specified. | none |
| SEM-NUM-11 | open | A decimal written with fewer fractional digits than the column's scale (`1.5` into scale 2): is it read back padded to the column scale (`1.50`, as PostgreSQL does) or as written (`1.5`)? Only the numeric value is specified. | value.rs; query_values.rs |
| SEM-NUM-10 | decided | A boolean stores and compares as true or false; `true <> false`. | value.rs |

## Dates and times

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-DAT-01 | decided | A date round-trips exactly across the supported range, years 0001 to 9999, including 0001-01-01, 9999-12-31 and a leap day, and is never shifted by a time zone. | expr-lang section 1; ADR-077 |
| SEM-DAT-02 | decided | Dates order and compare chronologically. | value.rs |
| SEM-DAT-03 | decided | An instant is stored and returned in UTC; an input with an offset is converted and the offset is not kept. | value.rs; ADR-053; ADR-077 |
| SEM-DAT-04 | decided | An instant keeps microsecond precision exactly across the range 0001-01-01T00:00:00Z to 9999-12-31T23:59:59.999999Z. | expr-lang section 1; brief |
| SEM-DAT-05 | open | An instant with sub-microsecond digits: truncate, round or refuse? The expression language truncates `now()`; PostgreSQL rounds. | none |
| SEM-DAT-06 | decided | Instants compare and order by the moment, whatever offset they were written with. | value.rs |
| SEM-DAT-07 | decided | A wall-clock time without date round-trips exactly, 00:00:00 to 23:59:59.999999, with no zone, and orders chronologically. | ADR-077 |
| SEM-DAT-08 | open | A wall-clock time with sub-microsecond digits: truncate, round or refuse? | none |
| SEM-DAT-09 | decided | A date cannot be compared with an instant or a time, or with text: the shared validation refuses it with `InvalidQuery`. | ADR-053 shared validation |
| SEM-DAT-10 | open | Arithmetic on dates or instants (`day + 1`): refuse as `InvalidQuery`, or allow as PostgreSQL does? The contract says arithmetic is on numbers. | query.rs ArithOp |

## JSON

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-JSN-01 | decided | Only valid JSON is stored: text that is not valid JSON is refused with `InvalidValue`. | value.rs |
| SEM-JSN-02 | decided | JSON is equal semantically: key order and insignificant whitespace are not significant after a round trip. | value.rs |
| SEM-JSN-03 | decided | Unicode escapes in JSON text survive a round trip semantically: `é` and `é` are the same document. | value.rs; brief |
| SEM-JSN-04 | decided | A JSON `null` document is a stored JSON value, different from the field being null. | value.rs |
| SEM-JSN-05 | open | A JSON object with a duplicate key (`{"a":1,"a":2}`): which value survives, or is it refused? | none |
| SEM-JSN-06 | open | JSON number forms (`1.0`, `1e2`, a 30-digit integer): are they kept as written or normalised? | none |
| SEM-JSN-08 | open | A JSON string containing the escape for U+0000 (`\u0000`): PostgreSQL `jsonb` cannot hold it. Should every backend refuse it with `InvalidValue`? | none |
| SEM-JSN-07 | open | Is `=` on a JSON field semantic (as PostgreSQL `jsonb` is) or exact text? The contract says equality is exact. | query.rs |

## Bytes

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-BYT-01 | decided | Bytes round-trip exactly: every byte value 0 to 255, empty bytes, and a megabyte. | value.rs |
| SEM-BYT-02 | decided | Empty bytes and null are different values. | value.rs |
| SEM-BYT-03 | decided | Bytes compare for equality exactly. | query.rs |
| SEM-BYT-04 | open | Ordering of bytes (`<`, sort, `Min`, `Max`): unsigned byte by byte, shorter first? The contract names no order for bytes. | none |

## Ids, links and versions

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-IDS-01 | decided | A record id is stored and returned unchanged, and sorting by id is the byte order of the UUIDv7, which is creation-time order. | ADR-053 keys |
| SEM-IDS-02 | decided | Inserting a document whose id already exists fails with `AlreadyExists`. | ADR-053 |
| SEM-IDS-03 | decided | A link to a missing record fails with `ReferenceViolation` by commit at the latest, and links within one transaction can be saved in any order. | ADR-053 link checks |
| SEM-IDS-04 | decided | Deleting a record that is still linked fails with `ReferenceViolation`; deletes never cascade to linking records. | ADR-053 link checks |
| SEM-IDS-05 | decided | A document's version is 1 after insert and rises by one on every update; update or delete with another version fails with `StaleVersion`. | ADR-053 versioning |
| SEM-IDS-06 | decided | Without aggregation, rows with equal sort keys come in id order, so results and pages are deterministic. | query.rs |
| SEM-IDS-07 | decided | A unique index refuses a second row with the same key with `UniqueViolation`, whatever the other fields. | ADR-053; schema.rs |

## Round trips

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-RND-01 | decided | A document written by insert reads back identical through `load`, `query` and `read`: every value kind at its edge values, with JSON compared semantically. | ADR-053 documents; brief |
| SEM-RND-02 | decided | A value written by `update` reads back identical through `load`, `query` and `read`, the same as one written by insert. There is no separate upsert operation in contract v0.1. | ADR-053 documents; brief |

## Child rows

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-CHD-01 | decided | Child rows come back in the order they were written, and their `idx` is the 1-based position. | ADR-053 documents; schema.rs system columns |
| SEM-CHD-02 | decided | An update replaces the child table: the order follows the new list and rows missing from it are gone. | ADR-053 documents |
| SEM-CHD-03 | decided | Child fields obey the same type, scale and nullability rules as parent fields. | ADR-053 documents |
| SEM-CHD-04 | decided | A document with no child rows reads back with an empty child table, and deleting a document deletes its child rows. | ADR-053 documents |

## Queries and limits

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-QRY-01 | decided | Every backend rejects the same invalid queries with the same kind: unknown entity or field, an ungrouped field, a type mismatch, an aggregate in a filter, an empty select. | query.rs validate; ADR-053 |
| SEM-QRY-02 | decided | Paging is stable: `limit` and `offset` apply after ordering, an offset past the end gives no rows, and `limit` 0 gives none. | query.rs |
| SEM-QRY-03 | decided | A transaction's `query` and a `read` outside one return the same rows for the same data. | ADR-053 |
| SEM-QRY-04 | decided | Joins: an inner join drops rows without a match and a left join keeps them. | query.rs JoinKind |
| SEM-QRY-06 | decided | A grouping key may be any expression, and selecting, ordering by or filtering groups on the identical expression is valid. | query.rs Query::validate |
| SEM-QRY-05 | open | Operands of different types in one expression that the validation does not check (`Coalesce` of text and integer, a date field compared with an instant field): refuse with `InvalidQuery` in every backend, or leave to the database? | none |
| SEM-LIM-01 | decided | A transaction that writes more than its `max_writes` (child rows count) fails with `TransactionTooLarge`. | ADR-053 transaction size |
| SEM-LIM-02 | decided | A storage name of 63 bytes is accepted in every backend and derived names (indexes, constraints) never collide or get silently truncated into one. | ADR-053 names |
| SEM-LIM-03 | open | A very long `In` list (70,000 values, above PostgreSQL's 65,535 bind-parameter limit): refuse with `InvalidQuery` at a stated size, or support it? ADR-069 proposes 1,000 and is not accepted. | ADR-069 (proposed) |
| SEM-LIM-04 | open | A very deep or very large expression tree is limited by ADR-069 (proposed, not implemented). Until it is accepted the limit is not a rule. | ADR-069 (proposed) |

## Spatial (geo)

ADR-087 makes `geo` a base type and a spatial filter a separate operator. The **storage** half
(storing and round-tripping a point, validating its coordinates) is modelled by both backends; the
**filter** half needs PostGIS and is tested on PostgreSQL only, as a case scoped `postgres`. The
fake answers `Unsupported` from its `Capabilities` (`geo_filters = false`), never by
approximating, and that answer is asserted by a case scoped `fake`. It is recorded as a
capability, not in `cases/divergences.toml`: a permanent design choice is not an accidental
difference (ADR-087 section 4.5).

The round trip is proven through `load` on both backends. Selecting a geo column in a **query** is
not compiled yet (`spot-pg-query` refuses a geo or range column until WP-STO-02.T9), so on
PostgreSQL that one case asserts the `Unsupported` answer rather than a row; the fake's query path
is asserted by its own unit test (`geo_points_round_trip_exactly_and_are_validated`). The case
expects that exact error, so it goes red — and must become the row assertion — the moment
WP-STO-02.T9 compiles geo columns; it cannot pass silently after the gap closes.

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-GEO-01 | decided | A geo point round-trips exactly at scale 7, and a null point is distinct from a point at 0,0. | ADR-087 section 4; value.rs |
| SEM-GEO-02 | decided | A latitude outside -90..90 or a longitude outside -180..180 is refused with InvalidQuery in a filter and InvalidValue on a write. | ADR-087 section 4.4 |
| SEM-GEO-03 | decided | The fake answers Unsupported for WithinDistance and WithinBox, advertised by Capabilities; it does not approximate. Geo filter cases run on real PostgreSQL with PostGIS only. Decided 27 Oct 2026 (option B). | ADR-087 section 4.5 |
| SEM-GEO-04 | decided | A geo field with no PostGIS available makes prepare fail with InvalidSchema, never a silent text column and never a built-in point fallback, on any tier. | ADR-087 sections 4.2, 4.7 |
| SEM-GEO-05 | decided | order_by on a geo field is refused with InvalidQuery in v1; distance ordering is the client's. | ADR-087 section 4.4 |

## Ranges, exclusion and holds

A range is half-open, `[start, end)`. The fake does not model range values, overlap or an
exclusion constraint at all (ADR-087 section 5.5, option B), so those cases are scoped `postgres`
and the refusal is asserted by a case scoped `fake`; a hold is an occupancy row, so the hold rules
run on PostgreSQL too, and the change-feed rule of a hold runs on both backends.

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-RNG-01 | decided | A range is half-open [start, end): [1,3) and [3,5) do not overlap, [1,3) and [2,4) do. | ADR-087 section 5.1 |
| SEM-RNG-02 | decided | An empty range is refused with InvalidValue on write; it is never stored. | ADR-087 section 5.1; PostgreSQL range types 8.17.5 |
| SEM-RNG-03 | decided | A null range never conflicts with anything, like a null unique key. | SEM-NUL-12; ADR-087 section 5.5 |
| SEM-RNG-04 | decided | A conflicting insert or update fails with ExclusionViolation naming the entity, the constraint and the field. | ADR-087 section 5.6 |
| SEM-RNG-05 | decided | The exclusion is checked at write time and again at commit, by PostgreSQL's constraint; the fake does not offer it (SEM-RNG-08). | ADR-087 section 5.5 |
| SEM-RNG-06 | decided | An equal-but-inclusive range such as [3,3] is refused as empty with InvalidValue in v1; a point in time is booked as a one-unit half-open range such as [3,4). A point type may be added later without touching existing data. | ADR-087 section 5.1 |
| SEM-RNG-07 | decided | An unbounded or infinite range bound is refused with InvalidValue on write. | ADR-087 section 5.1 |
| SEM-RNG-08 | decided | The fake answers Unsupported for range values, Overlaps and exclusion constraints, advertised by Capabilities; range, exclusion and hold cases run on real PostgreSQL only. Decided 27 Oct 2026 (option B). | ADR-087 section 5.5 |
| SEM-HLD-01 | decided | A hold is an occupancy row with a tentative state; the insert is the claim, and an expired hold blocks nothing once its row is deleted. | ADR-087 section 6 |
| SEM-HLD-02 | decided | A confirm whose hold has expired never extends the hold: storage answers NotFound (the contract's update rule for a stored document that no longer exists) and the confirm operation reports it to the caller as StaleVersion (ADR-087 section 6.3 cases 4–5). | ADR-087 section 6.3 case 5; `DocumentStore::update` |
| SEM-HLD-03 | decided | A document created and deleted in one transaction produces no change-feed entry, holds included. | ADR-053 change feed |

## Search

Search is a read projection (ADR-087 section 7). No backend serves it yet: `spot-postgres`
refuses a search declaration at `prepare` until WP-STO-13.T1 builds the projection, and the fake
has no inverted index (SEM-SCH-04). What the storage layer does enforce today is the shared
validation: a search term is accepted only over fields the entity declares searchable, and a rank
is valid only as a whole `order_by` key. The behaviour cases arrive with the projection; until
then the rules that need it are proven by the refusal cases, which fail the moment a backend
starts serving search without semantics to match.

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-SCH-01 | decided | Search matches only over fields declared searchable, and a term narrower than a stop word matches nothing rather than everything. | ADR-087 section 7.6 |
| SEM-SCH-02 | decided | A protected field never enters a search projection, so it cannot affect a match or a rank. | ADR-087 section 7.7 |
| SEM-SCH-03 | decided | SearchRank is refused outside order_by with InvalidQuery. | ADR-087 section 7.6 |
| SEM-SCH-04 | decided | The fake answers Unsupported for Search and SearchRank, advertised by Capabilities; it has no inverted index. Search cases run on real PostgreSQL with pg_trgm and unaccent only. Decided 27 Oct 2026 (option B). | ADR-087 section 7.9 |
| SEM-SCH-05 | decided | A search naming a protected field is refused with permission.field_denied in v1. | ADR-087 section 7.7 |

## Extensions

An extension is created only by the audited `extension.install` operation, through the Admin
Center database manager (ADR-087 section 4.3); a migration and a request never create one. The
operation itself is WP-STO-12.T2, so the parity suite stands in for it: a case declares the
extensions its scratch database needs, the harness creates them the way the operation would, and a
case that asserts what happens **without** an extension declares none. The extension cases need a
server that offers `postgis`, which `tools/ci/start-postgres.sh` installs and creates; a runner
without it fails, naming the package, and never skips.

| id | status | rule | source |
| --- | --- | --- | --- |
| SEM-EXT-01 | decided | extension.install runs the tenant privilege check, version compatibility, the allow-list check and a dry run before the real install, writes an audit record, and returns a typed error when the privilege is missing. | ADR-087 section 4.3 |
| SEM-EXT-02 | decided | An extension outside the platform allow-list is refused with migrate.extension_not_allowed. | ADR-087 section 4.3 |

## Divergences

A divergence is a case that a backend does not pass: the answer differs from the rule's
expectation, or the fake differs from PostgreSQL for a `same_as_reference` case. A known,
accepted one is listed in `cases/divergences.toml` with the rule, a reason and an owner. The
runner reports a listed divergence without failing, fails on an unlisted one, and fails on a
listed one that no longer differs, so the register cannot rot.

## Changing the rules

Adding a decided rule without a case fails the build. A rule's status changes only by the
lead's decision; an implementer never turns an `open` rule into a `decided` one, and never
loosens an expectation to make a backend pass.
