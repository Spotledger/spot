//! Shared harness for the side-effect hold's integration tests.
//!
//! A `World` is one site with an in-memory ledger, a policy table, a recording audit sink, a
//! recording dispatcher and a recording notifier. The scenarios build effects, push them through
//! the gate and then release or discard.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::future::BoxFuture;
use spot_core::SiteId;
use spot_effect_hold::class::ClassPolicyConfig;
use spot_effect_hold::{
    AuditSink, Dispatcher, EffectClass, EffectRef, EffectState, HoldService, HoldState,
    InMemoryLedger, LedgerStore, Notifier, PolicyConfig, PolicyTable, RecordingAudit,
    RecordingDispatcher, RecordingNotifier, ReleaseCursor, Result,
};

/// A fixed "now": 2023-11-14T22:13:20Z in Unix microseconds.
pub const NOW: i64 = 1_700_000_000_000_000;

/// The bank's `max_hold`: 24 hours in microseconds.
pub const DAY_MICROS: i64 = 24 * 60 * 60 * 1_000_000;

/// One site and its collaborators.
pub struct World {
    /// The site.
    pub site: SiteId,
    /// The reference ledger, shared by every service built over the world.
    pub ledger: Arc<InMemoryLedger>,
    /// The policy table.
    pub policies: Arc<PolicyTable>,
    /// The audit sink.
    pub audit: Arc<RecordingAudit>,
    /// The dispatcher (used by release).
    pub dispatcher: Arc<RecordingDispatcher>,
    /// The notifier (used by discard of `DropAndNotify` classes).
    pub notifier: Arc<RecordingNotifier>,
    /// The service over `ledger`.
    pub service: HoldService,
}

impl World {
    /// A service over an arbitrary ledger, sharing the world's audit and transports.
    #[must_use]
    pub fn service_over(&self, ledger: Arc<dyn LedgerStore>) -> HoldService {
        HoldService::new(
            self.site.clone(),
            ledger,
            Arc::clone(&self.policies),
            Arc::clone(&self.audit) as Arc<dyn AuditSink>,
            Arc::clone(&self.dispatcher) as Arc<dyn Dispatcher>,
            Arc::clone(&self.notifier) as Arc<dyn Notifier>,
        )
    }
}

/// The default policy table: fiscal never held; bank instructions notify a person on discard and
/// have a 24-hour maximum hold.
#[must_use]
pub fn policies() -> Arc<PolicyTable> {
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "bank_api".to_owned(),
        ClassPolicyConfig {
            hold: None,
            never_hold: false,
            notify_on_discard: true,
            max_hold_seconds: Some(24 * 60 * 60),
        },
    );
    Arc::new(
        PolicyTable::load(PolicyConfig {
            overrides,
            never_hold_allow: Vec::new(),
        })
        .expect("the default policy config is valid"),
    )
}

/// A fresh world with the default policies.
#[must_use]
pub fn world(name: &str) -> World {
    let site = SiteId::new(name).expect("the test site name is valid");
    let ledger = Arc::new(InMemoryLedger::new());
    let policies = policies();
    let audit = Arc::new(RecordingAudit::new());
    let dispatcher = Arc::new(RecordingDispatcher::new());
    let notifier = Arc::new(RecordingNotifier::new());
    let service = HoldService::new(
        site.clone(),
        Arc::clone(&ledger) as Arc<dyn LedgerStore>,
        Arc::clone(&policies),
        Arc::clone(&audit) as Arc<dyn AuditSink>,
        Arc::clone(&dispatcher) as Arc<dyn Dispatcher>,
        Arc::clone(&notifier) as Arc<dyn Notifier>,
    );
    World {
        site,
        ledger,
        policies,
        audit,
        dispatcher,
        notifier,
        service,
    }
}

/// A reference to an effect of `class`.
#[must_use]
pub fn effect(site: &SiteId, class: EffectClass, key: &str, source: &str) -> EffectRef {
    EffectRef::new(site.clone(), class, "effect", key, NOW).with_source(source)
}

/// The five holdable classes the scenario uses, round-robin.
#[must_use]
pub fn mixed_class(index: usize) -> EffectClass {
    match index % 5 {
        0 => EffectClass::Mail,
        1 => EffectClass::BankApi,
        2 => EffectClass::Webhook,
        3 => EffectClass::JobQueue("default".to_owned()),
        _ => EffectClass::EdgeSync,
    }
}

/// A ledger that fails every operation after `limit` calls, standing in for a crash between two
/// ledger operations. A fresh wrapper with a larger limit "restarts" and resumes from the ledger.
pub struct CrashyLedger {
    inner: Arc<InMemoryLedger>,
    limit: usize,
    count: AtomicUsize,
}

impl CrashyLedger {
    /// Wraps `inner`, failing after `limit` operations.
    #[must_use]
    pub fn new(inner: Arc<InMemoryLedger>, limit: usize) -> Arc<Self> {
        Arc::new(Self {
            inner,
            limit,
            count: AtomicUsize::new(0),
        })
    }

    fn tick(&self) -> Result<()> {
        let seen = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        if seen > self.limit {
            return Err(spot_effect_hold::Error::LedgerUnavailable {
                reason: "injected crash between ledger operations".to_owned(),
            });
        }
        Ok(())
    }
}

impl LedgerStore for CrashyLedger {
    fn append_effect<'a>(
        &'a self,
        effect: spot_effect_hold::NewEffect,
    ) -> BoxFuture<'a, Result<spot_effect_hold::EffectId>> {
        Box::pin(async move {
            self.tick()?;
            self.inner.append_effect(effect).await
        })
    }

    fn list_effects<'a>(
        &'a self,
        site: &'a SiteId,
        states: &'a [EffectState],
    ) -> BoxFuture<'a, Result<Vec<spot_effect_hold::EffectRecord>>> {
        Box::pin(async move {
            self.tick()?;
            self.inner.list_effects(site, states).await
        })
    }

    fn transition_effect<'a>(
        &'a self,
        id: spot_effect_hold::EffectId,
        expected: EffectState,
        next: EffectState,
        note: Option<String>,
    ) -> BoxFuture<'a, Result<spot_effect_hold::Transition>> {
        Box::pin(async move {
            self.tick()?;
            self.inner.transition_effect(id, expected, next, note).await
        })
    }

    fn get_hold<'a>(&'a self, site: &'a SiteId) -> BoxFuture<'a, Result<HoldState>> {
        Box::pin(async move {
            self.tick()?;
            self.inner.get_hold(site).await
        })
    }

    fn compare_and_set_hold<'a>(
        &'a self,
        site: &'a SiteId,
        expected: &'a HoldState,
        new: HoldState,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.tick()?;
            self.inner.compare_and_set_hold(site, expected, new).await
        })
    }

    fn checkpoint<'a>(
        &'a self,
        site: &'a SiteId,
        cursor: ReleaseCursor,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.tick()?;
            self.inner.checkpoint(site, cursor).await
        })
    }

    fn get_checkpoint<'a>(
        &'a self,
        site: &'a SiteId,
        migration_id: &'a spot_effect_hold::MigrationId,
    ) -> BoxFuture<'a, Result<Option<ReleaseCursor>>> {
        Box::pin(async move {
            self.tick()?;
            self.inner.get_checkpoint(site, migration_id).await
        })
    }
}
