//! Fenced leases: one row per name, with the database's clock deciding expiry.
//!
//! # Acquire is one statement
//!
//! [`acquire`] claims the name and issues the next token in one statement, and **wastes no
//! token when it refuses**: the token is bumped only for the statement that actually takes the
//! lease. Its data-modifying CTEs are:
//!
//! 1. `up` — `INSERT … ON CONFLICT (name) DO UPDATE … WHERE spot_coord_lease.expires_at <=
//!    now()`. The insert takes a fresh row; the conflict update takes over an **expired** row,
//!    bumping `token` and `lease_id` from the row it replaces. A live row makes the `WHERE`
//!    false, so the statement returns no row and the caller gets `coordinator.lease_held`. The
//!    row lock the update takes serializes concurrent acquirers of one name.
//! 2. `high` — carries the new token into `spot_coord_token`, gated on `up` returning a row, so
//!    a loser leaves the high-water mark alone and the next winner's token is exactly one more.
//!    `GREATEST` keeps it monotonic.
//!
//! # Isolation levels
//!
//! - `READ COMMITTED` (the default, and what a pool gives): when two acquirers race, the loser's
//!   `INSERT … ON CONFLICT` waits on the winner's row lock and then re-evaluates the `ON CONFLICT
//!   DO UPDATE … WHERE` against the winner's committed row; it is live, so the update is skipped
//!   and no token is taken. If the winner had already expired, the loser takes the row over.
//! - `REPEATABLE READ` and `SERIALIZABLE`: the loser's snapshot predates the winner's commit, so
//!   PostgreSQL fails it with `40001` (`SerializationFailure`) instead of taking over. A pooled
//!   connection runs each statement in its own transaction at the default level, so this does not
//!   arise; a caller wiring a stronger default must retry on `40001`.
//!
//! `renew`, `release` and `check_fence` match the row's `lease_id` and `token`, so a handle from
//! before a takeover renews or releases nothing and fails with `coordinator.lease_lost`. Expiry
//! is half-open: a lease is live while `expires_at > now()`.

use spot_coordinator::error::{LEASE_HELD, LEASE_LOST};
use spot_coordinator::{Error, FencingToken, Lease, MAX_TTL, Result, validate_name};
use time::OffsetDateTime;
use tokio_postgres::GenericClient;
use tokio_postgres::Row;

/// Claims `name` for `holder` for `ttl`, or fails with `coordinator.lease_held`.
pub(crate) async fn acquire<C: GenericClient + Sync>(
    client: &C,
    name: &str,
    holder: &str,
    ttl: std::time::Duration,
) -> Result<Lease> {
    validate_name(name)?;
    validate_name(holder)?;
    let micros = ttl_micros(ttl)?;
    let row = client
        .query_opt(ACQUIRE_SQL, &[&name, &holder, &micros])
        .await
        .map_err(|e| crate::error::pg("acquire", &e))?;
    match row {
        Some(row) => Ok(lease_of(name, row)),
        None => Err(Error::new(
            LEASE_HELD,
            format!("lease {name}: {holder} cannot acquire, another holder's lease is live"),
        )),
    }
}

/// Extends `lease` to `ttl` from the database's now, when it is still the current record.
pub(crate) async fn renew<C: GenericClient + Sync>(
    client: &C,
    lease: &Lease,
    ttl: std::time::Duration,
) -> Result<Lease> {
    let micros = ttl_micros(ttl)?;
    let row = client
        .query_opt(
            RENEW_SQL,
            &[
                &lease.name,
                &id(lease.id)?,
                &token_i64(lease.token)?,
                &micros,
            ],
        )
        .await
        .map_err(|e| crate::error::pg("renew", &e))?;
    match row {
        Some(row) => Ok(lease_of(&lease.name, row)),
        None => Err(lost(lease)),
    }
}

/// Gives `lease` up, when it is still the current record: the row is deleted and the token stays
/// in `spot_coord_token`.
pub(crate) async fn release<C: GenericClient + Sync>(client: &C, lease: &Lease) -> Result<()> {
    let row = client
        .query_opt(
            RELEASE_SQL,
            &[&lease.name, &id(lease.id)?, &token_i64(lease.token)?],
        )
        .await
        .map_err(|e| crate::error::pg("release", &e))?;
    match row {
        Some(_) => Ok(()),
        None => Err(lost(lease)),
    }
}

/// The live lease for `name`, if any.
pub(crate) async fn current<C: GenericClient + Sync>(
    client: &C,
    name: &str,
) -> Result<Option<Lease>> {
    validate_name(name)?;
    let row = client
        .query_opt(CURRENT_SQL, &[&name])
        .await
        .map_err(|e| crate::error::pg("current", &e))?;
    Ok(row.map(|row| lease_of(name, row)))
}

/// Succeeds only for the live lease's token; a stale writer fails with `coordinator.lease_lost`.
pub(crate) async fn check_fence<C: GenericClient + Sync>(
    client: &C,
    name: &str,
    token: FencingToken,
) -> Result<()> {
    assert_fence(client, name, token).await
}

/// The statement a store layer runs **inside its own transaction** to fence a write.
///
/// Run it with `$1 = name` and `$2 = token`; a returned row means the write is fenced by the
/// live lease, no row means the writer is stale. The `FOR SHARE` lock holds the lease row for the
/// rest of the transaction, so the lease cannot change between the check and the write.
pub const FENCE_SQL: &str = "SELECT 1 FROM spot_coord_lease \
     WHERE name = $1 AND token = $2 AND expires_at > now() FOR SHARE";

/// The fence statement and the values to bind, so a caller cannot bind them in the wrong order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FenceCheck {
    /// The statement to run (`$1` name, `$2` token).
    pub sql: &'static str,
    /// The lease name.
    pub name: String,
    /// The fencing token, as `bigint`.
    pub token: i64,
}

/// The fence check to run in a transaction, for `name` and `token`.
///
/// # Errors
///
/// `coordinator.invalid_name` for an invalid name, and `coordinator.invalid_argument` for a token
/// past `bigint`.
pub fn fence_sql(name: &str, token: FencingToken) -> Result<FenceCheck> {
    validate_name(name)?;
    Ok(FenceCheck {
        sql: FENCE_SQL,
        name: name.to_owned(),
        token: token_i64(token)?,
    })
}

/// Fences a write under `name`/`token` on `client`, which a store layer runs inside the write's
/// own transaction so the check and the write cannot be separated.
///
/// # Errors
///
/// `coordinator.lease_lost` when no live lease holds `token`, and `coordinator.unavailable` when
/// the statement fails.
pub async fn assert_fence<C: GenericClient + Sync>(
    client: &C,
    name: &str,
    token: FencingToken,
) -> Result<()> {
    let check = fence_sql(name, token)?;
    let row = client
        .query_opt(check.sql, &[&check.name, &check.token])
        .await
        .map_err(|e| crate::error::pg("check_fence", &e))?;
    match row {
        Some(_) => Ok(()),
        None => Err(Error::new(
            LEASE_LOST,
            format!(
                "lease {name}: token {} is not the live lease's token",
                token.0
            ),
        )),
    }
}

/// Fetches the lease row of a freshly acquired name, for `current`-style reads.
pub(crate) fn lease_of(name: &str, row: Row) -> Lease {
    Lease {
        name: name.to_owned(),
        holder: row.get(0),
        token: FencingToken(u64::try_from(row.get::<_, i64>(1)).unwrap_or(u64::MAX)),
        expires_at: row.get::<_, OffsetDateTime>(2),
        id: u64::try_from(row.get::<_, i64>(3)).unwrap_or(u64::MAX),
    }
}

fn lost(lease: &Lease) -> Error {
    Error::lease_lost(
        &lease.name,
        &lease.holder,
        lease.token.0,
        "the lease expired, was released or was taken over",
    )
}

fn token_i64(value: FencingToken) -> Result<i64> {
    i64::try_from(value.0).map_err(|_| {
        Error::invalid_argument(format!("fencing token {} does not fit bigint", value.0))
    })
}

fn id(value: u64) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| Error::invalid_argument(format!("lease id {value} does not fit bigint")))
}

/// Validates a ttl and returns it in whole microseconds.
pub(crate) fn ttl_micros(ttl: std::time::Duration) -> Result<i64> {
    if ttl.is_zero() || ttl > MAX_TTL {
        return Err(Error::invalid_argument(format!(
            "a ttl is more than zero and at most a year, not {ttl:?}"
        )));
    }
    i64::try_from(ttl.as_micros())
        .map_err(|_| Error::invalid_argument("the ttl is too large to express in microseconds"))
}

const ACQUIRE_SQL: &str = "\
WITH up AS (\
  INSERT INTO spot_coord_lease (name, holder, token, expires_at, lease_id) \
  VALUES (\
    $1, $2, \
    COALESCE((SELECT last_token FROM spot_coord_token WHERE name = $1), 0) + 1, \
    now() + $3::bigint * interval '1 microsecond', \
    COALESCE((SELECT last_token FROM spot_coord_token WHERE name = $1), 0) + 1\
  ) \
  ON CONFLICT (name) DO UPDATE \
    SET holder = EXCLUDED.holder, \
        token = spot_coord_lease.token + 1, \
        expires_at = EXCLUDED.expires_at, \
        lease_id = spot_coord_lease.lease_id + 1 \
    WHERE spot_coord_lease.expires_at <= now() \
  RETURNING holder, token, expires_at, lease_id\
), \
high AS (\
  INSERT INTO spot_coord_token (name, last_token) \
  SELECT $1, token FROM up WHERE EXISTS (SELECT 1 FROM up) \
  ON CONFLICT (name) DO UPDATE \
    SET last_token = GREATEST(spot_coord_token.last_token, EXCLUDED.last_token) \
  RETURNING last_token\
) \
SELECT holder, token, expires_at, lease_id FROM up";

const RENEW_SQL: &str = "\
UPDATE spot_coord_lease \
  SET expires_at = now() + $4::bigint * interval '1 microsecond' \
  WHERE name = $1 AND lease_id = $2 AND token = $3 AND expires_at > now() \
  RETURNING holder, token, expires_at, lease_id";

const RELEASE_SQL: &str = "\
DELETE FROM spot_coord_lease \
  WHERE name = $1 AND lease_id = $2 AND token = $3 AND expires_at > now() \
  RETURNING 1";

const CURRENT_SQL: &str = "\
SELECT holder, token, expires_at, lease_id FROM spot_coord_lease \
  WHERE name = $1 AND expires_at > now()";
