//! The sampler: once an interval it counts the `ai` role's backends in `pg_stat_activity`,
//! and reads the AIMD limit, the breaker states and the `ai` pool's waiters.
//!
//! The backend count is what proves the cap: PostgreSQL's own view of how many sessions of the
//! role are running a statement, not the application's.

use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_ai_isolation::{AiAdmission, BreakerState};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_postgres::Client;

use crate::clock::RunClock;
use crate::error::{HarnessError, Result};
use crate::report::BreakerSample;

/// What the sampler collected.
#[derive(Default)]
pub struct SamplerOutput {
    /// Backends of the role running a statement, workers included, per sample.
    pub active: Vec<u32>,
    /// The client backends among them, per sample.
    pub active_clients: Vec<u32>,
    /// Client connections of the role, per sample.
    pub open: Vec<u32>,
    /// The AIMD limit, per sample.
    pub aimd: Vec<u32>,
    /// The breaker states, per sample.
    pub breakers: Vec<BreakerSample>,
    /// The most waiters seen on the `ai` pool.
    pub pool_waiting_max: u32,
}

/// What `pg_stat_activity` shows for one role.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Backends {
    /// Backends running a statement, parallel workers included.
    pub active: u32,
    /// The client backends among the active ones.
    pub active_clients: u32,
    /// Client connections, running or idle.
    pub open: u32,
}

/// The backends of `role` right now.
///
/// # Errors
///
/// [`HarnessError::Database`] when the catalogue query fails.
pub async fn count_backends(observer: &Client, role: &str) -> Result<Backends> {
    let row = observer
        .query_one(
            "SELECT (count(*) FILTER (WHERE state = 'active'))::int, \
                    (count(*) FILTER (WHERE state = 'active' AND backend_type = 'client backend'))::int, \
                    (count(*) FILTER (WHERE backend_type = 'client backend'))::int \
             FROM pg_stat_activity WHERE usename = $1",
            &[&role],
        )
        .await
        .map_err(|e| HarnessError::db("read pg_stat_activity", &e))?;
    let get = |i: usize| u32::try_from(row.get::<_, i32>(i)).unwrap_or(0);
    Ok(Backends {
        active: get(0),
        active_clients: get(1),
        open: get(2),
    })
}

/// Starts the sampler. Send on the returned channel to stop it and await the handle for the
/// samples; one sample is always taken at the start and one at the stop.
pub fn spawn(
    observer: Client,
    role: String,
    admission: Arc<AiAdmission>,
    ai_pool: Pool,
    interval: Duration,
    clock: RunClock,
) -> (oneshot::Sender<()>, JoinHandle<Result<SamplerOutput>>) {
    let (stop, mut stopped) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let mut out = SamplerOutput::default();
        let began = clock.now();
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let finished = tokio::select! {
                _ = ticker.tick() => false,
                _ = &mut stopped => true,
            };
            let seen = count_backends(&observer, &role).await?;
            out.active.push(seen.active);
            out.active_clients.push(seen.active_clients);
            out.open.push(seen.open);
            out.aimd.push(admission.aimd_limit());
            out.pool_waiting_max = out
                .pool_waiting_max
                .max(u32::try_from(ai_pool.status().waiting).unwrap_or(u32::MAX));
            let mut sample = BreakerSample {
                at_ms: u64::try_from(clock.now().saturating_sub(began).as_millis())
                    .unwrap_or(u64::MAX),
                ..BreakerSample::default()
            };
            for breaker in admission.health().breakers {
                match breaker.state {
                    BreakerState::Closed => sample.closed += 1,
                    BreakerState::Open => sample.open += 1,
                    BreakerState::HalfOpen => sample.half_open += 1,
                }
            }
            out.breakers.push(sample);
            if finished {
                break;
            }
        }
        Ok(out)
    });
    (stop, handle)
}
