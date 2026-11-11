//! Shared rig for the router integration suites: builder inputs, a manual clock, a fresh
//! ledger, and the request/table helpers every suite uses.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_llm::Message;
use spot_llm::MockProvider;
use spot_llm::Provider;
use spot_llm::Request;
use spot_llm::Response;
use spot_llm::StopReason;
use spot_llm::Usage;
use spot_llm_router::BudgetBook;
use spot_llm_router::BudgetWarning;
use spot_llm_router::ManualClock;
use spot_llm_router::MemoryLedger;
use spot_llm_router::PriceTable;
use spot_llm_router::Route;
use spot_llm_router::Router;
use spot_llm_router::RouterConfig;
use spot_llm_router::RoutingTable;
use spot_llm_router::UsageLedger;

/// The manual clock's base in this suite.
pub const T0: u64 = 1_700_000_000_000;

/// A one-line text answer with this usage.
pub fn response(usage: Usage) -> Response {
    Response::new(Message::assistant("done"), StopReason::Stop, usage)
}

/// Usage of `input`/`output` tokens, no cache sides.
pub fn usage_at(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    }
}

/// A request for task `task` on tenant `acme`, caller id `caller`, with a user line.
pub fn request_for(task: &str, caller: &str) -> Request {
    Request::new("unset", format!("acme:{task}:{caller}")).with_message(Message::user("hello"))
}

/// A request for task `chat`, caller `r1`.
pub fn request(task: &str) -> Request {
    request_for(task, "r1")
}

/// Who a budget-warn crossing is handed to — the same callback type the router takes.
pub type WarnSink = Arc<dyn Fn(&BudgetWarning) + Send + Sync>;

/// Everything [`build_router`] consumes; fields default to their empty shapes.
pub struct Build {
    /// The routing table the router serves.
    pub table: RoutingTable,
    /// The providers, as `(id, mock)`; the id is the route's provider name.
    pub providers: Vec<(&'static str, Arc<MockProvider>)>,
    /// Retry and breaker numbers (defaults: 2 retries, 250→4000ms backoff, breaker at
    /// 3-in-60s for 30s).
    pub config: RouterConfig,
    /// Prices; an empty table leaves every model unpriced.
    pub prices: PriceTable,
    /// The budgets the router enforces.
    pub budgets: BudgetBook,
    /// The budget-warning callback, when the test observes crossings.
    pub on_warning: Option<WarnSink>,
}

impl Default for Build {
    fn default() -> Self {
        Self {
            table: RoutingTable::default(),
            providers: Vec::new(),
            config: RouterConfig::default(),
            prices: PriceTable::new(),
            budgets: BudgetBook::new(),
            on_warning: None,
        }
    }
}

/// Builds the router from `build`, backed by a manual clock at [`T0`] and a fresh ledger,
/// both returned for assertions.
pub fn build_router(build: Build) -> (Router, Arc<ManualClock>, Arc<MemoryLedger>) {
    let Build {
        table,
        providers,
        config,
        prices,
        budgets,
        on_warning,
    } = build;
    let mut map: BTreeMap<String, Arc<dyn Provider>> = BTreeMap::new();
    for (id, mock) in providers {
        map.insert(id.to_owned(), mock as Arc<dyn Provider>);
    }
    let ledger = Arc::new(MemoryLedger::new());
    let clock = Arc::new(ManualClock::at(T0));
    let mut router = Router::new(
        map,
        table,
        Arc::clone(&ledger) as Arc<dyn UsageLedger>,
        prices,
        budgets,
    )
    .with_config(config)
    .with_clock(Arc::clone(&clock) as Arc<dyn spot_llm_router::Clock>);
    if let Some(on_warning) = on_warning {
        router = router.with_warning_callback(on_warning);
    }
    (router, clock, ledger)
}

/// A chain `task -> [(provider, model)]`, built programmatically.
pub fn table_of(entries: &[(&str, &[(&str, &str)])]) -> RoutingTable {
    let mut routes: BTreeMap<String, Vec<Route>> = BTreeMap::new();
    for (task, chain) in entries {
        routes.insert(
            (*task).to_owned(),
            chain
                .iter()
                .map(|(provider, model)| Route::new(*provider, *model))
                .collect(),
        );
    }
    RoutingTable::new(routes)
}
