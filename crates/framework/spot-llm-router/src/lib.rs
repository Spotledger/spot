//! `spot-llm-router`: the layer between an agent and the model providers (WP-AI-03.T4).
//!
//! One [`Router`] that implements [`spot_llm::Provider`], so an agent that runs on one
//! provider runs unchanged behind the router. The router chooses a provider and model **by
//! task** from a [`RoutingTable`] (parsed from TOML with every error reported at once),
//! falls back to the next route when one fails, retries retryable errors with capped
//! exponential backoff on an injectable clock, skips a route that fails repeatedly for a
//! cool-down (circuit breaker), **counts usage** per tenant, task, provider, model and day
//! in a [`UsageLedger`], prices it against a [`PriceTable`] in integer micro-units, and
//! **stops** a tenant, task or day that has spent its [`Budget`] — before the provider is
//! called, with the reserve settled afterwards so concurrent calls never overshoot by more
//! than one in-flight request's estimate.
//!
//! # The request id carries the tenant and the task
//!
//! [`spot_llm::Provider::complete`] takes only a [`spot_llm::Request`], and the router is
//! a `Provider` — so the caller names its tenant and task inside the request id it already
//! owns: the convention is `{tenant}:{task}:{caller-id}`. The router refuses an id without
//! the two prefixes (`router.task_unrouted`) and forwards the id **unchanged** to the
//! provider, so the provider's logs keep the caller's correlation id and the scripted mock
//! provider keeps scripting against it. Tenants carry no secret material in the id (it
//! reaches the provider's logs).
//!
//! # Exact arithmetic
//!
//! No float crosses this crate (ADR-12): token counts are `u64`, prices and costs are
//! `u64` micro-units (millionths of a main unit), every sum and budget check runs in
//! `u128`, and pricing is [`cost`](ledger::cost)'s integer round-half-up.
//!
//! # Layout
//!
//! - [`table`]: the routing document, its TOML parser and the `router.*` codes it reports.
//! - [`clock`]: the injectable [`Clock`] — wall time for the ledger, waits for backoff and
//!   cool-downs, and the [`ManualClock`](clock::ManualClock) tests drive.
//! - [`ledger`]: the [`UsageRecord`], the [`UsageLedger`] trait and its in-memory
//!   [`MemoryLedger`], the [`PriceTable`] with the integer [`cost`](ledger::cost), and the
//!   budget shapes: [`Budget`], [`BudgetScope`], [`BudgetBook`], [`BudgetWarning`].
//! - [`router`]: the [`Router`] itself — routing, capability filtering, fallback, retry,
//!   circuit breaker, budget enforcement and the ledger hooks, behind the one
//!   [`Provider`](spot_llm::Provider) trait.
//! - [`error`]: the typed `router.*` errors.

pub mod clock;
pub mod error;
pub mod ledger;
pub mod router;
pub mod table;

pub use clock::{Clock, ManualClock, SystemClock};
pub use error::RouterError;
pub use ledger::{
    Budget, BudgetBook, BudgetContext, BudgetRefusal, BudgetScope, BudgetWarning, DayKey,
    MemoryLedger, Price, PriceKey, PriceTable, ReserveOutcome, SumQuery, TokenSums, UsageLedger,
    UsageRecord, cost,
};
pub use router::{Estimate, Router, RouterConfig, RouterStream, estimate_input_tokens};
pub use table::{Route, RoutingTable};
