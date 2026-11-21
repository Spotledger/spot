//! `AiAdmission`: the gate every AI call passes through before it touches the database.
//!
//! It sits in front of `spot-mcp` (and any other agent call path). A call is admitted only
//! when all of these hold:
//!
//! - it asks for at most the per-call row and byte limits;
//! - its session deadline has not passed;
//! - its agent's and its tenant's request rates have a token to spend;
//! - its agent's and its tenant's circuit breakers are not open;
//! - the node's effective admission limit (the AIMD value, at most the dedicated pool size)
//!   has room, and the per-agent and per-tenant concurrency limits have room.
//!
//! When interactive load crosses the high thresholds the AIMD limit shrinks — down to zero, at
//! which point AI calls are shed with [`crate::error::SHED`] — and recovers gradually. The
//! permit returned by [`AiAdmission::admit`] releases its slot when dropped, so a cancelled or
//! timed-out call never leaks one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_coordinator::{Admit, BoxFuture, Coordinator};

use crate::breaker::{BreakerPolicy, KeyedBreakers, Outcome};
use crate::cancel::{StatementCanceller, StatementId};
use crate::config::{AiLimits, AiPoolSettings};
use crate::error::{AiError, COORDINATOR_UNAVAILABLE, Result, SHED};
use crate::health::AiHealthReport;
use crate::identity::{AgentId, TenantId};
use crate::load::{AimdController, LoadSignal, LoadThresholds};
use crate::rate::{LocalRateGate, RateGate};

/// The fixed breaker seed, so jitter is reproducible in tests and across restarts.
const BREAKER_SEED: u64 = 0x00A1_1507_7E57;

/// One AI call's request to admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AiRequest {
    /// The calling agent.
    pub agent: AgentId,
    /// The tenant the call acts for.
    pub tenant: TenantId,
    /// How many rows the call may ask for or return.
    pub rows: u32,
    /// How many bytes the call's input or result may carry.
    pub bytes: usize,
    /// The absolute monotonic instant by which the whole session must finish, when it has one.
    pub session_deadline: Option<Duration>,
}

impl AiRequest {
    /// A request from `agent` for `tenant`, within the default limits.
    #[must_use]
    pub fn new(agent: AgentId, tenant: TenantId) -> Self {
        Self {
            agent,
            tenant,
            rows: 0,
            bytes: 0,
            session_deadline: None,
        }
    }

    /// Sets the per-call row count.
    #[must_use]
    pub fn with_rows(mut self, rows: u32) -> Self {
        self.rows = rows;
        self
    }

    /// Sets the per-call byte count.
    #[must_use]
    pub fn with_bytes(mut self, bytes: usize) -> Self {
        self.bytes = bytes;
        self
    }

    /// Sets the session deadline (an absolute monotonic instant).
    #[must_use]
    pub fn with_session_deadline(mut self, deadline: Duration) -> Self {
        self.session_deadline = Some(deadline);
        self
    }
}

/// The AI admission gate for one node.
pub struct AiAdmission {
    limits: AiLimits,
    pool: AiPoolSettings,
    thresholds: LoadThresholds,
    load: Arc<dyn LoadSignal>,
    aimd: Mutex<AimdController>,
    agent_breakers: Mutex<KeyedBreakers<AgentId>>,
    tenant_breakers: Mutex<KeyedBreakers<TenantId>>,
    rate: LocalRateGate,
    counters: Mutex<Counters>,
    state: Mutex<Concurrency>,
    next_statement: Mutex<u64>,
}

/// The shed and refused totals.
#[derive(Debug, Default)]
struct Counters {
    shed: u64,
    refused: u64,
}

/// The live concurrency, per node, per agent and per tenant.
#[derive(Debug, Default)]
struct Concurrency {
    in_flight: u32,
    per_agent: HashMap<AgentId, u32>,
    per_tenant: HashMap<TenantId, u32>,
}

impl AiAdmission {
    /// A gate whose admission ceiling is the dedicated pool's size.
    ///
    /// # Errors
    ///
    /// [`crate::error::INVALID_ARGUMENT`] when the pool size is zero or a concurrency limit is
    /// zero.
    pub fn new(limits: AiLimits, pool: AiPoolSettings, load: Arc<dyn LoadSignal>) -> Result<Self> {
        if pool.pool_size == 0 {
            return Err(AiError::invalid_argument(
                "the AI pool size must be at least 1",
            ));
        }
        if limits.per_agent_concurrency == 0 || limits.per_tenant_concurrency == 0 {
            return Err(AiError::invalid_argument(
                "the per-agent and per-tenant concurrency limits must be at least 1",
            ));
        }
        let policy = BreakerPolicy::default();
        Ok(Self {
            limits,
            pool,
            thresholds: LoadThresholds::default(),
            load,
            aimd: Mutex::new(AimdController::new(pool.pool_size)?),
            agent_breakers: Mutex::new(KeyedBreakers::new(policy, BREAKER_SEED)),
            tenant_breakers: Mutex::new(KeyedBreakers::new(policy, BREAKER_SEED)),
            rate: LocalRateGate::new(),
            counters: Mutex::new(Counters::default()),
            state: Mutex::new(Concurrency::default()),
            next_statement: Mutex::new(0),
        })
    }

    /// Uses explicit load thresholds.
    #[must_use]
    pub fn with_thresholds(mut self, thresholds: LoadThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Uses an explicit breaker policy for both the agent and the tenant breakers.
    #[must_use]
    pub fn with_breaker_policy(mut self, policy: BreakerPolicy) -> Self {
        self.agent_breakers = Mutex::new(KeyedBreakers::new(policy, BREAKER_SEED));
        self.tenant_breakers = Mutex::new(KeyedBreakers::new(policy, BREAKER_SEED));
        self
    }

    /// The admission limits.
    #[must_use]
    pub fn limits(&self) -> &AiLimits {
        &self.limits
    }

    /// The dedicated pool budget.
    #[must_use]
    pub fn pool(&self) -> &AiPoolSettings {
        &self.pool
    }

    /// Admits one call on this node, using the in-process rate gate.
    ///
    /// # Errors
    ///
    /// [`crate::error::LIMIT_EXCEEDED`], [`crate::error::DEADLINE_EXCEEDED`],
    /// [`crate::error::RATE_LIMITED`], [`crate::error::CIRCUIT_OPEN`],
    /// [`crate::error::SHED`] or [`crate::error::BUSY`].
    pub fn admit(self: &Arc<Self>, request: &AiRequest, now: Duration) -> Result<Admission> {
        self.check_request(request)?;
        self.check_deadline(request, now)?;
        self.rate
            .take(&agent_key(request), self.limits.per_agent_rate, 1, now)
            .map_err(AiError::rate_limited)?;
        self.rate
            .take(&tenant_key(request), self.limits.per_tenant_rate, 1, now)
            .map_err(AiError::rate_limited)?;
        self.admit_after_rate(request, now)
    }

    /// Admits one call taking its request token from the cluster-wide coordinator first, so a
    /// fleet never admits more than the rate however many nodes race. The in-process gate is
    /// the fallback [`AiAdmission::admit`] uses when no coordinator is configured.
    pub fn admit_cluster<'a>(
        self: &'a Arc<Self>,
        coordinator: &'a dyn Coordinator,
        request: &'a AiRequest,
        now: Duration,
    ) -> BoxFuture<'a, Result<Admission>> {
        Box::pin(async move {
            self.check_request(request)?;
            self.check_deadline(request, now)?;
            take_cluster(coordinator, &agent_key(request), self.limits.per_agent_rate).await?;
            take_cluster(
                coordinator,
                &tenant_key(request),
                self.limits.per_tenant_rate,
            )
            .await?;
            self.admit_after_rate(request, now)
        })
    }

    /// How many AI calls are admitted right now.
    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.lock_state().in_flight
    }

    /// The current AIMD admission limit.
    #[must_use]
    pub fn aimd_limit(&self) -> u32 {
        self.lock_aimd().limit()
    }

    /// The typed health snapshot.
    #[must_use]
    pub fn health(&self) -> AiHealthReport {
        let in_flight = self.lock_state().in_flight;
        let aimd_limit = self.lock_aimd().limit();
        let (shed_total, refused_total) = {
            let counters = self.counters.lock().unwrap_or_else(|p| p.into_inner());
            (counters.shed, counters.refused)
        };
        let mut breakers = self
            .lock_agent_breakers()
            .health(|agent| format!("agent:{}", agent.as_str()));
        breakers.extend(
            self.lock_tenant_breakers()
                .health(|tenant| format!("tenant:{}", tenant.as_str())),
        );
        breakers.sort_by(|a, b| a.scope.cmp(&b.scope));
        AiHealthReport {
            in_flight,
            aimd_limit,
            shed_total,
            refused_total,
            breakers,
        }
    }

    /// The per-call row and byte bounds.
    fn check_request(&self, request: &AiRequest) -> Result<()> {
        if request.rows > self.limits.max_rows {
            return Err(AiError::limit_exceeded("row"));
        }
        if request.bytes > self.limits.max_bytes {
            return Err(AiError::limit_exceeded("byte"));
        }
        Ok(())
    }

    /// The session deadline.
    fn check_deadline(&self, request: &AiRequest, now: Duration) -> Result<()> {
        if let Some(deadline) = request.session_deadline
            && now >= deadline
        {
            return Err(AiError::deadline_exceeded("session"));
        }
        Ok(())
    }

    /// The shared tail of both admission paths: load, breakers and concurrency.
    fn admit_after_rate(self: &Arc<Self>, request: &AiRequest, now: Duration) -> Result<Admission> {
        let level = {
            let sample = self.load.sample(now);
            self.thresholds.level(&sample)
        };
        let limit = {
            let mut aimd = self.lock_aimd();
            aimd.on_sample(level);
            aimd.limit()
        };
        if limit == 0 {
            self.count_shed();
            return Err(AiError::new(
                SHED,
                "interactive load is high; AI calls are shed first",
            ));
        }

        self.lock_agent_breakers().admit(&request.agent, now)?;
        self.lock_tenant_breakers().admit(&request.tenant, now)?;

        let mut state = self.lock_state();
        if state.in_flight >= limit {
            drop(state);
            self.count_refused();
            return Err(AiError::busy("node"));
        }
        let agent_count = state.per_agent.get(&request.agent).copied().unwrap_or(0);
        if agent_count >= self.limits.per_agent_concurrency {
            drop(state);
            self.count_refused();
            return Err(AiError::busy("agent"));
        }
        let tenant_count = state.per_tenant.get(&request.tenant).copied().unwrap_or(0);
        if tenant_count >= self.limits.per_tenant_concurrency {
            drop(state);
            self.count_refused();
            return Err(AiError::busy("tenant"));
        }
        state
            .per_agent
            .insert(request.agent.clone(), agent_count + 1);
        state
            .per_tenant
            .insert(request.tenant.clone(), tenant_count + 1);
        state.in_flight += 1;
        drop(state);

        let statement = {
            let mut next = self
                .next_statement
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *next = next.wrapping_add(1);
            StatementId(*next)
        };
        Ok(Admission {
            shared: Arc::clone(self),
            agent: request.agent.clone(),
            tenant: request.tenant.clone(),
            statement,
            session_deadline: request.session_deadline,
            started: now,
        })
    }

    /// Records one outcome for an admitted call against both breakers.
    fn record(&self, agent: &AgentId, tenant: &TenantId, now: Duration, outcome: Outcome) {
        self.lock_agent_breakers().record(agent, now, outcome);
        self.lock_tenant_breakers().record(tenant, now, outcome);
    }

    /// Releases one admitted call's concurrency.
    fn release(&self, agent: &AgentId, tenant: &TenantId) {
        let mut state = self.lock_state();
        state.in_flight = state.in_flight.saturating_sub(1);
        if decrement(&mut state.per_agent, agent) {
            state.per_agent.remove(agent);
        }
        if decrement(&mut state.per_tenant, tenant) {
            state.per_tenant.remove(tenant);
        }
    }

    fn count_shed(&self) {
        let mut counters = self.counters.lock().unwrap_or_else(|p| p.into_inner());
        counters.shed = counters.shed.saturating_add(1);
    }

    fn count_refused(&self) {
        let mut counters = self.counters.lock().unwrap_or_else(|p| p.into_inner());
        counters.refused = counters.refused.saturating_add(1);
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, Concurrency> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn lock_aimd(&self) -> std::sync::MutexGuard<'_, AimdController> {
        self.aimd.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn lock_agent_breakers(&self) -> std::sync::MutexGuard<'_, KeyedBreakers<AgentId>> {
        self.agent_breakers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn lock_tenant_breakers(&self) -> std::sync::MutexGuard<'_, KeyedBreakers<TenantId>> {
        self.tenant_breakers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }
}

/// An admitted AI call. Dropping it releases its concurrency; [`Admission::cancel`] and
/// [`Admission::expire_if_past`] additionally cancel its database statement.
#[must_use = "the call's concurrency is released as soon as the permit is dropped"]
pub struct Admission {
    shared: Arc<AiAdmission>,
    agent: AgentId,
    tenant: TenantId,
    statement: StatementId,
    session_deadline: Option<Duration>,
    started: Duration,
}

impl Admission {
    /// The running statement's id.
    #[must_use]
    pub fn statement(&self) -> StatementId {
        self.statement
    }

    /// The instant this call must finish by: the earlier of its call timeout and its session
    /// deadline.
    #[must_use]
    pub fn deadline(&self) -> Duration {
        let call = self.started + self.shared.limits.call_timeout;
        match self.session_deadline {
            Some(session) => call.min(session),
            None => call,
        }
    }

    /// How long is left before the deadline.
    #[must_use]
    pub fn remaining(&self, now: Duration) -> Duration {
        self.deadline().saturating_sub(now)
    }

    /// Whether the deadline has passed.
    #[must_use]
    pub fn expired(&self, now: Duration) -> bool {
        now >= self.deadline()
    }

    /// Cancels the call: its statement is cancelled, its permit released, and the error the
    /// caller should report to the client is returned.
    pub fn cancel(self, canceller: &dyn StatementCanceller) -> AiError {
        canceller.cancel(self.statement);
        AiError::cancelled()
    }

    /// If the deadline has passed, cancels the statement and releases the permit, returning
    /// `true`; otherwise does nothing and returns `false`.
    pub fn expire_if_past(self, now: Duration, canceller: &dyn StatementCanceller) -> bool {
        if self.expired(now) {
            canceller.cancel(self.statement);
            true
        } else {
            false
        }
    }

    /// Records the call's outcome against the agent and tenant breakers and releases the permit.
    pub fn complete(self, now: Duration, outcome: Outcome) {
        self.shared.record(&self.agent, &self.tenant, now, outcome);
    }
}

impl Drop for Admission {
    fn drop(&mut self) {
        self.shared.release(&self.agent, &self.tenant);
    }
}

/// Decrements a scope's count, returning whether it reached zero.
fn decrement<K: std::hash::Hash + Eq>(map: &mut HashMap<K, u32>, key: &K) -> bool {
    match map.get_mut(key) {
        Some(count) => {
            *count = count.saturating_sub(1);
            *count == 0
        }
        None => false,
    }
}

/// Takes one token for `key` from the cluster-wide coordinator.
async fn take_cluster(
    coordinator: &dyn Coordinator,
    key: &str,
    limit: spot_coordinator::RateLimit,
) -> Result<()> {
    match coordinator.take(key, limit, 1).await {
        Ok(Admit::Allowed { .. }) => Ok(()),
        Ok(Admit::Denied { retry_after }) => Err(AiError::rate_limited(retry_after)),
        Err(error) => Err(AiError::new(
            COORDINATOR_UNAVAILABLE,
            format!(
                "the coordinator could not take the AI token: {}",
                error.code
            ),
        )),
    }
}

/// The rate-limit key for an agent.
fn agent_key(request: &AiRequest) -> String {
    format!("ai_agent_rate:{}", request.agent.as_str())
}

/// The rate-limit key for a tenant.
fn tenant_key(request: &AiRequest) -> String {
    format!("ai_tenant_rate:{}", request.tenant.as_str())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::breaker::{BreakerPolicy, BreakerState};
    use crate::error::{BUSY, DEADLINE_EXCEEDED, LIMIT_EXCEEDED, RATE_LIMITED, SHED};
    use crate::load::{LoadSample, StaticLoad};

    fn request(agent: &str, tenant: &str) -> AiRequest {
        AiRequest::new(
            AgentId::new(agent).expect("a valid agent"),
            TenantId::new(tenant).expect("a valid tenant"),
        )
    }

    fn gate(load: StaticLoad, pool_size: u32) -> Arc<AiAdmission> {
        let pool = AiPoolSettings {
            pool_size,
            ..AiPoolSettings::default()
        };
        Arc::new(
            AiAdmission::new(AiLimits::default(), pool, Arc::new(load))
                .expect("a valid configuration"),
        )
    }

    #[test]
    fn a_call_over_the_row_or_byte_limit_is_refused() {
        let gate = gate(StaticLoad::calm(), 4);
        let too_many_rows = request("a", "t").with_rows(10_000);
        assert_eq!(
            gate.admit(&too_many_rows, Duration::ZERO)
                .err()
                .map(|e| e.code),
            Some(LIMIT_EXCEEDED)
        );
        let too_many_bytes = request("a", "t").with_bytes(usize::MAX);
        assert_eq!(
            gate.admit(&too_many_bytes, Duration::ZERO)
                .err()
                .map(|e| e.code),
            Some(LIMIT_EXCEEDED)
        );
    }

    #[test]
    fn a_past_session_deadline_is_refused() {
        let gate = gate(StaticLoad::calm(), 4);
        let late = request("a", "t").with_session_deadline(Duration::from_secs(5));
        assert_eq!(
            gate.admit(&late, Duration::from_secs(10))
                .err()
                .map(|e| e.code),
            Some(DEADLINE_EXCEEDED)
        );
    }

    #[test]
    fn the_node_budget_is_bounded_by_the_pool_size() {
        let gate = gate(StaticLoad::calm(), 2);
        let first = gate
            .admit(&request("a", "t"), Duration::ZERO)
            .expect("first call");
        let second = gate
            .admit(&request("b", "t"), Duration::ZERO)
            .expect("second call");
        assert_eq!(
            gate.admit(&request("c", "t"), Duration::ZERO)
                .err()
                .map(|e| e.code),
            Some(BUSY)
        );
        drop(first);
        drop(second);
        assert_eq!(gate.in_flight(), 0);
        assert!(gate.admit(&request("c", "t"), Duration::ZERO).is_ok());
    }

    #[test]
    fn the_per_agent_limit_isolates_one_agent_from_another() {
        let pool = AiPoolSettings {
            pool_size: 8,
            ..AiPoolSettings::default()
        };
        let limits = AiLimits {
            per_agent_concurrency: 2,
            ..AiLimits::default()
        };
        let gate =
            Arc::new(AiAdmission::new(limits, pool, Arc::new(StaticLoad::calm())).expect("valid"));
        let _a1 = gate
            .admit(&request("a", "t"), Duration::ZERO)
            .expect("a first");
        let _a2 = gate
            .admit(&request("a", "t"), Duration::ZERO)
            .expect("a second");
        assert_eq!(
            gate.admit(&request("a", "t"), Duration::ZERO)
                .err()
                .map(|e| e.code),
            Some(BUSY)
        );
        assert!(gate.admit(&request("b", "t"), Duration::ZERO).is_ok());
    }

    #[test]
    fn an_overloaded_signal_shrinks_to_shedding() {
        let hot = StaticLoad::new(LoadSample {
            queue_depth: 1000,
            ..LoadSample::calm()
        });
        let gate = gate(hot, 4);
        let first = gate
            .admit(&request("a", "t"), Duration::ZERO)
            .expect("the first call is admitted");
        assert_eq!(
            gate.admit(&request("a", "t"), Duration::ZERO)
                .err()
                .map(|e| e.code),
            Some(BUSY)
        );
        assert_eq!(
            gate.admit(&request("a", "t"), Duration::ZERO)
                .err()
                .map(|e| e.code),
            Some(SHED)
        );
        assert_eq!(gate.aimd_limit(), 0);
        drop(first);
        let report = gate.health();
        assert_eq!(report.shed_total, 1);
        assert_eq!(report.refused_total, 1);
    }

    #[test]
    fn the_rate_limit_is_per_agent() {
        let limits = AiLimits {
            per_agent_rate: spot_coordinator::RateLimit {
                capacity: 1,
                per: Duration::from_secs(60),
            },
            ..AiLimits::default()
        };
        let pool = AiPoolSettings::default();
        let gate =
            Arc::new(AiAdmission::new(limits, pool, Arc::new(StaticLoad::calm())).expect("valid"));
        let first = gate
            .admit(&request("a", "t"), Duration::ZERO)
            .expect("first");
        drop(first);
        assert_eq!(
            gate.admit(&request("a", "t"), Duration::ZERO)
                .err()
                .map(|e| e.code),
            Some(RATE_LIMITED)
        );
        assert!(gate.admit(&request("b", "t"), Duration::ZERO).is_ok());
    }

    #[test]
    fn repeated_failures_open_the_breaker_and_the_health_surface_shows_it() {
        let policy = BreakerPolicy {
            window: 2,
            min_calls: 2,
            ..BreakerPolicy::default()
        };
        let admission = AiAdmission::new(
            AiLimits::default(),
            AiPoolSettings::default(),
            Arc::new(StaticLoad::calm()),
        )
        .expect("valid")
        .with_breaker_policy(policy);
        let gate = Arc::new(admission);
        for _ in 0..2 {
            let permit = gate
                .admit(&request("bad", "t"), Duration::ZERO)
                .expect("admitted");
            permit.complete(Duration::ZERO, Outcome::Failure);
        }
        assert_eq!(
            gate.admit(&request("bad", "t"), Duration::ZERO)
                .err()
                .map(|e| e.code),
            Some(crate::error::CIRCUIT_OPEN)
        );
        assert!(gate.admit(&request("good", "t2"), Duration::ZERO).is_ok());
        let report = gate.health();
        assert!(report.breakers.iter().any(|b| {
            b.scope == "agent:bad" && b.state == BreakerState::Open && b.open_until.is_some()
        }));
        assert!(!report.is_healthy());
    }

    #[test]
    fn a_cancelled_call_releases_its_slot() {
        let gate = gate(StaticLoad::calm(), 1);
        let canceller = crate::cancel::RecordingCanceller::new();
        let permit = gate
            .admit(&request("a", "t"), Duration::ZERO)
            .expect("admitted");
        let statement = permit.statement();
        let error = permit.cancel(&canceller);
        assert_eq!(error.code, crate::error::CANCELLED);
        assert_eq!(gate.in_flight(), 0);
        assert!(canceller.contains(statement));
        assert!(gate.admit(&request("a", "t"), Duration::ZERO).is_ok());
    }

    /// A load signal that can be switched from overloaded to calm, for the AIMD recovery test.
    struct Switch(Arc<AtomicBool>);

    impl LoadSignal for Switch {
        fn sample(&self, _now: Duration) -> LoadSample {
            if self.0.load(Ordering::SeqCst) {
                LoadSample {
                    queue_depth: 1000,
                    ..LoadSample::calm()
                }
            } else {
                LoadSample::calm()
            }
        }
    }

    #[test]
    fn aimd_recovers_gradually_after_the_load_clears() {
        let hot = Arc::new(AtomicBool::new(true));
        let gate = Arc::new(
            AiAdmission::new(
                AiLimits::default(),
                AiPoolSettings::default(),
                Arc::new(Switch(Arc::clone(&hot))),
            )
            .expect("valid"),
        );
        // Drive the limit to zero.
        for _ in 0..4 {
            let _ = gate.admit(&request("a", "t"), Duration::ZERO);
        }
        assert_eq!(gate.aimd_limit(), 0);
        hot.store(false, Ordering::SeqCst);
        // Each calm reading grows the limit by one, up to the pool size.
        for _ in 0..8 {
            let _ = gate.admit(&request("a", "t"), Duration::ZERO);
        }
        assert_eq!(gate.aimd_limit(), AiPoolSettings::default().pool_size);
    }
}
