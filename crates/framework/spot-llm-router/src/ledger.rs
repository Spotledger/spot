//! Usage accounting and budgets, in exact integers (no float ever crosses this crate).
//!
//! The pieces:
//!
//! - [`UsageRecord`] is one settled request's usage; [`UsageLedger`] is the sink it is
//!   recorded into, idempotently per request id, and [`MemoryLedger`] is the in-memory
//!   implementation with [`SumQuery`] / [`TokenSums`] lookup by tenant, task, provider,
//!   model and day.
//! - Money lives only in *prices as data*: [`PriceTable`] maps `(provider, model)` to a
//!   [`Price`] in micro-units per million tokens, and [`cost`] prices a request with
//!   integer arithmetic, round-half-up (ADR-12).
//! - [`Budget`] limits a [`BudgetScope`] — one tenant, one task, or one day — and is
//!   enforced by the [`BudgetBook`]: a caller reserves the worst-case estimate before the
//!   provider is called and settles the actual cost after, so concurrent calls cannot
//!   overshoot by more than the estimates they already hold. Crossing the budget's warn
//!   level produces a [`BudgetWarning`] for the router's callback.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

/// One settled request's usage: who, what, when, and the token counts it consumed.
///
/// The ledger records one record per [`UsageRecord::request_id`] — a later record for the
/// same id is dropped, so a retried settlement can never double-count.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UsageRecord {
    /// When the request settled, in milliseconds since the Unix epoch (UTC).
    pub at: u64,
    /// The tenant that owns the request (the id's `tenant:` prefix).
    pub tenant: String,
    /// The task the request served — the routing table's entry name.
    pub task: String,
    /// The provider that answered, as [`spot_llm::Provider::id`] reports it.
    pub provider: String,
    /// The model, in that provider's own spelling.
    pub model: String,
    /// Prompt tokens read fresh.
    pub input: u64,
    /// Tokens generated.
    pub output: u64,
    /// Prompt tokens served from the provider's cache.
    pub cache_read: u64,
    /// Prompt tokens written to the provider's cache.
    pub cache_write: u64,
    /// The request id the record settles; duplicates for one id are dropped.
    pub request_id: String,
}

impl UsageRecord {
    /// The input-side token total the [`PriceTable`] prices: fresh, cache-read and
    /// cache-write prompt tokens, matching [`spot_llm::Usage::input_total`].
    #[must_use]
    pub fn priced_input(&self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }
}

/// Sums of the token counts over the records one query selected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TokenSums {
    /// Sum of fresh input tokens.
    pub input: u64,
    /// Sum of generated tokens.
    pub output: u64,
    /// Sum of cache-read tokens.
    pub cache_read: u64,
    /// Sum of cache-write tokens.
    pub cache_write: u64,
    /// How many records the sum covered.
    pub requests: u64,
}

/// Which records a [`UsageLedger::sum`] covers: every field set filters; every field
/// `None` selects everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SumQuery {
    /// Only this tenant.
    pub tenant: Option<String>,
    /// Only this task.
    pub task: Option<String>,
    /// Only this provider.
    pub provider: Option<String>,
    /// Only this model.
    pub model: Option<String>,
    /// Only this day.
    pub day: Option<DayKey>,
}

impl SumQuery {
    /// The everything query.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// The query filtered to one tenant.
    #[must_use]
    pub fn tenant(tenant: impl Into<String>) -> Self {
        Self {
            tenant: Some(tenant.into()),
            ..Self::default()
        }
    }

    /// Narrows the query to one task.
    #[must_use]
    pub fn and_task(mut self, task: impl Into<String>) -> Self {
        self.task = Some(task.into());
        self
    }

    /// Narrows the query to one provider.
    #[must_use]
    pub fn and_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Narrows the query to one model.
    #[must_use]
    pub fn and_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Narrows the query to one day.
    #[must_use]
    pub fn and_day(mut self, day: DayKey) -> Self {
        self.day = Some(day);
        self
    }
}

/// A day bucket: epoch milliseconds divided by day length, in UTC integer arithmetic.
///
/// No calendar crate: day boundaries are the plain `86_400_000`-millis grid, which is what
/// every billing window and [`BudgetScope::Day`] limit needs.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct DayKey(pub u64);

impl DayKey {
    /// The day of an epoch-milliseconds stamp.
    #[must_use]
    pub fn from_millis(at_millis: u64) -> Self {
        Self(at_millis / 86_400_000)
    }
}

/// Where settled usage is recorded: once per request id, summed by every dimension.
pub trait UsageLedger: Send + Sync {
    /// Records one settled request. A record for an id already recorded is dropped, so a
    /// settlement retry never double-counts.
    fn record(&self, record: UsageRecord);

    /// Sums the token counts of the records [`query`](SumQuery) selects.
    fn sum(&self, query: &SumQuery) -> TokenSums;
}

/// The in-memory ledger: records keyed by request id, first record wins.
#[derive(Debug, Default)]
pub struct MemoryLedger {
    records: Mutex<BTreeMap<String, UsageRecord>>,
}

impl MemoryLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many distinct requests the ledger holds.
    #[must_use]
    pub fn records_count(&self) -> usize {
        self.locked().len()
    }

    fn locked(&self) -> MutexGuard<'_, BTreeMap<String, UsageRecord>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl UsageLedger for MemoryLedger {
    fn record(&self, record: UsageRecord) {
        self.locked()
            .entry(record.request_id.clone())
            .or_insert(record);
    }

    fn sum(&self, query: &SumQuery) -> TokenSums {
        let mut sums = TokenSums::default();
        for record in self.locked().values() {
            let tenant_ok = query
                .tenant
                .as_ref()
                .is_none_or(|tenant| *tenant == record.tenant);
            let task_ok = query.task.as_ref().is_none_or(|task| *task == record.task);
            let provider_ok = query
                .provider
                .as_ref()
                .is_none_or(|provider| *provider == record.provider);
            let model_ok = query
                .model
                .as_ref()
                .is_none_or(|model| *model == record.model);
            let day_ok = query
                .day
                .is_none_or(|day| day == DayKey::from_millis(record.at));
            if tenant_ok && task_ok && provider_ok && model_ok && day_ok {
                sums.input += record.input;
                sums.output += record.output;
                sums.cache_read += record.cache_read;
                sums.cache_write += record.cache_write;
                sums.requests += 1;
            }
        }
        sums
    }
}

/// A price, in micro-units (millionths of a main unit) **per million tokens**.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Price {
    /// Micro-units per million input tokens.
    pub per_million_input: u64,
    /// Micro-units per million output tokens.
    pub per_million_output: u64,
}

impl Price {
    /// A price of `input` / `output` micro-units per million tokens.
    #[must_use]
    pub fn new(per_million_input: u64, per_million_output: u64) -> Self {
        Self {
            per_million_input,
            per_million_output,
        }
    }
}

/// Which model a price is for.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct PriceKey {
    /// The provider's id.
    pub provider: String,
    /// The model, in the provider's own spelling.
    pub model: String,
}

impl PriceKey {
    /// A key for `model` on `provider`.
    #[must_use]
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
        }
    }
}

/// Prices as data: `(provider, model)` to a [`Price`], with an optional default used for
/// any model the table does not name.
///
/// No money arithmetic lives here — [`cost`] does the one thing prices exist for.
#[derive(Clone, Debug, Default)]
pub struct PriceTable {
    prices: BTreeMap<PriceKey, Price>,
    default: Option<Price>,
}

impl PriceTable {
    /// An empty table: nothing is priced.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces one model's price; builders keep chaining.
    pub fn with_price(
        mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
        price: Price,
    ) -> Self {
        self.prices.insert(PriceKey::new(provider, model), price);
        self
    }

    /// Sets the default price for models without a row of their own; builders keep chaining.
    pub fn with_default(mut self, price: Price) -> Self {
        self.default = Some(price);
        self
    }

    /// The price for `model` on `provider`, or the table's default, or `None` when the model
    /// is unpriced.
    #[must_use]
    pub fn price_for(&self, provider: &str, model: &str) -> Option<Price> {
        self.prices
            .get(&PriceKey::new(provider, model))
            .copied()
            .or(self.default)
    }

    /// How many exact rows the table holds.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.prices.len()
    }
}

/// The cost of `(input, output)` tokens at a [`Price`], in micro-units, as an exact
/// integer with **round-half-up**: the token-side sums are multiplied first and divided by
/// a million last, so no rounding hides inside the multiplication and a half at the final
/// division rounds away from the cheaper side (up).
///
/// `u128` carries the products: the largest possible `u64` token count at the largest
/// possible `u64` micro-price stays far below `u128`'s range, so the arithmetic cannot
/// overflow. This is the crate's only money arithmetic (ADR-12: no floats).
#[must_use]
pub fn cost(input_tokens: u64, output_tokens: u64, price: &Price) -> u128 {
    let input = u128::from(input_tokens) * u128::from(price.per_million_input);
    let output = u128::from(output_tokens) * u128::from(price.per_million_output);
    (input + output + 500_000) / 1_000_000
}

/// Whose pot a [`Budget`] limits.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum BudgetScope {
    /// Everything one tenant spends, across every task and provider.
    Tenant {
        /// The tenant, as the request id's `tenant:` prefix names it.
        tenant: String,
    },
    /// Everything spent on one task, across every tenant (a task budget is platform-wide).
    Task {
        /// The task, as the routing table names it.
        task: String,
    },
    /// Everything spent on one UTC day ([`DayKey`]), across every tenant and task.
    Day {
        /// The day the budget limits.
        day: DayKey,
    },
}

impl BudgetScope {
    /// Whether the scope covers a request described by `context`.
    #[must_use]
    pub fn covers(&self, context: &BudgetContext) -> bool {
        match self {
            Self::Tenant { tenant } => tenant == &context.tenant,
            Self::Task { task } => task == &context.task,
            Self::Day { day } => day == &context.day,
        }
    }

    /// The scope's name, as errors and warnings render it.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::Tenant { tenant } => format!("tenant `{tenant}`"),
            Self::Task { task } => format!("task `{task}`"),
            Self::Day { day } => format!("day {}", day.0),
        }
    }
}

/// What a request is, to the budgets: the shapes the scopes match against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BudgetContext {
    /// The request's tenant.
    pub tenant: String,
    /// The request's task.
    pub task: String,
    /// The UTC day the request settles on.
    pub day: DayKey,
}

impl BudgetContext {
    /// A context for `tenant`, `task` and `day`.
    #[must_use]
    pub fn new(tenant: impl Into<String>, task: impl Into<String>, day: DayKey) -> Self {
        Self {
            tenant: tenant.into(),
            task: task.into(),
            day,
        }
    }
}

/// A spending cap on one scope, with the level a crossing warns at.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Budget {
    /// The pot the budget limits.
    pub scope: BudgetScope,
    /// The cap in micro-units.
    pub limit_micro: u64,
    /// The percent of the cap a crossing of which emits a warning; `0` disables warnings.
    pub warn_at_percent: u64,
}

impl Budget {
    /// A budget of `limit_micro` on `scope`, warning at `warn_at_percent` percent.
    #[must_use]
    pub fn new(scope: BudgetScope, limit_micro: u64, warn_at_percent: u64) -> Self {
        Self {
            scope,
            limit_micro,
            warn_at_percent,
        }
    }
}

/// A budget's warn level was crossed — the router hands this to its callback once per
/// budget, the first time projected spending reaches the level.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BudgetWarning {
    /// The budget that is running out.
    pub scope: BudgetScope,
    /// Projected spending at the crossing, in micro-units (settled spend, in-flight
    /// reservations, and the estimate that crossed the level).
    pub projected_micro: u128,
    /// The budget's cap, in micro-units.
    pub limit_micro: u64,
    /// The percent of the cap the projection reached.
    pub percent: u64,
}

/// Whether a [`BudgetBook::reserve`] admitted the request or a budget stopped it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReserveOutcome {
    /// Admitted: every budget covering the context reserved the estimate, and `warnings`
    /// lists each budget whose warn level the projection newly crossed.
    Reserved {
        /// The newly crossed warn levels, one per budget.
        warnings: Vec<BudgetWarning>,
    },
    /// Which scope was refused, with its spend and its limit.
    Exhausted(BudgetRefusal),
}

/// A budget refused a request's worst-case estimate.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BudgetRefusal {
    /// The scope that refused.
    pub scope: BudgetScope,
    /// What the scope had settled and held in reservations at the refusal, in micro-units.
    pub committed_micro: u128,
    /// The scope's cap, in micro-units.
    pub limit_micro: u64,
}

impl BudgetRefusal {
    /// The scope's name, as errors render it.
    #[must_use]
    pub fn name(&self) -> String {
        self.scope.name()
    }

    /// What the scope had settled and held in reservations, in micro-units.
    #[must_use]
    pub fn spent_micro(&self) -> u128 {
        self.committed_micro
    }

    /// The scope's cap, in micro-units.
    #[must_use]
    pub fn limit_micro(&self) -> u64 {
        self.limit_micro
    }
}

/// One budget's running state: the settled spend, the in-flight reservations, and whether
/// the warn level has already fired.
#[derive(Debug)]
struct BudgetState {
    limit_micro: u64,
    warn_at_percent: u64,
    settled_micro: u128,
    reserved_micro: u128,
    warned: bool,
}

impl BudgetState {
    fn percent_of(&self, committed_micro: u128) -> u64 {
        if self.limit_micro == 0 {
            return u64::MAX;
        }
        let percent = committed_micro * 100 / u128::from(self.limit_micro);
        u64::try_from(percent).unwrap_or(u64::MAX)
    }
}

/// The in-memory budgets and their reserves: the router's guard against a tenant, task or
/// day spending past its cap, including the estimates of concurrent in-flight requests.
///
/// The flow per request is `reserve` (worst-case estimate, before any provider call), then
/// `settle` (drop the estimate, add the actual cost). Between the two, the estimate counts
/// as spent on every scope it matched, so two concurrent calls each see the other's
/// reservation and the sum can overshoot only by the last admitted request's estimate.
#[derive(Debug, Default)]
pub struct BudgetBook {
    budgets: Mutex<BTreeMap<BudgetScope, BudgetState>>,
}

impl BudgetBook {
    /// An empty book: no budgets, everything admitted.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs or replaces the budget for its scope; builders keep chaining.
    pub fn set_budget(&self, budget: Budget) {
        self.locked().insert(
            budget.scope.clone(),
            BudgetState {
                limit_micro: budget.limit_micro,
                warn_at_percent: budget.warn_at_percent,
                settled_micro: 0,
                reserved_micro: 0,
                warned: false,
            },
        );
    }

    /// Reserves `estimate_micro` on every budget whose scope covers `context` — the
    /// worst-case admission check. Over-budget scopes refuse and nothing is reserved.
    #[must_use]
    pub fn reserve(&self, context: &BudgetContext, estimate_micro: u128) -> ReserveOutcome {
        let mut budgets = self.locked();
        let mut matched: Vec<(BudgetScope, u128, u64)> = Vec::new();
        for (scope, state) in budgets.iter_mut() {
            if !scope.covers(context) {
                continue;
            }
            let projected = state.settled_micro + state.reserved_micro + estimate_micro;
            if projected > u128::from(state.limit_micro) {
                let refusal = ReserveOutcome::Exhausted(BudgetRefusal {
                    scope: scope.clone(),
                    committed_micro: state.settled_micro + state.reserved_micro,
                    limit_micro: state.limit_micro,
                });
                for (rolled, rolled_estimate, _) in matched {
                    if let Some(state) = budgets.get_mut(&rolled) {
                        state.reserved_micro -= rolled_estimate;
                    }
                }
                return refusal;
            }
            matched.push((scope.clone(), estimate_micro, state.warn_at_percent));
            state.reserved_micro += estimate_micro;
        }

        let mut warnings = Vec::new();
        for (scope, _estimate, warn_at_percent) in matched {
            let Some(state) = budgets.get_mut(&scope) else {
                continue;
            };
            let projected = state.settled_micro + state.reserved_micro;
            if warn_at_percent == 0 || state.warned {
                continue;
            }
            let percent = state.percent_of(projected);
            if percent >= warn_at_percent {
                state.warned = true;
                warnings.push(BudgetWarning {
                    scope,
                    projected_micro: projected,
                    limit_micro: state.limit_micro,
                    percent,
                });
            }
        }
        ReserveOutcome::Reserved { warnings }
    }

    /// Settles one reserved request: releases its `estimate_micro` from every scope it
    /// matched and adds `actual_micro` to their settled spend, exactly once per request.
    pub fn settle(&self, context: &BudgetContext, estimate_micro: u128, actual_micro: u128) {
        let mut budgets = self.locked();
        for (scope, state) in budgets.iter_mut() {
            if !scope.covers(context) {
                continue;
            }
            state.reserved_micro -= state.reserved_micro.min(estimate_micro);
            state.settled_micro += actual_micro;
        }
    }

    /// What `scope` has settled and what it holds in reservations, for tests and reads.
    #[must_use]
    pub fn view(&self, scope: &BudgetScope) -> Option<(u128, u128)> {
        let budgets = self.locked();
        budgets
            .get(scope)
            .map(|state| (state.settled_micro, state.reserved_micro))
    }

    /// Releases a reservation without settling anything — what a dropped stream's
    /// accounting does when its consumer walked away before any usage was reported.
    pub fn release(&self, context: &BudgetContext, estimate_micro: u128) {
        let mut budgets = self.locked();
        for (scope, state) in budgets.iter_mut() {
            if !scope.covers(context) {
                continue;
            }
            state.reserved_micro -= state.reserved_micro.min(estimate_micro);
        }
    }

    fn locked(&self) -> MutexGuard<'_, BTreeMap<BudgetScope, BudgetState>> {
        self.budgets.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
