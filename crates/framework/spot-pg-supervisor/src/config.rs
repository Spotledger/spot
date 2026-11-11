//! Supervisor configuration: timeouts, readiness probe injection and the restart
//! budget. Everything has a sensible default; the service wrapper only overrides what
//! it needs.

use std::collections::HashMap;
use std::time::Duration;

/// Default `start_timeout`: how long `Starting` may last before `Failed`.
pub const START_TIMEOUT_DEFAULT: Duration = Duration::from_secs(60);

/// Default `stop` grace before the child is killed.
pub const STOP_GRACE_DEFAULT: Duration = Duration::from_secs(30);

/// Default stability window: a restart that stays ready this long resets the counter.
pub const STABLE_WINDOW_DEFAULT: Duration = Duration::from_secs(600);

/// Maximum restarts inside the budget window before the supervisor gives up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartBudget {
    /// Restarts allowed inside [`RestartBudget::window`].
    pub max_restarts: u32,
    /// Window over which restarts are counted.
    pub window: Duration,
}

impl Default for RestartBudget {
    fn default() -> Self {
        RestartBudget {
            max_restarts: 5,
            window: Duration::from_secs(600),
        }
    }
}

/// Readiness probe: runs after the TCP connection to the port succeeded so a backend
/// (e.g. `SELECT 1` over a real PostgreSQL connection) can veto readiness.
pub type ReadyProbe = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> + Send + Sync,
>;

/// All tunables of one supervisor.
#[derive(Clone)]
pub struct SupervisorConfig {
    /// How long `Starting` may last before the state becomes `Failed`.
    pub start_timeout: Duration,
    /// Default grace passed to a `stop()` without arguments.
    pub stop_grace: Duration,
    /// Crash-restart budget.
    pub restart_budget: RestartBudget,
    /// Stability window that resets the restart counter.
    pub stable_window: Duration,
    /// Optional readiness veto (defaults to none: TCP accept is enough).
    pub ready_probe: Option<ReadyProbe>,
    /// Extra environment variables for the spawned child (locale or auth settings in
    /// production; the tests drive the fake bundle through it).
    pub child_env: HashMap<String, String>,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            start_timeout: START_TIMEOUT_DEFAULT,
            stop_grace: STOP_GRACE_DEFAULT,
            restart_budget: RestartBudget::default(),
            stable_window: STABLE_WINDOW_DEFAULT,
            ready_probe: None,
            child_env: HashMap::new(),
        }
    }
}

impl std::fmt::Debug for SupervisorConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorConfig")
            .field("start_timeout", &self.start_timeout)
            .field("stop_grace", &self.stop_grace)
            .field("restart_budget", &self.restart_budget)
            .field("stable_window", &self.stable_window)
            .field("ready_probe", &self.ready_probe.is_some())
            .field("child_env_keys", &self.child_env.keys().collect::<Vec<_>>())
            .finish()
    }
}
