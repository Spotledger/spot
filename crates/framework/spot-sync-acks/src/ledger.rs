//! The ledger: node state, the ladder and the conditions it drives.

use spot_conditions::{
    AuditSink, ConditionDef, ConditionEngine, ConditionParams, Millis, Registry, ResponseStep,
    TickReport as ConditionsTick,
};
use spot_sync_obligations::{
    Crossing, Level, MandatoryEvent, Notify, ObligationSatisfied, ObligationSettings, ReportRow,
    evidence, ladder, level_of, outstanding, report,
};

use crate::defs::{OBLIGATION_DUE_SOON, OBLIGATION_OVERDUE, OBLIGATION_RED, catalogue_defs};
use crate::error::AckError;
use crate::head::StreamHead;
use crate::rows::NodeReportRow;
use crate::store::{NodeRecord, NodeStateStore};

/// At most this many positions are written into a condition's `positions` parameter.
const MAX_POSITIONS: usize = 16;

/// One catalogue response step now due for a node that reached a ladder level.
///
/// The action id comes from the `spot-sync-conditions` catalogue (`mandatory_lane_first`,
/// `notify_contact`, `notify_role`). The ledger only reports it: the caller performs it (puts the
/// mandatory lane first, delivers the notification). Ids, never record data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueAction {
    /// The node the action concerns.
    pub node: String,
    /// The tenant whose obligations made it due.
    pub tenant: String,
    /// The level that made it due.
    pub level: Level,
    /// The catalogue's response action id.
    pub action: String,
    /// The catalogue's attempt budget for the step.
    pub limit: u32,
}

/// A notification to send, with the tenant whose obligations it is about.
///
/// The obligations crate's `Notify` names the node but no tenant, and a node serving two tenants
/// is notified per tenant, so the ledger wraps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantNotify {
    /// The tenant.
    pub tenant: String,
    /// The notification.
    pub notify: Notify,
}

/// A `(node, tenant)` pair whose level changed at a tick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelChange {
    /// The node.
    pub node: String,
    /// The tenant.
    pub tenant: String,
    /// The new level.
    pub level: Level,
}

/// What an ack, a check-in or a change of holds did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AckOutcome {
    /// The satisfaction evidence, when the move applied mandatory events. Both times are hub
    /// time: the hub records when it learned of the application, never the node's own clock.
    pub satisfied: Option<ObligationSatisfied>,
    /// Notifications now due (normally none: an ack only lowers a level).
    pub notifications: Vec<TenantNotify>,
    /// The catalogue's response actions now due (normally none), in rung order.
    pub actions: Vec<DueAction>,
}

/// What one [`AckLedger::tick`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Notifications to send, one per recipient per level reached, in (tenant, node) order.
    pub notifications: Vec<TenantNotify>,
    /// The catalogue's response actions due, one per step per level reached, in (tenant, node) order.
    pub actions: Vec<DueAction>,
    /// The `(node, tenant)` pairs whose level changed, with the new level, in (tenant, node)
    /// order.
    pub level_changes: Vec<LevelChange>,
    /// What the conditions engine committed (cleared after debounce, escalated).
    pub conditions: ConditionsTick,
}

/// The hub's acknowledgement ledger over a [`NodeStateStore`] and an [`AuditSink`].
///
/// See the crate documentation for the trust and time rules.
#[derive(Debug)]
pub struct AckLedger<S: NodeStateStore, A: AuditSink> {
    store: S,
    engine: ConditionEngine<A>,
    settings: ObligationSettings,
    /// The catalogue response steps of the overdue and red conditions, read at construction.
    overdue_steps: Vec<ResponseStep>,
    red_steps: Vec<ResponseStep>,
}

impl<S: NodeStateStore, A: AuditSink> AckLedger<S, A> {
    /// A ledger over `store`, writing audit events to `audit`, with the condition definitions
    /// `defs` (normally the catalogue's: see [`with_catalogue`](Self::with_catalogue)) and the
    /// obligation `settings`.
    ///
    /// # Errors
    /// [`AckError::Obligations`] when the settings are out of range; [`AckError::Conditions`]
    /// for a malformed or duplicate definition; [`AckError::ConditionMissing`] when `defs` lacks
    /// one of the three conditions the ledger raises.
    pub fn new(
        store: S,
        audit: A,
        defs: Vec<ConditionDef>,
        settings: ObligationSettings,
    ) -> Result<Self, AckError> {
        Self::with_debounce(
            store,
            audit,
            defs,
            settings,
            spot_conditions::DEFAULT_DEBOUNCE_MILLIS,
        )
    }

    /// A ledger whose three condition definitions come from the `spot-sync-conditions`
    /// catalogue, the one source of truth for their message keys, parameters and response steps.
    ///
    /// # Errors
    /// [`AckError::ConditionMissing`] when the catalogue lacks one of the three ids; otherwise as
    /// [`new`](Self::new).
    pub fn with_catalogue(
        store: S,
        audit: A,
        settings: ObligationSettings,
    ) -> Result<Self, AckError> {
        Self::new(store, audit, catalogue_defs()?, settings)
    }

    /// Like [`new`](Self::new) with an explicit debounce window for clearing conditions.
    ///
    /// # Errors
    /// As [`new`](Self::new).
    pub fn with_debounce(
        store: S,
        audit: A,
        defs: Vec<ConditionDef>,
        settings: ObligationSettings,
        debounce: Millis,
    ) -> Result<Self, AckError> {
        settings.validate()?;
        let mut registry = Registry::new();
        for def in defs {
            registry.register(def)?;
        }
        let steps = |id: &str| {
            registry
                .get(id)
                .map(|d| d.response.clone())
                .unwrap_or_default()
        };
        let overdue_steps = steps(OBLIGATION_OVERDUE);
        let red_steps = steps(OBLIGATION_RED);
        for id in [OBLIGATION_DUE_SOON, OBLIGATION_OVERDUE, OBLIGATION_RED] {
            if registry.get(id).is_none() {
                return Err(AckError::ConditionMissing { id });
            }
        }
        Ok(Self {
            store,
            engine: ConditionEngine::with_debounce(registry, audit, debounce),
            settings,
            overdue_steps,
            red_steps,
        })
    }

    /// The store, for a caller that snapshots it.
    #[must_use]
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The conditions engine: what is open, its history and its health summary.
    #[must_use]
    pub fn conditions(&self) -> &ConditionEngine<A> {
        &self.engine
    }

    /// Register a mandatory event of `tenant`. Idempotent by `(tenant, position)`: a second
    /// event at a position the tenant already has is ignored (the first stays), and the same
    /// position in another tenant is a different event. Returns whether the event was new.
    pub fn register_mandatory(&mut self, tenant: &str, event: MandatoryEvent) -> bool {
        if self
            .store
            .events(tenant)
            .iter()
            .any(|e| e.position == event.position)
        {
            return false;
        }
        self.store.save_event(tenant, event);
        true
    }

    /// Register a node for `tenant` at hub time `at`, with its software version tag, and an
    /// applied prefix of zero. Idempotent: a pair already registered is left untouched. Returns
    /// whether it was new.
    ///
    /// # Errors
    /// [`AckError::IdInvalid`] when the node or tenant id contains `@`.
    pub fn register_node(
        &mut self,
        node: &str,
        tenant: &str,
        version: &str,
        at: i64,
    ) -> Result<bool, AckError> {
        for id in [node, tenant] {
            if id.contains('@') {
                return Err(AckError::IdInvalid { id: id.to_owned() });
            }
        }
        if self.store.load(node, tenant).is_some() {
            return Ok(false);
        }
        self.store.save(NodeRecord {
            node: node.to_owned(),
            tenant: tenant.to_owned(),
            applied_prefix: 0,
            holds: Vec::new(),
            last_contact_millis: at,
            version: version.to_owned(),
            level: Level::Ok,
            crossings: Vec::new(),
        });
        Ok(true)
    }

    /// Record the software version tag a node reported. Unlike [`Self::on_checkin`], the version
    /// belongs to the node, not to a tenant, so every record of the node is updated.
    ///
    /// # Errors
    /// [`AckError::UnknownNode`] (with an empty tenant) when the node has no record at all.
    pub fn set_version(&mut self, node: &str, version: &str) -> Result<(), AckError> {
        let pairs = self.pairs_of(node)?;
        for tenant in pairs {
            let mut record = self.record(node, &tenant)?;
            version.clone_into(&mut record.version);
            self.store.save(record);
        }
        Ok(())
    }

    /// A node says it has applied `tenant`'s stream through `applied_prefix`, heard at hub time
    /// `at`.
    ///
    /// The node id and the tenant must be the authenticated ones (see the crate documentation).
    /// Only the `(node, tenant)` record is read or changed. A prefix equal to
    /// the recorded one is a check-in; a lower one is refused with nothing changed. `stream_head`
    /// is the newest position the hub's own stream for `tenant` holds, from the hub and never from the node;
    /// a prefix above it is refused with nothing changed (no record update, no crossing, no
    /// contact bump, no notification), so an ack never covers a position the hub had not written
    /// when the ack arrived.
    ///
    /// # Errors
    /// [`AckError::UnknownNode`]; [`AckError::PrefixRegressed`] (checked first);
    /// [`AckError::PrefixBeyondHead`].
    pub fn on_ack(
        &mut self,
        node: &str,
        tenant: &str,
        applied_prefix: u64,
        stream_head: StreamHead,
        at: i64,
    ) -> Result<AckOutcome, AckError> {
        let mut record = self.record(node, tenant)?;
        if applied_prefix < record.applied_prefix {
            return Err(AckError::PrefixRegressed {
                node: node.to_owned(),
                tenant: tenant.to_owned(),
                before: record.applied_prefix,
                after: applied_prefix,
            });
        }
        if applied_prefix > stream_head.get() {
            return Err(AckError::PrefixBeyondHead {
                node: node.to_owned(),
                tenant: tenant.to_owned(),
                claimed: applied_prefix,
                head: stream_head.get(),
            });
        }
        // Held records are below the prefix but not applied, so they do not count as crossed.
        let crossed: Vec<MandatoryEvent> = self
            .store
            .events(tenant)
            .into_iter()
            .filter(|e| !record.holds.contains(&e.position))
            .collect();
        let satisfied = evidence(
            node,
            record.applied_prefix,
            applied_prefix,
            &crossed,
            at,
            at,
        )?;
        if satisfied.is_some() {
            record.crossings.push(Crossing {
                through_position: applied_prefix,
                applied_at_millis: at,
            });
        }
        record.applied_prefix = applied_prefix;
        record.last_contact_millis = record.last_contact_millis.max(at);
        self.store.save(record);
        let (notifications, actions) = self.evaluate(node, tenant, at)?;
        Ok(AckOutcome {
            satisfied,
            notifications,
            actions,
        })
    }

    /// A node reported at hub time `at` for `tenant` and applied nothing new.
    ///
    /// Only the `(node, tenant)` record is read or changed: its last contact becomes
    /// `max(recorded, at)` and that pair is re-evaluated, so a check-in on one tenant's relay can
    /// never keep another tenant's silent node looking alive or delay its overdue notification.
    /// A caller that means every tenant calls once per pair. The call takes no node-supplied
    /// position, so there is nothing to bound by the stream head; `at` is the hub's clock.
    ///
    /// # Errors
    /// [`AckError::UnknownNode`] (with the tenant filled in) when the pair is not registered,
    /// also when the node is registered for other tenants; nothing is changed.
    pub fn on_checkin(
        &mut self,
        node: &str,
        tenant: &str,
        at: i64,
    ) -> Result<AckOutcome, AckError> {
        let mut record = self.record(node, tenant)?;
        record.last_contact_millis = record.last_contact_millis.max(at);
        self.store.save(record);
        let (notifications, actions) = self.evaluate(node, tenant, at)?;
        Ok(AckOutcome {
            satisfied: None,
            notifications,
            actions,
        })
    }

    /// Replace the set of mandatory positions a node holds back for `tenant`, as reported at hub
    /// time `at`; `stream_head` is the head of that tenant's stream.
    /// A position released by this call (held before, not now) counts as applied: it is recorded
    /// as a crossing and returned as evidence.
    ///
    /// # Errors
    /// [`AckError::UnknownNode`]; [`AckError::HoldUnknown`] for a position that is not a
    /// registered mandatory event, and [`AckError::HoldBeyondHead`] for a position above
    /// `stream_head`, the hub's own newest position (nothing is changed in either case; the head
    /// bound is checked first).
    pub fn set_holds(
        &mut self,
        node: &str,
        tenant: &str,
        mut holds: Vec<u64>,
        stream_head: StreamHead,
        at: i64,
    ) -> Result<AckOutcome, AckError> {
        let mut record = self.record(node, tenant)?;
        if let Some(position) = holds.iter().copied().find(|p| *p > stream_head.get()) {
            return Err(AckError::HoldBeyondHead {
                node: node.to_owned(),
                tenant: tenant.to_owned(),
                claimed: position,
                head: stream_head.get(),
            });
        }
        let events = self.store.events(tenant);
        for position in &holds {
            if !events.iter().any(|e| e.position == *position) {
                return Err(AckError::HoldUnknown {
                    node: node.to_owned(),
                    tenant: tenant.to_owned(),
                    position: *position,
                });
            }
        }
        holds.sort_unstable();
        holds.dedup();
        let released = record
            .holds
            .iter()
            .filter(|p| !holds.contains(p) && **p <= record.applied_prefix)
            .count() as u64;
        let satisfied = (released > 0).then(|| ObligationSatisfied {
            node: node.to_owned(),
            through_position: record.applied_prefix,
            count: released,
            applied_at_reported_millis: at,
            received_at_observed_millis: at,
        });
        if satisfied.is_some() {
            record.crossings.push(Crossing {
                through_position: record.applied_prefix,
                applied_at_millis: at,
            });
        }
        record.holds = holds;
        record.last_contact_millis = record.last_contact_millis.max(at);
        self.store.save(record);
        let (notifications, actions) = self.evaluate(node, tenant, at)?;
        Ok(AckOutcome {
            satisfied,
            notifications,
            actions,
        })
    }

    /// Evaluate every `(node, tenant)` pair at hub time `now`: compute the ladder level, send
    /// back the notifications for each rise, raise or clear the level conditions, and let the
    /// conditions engine commit what is due. Repeating a tick at the same level changes and
    /// emits nothing.
    ///
    /// # Errors
    /// Only from a corrupt store (a hold at an unregistered position).
    pub fn tick(&mut self, now: i64) -> Result<TickReport, AckError> {
        let mut report = TickReport::default();
        for (node, tenant) in self.store.nodes() {
            let before = self.level(&node, &tenant);
            let (notifications, actions) = self.evaluate(&node, &tenant, now)?;
            let after = self.level(&node, &tenant);
            if before != after
                && let Some(level) = after
            {
                report.level_changes.push(LevelChange {
                    node: node.clone(),
                    tenant: tenant.clone(),
                    level,
                });
            }
            report.notifications.extend(notifications);
            report.actions.extend(actions);
        }
        report.conditions = self.engine.tick(millis(now));
        Ok(report)
    }

    /// A node's level for `tenant` at its last evaluation, `None` for a pair that is not
    /// registered.
    #[must_use]
    pub fn level(&self, node: &str, tenant: &str) -> Option<Level> {
        self.store.load(node, tenant).map(|r| r.level)
    }

    /// Who has not applied `tenant`'s mandatory event at `event_position`, as of each node's
    /// last report, in node-id order. Only records of `tenant` are considered. A node counts
    /// when the position is above its prefix or held.
    ///
    /// # Errors
    /// [`AckError::UnknownEvent`] when the tenant has no event registered there.
    pub fn who_is_missing(
        &self,
        tenant: &str,
        event_position: u64,
    ) -> Result<Vec<NodeReportRow>, AckError> {
        self.event(tenant, event_position)?;
        let mut rows = Vec::new();
        for (node, t) in self.store.nodes() {
            if t != tenant {
                continue;
            }
            let Some(r) = self.store.load(&node, tenant) else {
                continue;
            };
            let held = r.holds.contains(&event_position);
            if event_position > r.applied_prefix || held {
                rows.push(NodeReportRow {
                    node: r.node,
                    tenant: r.tenant,
                    last_contact_millis: r.last_contact_millis,
                    applied_prefix: r.applied_prefix,
                    held,
                    version: r.version,
                    level: r.level,
                });
            }
        }
        Ok(rows)
    }

    /// The auditor's answer for one event of one tenant at one node: applied (and when),
    /// applied but not recorded, or outstanding since the event was published, as of the node's
    /// last contact.
    ///
    /// # Errors
    /// [`AckError::UnknownEvent`]; [`AckError::UnknownNode`].
    pub fn evidence(
        &self,
        tenant: &str,
        event_position: u64,
        node: &str,
    ) -> Result<ReportRow, AckError> {
        let event = self.event(tenant, event_position)?;
        let record = self.record(node, tenant)?;
        let rows = report(&[event], &[record.to_state()]);
        rows.into_iter().next().ok_or(AckError::UnknownEvent {
            tenant: tenant.to_owned(),
            position: event_position,
        })
    }

    fn record(&self, node: &str, tenant: &str) -> Result<NodeRecord, AckError> {
        self.store
            .load(node, tenant)
            .ok_or_else(|| AckError::UnknownNode {
                node: node.to_owned(),
                tenant: tenant.to_owned(),
            })
    }

    /// The tenants `node` is registered for, in tenant order; refused when there are none.
    fn pairs_of(&self, node: &str) -> Result<Vec<String>, AckError> {
        let tenants: Vec<String> = self
            .store
            .nodes()
            .into_iter()
            .filter(|(n, _)| n == node)
            .map(|(_, t)| t)
            .collect();
        if tenants.is_empty() {
            return Err(AckError::UnknownNode {
                node: node.to_owned(),
                tenant: String::new(),
            });
        }
        Ok(tenants)
    }

    fn event(&self, tenant: &str, position: u64) -> Result<MandatoryEvent, AckError> {
        self.store
            .events(tenant)
            .into_iter()
            .find(|e| e.position == position)
            .ok_or_else(|| AckError::UnknownEvent {
                tenant: tenant.to_owned(),
                position,
            })
    }

    /// Recompute one node's level at `now`; store it, raise or clear the level conditions, and
    /// return the notifications for a rise.
    fn evaluate(
        &mut self,
        node: &str,
        tenant: &str,
        now: i64,
    ) -> Result<(Vec<TenantNotify>, Vec<DueAction>), AckError> {
        let mut record = self.record(node, tenant)?;
        let obligations = outstanding(
            &self.store.events(tenant),
            record.applied_prefix,
            &record.holds,
        )?;
        let outcome = ladder(node, &obligations, now, &self.settings, record.level)?;
        // One condition per (node, tenant): `@` is refused in both ids at registration, so the
        // subject is unambiguous.
        let subject = format!("node:{node}@tenant:{tenant}");
        let at = millis(now);
        for (id, rung) in [
            (OBLIGATION_DUE_SOON, Level::Amber),
            (OBLIGATION_OVERDUE, Level::Overdue),
            (OBLIGATION_RED, Level::Red),
        ] {
            if outcome.level >= rung {
                let positions: Vec<u64> = obligations
                    .iter()
                    .filter(|o| level_of(o, now, &self.settings) >= rung)
                    .map(|o| o.event.position)
                    .collect();
                let params = condition_params(node, tenant, rung, &positions);
                self.engine.raise(id, &subject, params, at)?;
            } else {
                self.engine.observe_cleared(id, &subject, at)?;
            }
        }
        let mut actions = Vec::new();
        for (rung, steps) in [
            (Level::Overdue, &self.overdue_steps),
            (Level::Red, &self.red_steps),
        ] {
            if record.level < rung && outcome.level >= rung {
                actions.extend(steps.iter().map(|step| DueAction {
                    node: node.to_owned(),
                    tenant: tenant.to_owned(),
                    level: rung,
                    action: step.action.clone(),
                    limit: step.limit,
                }));
            }
        }
        if record.level != outcome.level {
            record.level = outcome.level;
            self.store.save(record);
        }
        let notifications = outcome
            .notifications
            .into_iter()
            .map(|notify| TenantNotify {
                tenant: tenant.to_owned(),
                notify,
            })
            .collect();
        Ok((notifications, actions))
    }
}

/// Hub time as the conditions crate's unsigned instant; a negative time reads as zero.
fn millis(now: i64) -> Millis {
    Millis::new(u64::try_from(now).unwrap_or(0))
}

/// The explanation parameters: the node, the tenant, the rung, and the positions at or above it. Never a
/// record key or any payload.
fn condition_params(node: &str, tenant: &str, rung: Level, positions: &[u64]) -> ConditionParams {
    let shown: Vec<String> = positions
        .iter()
        .take(MAX_POSITIONS)
        .map(u64::to_string)
        .collect();
    ConditionParams::new()
        .with("node", node)
        .with("tenant", tenant)
        .with(
            "level",
            match rung {
                Level::Ok => "ok",
                Level::Amber => "amber",
                Level::Overdue => "overdue",
                Level::Red => "red",
            },
        )
        .with("positions", shown.join(","))
        .with("count", positions.len().to_string())
}
