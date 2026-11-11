//! The run configuration: phase lengths, loads, pool sizes and the admission settings.

use std::time::Duration;

use spot_ai_isolation::{AiLimits, AiPoolSettings, BreakerPolicy, LoadThresholds};

use spot_coordinator::RateLimit;

/// Everything one run needs. [`RunConfig::short`] and [`RunConfig::full`] are the two shapes.
#[derive(Clone, Debug)]
pub struct RunConfig {
    /// `short` or `full`, copied into the report.
    pub mode: &'static str,
    /// Rows in the seeded table.
    pub rows: u64,
    /// How long each of the three phases runs.
    pub phase_duration: Duration,
    /// Virtual human users.
    pub human_users: u32,
    /// Size of the human pool.
    pub human_pool_size: u32,
    /// A human's pause between operations.
    pub human_think: Duration,
    /// Out of 100 human operations, how many are the report-shaped aggregate.
    pub report_percent: u32,
    /// Agents in the `with_agents` phase.
    pub agents: u32,
    /// Agents in the `overload` phase.
    pub overload_agents: u32,
    /// In the `overload` phase every n-th agent only makes calls that exceed their deadline.
    pub runaway_every: u32,
    /// In the mixed load, every n-th call of an agent exceeds its deadline.
    pub overdue_every: u32,
    /// The `ai` role and pool budget.
    pub ai_pool: AiPoolSettings,
    /// The admission limits (the per-call deadline is `call_timeout`).
    pub limits: AiLimits,
    /// The breaker policy for agents and tenants.
    pub breaker: BreakerPolicy,
    /// The thresholds at which interactive load sheds AI calls.
    pub thresholds: LoadThresholds,
    /// How often the sampler reads `pg_stat_activity`.
    pub sample_interval: Duration,
    /// How long a refused agent waits before it asks again.
    pub retry_pause: Duration,
    /// How often the recovery probe asks admission.
    pub probe_interval: Duration,
    /// How long to poll for statements still running after the agents stopped.
    pub settle: Duration,
    /// The seed of the deterministic call plan.
    pub seed: u64,
    /// Extra `ALTER ROLE <ai> SET <name> = <value>` settings applied after the T1 role SQL, to
    /// try a knob without changing the crate under test. Empty by default.
    pub role_settings: Vec<(String, String)>,
}

impl RunConfig {
    /// Adds role settings from a comma-separated `name=value` list (for example
    /// `max_parallel_workers_per_gather=0`). A name must be lower-case letters and
    /// underscores and a value lower-case letters, digits and underscores; anything else is
    /// ignored, because the text ends up in SQL.
    #[must_use]
    pub fn with_role_settings(mut self, list: &str) -> Self {
        for item in list.split(',') {
            let Some((name, value)) = item.split_once('=') else {
                continue;
            };
            let (name, value) = (name.trim(), value.trim());
            let name_ok =
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
            let value_ok = !value.is_empty()
                && value
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            if name_ok && value_ok {
                self.role_settings.push((name.to_owned(), value.to_owned()));
            }
        }
        self
    }
}

impl RunConfig {
    /// The short mode: 5-second phases, 5 agents, 10,000 rows. Runs in normal CI.
    #[must_use]
    pub fn short() -> Self {
        Self {
            mode: "short",
            rows: 10_000,
            phase_duration: Duration::from_secs(5),
            human_users: 10,
            human_pool_size: 8,
            human_think: Duration::from_millis(20),
            report_percent: 3,
            agents: 5,
            overload_agents: 12,
            runaway_every: 4,
            overdue_every: 8,
            ai_pool: AiPoolSettings {
                pool_size: 4,
                statement_timeout: Duration::from_secs(10),
                idle_in_transaction_timeout: Duration::from_secs(10),
                work_mem_kb: 16 * 1024,
            },
            limits: load_limits(Duration::from_millis(300)),
            breaker: BreakerPolicy {
                window: 8,
                min_calls: 4,
                open_base: Duration::from_secs(1),
                open_max: Duration::from_secs(2),
                ..BreakerPolicy::default()
            },
            thresholds: LoadThresholds::default(),
            sample_interval: Duration::from_millis(500),
            retry_pause: Duration::from_millis(10),
            probe_interval: Duration::from_millis(25),
            settle: Duration::from_secs(5),
            seed: 0x5EED,
            role_settings: Vec::new(),
        }
    }

    /// The full mode: 60-second phases, 50 human users, 20 and 60 agents, 200,000 rows.
    #[must_use]
    pub fn full() -> Self {
        Self {
            mode: "full",
            rows: 200_000,
            phase_duration: Duration::from_secs(60),
            human_users: 50,
            human_pool_size: 16,
            human_think: Duration::from_millis(100),
            report_percent: 3,
            agents: 20,
            overload_agents: 60,
            runaway_every: 4,
            overdue_every: 50,
            ai_pool: AiPoolSettings {
                pool_size: 4,
                statement_timeout: Duration::from_secs(30),
                idle_in_transaction_timeout: Duration::from_secs(60),
                work_mem_kb: 64 * 1024,
            },
            limits: load_limits(Duration::from_secs(2)),
            breaker: BreakerPolicy::default(),
            thresholds: LoadThresholds::default(),
            sample_interval: Duration::from_secs(1),
            retry_pause: Duration::from_millis(20),
            probe_interval: Duration::from_millis(50),
            settle: Duration::from_secs(5),
            seed: 0x5EED,
            role_settings: Vec::new(),
        }
    }
}

/// Admission limits for a load test: the rate limits are lifted so they never shape the run
/// (rate limiting has its own T1 tests), the concurrency limits leave the node ceiling, the
/// AIMD value and the pool, as the thing that decides.
fn load_limits(call_timeout: Duration) -> AiLimits {
    AiLimits {
        per_agent_concurrency: 2,
        per_tenant_concurrency: 16,
        per_agent_rate: lifted_rate(),
        per_tenant_rate: lifted_rate(),
        call_timeout,
        ..AiLimits::default()
    }
}

/// A rate no run reaches.
fn lifted_rate() -> RateLimit {
    RateLimit {
        capacity: 1_000_000,
        per: Duration::from_secs(60),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn role_settings_keep_only_plain_names_and_values() {
        let c = RunConfig::short().with_role_settings(
            "max_parallel_workers_per_gather=0, work_mem=4mb,evil;drop=1,x=y z,=1,noequals",
        );
        assert_eq!(
            c.role_settings,
            [
                ("max_parallel_workers_per_gather".to_owned(), "0".to_owned()),
                ("work_mem".to_owned(), "4mb".to_owned())
            ]
        );
        assert!(RunConfig::full().role_settings.is_empty());
    }

    #[test]
    fn the_two_modes_have_the_shapes_the_brief_names() {
        let (short, full) = (RunConfig::short(), RunConfig::full());
        assert_eq!((short.rows, short.agents), (10_000, 5));
        assert_eq!(short.phase_duration, Duration::from_secs(5));
        assert_eq!((full.rows, full.human_users), (200_000, 50));
        assert_eq!((full.agents, full.overload_agents), (20, 60));
        assert_eq!(full.phase_duration, Duration::from_secs(60));
    }
}
