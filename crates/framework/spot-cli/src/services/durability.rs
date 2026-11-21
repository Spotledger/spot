//! The durability gate hosted (WP-RTB-01.T4, ADR-061 section 4,
//! `docs/running-spot/durability.md`).
//!
//! # What it does
//!
//! Installs the [`spot_durability_gate`] middleware on the site router
//! (`ctx.router_layer(.. route_layer(from_fn_with_state(..)))`): every mutating request
//! (`POST`, `PUT`, `PATCH`, `DELETE`) of a site whose journal is on waits, after its handler
//! succeeded, until the journal is durable up to the outbox head read after the commit; reads and
//! health routes are never gated. What a policy does and how the gate answers is in the crate's
//! docs; this module wires it to the site: the policy comes from the journal's settings
//! ([`JournalHandle`]), the journal position from its [`ProgressHandle`], the head from the
//! site's outbox.
//!
//! # Configuration
//!
//! `[services.durability]`: `wait_seconds` (default 5: how long a committed write waits for the
//! journal) and `retry_after_seconds` (default 5: the `Retry-After` of a refusal). The policy
//! itself is `[services.journal]` (`policy`, `on_unreachable`). `replica-sync` is refused at start
//! with `durability.no_standby`: this build has no standby confirmation, and the policy must not
//! silently behave as `local`.
//!
//! # Not gated here
//!
//! Non-HTTP writes (jobs, sync apply, scheduled work) are journalled and shipped by the journal
//! service and covered by the RPO alarm; a job whose effect must be journalled before an external
//! call is the job engine's committed-step concern.
//!
//! # Policy at runtime
//!
//! [`DurabilityHandle::change_policy`]: a change takes effect on the next request and writes
//! `sys_journal_admin` (`policy_changed`). A downgrade needs the tenant administrator and is
//! always audited; an upgrade needs the shipper healthy (`durability_gate.shipper_unhealthy`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use axum::middleware::from_fn_with_state;
use serde::Deserialize;
use spot_core::Clock;
use spot_durability_gate::{
    GateConfig, GateError, GateFuture, GateState, HeadSource, Journal, Kind, OnUnreachable, Policy,
    PolicyCell, PolicyChange, Stats, gate,
};
use spot_storage_contract::{
    Catalog, ChangeFeed, FeedPosition, Isolation, Name, NodeRef, Outbox, OutboxEvent, RecordId,
    StorageBackend, Transaction, TxOptions,
};

use crate::services::journal::{
    JournalHandle, OnUnreachable as JournalOnUnreachable, PolicyName, ProgressHandle,
};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// After the journal module (1100), whose handle it reads.
pub const ORDER: u32 = 1130;

/// `[services.durability]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// How long a committed write waits for the journal, in seconds (default 5).
    pub wait_seconds: u64,
    /// The `Retry-After` of a refusal, in seconds (default 5).
    pub retry_after_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            wait_seconds: 5,
            retry_after_seconds: 5,
        }
    }
}

/// The site's outbox head: published, read after the commit.
struct PgHead {
    backend: spot_postgres::PgBackend,
}

const HEAD_SQL: &str = "SELECT (SELECT value::bigint FROM spot_meta WHERE key = 'feed_generation'), \
                        last_seq FROM spot_feed_state WHERE log = 'outbox'";

impl HeadSource for PgHead {
    fn head(&self) -> GateFuture<'_, Option<(u64, u64)>> {
        Box::pin(async move {
            // Reading publishes what is pending, so an entry committed before this call has a
            // position; the read itself may be refused (resync-required) and has published anyway.
            let _ = self.backend.read_outbox(FeedPosition::START, 1).await;
            let conn = self.backend.pool().get().await.ok()?;
            let row = conn.query_one(HEAD_SQL, &[]).await.ok()?;
            let generation: i64 = row.try_get(0).ok()?;
            let seq: i64 = row.try_get(1).ok()?;
            Some((u64::try_from(generation).ok()?, u64::try_from(seq).ok()?))
        })
    }
}

/// The journal service's progress as the gate's [`Journal`].
struct ProgressJournal(ProgressHandle);

impl Journal for ProgressJournal {
    fn durable(&self) -> (u64, u64) {
        let position = self.0.durable();
        (position.generation, position.seq)
    }

    fn wait_durable(&self, position: (u64, u64), timeout: Duration) -> GateFuture<'_, bool> {
        Box::pin(async move {
            self.0
                .wait_durable(
                    spot_events_contract::envelope::Position {
                        generation: position.0,
                        seq: position.1,
                    },
                    timeout,
                )
                .await
        })
    }

    fn unreachable(&self) -> bool {
        let progress = self.0.now();
        progress.degraded || progress.stopped.is_some()
    }
}

fn policy_of(journal: &JournalHandle) -> Policy {
    Policy {
        kind: match journal.policy() {
            PolicyName::Local => Kind::Local,
            PolicyName::ReplicaSync => Kind::ReplicaSync,
            PolicyName::RemoteSync => Kind::RemoteSync,
        },
        on_unreachable: match journal.on_unreachable() {
            JournalOnUnreachable::Degrade => OnUnreachable::Degrade,
            JournalOnUnreachable::Halt => OnUnreachable::Halt,
        },
        journal_on: journal.enabled(),
    }
}

/// What other modules read: the policy, what the gate counted, and the policy change.
pub struct DurabilityHandle {
    policy: Arc<PolicyCell>,
    stats: Arc<Stats>,
    journal: ProgressHandle,
    backend: spot_postgres::PgBackend,
    catalog: Arc<Catalog>,
    clock: Arc<dyn Clock>,
}

impl DurabilityHandle {
    /// The policy in force.
    #[must_use]
    pub fn policy(&self) -> Policy {
        self.policy.current()
    }

    /// What the gate counted (acks, refusals, unknown outcomes, the p99 wait).
    #[must_use]
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// `durability: remote-sync (degrade), acks p99 41ms`, or `durability: off` without a journal.
    #[must_use]
    pub fn readiness_text(&self) -> String {
        let policy = self.policy.current();
        if !policy.journal_on {
            return "durability: off (no journal)".to_owned();
        }
        let mode = match policy.on_unreachable {
            OnUnreachable::Degrade => "degrade",
            OnUnreachable::Halt => "halt",
        };
        format!(
            "durability: {} ({mode}), acks p99 {}ms",
            policy.kind.text(),
            self.stats.p99_millis()
        )
    }

    /// Changes the policy (see the module docs). `requester` is a principal id and
    /// `is_tenant_admin` whether it holds the tenant administrator role.
    ///
    /// # Errors
    /// [`GateError`]; nothing changed and nothing is recorded as changed.
    pub async fn change_policy(
        &self,
        to: Policy,
        requester: &str,
        is_tenant_admin: bool,
    ) -> std::result::Result<PolicyChange, GateError> {
        let progress = self.journal.now();
        let healthy = progress.writer && !progress.degraded && progress.stopped.is_none();
        let change = self.policy.change(to, is_tenant_admin, healthy, false)?;
        self.audit_change(&change, requester).await;
        Ok(change)
    }

    async fn audit_change(&self, change: &PolicyChange, requester: &str) {
        let id = RecordId::new_v7();
        let now = self
            .clock
            .now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
        let target = format!("{}->{}", change.from.kind.text(), change.to.kind.text());
        let payload = serde_json::json!({
            "meta": {"occurred_at": now,
                     "actor": {"kind": "system", "component": "durability"},
                     "correlation_id": id.to_string()},
            "data": {"action": "policy_changed", "actor": requester, "target": target,
                     "outcome": "ok", "dry_run": false},
        })
        .to_string();
        let written = async {
            let event = OutboxEvent {
                id,
                topic: Name::new("sys_journal_admin").ok()?,
                subject: NodeRef {
                    entity: Name::new("journal_admin").ok()?,
                    id: RecordId::new_v7(),
                },
                subject_version: None,
                schema_version: 1,
                payload,
            };
            let mut options = TxOptions::new(
                Name::new("durability_audit").ok()?,
                Isolation::ReadCommitted,
            );
            options.max_writes = 10;
            let mut tx = self
                .backend
                .begin(Arc::clone(&self.catalog), options)
                .await
                .ok()?;
            tx.enqueue(&event).await.ok()?;
            tx.commit().await.ok()
        }
        .await;
        if written.is_none() {
            tracing::warn!("durability: the policy_changed record could not be written");
        }
    }
}

/// The module's service: the gate has no loop, but a service gives it a readiness entry.
struct DurabilityService;

impl Service for DurabilityService {
    fn name(&self) -> &'static str {
        "durability"
    }

    fn level(&self) -> Level {
        // The gate itself refuses writes under halt; losing the module means no gate, which is
        // the journal service's own `required` decision.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, mut stop: tokio::sync::watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let _ = stop.wait_for(|stopped| *stopped).await;
            Ok(())
        })
    }
}

fn provided() -> &'static Mutex<HashMap<String, Arc<DurabilityHandle>>> {
    static PROVIDED: OnceLock<Mutex<HashMap<String, Arc<DurabilityHandle>>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// The handle of a running site (for a harness).
#[must_use]
pub fn hosted(site: &str) -> Option<Arc<DurabilityHandle>> {
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(site)
        .cloned()
}

/// Registers the gate. The router layer is installed on every build (a reload rebuilds the
/// router); the service and the handle only on the first.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("durability")?;
    let site = ctx.label().to_owned();
    let env = *ctx.env()?;
    let backend = env.backend.clone();
    let catalog = Arc::clone(&env.permission.catalog);
    let clock: Arc<dyn Clock> = Arc::clone(ctx.clock());
    ctx.finally(move |ctx| {
        let journal = ctx.require_port::<JournalHandle>()?;
        let policy = policy_of(&journal);
        if policy.journal_on && policy.kind == Kind::ReplicaSync {
            return Err(Error(
                "durability_gate.no_standby: replica-sync needs a configured standby, and this build has none; \
                 use remote-sync or local"
                    .to_owned(),
            ));
        }
        let handle = match crate::services::durability::hosted(&site) {
            Some(existing) if !ctx.assembling() => existing,
            _ => {
                let progress = journal.progress().clone();
                Arc::new(DurabilityHandle {
                    policy: Arc::new(PolicyCell::new(policy)),
                    stats: Arc::new(Stats::default()),
                    journal: progress,
                    backend: backend.clone(),
                    catalog,
                    clock,
                })
            }
        };
        let state = Arc::new(GateState {
            policy: Arc::clone(&handle.policy),
            head: Arc::new(PgHead { backend }),
            journal: Arc::new(ProgressJournal(journal.progress().clone())),
            standby: None,
            config: GateConfig {
                wait: Duration::from_secs(settings.wait_seconds),
                retry_after_secs: settings.retry_after_seconds.max(1),
            },
            stats: Arc::clone(&handle.stats),
        });
        ctx.router_layer(move |router| router.route_layer(from_fn_with_state(state, gate)));
        if ctx.assembling() {
            ctx.add(Arc::new(DurabilityService));
            provided()
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(site, Arc::clone(&handle));
        }
        ctx.provide::<DurabilityHandle>(handle)?;
        Ok(())
    });
    Ok(())
}
