//! Cluster-wide rate limits: the generic cell rate algorithm (GCRA) in one statement.
//!
//! Same semantics as the in-process backend: a bucket per key that refills continuously at
//! `capacity` units per `per`, holding at most `capacity`; a take of `cost` units is admitted
//! only when that many are present. The state per key is the bucket's **theoretical arrival
//! time**, held in `spot_coord_rate.window_start`; `used` records the capacity the bucket was
//! built for, so a caller that changes a key's limit starts that bucket afresh. All arithmetic is
//! exact integer microseconds: one emission interval is `per / capacity`, and the burst tolerance
//! is `per`, so the whole bucket is `capacity` emissions. PostgreSQL turns the interval into
//! microseconds for the reply with `EXTRACT(EPOCH …)`; no Rust code sees a float.
//!
//! The admission decision is one statement: the `INSERT … ON CONFLICT DO UPDATE … WHERE` admits
//! only when `candidate - now() <= burst`, so however many callers race, no window admits more
//! than its capacity. A denial takes a second, read-only statement to compute `retry_after` from
//! the current bucket; another caller moving the bucket first can only make that estimate
//! conservative.

use std::time::Duration;

use spot_coordinator::{Admit, Error, RateLimit, Result, validate_name};
use tokio_postgres::GenericClient;

/// Takes `cost` units from `key`, or denies and says how long to wait.
pub(crate) async fn take<C: GenericClient + Sync>(
    client: &C,
    key: &str,
    limit: RateLimit,
    cost: u64,
) -> Result<Admit> {
    validate_name(key)?;
    limit.validate()?;
    if cost == 0 || cost > limit.capacity {
        return Err(Error::invalid_argument(format!(
            "a take costs between 1 and the capacity {}, not {cost}",
            limit.capacity
        )));
    }
    let interval = emission_micros(limit)?;
    let burst = i64::try_from(limit.per.as_micros())
        .map_err(|_| Error::invalid_argument("the rate-limit window is too large"))?;
    let cost = i64::try_from(cost).unwrap_or(i64::MAX);
    let capacity = i64::try_from(limit.capacity).unwrap_or(i64::MAX);

    let row = client
        .query_opt(TAKE_SQL, &[&key, &interval, &burst, &cost, &capacity])
        .await
        .map_err(|e| crate::error::pg("take", &e))?;
    if let Some(row) = row {
        let remaining_us = row.get::<_, i64>(0).max(0);
        let remaining = u64::try_from(remaining_us / interval).unwrap_or(0);
        return Ok(Admit::Allowed { remaining });
    }
    let row = client
        .query_opt(RETRY_SQL, &[&key, &interval, &burst, &cost, &capacity])
        .await
        .map_err(|e| crate::error::pg("take", &e))?;
    let retry_us = row.map_or(interval, |row| row.get::<_, i64>(0).max(1));
    Ok(Admit::Denied {
        retry_after: Duration::from_micros(u64::try_from(retry_us).unwrap_or(1)),
    })
}

/// One emission interval (`per / capacity`) in whole microseconds; at least one.
fn emission_micros(limit: RateLimit) -> Result<i64> {
    let per = limit.per.as_micros();
    let interval = per / u128::from(limit.capacity);
    if interval == 0 {
        return Err(Error::invalid_argument(
            "the rate-limit window is shorter than one microsecond per unit",
        ));
    }
    i64::try_from(interval)
        .map_err(|_| Error::invalid_argument("the rate-limit window is too large"))
}

const TAKE_SQL: &str = "\
INSERT INTO spot_coord_rate (key, window_start, used) \
VALUES ($1, now() + $2::bigint * $4::bigint * interval '1 microsecond', $5) \
ON CONFLICT (key) DO UPDATE \
  SET window_start = GREATEST(\
        CASE WHEN spot_coord_rate.used = EXCLUDED.used \
             THEN spot_coord_rate.window_start ELSE now() END, \
        now()\
      ) + $2::bigint * $4::bigint * interval '1 microsecond', \
      used = EXCLUDED.used \
  WHERE GREATEST(\
        CASE WHEN spot_coord_rate.used = EXCLUDED.used \
             THEN spot_coord_rate.window_start ELSE now() END, \
        now()\
      ) + $2::bigint * $4::bigint * interval '1 microsecond' - now() \
      <= $3::bigint * interval '1 microsecond' \
RETURNING (EXTRACT(EPOCH FROM ($3::bigint * interval '1 microsecond' \
             - (window_start - now()))) * 1000000)::bigint";

const RETRY_SQL: &str = "\
SELECT (EXTRACT(EPOCH FROM (\
         GREATEST(CASE WHEN used = $5 THEN window_start ELSE now() END, now()) \
         + $2::bigint * $4::bigint * interval '1 microsecond' \
         - now() - $3::bigint * interval '1 microsecond'\
       )) * 1000000)::bigint \
FROM spot_coord_rate WHERE key = $1";
