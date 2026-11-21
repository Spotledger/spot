//! Typed errors with stable `router.*` codes.
//!
//! The router reports its own failures — a routing document that does not parse, a task
//! with no route, a spent budget — separately from the provider failures it forwards, so a
//! caller can tell *whose* fault a failure is. Callers match on the
//! [`RouterError::code`], never on the message: the codes are this crate's API contract.
//!
//! Note for reviewers: the `router.*` codes are new vocabulary; the shared catalogue in
//! `spot-errors` gains its `router.toml` in a lead-side commit (the completeness lint and
//! the crate tests are the arbiter, and this crate's codes are raised in `table.rs` and
//! `router.rs` as literal codes precisely so that scan sees them).

/// Every failure the router itself can produce, with a stable [`RouterError::code`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouterError {
    /// The routing document has at least one problem; `problems` names them all at once
    /// (`router.config_invalid`).
    #[error("the routing document is invalid: {}", problems.join("; "))]
    ConfigInvalid {
        /// Every problem found, one human-readable sentence each, in document order.
        problems: Vec<String>,
    },

    /// The request named no task, or a task the routing table does not know
    /// (`router.task_unrouted`).
    #[error("no route for task `{task}`")]
    TaskUnrouted {
        /// The task name the request carried.
        task: String,
    },

    /// Every route for the task was tried and failed, or was skipped (`router.routes_exhausted`).
    #[error("every route for task `{task}` failed: {detail}")]
    RoutesExhausted {
        /// The task name.
        task: String,
        /// What the last route failed with, or why the routes were skipped.
        detail: String,
    },

    /// A budget in scope is exhausted; raised before the provider is called
    /// (`router.budget_exceeded`).
    #[error("the {scope} budget is spent: {spent} of {limit} micro-units used")]
    BudgetExceeded {
        /// The scope that is over budget.
        scope: String,
        /// What the scope has already spent, in micro-units.
        spent: u64,
        /// The budget's limit, in micro-units.
        limit: u64,
    },
}

impl RouterError {
    /// The stable machine-readable code (`router.*`).
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid { .. } => "router.config_invalid",
            Self::TaskUnrouted { .. } => "router.task_unrouted",
            Self::RoutesExhausted { .. } => "router.routes_exhausted",
            Self::BudgetExceeded { .. } => "router.budget_exceeded",
        }
    }
}

/// Renders a [`RouterError`] as `code: message`, the shape log lines want.
impl RouterError {
    /// `code: message` in one string.
    #[must_use]
    pub fn with_code(&self) -> String {
        format!("{}: {self}", self.code())
    }
}
