//! One agent: a loop of `query` tool calls through `AiAdmission` onto the `ai` pool, with a
//! deterministic fake provider deciding what each call costs.
//!
//! A refused call waits and asks again; the time from the first refusal to the admission is
//! the call's queue wait. A call that passes its deadline has its statement cancelled and its
//! outcome recorded as a failure, which is what feeds the circuit breakers.

use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_ai_isolation::error::{BUSY, CIRCUIT_OPEN, RATE_LIMITED, SHED};
use spot_ai_isolation::{Admission, AgentId, AiAdmission, AiRequest, Outcome, TenantId};

use crate::calls::{Kind, Rng, execute, plan};
use crate::cancel::PgCanceller;
use crate::clock::RunClock;
use crate::config::RunConfig;
use crate::report::AgentStats;
use crate::stats::Samples;

/// How long the caller of an [`Kind::AbandonedScan`] waits before it gives up.
const ABANDON_AFTER: Duration = Duration::from_millis(5);
/// The longest a refused agent pauses before asking again.
const MAX_PAUSE: Duration = Duration::from_secs(1);
/// How long a cancelled statement may take to end before the cancel counts as unconfirmed.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// What the agents of one phase share.
pub struct AgentEnv {
    /// The admission control under test.
    pub admission: Arc<AiAdmission>,
    /// The `ai` pool.
    pub pool: Pool,
    /// Cancels statements on the real connections.
    pub canceller: Arc<PgCanceller>,
    /// The run clock.
    pub clock: RunClock,
    /// The schema-qualified table.
    pub table: String,
    /// Rows in the table.
    pub rows: u64,
    /// The phase's counters.
    pub stats: Arc<AgentStats>,
    /// The run configuration.
    pub config: Arc<RunConfig>,
}

/// What one agent measured.
#[derive(Default)]
pub struct AgentOutput {
    /// The wait for an `ai` pool connection.
    pub pool_wait: Samples,
    /// The wait from the first refusal to the admission.
    pub queue_wait: Samples,
}

impl AgentOutput {
    /// Adds another agent's measurements.
    pub fn merge(&mut self, other: &AgentOutput) {
        self.pool_wait.merge(&other.pool_wait);
        self.queue_wait.merge(&other.queue_wait);
    }
}

/// Runs agent `index` until `end`. A `runaway` agent only makes calls that exceed their
/// deadline.
pub async fn run_agent(
    env: Arc<AgentEnv>,
    index: u32,
    runaway: bool,
    end: Duration,
) -> AgentOutput {
    let mut out = AgentOutput::default();
    let (Ok(agent), Ok(tenant)) = (
        AgentId::new(format!("agent-{index}")),
        TenantId::new(format!("tenant-{}", index % 4)),
    ) else {
        return out;
    };
    let mut rng =
        Rng::new(env.config.seed ^ (u64::from(index) + 1).wrapping_mul(0xA24B_AED4_963E_E407));
    let mut refused_since: Option<Duration> = None;
    let mut n = 0_u64;
    loop {
        let now = env.clock.now();
        if now >= end {
            break;
        }
        let request = AiRequest::new(agent.clone(), tenant.clone()).with_rows(100);
        match env.admission.admit(&request, now) {
            Ok(admission) => {
                AgentStats::bump(&env.stats.admitted);
                if let Some(since) = refused_since.take() {
                    out.queue_wait.push(now.saturating_sub(since));
                }
                let kind = if runaway {
                    Kind::Overdue
                } else {
                    plan(index, n, env.config.overdue_every)
                };
                n += 1;
                one_call(&env, admission, kind, rng.next_u64(), &mut out).await;
            }
            Err(error) => {
                let first_refusal = refused_since.is_none();
                refused_since.get_or_insert(now);
                let base = env.config.retry_pause;
                let pause = match error.code {
                    BUSY => {
                        if first_refusal {
                            AgentStats::bump(&env.stats.queued);
                        }
                        AgentStats::bump(&env.stats.busy_refusals);
                        base
                    }
                    SHED => {
                        AgentStats::bump(&env.stats.shed);
                        base * 10
                    }
                    RATE_LIMITED => {
                        AgentStats::bump(&env.stats.rate_limited);
                        error.retry_after.unwrap_or(base)
                    }
                    CIRCUIT_OPEN => {
                        AgentStats::bump(&env.stats.circuit_open);
                        error.retry_after.unwrap_or(base * 10)
                    }
                    _ => {
                        AgentStats::bump(&env.stats.failed);
                        base
                    }
                };
                tokio::time::sleep(pause.min(MAX_PAUSE)).await;
            }
        }
    }
    out
}

/// Runs one admitted call to its end: completion, failure, deadline or give-up.
async fn one_call(
    env: &AgentEnv,
    admission: Admission,
    kind: Kind,
    draw: u64,
    out: &mut AgentOutput,
) {
    let statement = admission.statement();
    let asked = env.clock.now();
    let client = match env.pool.get().await {
        Ok(client) => client,
        Err(_) => {
            AgentStats::bump(&env.stats.failed);
            admission.complete(env.clock.now(), Outcome::Failure);
            return;
        }
    };
    out.pool_wait.push(env.clock.now().saturating_sub(asked));
    env.canceller.register(statement, client.cancel_token());

    let sleep_secs = (env.config.limits.call_timeout.as_secs() + 1) * 3;
    let fut = execute(&client, kind, &env.table, env.rows, draw, sleep_secs);
    tokio::pin!(fut);
    let remaining = admission.remaining(env.clock.now());
    let abandon =
        (kind == Kind::AbandonedScan && ABANDON_AFTER < remaining).then_some(ABANDON_AFTER);
    let wait = abandon.unwrap_or(remaining);

    match tokio::time::timeout(wait, &mut fut).await {
        Ok(Ok(_)) => {
            AgentStats::bump(&env.stats.completed);
            admission.complete(env.clock.now(), Outcome::Success);
        }
        Ok(Err(_)) => {
            AgentStats::bump(&env.stats.failed);
            admission.complete(env.clock.now(), Outcome::Failure);
        }
        Err(_) => {
            if abandon.is_some() {
                // The caller gave up: cancel the statement and release the permit.
                AgentStats::bump(&env.stats.cancelled);
                let _ = admission.cancel(&*env.canceller);
            } else {
                // The deadline passed: cancel the statement and count the call as a failure.
                AgentStats::bump(&env.stats.timed_out);
                spot_ai_isolation::StatementCanceller::cancel(&*env.canceller, statement);
                admission.complete(env.clock.now(), Outcome::Failure);
            }
            // The connection goes back to the pool only once the server ended the statement.
            if tokio::time::timeout(CANCEL_GRACE, &mut fut).await.is_err() {
                AgentStats::bump(&env.stats.cancel_unconfirmed);
            }
        }
    }
    env.canceller.forget(statement);
}
