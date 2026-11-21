//! The [`Router`]: one [`Provider`] that routes by task, falls back, trips breakers and
//! enforces budgets.
//!
//! The flow of one [`Provider::complete`] through the router:
//!
//! 1. The request id's `tenant:task:` prefixes are parsed (the router's convention, see the
//!    crate documentation); anything else is `router.task_unrouted`.
//! 2. The task's route chain is read from the [`RoutingTable`]; an unknown task is
//!    `router.task_unrouted`.
//! 3. The first route whose breaker allows a try and whose [`Capabilities`] fit the request
//!    is chosen — a request with tools needs a tool-capable model, and the input estimate
//!    plus the worst-case output must fit the context window. A route the map does not
//!    name a provider for is skipped like an unfit one.
//! 4. The worst-case estimate ([`estimate_input_tokens`] plus `max_output_tokens`, priced
//!    when the route's model has a price) is reserved on every budget covering the request
//!    *before* any provider is called ([`BudgetBook::reserve`]); over budget the call ends
//!    `llm.budget_exceeded` with the provider still untouched. New warn crossings reach
//!    [`RouterProvider::on_warning`]'s callback.
//! 5. The chosen provider is called. A retryable failure (`llm.rate_limited`,
//!    `llm.overloaded`, `llm.timeout`, `llm.unavailable`) is retried up to
//!    [`RouterConfig::max_retries`] times on the same route — waiting the provider's
//!    `retry_after` when it named one, else capped exponential backoff, all on the
//!    [`Clock`]; `llm.auth`, `llm.bad_request`, `llm.context_too_long`,
//!    `llm.content_filtered` and `llm.protocol` are not retried: they move to the next
//!    route at once, as does a retryable failure whose retries are spent.
//! 6. Every attempt lands in the route's breaker: `breaker_max_failures` failures inside
//!    the failure window open it for the cool-down (so the *next* request skips the
//!    route), any success clears the count and closes the breaker.
//! 7. The first success settles the reserve to the real cost and records the usage in the
//!    ledger (idempotent per request id) on the way out. When every route has failed or
//!    been skipped, the result is `router.routes_exhausted` with the last failure as the
//!    detail.
//!
//! `Provider::stream` follows the same steps to the first opened stream, then hands the
//! consumer a [`RouterStream`] that wraps the provider's events: the reserve settles and
//! the usage records the moment the stream reports a usage event, and a stream the consumer
//! abandons early releases its reserve on drop, so it cannot hang over the budget forever.
//! A mid-stream failure is passed through, not rerouted — the consumer has already seen
//! events, and replaying them onto another provider is its decision.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use spot_llm::BoxFuture;
use spot_llm::BoxStream;
use spot_llm::Capabilities;
use spot_llm::LlmError;
use spot_llm::Provider;
use spot_llm::Request;
use spot_llm::Response;
use spot_llm::Usage;

use crate::clock::Clock;
use crate::error::RouterError;
use crate::ledger::BudgetBook;
use crate::ledger::BudgetContext;
use crate::ledger::BudgetRefusal;
use crate::ledger::BudgetWarning;
use crate::ledger::DayKey;
use crate::ledger::PriceTable;
use crate::ledger::ReserveOutcome;
use crate::ledger::SumQuery;
use crate::ledger::TokenSums;
use crate::ledger::UsageLedger;
use crate::ledger::UsageRecord;
use crate::ledger::cost;
use crate::table::Route;
use crate::table::RoutingTable;

/// The numbers retry and circuit breaker run with; defaults are 2 retries on the same
/// route, backoff from 250 ms doubling to a 4 s cap, and a breaker at 3 failures inside a
/// 60 s window for a 30 s cool-down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouterConfig {
    /// Retries of one retryable failure **on the same route** before falling back.
    pub max_retries: u32,
    /// The first backoff wait on a retryable failure without a provider `retry_after`.
    pub base_backoff_millis: u64,
    /// The backoff ceiling; `min(base << n, cap)` is the wait.
    pub max_backoff_millis: u64,
    /// How many failures inside the window open a route's breaker.
    pub breaker_max_failures: u32,
    /// The window a failure count lives in, in milliseconds.
    pub breaker_failure_window_millis: u64,
    /// How long an open breaker skips its route, in milliseconds.
    pub breaker_cooldown_millis: u64,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            max_retries: 2,
            base_backoff_millis: 250,
            max_backoff_millis: 4_000,
            breaker_max_failures: 3,
            breaker_failure_window_millis: 60_000,
            breaker_cooldown_millis: 30_000,
        }
    }
}

impl RouterConfig {
    /// A copy with `max_retries` replaced; builders keep chaining.
    #[must_use]
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// A copy with the backoff schedule replaced; builders keep chaining.
    #[must_use]
    pub fn with_backoff(mut self, base_backoff_millis: u64, max_backoff_millis: u64) -> Self {
        self.base_backoff_millis = base_backoff_millis;
        self.max_backoff_millis = max_backoff_millis;
        self
    }

    /// A copy with the breaker numbers replaced; builders keep chaining.
    #[must_use]
    pub fn with_breaker(
        mut self,
        breaker_max_failures: u32,
        breaker_failure_window_millis: u64,
        breaker_cooldown_millis: u64,
    ) -> Self {
        self.breaker_max_failures = breaker_max_failures;
        self.breaker_failure_window_millis = breaker_failure_window_millis;
        self.breaker_cooldown_millis = breaker_cooldown_millis;
        self
    }
}

/// One route's breaker: the recent failure stamps, and the cool-down an opener granted.
///
/// Count inside a window: `max_failures` failures newer than `now - window` open the
/// breaker until `now + cooldown`; a success clears the count and closes it; after the
/// cool-down the route is tried again (success closing, failure re-counting).
#[derive(Debug)]
struct Breaker {
    failures: Vec<u64>,
    open_until: Option<u64>,
}

impl Breaker {
    fn new() -> Self {
        Self {
            failures: Vec::new(),
            open_until: None,
        }
    }

    /// Whether the breaker is open (route must be skipped) at `now`.
    fn is_open(&self, now: u64) -> bool {
        matches!(self.open_until, Some(until) if now < until)
    }

    /// Records one failure; opens the breaker when the window's count reaches the cap.
    fn record_failure(&mut self, config: &RouterConfig, now: u64) {
        self.failures.push(now);
        let window_start = now.saturating_sub(config.breaker_failure_window_millis);
        self.failures.retain(|at| *at >= window_start);
        if self.failures.len() >= usize::try_from(config.breaker_max_failures).unwrap_or(usize::MAX)
        {
            self.open_until = Some(now.saturating_add(config.breaker_cooldown_millis));
            self.failures.clear();
        }
    }

    /// Records one success: clears the count and closes the breaker.
    fn record_success(&mut self) {
        self.failures.clear();
        self.open_until = None;
    }
}

/// The breaker state of every `(provider, model)` route that has an attempt behind it.
#[derive(Debug, Default)]
struct RouteState {
    breakers: BTreeMap<(String, String), Breaker>,
}

/// The `tenant:task:` prefixes parsed from a request id (`{tenant}:{task}:{caller-id}`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct RequestScope {
    tenant: String,
    task: String,
}

impl RequestScope {
    fn parse(request_id: &str) -> Option<Self> {
        let (tenant, rest) = request_id.split_once(':')?;
        let (task, _caller) = rest.split_once(':')?;
        let tenant = tenant.trim();
        let task = task.trim();
        if tenant.is_empty() || task.is_empty() {
            return None;
        }
        Some(Self {
            tenant: tenant.to_owned(),
            task: task.to_owned(),
        })
    }
}

/// The worst-case cost a request is admitted on, priced when the route's model has a row
/// in the [`PriceTable`].
///
/// The budget's unit is the micro-unit; an unpriced model reserves its token estimate
/// one-for-one (a token a micro-unit), so budgets stop it too, coarsely. A settled request
/// settles at its real price, or at its real token sum when unpriced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Estimate {
    /// Reserved micro-units (token estimate on an unpriced model).
    pub estimate_micro: u128,
    /// The context the estimate was reserved against — the settle key must be the same
    /// scopes, including the same day, or a reservation could leak.
    pub context: BudgetContext,
}

/// What a completed attempt left behind for the accounting to settle.
enum Completion {
    /// A one-shot answer: the accounting settles before `complete` returns.
    Answer(Response),
    /// An opened stream: the accounting settles when the [`RouterStream`] sees a usage
    /// event, or releases on drop when the consumer walks away earlier.
    Opened(BoxStream, StreamAccounting),
}

/// What the [`RouterStream`] needs to settle or release its reserve.
struct StreamAccounting {
    book: Arc<BudgetBook>,
    ledger: Arc<dyn UsageLedger>,
    prices: PriceTable,
    request_id: String,
    estimate: Estimate,
    provider: String,
    model: String,
    settled: bool,
}

impl StreamAccounting {
    /// Settles the reserve against `usage`, exactly once.
    fn settle(&mut self, usage: &Usage) {
        if self.settled {
            return;
        }
        self.settled = true;
        let actual = if let Some(price) = self.prices.price_for(&self.provider, &self.model) {
            cost(usage.input_total(), usage.output_tokens, &price)
        } else {
            u128::from(usage.input_total().saturating_add(usage.output_tokens))
        };
        self.book
            .settle(&self.estimate.context, self.estimate.estimate_micro, actual);
        self.ledger.record(UsageRecord {
            at: self.ledger_now(),
            tenant: self.estimate.context.tenant.clone(),
            task: self.estimate.context.task.clone(),
            provider: self.provider.clone(),
            model: self.model.clone(),
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_read: usage.cache_read_tokens,
            cache_write: usage.cache_write_tokens,
            request_id: self.request_id.clone(),
        });
    }

    /// The settlement stamp: the stream carries no clock, so the day bucket is opened —
    /// the first millisecond of the day the request's context reserved against. A record
    /// on an open day keeps the request's own day sums and budgets exact; only a
    /// wall-clock sort inside one day loses sub-day order.
    fn ledger_now(&self) -> u64 {
        self.estimate.context.day.0.saturating_mul(86_400_000)
    }
}

impl Drop for StreamAccounting {
    fn drop(&mut self) {
        if !self.settled {
            self.book
                .release(&self.estimate.context, self.estimate.estimate_micro);
        }
    }
}

/// Who a budget-warn crossing is handed to, once per newly crossed level.
pub type WarnCallback = Arc<dyn Fn(&BudgetWarning) + Send + Sync>;

/// The router as a [`Provider`]: build it, wire its budgets, hand it to an agent as
/// `Arc<dyn Provider>`.
pub struct Router {
    providers: BTreeMap<String, Arc<dyn Provider>>,
    table: RoutingTable,
    config: RouterConfig,
    clock: Arc<dyn Clock>,
    ledger: Arc<dyn UsageLedger>,
    prices: PriceTable,
    book: Arc<BudgetBook>,
    on_warning: Option<WarnCallback>,
    state: Mutex<RouteState>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("table", &self.table)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// A rough input-token estimate: text bytes across messages and system, plus tool
/// schemas, divided by four (the usual chars-per-token ballpark for the models this
/// platform routes to). The budget wants a request's size before any provider is called;
/// the provider reports the real usage afterwards.
#[must_use]
pub fn estimate_input_tokens(request: &Request) -> u64 {
    let mut bytes = request.system.as_deref().map_or(0, str::len);
    for message in &request.messages {
        bytes += message.text_content().len();
        for tool_use in message.tool_uses() {
            bytes += tool_use.arguments.to_string().len();
        }
    }
    for tool in &request.tools {
        bytes += tool.name.len() + tool.description.len() + tool.input_schema.to_string().len();
    }
    (bytes as u64 / 4).max(1)
}

impl Router {
    /// A router over `providers`, routing `table`'s tasks, accounting into `ledger`,
    /// pricing with `prices` and enforcing `budgets`, with default retry and breaker
    /// numbers and the system clock; `with_*` builders change the rest.
    pub fn new(
        providers: BTreeMap<String, Arc<dyn Provider>>,
        table: RoutingTable,
        ledger: Arc<dyn UsageLedger>,
        prices: PriceTable,
        budgets: BudgetBook,
    ) -> Self {
        Self {
            providers,
            table,
            config: RouterConfig::default(),
            clock: Arc::new(crate::clock::SystemClock),
            ledger,
            prices,
            book: Arc::new(budgets),
            on_warning: None,
            state: Mutex::new(RouteState::default()),
        }
    }

    /// A copy with `config` replaced; builders keep chaining.
    #[must_use]
    pub fn with_config(mut self, config: RouterConfig) -> Self {
        self.config = config;
        self
    }

    /// A copy with `clock` replaced (tests pass a `ManualClock`); builders keep chaining.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// A copy with the budget-warning callback installed — every new warn crossing a
    /// reservation produces is handed to it; builders keep chaining.
    #[must_use]
    pub fn with_warning_callback(mut self, on_warning: WarnCallback) -> Self {
        self.on_warning = Some(on_warning);
        self
    }

    /// The ledger's token sums for `query` — the read side of the accounting.
    #[must_use]
    pub fn usage(&self, query: &SumQuery) -> TokenSums {
        self.ledger.sum(query)
    }

    /// The budget book — budgets, spends and reservations, for reads and tests.
    #[must_use]
    pub fn budgets(&self) -> &BudgetBook {
        &self.book
    }

    /// The routing table, for reads and tests.
    #[must_use]
    pub fn table(&self) -> &RoutingTable {
        &self.table
    }

    /// The ledger the router records into, so an embedder shares the sink with it.
    #[must_use]
    pub fn ledger(&self) -> &Arc<dyn UsageLedger> {
        &self.ledger
    }

    fn locked(&self) -> MutexGuard<'_, RouteState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The chain's first route from `min_index` on that the breaker allows and whose
    /// capabilities fit the request; `None` when nothing after that is usable. Routes the
    /// provider map does not name, breaker-open routes and unfit routes are all skipped
    /// without touching their breaker state.
    fn select_route(
        &self,
        task_routes: &[Route],
        request: &Request,
        min_index: usize,
        now: u64,
    ) -> Option<(usize, Arc<dyn Provider>, Capabilities)> {
        let needs_tools = !request.tools.is_empty();
        for (index, route) in task_routes.iter().enumerate().skip(min_index) {
            let Some(provider) = self.providers.get(&route.provider) else {
                continue;
            };
            let locked = self.locked();
            if locked
                .breakers
                .get(&(route.provider.clone(), route.model.clone()))
                .is_some_and(|breaker| breaker.is_open(now))
            {
                continue;
            }
            drop(locked);
            let capabilities = provider.capabilities(&route.model);
            if needs_tools && !capabilities.tools {
                continue;
            }
            let wanted_output =
                u64::from(request.max_output_tokens.unwrap_or(0)).min(capabilities.max_output);
            if u128::from(estimate_input_tokens(request)) + u128::from(wanted_output)
                > u128::from(capabilities.context_window)
            {
                continue;
            }
            return Some((index, Arc::clone(provider), capabilities));
        }
        None
    }

    /// Records one attempt's outcome in the attempt's route breaker.
    fn record_attempt(&self, route: &Route, succeeded: bool, now: u64) {
        let mut locked = self.locked();
        let breaker = locked
            .breakers
            .entry((route.provider.clone(), route.model.clone()))
            .or_insert_with(Breaker::new);
        if succeeded {
            breaker.record_success();
        } else {
            breaker.record_failure(&self.config, now);
        }
    }

    /// Reserves `estimate`'s worst case on the budgets covering `context`, handing warn
    /// crossings to the installed callback; a spent budget's refusal is returned.
    fn reserve(&self, context: &BudgetContext, estimate_micro: u128) -> Result<(), BudgetRefusal> {
        match self.book.reserve(context, estimate_micro) {
            ReserveOutcome::Reserved { warnings } => {
                for warning in &warnings {
                    if let Some(on_warning) = &self.on_warning {
                        on_warning(warning);
                    }
                }
                Ok(())
            }
            ReserveOutcome::Exhausted(refusal) => Err(refusal),
        }
    }

    /// The one flow `complete` and `stream` share: resolve, route, reserve, call with
    /// retries across the chain, keeping every breaker current. The route names the model:
    /// the caller addresses a task, so the `outbound` request carries the route's model —
    /// the request's own model id (if any) is the caller's echo of the last route.
    async fn execute(&self, request: &Request, streaming: bool) -> Result<Completion, RouterError> {
        let Some(scope) = RequestScope::parse(&request.request_id) else {
            return Err(RouterError::TaskUnrouted {
                task: request.request_id.clone(),
            });
        };
        let Some(chain) = self.table.chain(&scope.task).map(<[Route]>::to_vec) else {
            return Err(RouterError::TaskUnrouted {
                task: scope.task.clone(),
            });
        };

        let mut last_failure: Option<String> = None;
        let mut budget_refusal: Option<BudgetRefusal> = None;
        let mut any_provider_attempt = false;
        let mut min_index = 0usize;
        while let Some((index, provider, capabilities)) =
            self.select_route(&chain, request, min_index, self.clock.now_millis())
        {
            let route = &chain[index];
            min_index = index + 1;

            let context = BudgetContext::new(
                scope.tenant.clone(),
                scope.task.clone(),
                DayKey::from_millis(self.clock.now_millis()),
            );
            let estimate = self.worst_case(
                request,
                &route.provider,
                &route.model,
                &capabilities,
                &context,
            );
            if let Err(budget) = self.reserve(&context, estimate.estimate_micro) {
                // A refusal is this route's loss; a cheaper fallback may still fit the
                // estimate under the same budget. Only when no provider was ever attempted
                // does the request answer budget_exceeded — the brief's zero-calls rule.
                if budget_refusal.is_none() {
                    budget_refusal = Some(budget);
                }
                continue;
            }
            any_provider_attempt = true;

            let mut failures: u32 = 0;
            // The route names the model: the caller addresses a task (its request may name
            // any model id, or none), so the routed request carries the route's model.
            let mut outbound = request.clone();
            outbound.model = route.model.clone();
            loop {
                let outcome = if streaming {
                    provider.stream(outbound.clone()).await.map(|stream| {
                        Completion::Opened(
                            stream,
                            StreamAccounting {
                                book: Arc::clone(&self.book),
                                ledger: Arc::clone(&self.ledger),
                                prices: self.prices.clone(),
                                request_id: request.request_id.clone(),
                                estimate: estimate.clone(),
                                provider: route.provider.clone(),
                                model: route.model.clone(),
                                settled: false,
                            },
                        )
                    })
                } else {
                    provider
                        .complete(outbound.clone())
                        .await
                        .map(Completion::Answer)
                };
                match outcome {
                    Ok(completion) => {
                        self.record_attempt(route, true, self.clock.now_millis());
                        if let Completion::Answer(response) = &completion {
                            self.book.settle(
                                &estimate.context,
                                estimate.estimate_micro,
                                self.settle_micro(&response.usage, &route.provider, &route.model),
                            );
                            self.record_usage(
                                request,
                                &route.provider,
                                &route.model,
                                &response.usage,
                                &scope,
                            );
                        }
                        return Ok(completion);
                    }
                    Err(error) => {
                        self.record_attempt(route, false, self.clock.now_millis());
                        last_failure = Some(format!("{}: {error}", error.code()));
                        failures += 1;
                        if failures > self.config.max_retries || !error.retryable() {
                            break;
                        }
                        let wait_millis = error
                            .retry_after()
                            .map_or(self.next_backoff(failures - 1), |wait| {
                                u64::try_from(wait.as_millis()).unwrap_or(u64::MAX)
                            });
                        self.clock.wait(wait_millis).await;
                    }
                }
            }

            // The route's attempts are spent: the route never settles a usage, so its
            // reserve is released before the next route (or the return) is considered —
            // otherwise a spent route's estimate would hang over every budget it matched.
            self.book
                .release(&estimate.context, estimate.estimate_micro);
        }

        if let (Some(budget), false) = (budget_refusal.clone(), any_provider_attempt) {
            return Err(RouterError::BudgetExceeded {
                scope: budget.name(),
                spent: u64::try_from(budget.committed_micro).unwrap_or(u64::MAX),
                limit: budget.limit_micro,
            });
        }

        Err(RouterError::RoutesExhausted {
            task: scope.task.clone(),
            detail: last_failure.unwrap_or_else(|| {
                if let Some(budget) = budget_refusal {
                    format!("every route was skipped or refused: {}", budget.name())
                } else {
                    "every route was skipped".to_owned()
                }
            }),
        })
    }

    /// The backoff wait before retry number `failures_so_far` (0-based): the base doubled
    /// per failure, under the cap.
    fn next_backoff(&self, failures_so_far: u32) -> u64 {
        let doubling = failures_so_far.min(16);
        self.config
            .base_backoff_millis
            .saturating_mul(1_u64.checked_shl(doubling).unwrap_or(u64::MAX))
            .min(self.config.max_backoff_millis)
    }

    /// The worst-case estimate a request books on a route: input estimate plus the
    /// request's own `max_output_tokens` (or the model's cap), priced at the **route's**
    /// model — the outbound carries it, so the price row that would settle the real
    /// usage is the row that prices the estimate.
    fn worst_case(
        &self,
        request: &Request,
        provider_id: &str,
        route_model: &str,
        capabilities: &Capabilities,
        context: &BudgetContext,
    ) -> Estimate {
        let input_tokens = estimate_input_tokens(request);
        let output_tokens = u64::from(request.max_output_tokens.unwrap_or(0))
            .min(capabilities.max_output)
            .max(1);
        let estimate_micro = match self.prices.price_for(provider_id, route_model) {
            Some(price) => cost(input_tokens, output_tokens, &price),
            None => u128::from(input_tokens.saturating_add(output_tokens)),
        };
        Estimate {
            estimate_micro,
            context: context.clone(),
        }
    }

    /// What a settled [`Usage`] costs on a route, in the estimate's unit.
    fn settle_micro(&self, usage: &Usage, provider_id: &str, model: &str) -> u128 {
        if let Some(price) = self.prices.price_for(provider_id, model) {
            cost(usage.input_total(), usage.output_tokens, &price)
        } else {
            u128::from(usage.input_total().saturating_add(usage.output_tokens))
        }
    }

    /// Records settled usage in the ledger; failed attempts never record.
    fn record_usage(
        &self,
        request: &Request,
        provider_id: &str,
        model: &str,
        usage: &Usage,
        scope: &RequestScope,
    ) {
        self.ledger.record(UsageRecord {
            at: self.clock.now_millis(),
            tenant: scope.tenant.clone(),
            task: scope.task.clone(),
            provider: provider_id.to_owned(),
            model: model.to_owned(),
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_read: usage.cache_read_tokens,
            cache_write: usage.cache_write_tokens,
            request_id: request.request_id.clone(),
        });
    }

    /// The [`LlmError`] a [`RouterError`] shows through the [`Provider`] trait: the budget
    /// refusal is the trait's own `llm.budget_exceeded`, a request id without a readable
    /// `tenant:task:` (or a task the table does not know) is `llm.bad_request` with the
    /// typed code as detail, and exhaustion at every route is `llm.protocol` with its
    /// typed code — the caller sees an explained producer failure, not a bare code.
    fn as_llm_error(error: &RouterError) -> LlmError {
        match error {
            RouterError::BudgetExceeded { .. } => LlmError::BudgetExceeded,
            RouterError::TaskUnrouted { .. } => LlmError::BadRequest {
                detail: error.with_code(),
            },
            RouterError::ConfigInvalid { .. } | RouterError::RoutesExhausted { .. } => {
                LlmError::Protocol {
                    detail: error.with_code(),
                }
            }
        }
    }
}

/// The one [`Completion`] unwrap each flow uses — a crossed kind is programmer error.
impl Router {
    fn unwrap_answer(completion: Completion) -> Response {
        match completion {
            Completion::Answer(response) => response,
            Completion::Opened(..) => unreachable_error(),
        }
    }

    fn unwrap_opened(completion: Completion) -> (BoxStream, StreamAccounting) {
        match completion {
            Completion::Opened(stream, accounting) => (stream, accounting),
            Completion::Answer(..) => unreachable_error(),
        }
    }
}

impl Router {
    /// The full [`RouterError`] of a failed drive — the typed `router.*` code and its
    /// fields — for embedders that route through [`Provider::complete`] but report
    /// through [`RouterError`]; runs the same flow `complete` runs.
    pub async fn complete_detail(&self, request: Request) -> Result<Response, RouterError> {
        self.execute(&request, false)
            .await
            .map(Router::unwrap_answer)
    }

    /// The full [`RouterError`] of a failed streaming open, likewise typed.
    pub async fn stream_detail(&self, request: Request) -> Result<BoxStream, RouterError> {
        self.execute(&request, true).await.map(|completion| {
            let (inner, accounting) = Router::unwrap_opened(completion);
            Box::pin(RouterStream {
                inner,
                accounting: Some(accounting),
            }) as BoxStream
        })
    }
}

/// The impossible arm of a completion unwrap; the two flows call with disjoint modes.
fn unreachable_error() -> ! {
    panic!("the complete and stream flows never cross their completion kinds")
}

impl Provider for Router {
    fn id(&self) -> &str {
        "router"
    }

    /// The union of what the routes can serve: the router accepts every model id its
    /// table maps and every feature some route offers, so an agent asking
    /// `capabilities` about a routed model sees no artificial limit the chain's later
    /// routes would lift anyway.
    fn capabilities(&self, _model: &str) -> Capabilities {
        let mut union = Capabilities {
            context_window: 0,
            max_output: 0,
            tools: true,
            streaming: true,
            vision: false,
            prompt_caching: false,
        };
        for route in self.table.all().values().flatten() {
            if let Some(provider) = self.providers.get(&route.provider) {
                let capabilities = provider.capabilities(&route.model);
                union.context_window = union.context_window.max(capabilities.context_window);
                union.max_output = union.max_output.max(capabilities.max_output);
                union.vision |= capabilities.vision;
                union.prompt_caching |= capabilities.prompt_caching;
            }
        }
        union
    }

    fn complete<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<Response, LlmError>> {
        Box::pin(async move {
            self.execute(&request, false)
                .await
                .map(Router::unwrap_answer)
                .map_err(|error| Self::as_llm_error(&error))
        })
    }

    fn stream<'a>(&'a self, request: Request) -> BoxFuture<'a, Result<BoxStream, LlmError>> {
        Box::pin(async move {
            self.execute(&request, true)
                .await
                .map(|completion| {
                    let (inner, accounting) = Router::unwrap_opened(completion);
                    Box::pin(RouterStream {
                        inner,
                        accounting: Some(accounting),
                    }) as BoxStream
                })
                .map_err(|error| Self::as_llm_error(&error))
        })
    }
}

/// The routed stream: the provider's events with the router's accounting hooked on — the
/// reserve settles and records when the provider reports usage, and the consumer walking
/// away early releases the reserve on drop.
pub struct RouterStream {
    inner: BoxStream,
    accounting: Option<StreamAccounting>,
}

impl futures_core::Stream for RouterStream {
    type Item = Result<spot_llm::StreamEvent, LlmError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            std::task::Poll::Ready(Some(Ok(spot_llm::StreamEvent::Usage { usage }))) => {
                if let Some(accounting) = this.accounting.as_mut() {
                    accounting.settle(&usage);
                }
                std::task::Poll::Ready(Some(Ok(spot_llm::StreamEvent::Usage { usage })))
            }
            other => other,
        }
    }
}
