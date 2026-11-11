# spot-storage-contract changelog

Every change is an ADR plus a semantic version bump (blueprint §4, contracts-first rule).

## 0.3.0 (draft, awaiting architecture-owner approval)

Additive amendment for the document engine's command ledger (WP-DOC-04.T8; approved by the architecture owner 5 Nov 2026, with configurable per-class retention). Nothing in 0.2.0 changes meaning.

Added:

- **`Transaction::command_get(principal, command_id) -> Option<CommandRecord>`**, **`command_put(CommandRecord)`** (a second insert of the same key is `UniqueViolation`; the record is validated first) and **`command_prune(before) -> u64`**.
- **`CommandRecord { principal, command_id, fingerprint, outcome, recorded_at, retain_until }`** (module `commands`; `outcome` is JSON text, at most 16 KiB; keys at most 255 bytes) with `CommandRecord::new` and `validate`.
- **Retention rule.** Retention is a per-site setting with per-caller-class overrides, never a constant in a backend or in the library. The writer computes `retain_until` per row at write time from the policy of the caller's class (default 7 days, clamped to a configured maximum), so changing the setting never shortens or extends an existing row. `command_prune` deletes by `retain_until` only, rows with `retain_until <= before`. Neither the contract nor a backend reads the wall clock: `recorded_at`, `retain_until` and `before` are passed in.

Compatibility: the three methods have **default implementations that return `ErrorKind::Unsupported`**, so every existing implementer of `Transaction` (`spot-storage-fake`, `spot-postgres`, the test wrappers) still compiles unchanged. WP-DOC-04.T8 overrides them in the fake and in PostgreSQL. The contract test suite does not yet include ledger cases (they need a backend that implements it); T8 adds its own tests. `CONTRACT_VERSION` is `0.3.0`.

## 0.2.0 (draft, awaiting architecture-owner approval)

Additive amendment of ADR-053 and ADR-068, exactly the additions of ADR-087 section 8 (WP-CON-06.T1). Nothing in 0.1.0 changes meaning; the ADR texts (053, 068, 069) are not edited, and this note is the record of the amendment. `Value` still has no floating-point variant.

Added:

- **Values.** `Value::GeoPoint(GeoPoint)` (WGS-84 longitude and latitude as `Decimal`, at most 7 decimal places, longitude `-180..=180`, latitude `-90..=90`) and `Value::Range(RangeValue)` (decimal, date, datetime and integer payloads only; half-open `[start, end)`; empty, unbounded, not-half-open and `[3,3]` ranges are `InvalidValue`).
- **Field types.** `FieldType::Geo { srid }` (4326 only in v1) and `FieldType::Range { of: RangeOf }` (`Int`, `Decimal`, `Date`, `DateTime`).
- **Catalog.** `IndexDef.method` (`IndexMethod::BTree`, `Gist`, `Gin`), `IndexDef.opclass` (closed set `OpClass`), `IndexDef.include`; `EntityDef.exclusions` (`ExclusionDef`) and `EntityDef.search` (`SearchDef`, `SearchField`, `SearchWeight`). `IndexDef::btree` builds the plain index. Catalog validation refuses a non-4326 SRID, an exclusion on a non-range field or with a bad scope, a search declaration over non-text fields or over more than `MAX_SEARCH_FIELDS`, a unique non-B-tree index, a geo or range field under a non-GiST index, and a mismatched operator class.
- **Query IR.** `Expr::Geo` (`GeoTest::WithinDistance`, `WithinBox`), `Expr::Range` (`RangeTest::Overlaps`, `Contains`, `ContainsRange`), `Expr::Search` and `Expr::SearchRank` (`SearchTerm`, `SearchMode`; `SearchRank` is valid only as a whole `order_by` key). `Query::validate` checks the operand type, coordinates, radius, half-open non-empty tested ranges and their span, typed bounds (a date is never compared with an instant), declared search fields and term size, and refuses `order_by` on a geo field and `Sum`/`Min`/`Max` over a geo or range field. `Query::constructs()` reports which constructs a query uses.
- **Errors.** `ErrorKind::ExclusionViolation` (`storage.exclusion_violation`), naming entity, constraint and field. Storage never carries the conflicting record's id (it cannot know permission); the layer above adds it only when the caller may read that record.
- **Limits** (new module `limits`, configurable defaults, not measured): `MAX_GEO_RADIUS_M = 500_000`, `MAX_RANGE_SPAN_DAYS = 36_600`, `MAX_SEARCH_TERM_CHARS = 512`, `MAX_SEARCH_FIELDS = 16`, plus `GEO_SCALE = 7` and `GEO_SRID = 4326`. The five ADR-069 limits are unchanged.
- **Capabilities.** `geo_filters`, `range_exclusion` and `search`, with `Capabilities::check_catalog` and `Capabilities::check_query`, so a backend answers `Unsupported` by capability (option B), after shared validation. A backend that sets a flag to `false` refuses a range field or exclusion (`range_exclusion`), a search declaration (`search`), and a query that uses the construct; geo fields are always storable.
- **Contract tests.** 13 construct cases (`CONSTRUCT_CASES`), so the suite is 43 cases (30 + 13); `geo_catalog`, `range_catalog`, `search_catalog`; `REAL_CONSTRUCT_CASES` lists the cases whose real branch runs on PostgreSQL only.

Changed for implementers (source-compatible only for code that does not build these types with struct literals): `IndexDef` gains three fields, `EntityDef` two, `Capabilities` three. Build an index with `IndexDef::btree`. `CONTRACT_VERSION` is `0.2.0`.

## 0.1.0 (draft, awaiting architecture-owner approval)

First version (WP-CON-01.T1, ADR-053): backend and transaction traits, documents with child rows, catalog with per-tenant collation, deferred link checks, typed query IR with validation, graph traversal, row and guard locks, gapless sequences, a transactional outbox and change feed published with safe bookmarks and causal order, typed errors, and the contract test suite.
