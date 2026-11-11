//! Cluster-wide counters: atomic, monotonic, not gapless across a crash.
//!
//! `next` is one statement. `value` is `numeric(20,0)` because the contract's counters are `u64`
//! and run to `u64::MAX`, which does not fit a `bigint`; the arithmetic and the overflow check
//! happen in the database, so two callers of `next` never receive the same value and never skip
//! one another. The `WHERE` on the conflict update refuses to pass `u64::MAX`, so a `next` past
//! the top takes nothing and returns `coordinator.counter_overflow`.
//!
//! # Isolation levels
//!
//! - `READ COMMITTED`: the loser of a race waits for the row lock, then re-reads the winner's
//!   committed value and adds to it, so the two reservations are consecutive.
//! - `REPEATABLE READ` and `SERIALIZABLE`: a concurrent commit makes the update fail with `40001`
//!   (`SerializationFailure`) rather than overwrite; a pooled connection runs each statement in
//!   its own default transaction, so this does not arise.

use spot_coordinator::error::COUNTER_OVERFLOW;
use spot_coordinator::{Error, Result, validate_name};
use tokio_postgres::GenericClient;

/// Reserves `by` consecutive values and returns the first; the first ever is 1.
pub(crate) async fn next<C: GenericClient + Sync>(client: &C, name: &str, by: u64) -> Result<u64> {
    validate_name(name)?;
    if by == 0 {
        return Err(Error::invalid_argument("a counter advances by at least 1"));
    }
    let by = by.to_string();
    let row = client
        .query_opt(NEXT_SQL, &[&name, &by])
        .await
        .map_err(|e| crate::error::pg("next", &e))?;
    match row {
        Some(row) => parse(&row.get::<_, String>(0)),
        None => Err(Error::new(
            COUNTER_OVERFLOW,
            format!("counter {name}: the reservation would pass the largest value"),
        )),
    }
}

/// The last value issued by `name` (0 when it has never been used).
pub(crate) async fn peek<C: GenericClient + Sync>(client: &C, name: &str) -> Result<u64> {
    validate_name(name)?;
    let row = client
        .query_opt(PEEK_SQL, &[&name])
        .await
        .map_err(|e| crate::error::pg("peek", &e))?;
    match row {
        Some(row) => parse(&row.get::<_, String>(0)),
        None => Ok(0),
    }
}

/// Raises `name`'s counter so the last value issued is at least `value`; never lowers it.
pub(crate) async fn set_at_least<C: GenericClient + Sync>(
    client: &C,
    name: &str,
    value: u64,
) -> Result<u64> {
    validate_name(name)?;
    let value = value.to_string();
    let row = client
        .query_one(SET_AT_LEAST_SQL, &[&name, &value])
        .await
        .map_err(|e| crate::error::pg("set_at_least", &e))?;
    parse(&row.get::<_, String>(0))
}

fn parse(text: &str) -> Result<u64> {
    text.parse::<u64>().map_err(|_| {
        Error::new(
            crate::error::UNAVAILABLE,
            format!("a counter row holds {text:?}, which is not a u64"),
        )
    })
}

const NEXT_SQL: &str = "\
INSERT INTO spot_coord_counter (name, value) VALUES ($1, $2::text::numeric) \
ON CONFLICT (name) DO UPDATE \
  SET value = spot_coord_counter.value + $2::text::numeric \
  WHERE spot_coord_counter.value + $2::text::numeric <= 18446744073709551615 \
RETURNING (value - $2::text::numeric + 1)::text";

const PEEK_SQL: &str = "SELECT value::text FROM spot_coord_counter WHERE name = $1";

const SET_AT_LEAST_SQL: &str = "\
INSERT INTO spot_coord_counter (name, value) VALUES ($1, $2::text::numeric) \
ON CONFLICT (name) DO UPDATE \
  SET value = GREATEST(spot_coord_counter.value, EXCLUDED.value) \
RETURNING value::text";
