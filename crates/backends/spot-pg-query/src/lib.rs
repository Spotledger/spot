//! Compiles the storage query IR to parameterised PostgreSQL (WP-STO-02.T1; ADR-45, ADR-053).
//!
//! [`compile`] takes a validated [`Query`](spot_storage_contract::Query) and the
//! [`Catalog`](spot_storage_contract::Catalog) and returns a [`CompiledQuery`]: SQL text with
//! numbered placeholders and typed [`Param`]s. It is pure code with no I/O; WP-STO-03 binds and
//! runs the output and checks it against the contract suite on a real database. The output has
//! the semantics the contract fixes, which the in-memory fake implements.
//!
//! # Rules
//!
//! - **Values never reach the SQL text.** Every value, including null, is a `$n` parameter, and
//!   limit and offset too. Identifiers come from the validated catalog through a
//!   [`SchemaMap`], and are always double-quoted. The SQL text does not depend on any value.
//! - **Three-valued logic** is SQL's own. `NOT (a = $1)` stays unknown when `a` is null; no
//!   rewrite is applied to conditions.
//! - **Numbers.** An `Int` meeting a `Decimal` is cast to `numeric`. Integer arithmetic and
//!   `SUM` overflow raise `22003`, which maps to `InvalidValue`, as in the fake. Decimal
//!   results stay `numeric`; the executor's decoder refuses a value that does not fit
//!   `rust_decimal` with `InvalidValue` (see [`ColumnType`]).
//! - **Text is exact or collated.** Equality, `In`, grouping, `COUNT(DISTINCT …)` and matching
//!   compare under `COLLATE "C"`. Ranges (`<`, `<=`, `>`, `>=`), `ORDER BY` and text
//!   `MIN`/`MAX` use the tenant collation, named by [`collation_name`] (`en` → `spot_en`),
//!   which WP-STO-03 creates **deterministic**, so ties break by code point. `ORDER BY` adds a
//!   second key `COLLATE "C"` on the same expression to make that explicit.
//! - **Text matching is literal**, without `LIKE`: `starts_with`, `strpos`, and `right` with
//!   `char_length`, so `%`, `_` and `\` need no escaping.
//! - **Case-insensitive matching** compares `lower(…)` of both sides. `lower` runs under the
//!   tenant's ICU collation; the fake uses Rust's `to_lowercase`. They agree for `und`, `en`
//!   and most locales; locales that tailor case mapping (`tr`, `az`, `lt`) differ from Rust
//!   (for example Turkish `I` lowers to dotless `ı`). That is a WP-STO-10 parity item.
//! - **Aggregates.** `SUM`, `MIN` and `MAX` ignore nulls and give null over no rows; `COUNT`
//!   gives 0. `MIN`/`MAX` of booleans use `bool_and`/`bool_or`, and of ids and bytes
//!   `array_agg` with an ordering, because PostgreSQL has no aggregate for them. JSON has no
//!   order, so `MIN`/`MAX` over JSON is `InvalidQuery`.
//! - **Grouping** compares null equal to null. A text grouping key is emitted with an explicit
//!   `COLLATE "C"`, in `GROUP BY` and wherever the same expression recurs.
//! - **Ordering** has explicit `NULLS FIRST|LAST`; without aggregation the `from` alias's `id`
//!   is the last key. Then `LIMIT` and `OFFSET`; `OFFSET` is always present so the text of a
//!   query does not change with its page.
//! - **Row locks** are `FOR UPDATE|SHARE OF "<from alias>"` with `NOWAIT` or `SKIP LOCKED`.
//!
//! - **Geo, range and search (ADR-087)** are compiled as documented in the private `constructs`
//!   module: values stay parameters (coordinates bound as exact `numeric`, cast to `float8` in
//!   PostgreSQL), ranges are half-open, and `ts_rank` is only ever an `ORDER BY` key.
//!
//! # Case folding and other parity items
//!
//! Besides case folding above, the compiler is stricter than `Query::validate` where PostgreSQL
//! would fail at run time anyway: incompatible operand types, arithmetic or `SUM` on
//! non-numbers, a non-boolean condition and an empty `COALESCE` give `InvalidQuery`.

mod compile;
mod constructs;
pub mod coverage;
mod expr;
mod schema_map;
mod types;

pub use compile::compile;
pub use schema_map::{
    DefaultSchemaMap, SEARCH_DOC_ID, SEARCH_DOCTYPE, SEARCH_TABLE, SEARCH_TERMS, SchemaMap,
    collation_name,
};
pub use types::{ColumnType, CompiledQuery, Param, PgType};
