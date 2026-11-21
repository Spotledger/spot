//! Alerting: turn findings into a small number of honest notifications.
//!
//! [`AlertManager`] is the stateful side of the engine. Each evaluation is passed in; the
//! manager decides what is new, what is still open, what has recovered and what has been
//! silenced. The behaviour mirrors ADR-067:
//!
//! - **Deduplication** is per `(subject, rule)`: one open alert per failure mode per subject,
//!   however many evaluation ticks see it.
//! - **Flap damping** is hysteresis on the way out: a finding that disappears does not resolve
//!   the alert until the subject has been clear for [`AlertSettings::resolve_after_seconds`].
//!   If the finding returns inside that window, the same alert stays open and nothing new is
//!   sent.
//! - **Grouping** is per tenant: one [`AlertNotification`] carries every transition for that
//!   tenant in the tick, instead of one message per rule.
//! - **Silences** suppress delivery until an explicit expiry; the alert still opens and its
//!   state is still visible.
//! - **Escalation** is a data-defined list of steps. A red alert that stays open past a step
//!   sends one escalation, once.
//!
//! Time is still an argument: [`AlertManager::observe`] and
//! [`AlertManager::add_silence`] take `now`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::FleetHealthError;
use crate::health::{Finding, FleetHealth, Health};
use crate::rules::RuleId;
use crate::signal::{Subject, TenantId};
use crate::sinks::{AlertSink, SinkError};
use crate::time::Timestamp;

/// The identity of one open alert: one rule on one subject.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AlertKey {
    /// The tenant or node.
    pub subject: Subject,
    /// The rule that opened it.
    pub rule_id: RuleId,
}

impl AlertKey {
    /// The key for `finding`.
    #[must_use]
    pub fn of(finding: &Finding) -> Self {
        Self {
            subject: finding.subject.clone(),
            rule_id: finding.rule_id.clone(),
        }
    }
}

/// One open alert.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alert {
    /// What the alert is about.
    pub key: AlertKey,
    /// The current colour (amber or red).
    pub health: Health,
    /// The plain-language sentence, kept up to date with the latest finding.
    pub sentence: String,
    /// The suggested action.
    pub action: String,
    /// When the alert first opened.
    pub opened_at: Timestamp,
    /// When the manager last saw the finding.
    pub updated_at: Timestamp,
    /// When the underlying breach began.
    pub since: Timestamp,
    /// Whether an active silence suppressed its notifications.
    pub silenced: bool,
    /// How many escalation steps have fired.
    pub escalated_steps: usize,
    /// When the finding was first absent, while the alert is damping.
    pub clear_since: Option<Timestamp>,
}

/// One escalation step: after this long open, say this once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EscalationStep {
    /// Seconds since the alert opened before the step fires.
    pub after_seconds: u64,
    /// The note appended to the notification.
    pub note: String,
}

/// The alert manager's data-defined policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertSettings {
    /// How long a finding must be absent before its alert resolves, in seconds.
    pub resolve_after_seconds: u64,
    /// Escalation steps, in increasing time order, applied to red alerts.
    pub escalation: Vec<EscalationStep>,
}

impl AlertSettings {
    /// The default policy: resolve after 60 clear seconds, escalate at 5 and 15 minutes.
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            resolve_after_seconds: 60,
            escalation: vec![
                EscalationStep {
                    after_seconds: 300,
                    note: "The alert is still open after five minutes.".to_owned(),
                },
                EscalationStep {
                    after_seconds: 900,
                    note: "The alert is still open after fifteen minutes; the on-call lead is now involved."
                        .to_owned(),
                },
            ],
        }
    }

    /// Checks the policy.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidSettings`] when the resolve delay is zero or a step is not
    /// strictly later than the one before it.
    pub fn validate(&self) -> Result<(), FleetHealthError> {
        if self.resolve_after_seconds == 0 {
            return Err(FleetHealthError::InvalidSettings {
                reason: "the alert resolve delay must be at least one second".to_owned(),
            });
        }
        let mut previous = 0_u64;
        for step in &self.escalation {
            if step.after_seconds <= previous {
                return Err(FleetHealthError::InvalidSettings {
                    reason: "escalation steps must be strictly increasing".to_owned(),
                });
            }
            if step.note.trim().is_empty() {
                return Err(FleetHealthError::InvalidSettings {
                    reason: "an escalation step needs a note".to_owned(),
                });
            }
            previous = step.after_seconds;
        }
        Ok(())
    }
}

impl Default for AlertSettings {
    fn default() -> Self {
        Self::defaults()
    }
}

/// A time-limited suppression of notifications.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Silence {
    /// The silence's id, returned when it was added.
    pub id: u64,
    /// Restrict to one tenant; `None` means any subject.
    pub tenant: Option<TenantId>,
    /// Restrict to one rule; `None` means any rule.
    pub rule: Option<RuleId>,
    /// The silence expires at this instant.
    pub until: Timestamp,
    /// Why it was silenced.
    pub reason: String,
}

impl Silence {
    /// Whether this silence covers `key` at `now`.
    #[must_use]
    pub fn covers(&self, key: &AlertKey, now: Timestamp) -> bool {
        if self.until <= now {
            return false;
        }
        if self.rule.as_ref().is_some_and(|rule| rule != &key.rule_id) {
            return false;
        }
        match &self.tenant {
            Some(tenant) => key.subject.tenant() == Some(tenant),
            None => true,
        }
    }
}

/// Why a notification was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationReason {
    /// A new alert opened.
    Opened,
    /// An open alert crossed an escalation step, or became red.
    Escalated,
    /// A silenced alert became deliverable after its silence expired.
    Reopened,
    /// A damped alert stayed clear long enough and resolved.
    Resolved,
    /// The group contained more than one kind of transition in the same tick.
    Mixed,
}

/// The compact form of an alert inside a notification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertSummary {
    /// The tenant or node.
    pub subject: Subject,
    /// The rule.
    pub rule_id: RuleId,
    /// Amber or red.
    pub health: Health,
    /// The sentence to show.
    pub sentence: String,
}

impl From<&Alert> for AlertSummary {
    fn from(alert: &Alert) -> Self {
        Self {
            subject: alert.key.subject.clone(),
            rule_id: alert.key.rule_id.clone(),
            health: alert.health,
            sentence: alert.sentence.clone(),
        }
    }
}

/// One grouped delivery: every transition for one tenant in one evaluation tick.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertNotification {
    /// The tenant the group belongs to; node alerts have no tenant.
    pub tenant: Option<TenantId>,
    /// The group's dominant transition.
    pub reason: NotificationReason,
    /// The alerts, worst first.
    pub alerts: Vec<AlertSummary>,
}

impl AlertNotification {
    /// A one-line subject for a mail or webhook sink.
    #[must_use]
    pub fn title(&self) -> String {
        let scope = self.tenant.as_ref().map_or_else(
            || "the fleet".to_owned(),
            |tenant| format!("tenant {tenant}"),
        );
        format!("{} alert for {scope}", self.reason_as_str())
    }

    /// The reason as a lower-case word.
    #[must_use]
    pub const fn reason_as_str(&self) -> &'static str {
        match self.reason {
            NotificationReason::Opened => "new",
            NotificationReason::Escalated => "escalated",
            NotificationReason::Reopened => "reopened",
            NotificationReason::Resolved => "resolved",
            NotificationReason::Mixed => "updated",
        }
    }
}

/// A sink that refused a notification. Delivery failure never stops the manager; the batch
/// carries the failure so the host can retry or log it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkFailure {
    /// The sink's name.
    pub sink: String,
    /// What went wrong; never a secret.
    pub message: String,
}

/// What one [`AlertManager::observe`] call did.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AlertBatch {
    /// Alerts opened in this tick.
    pub opened: Vec<Alert>,
    /// Alerts escalated (by time or by becoming red) in this tick.
    pub escalated: Vec<Alert>,
    /// Alerts resolved in this tick.
    pub resolved: Vec<Alert>,
    /// The grouped notifications the sinks received.
    pub notifications: Vec<AlertNotification>,
    /// Alerts whose notifications an active silence suppressed.
    pub suppressed: Vec<AlertKey>,
    /// Sink refusals.
    pub sink_failures: Vec<SinkFailure>,
}

/// The stateful alert engine.
pub struct AlertManager {
    settings: AlertSettings,
    open: BTreeMap<AlertKey, Alert>,
    silences: Vec<Silence>,
    next_silence_id: u64,
    sinks: Vec<Arc<dyn AlertSink>>,
}

impl AlertManager {
    /// Builds an empty manager with the policy `settings`.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidSettings`] when the policy is invalid.
    pub fn new(settings: AlertSettings) -> Result<Self, FleetHealthError> {
        settings.validate()?;
        Ok(Self {
            settings,
            open: BTreeMap::new(),
            silences: Vec::new(),
            next_silence_id: 1,
            sinks: Vec::new(),
        })
    }

    /// Builds a manager with delivery sinks.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidSettings`] when the policy is invalid.
    pub fn with_sinks(
        settings: AlertSettings,
        sinks: Vec<Arc<dyn AlertSink>>,
    ) -> Result<Self, FleetHealthError> {
        let mut manager = Self::new(settings)?;
        manager.sinks = sinks;
        Ok(manager)
    }

    /// Adds a sink after construction.
    pub fn add_sink(&mut self, sink: Arc<dyn AlertSink>) {
        self.sinks.push(sink);
    }

    /// The policy in force.
    #[must_use]
    pub fn settings(&self) -> &AlertSettings {
        &self.settings
    }

    /// The open alerts, in key order.
    pub fn open_alerts(&self) -> impl Iterator<Item = &Alert> {
        self.open.values()
    }

    /// The open alert for `key`, when it exists.
    #[must_use]
    pub fn alert(&self, key: &AlertKey) -> Option<&Alert> {
        self.open.get(key)
    }

    /// The silences currently known, including expired ones until they are pruned.
    #[must_use]
    pub fn silences(&self) -> &[Silence] {
        &self.silences
    }

    /// Adds a time-limited silence.
    ///
    /// # Errors
    ///
    /// [`FleetHealthError::InvalidSilence`] when the expiry is not after `now` or the reason is
    /// blank. Invalid silence: an operator must say why.
    pub fn add_silence(
        &mut self,
        tenant: Option<TenantId>,
        rule: Option<RuleId>,
        until: Timestamp,
        now: Timestamp,
        reason: impl Into<String>,
    ) -> Result<u64, FleetHealthError> {
        let reason = reason.into();
        if until <= now {
            return Err(FleetHealthError::InvalidSilence {
                reason: "a silence must expire in the future".to_owned(),
            });
        }
        if reason.trim().is_empty() {
            return Err(FleetHealthError::InvalidSilence {
                reason: "a silence needs a reason".to_owned(),
            });
        }
        let id = self.next_silence_id;
        self.next_silence_id = self.next_silence_id.saturating_add(1);
        self.silences.push(Silence {
            id,
            tenant,
            rule,
            until,
            reason,
        });
        Ok(id)
    }

    /// Drops expired silences, returning how many were removed.
    pub fn prune_silences(&mut self, now: Timestamp) -> usize {
        let before = self.silences.len();
        self.silences.retain(|silence| silence.until > now);
        before - self.silences.len()
    }

    /// Applies one evaluation to the alert state and delivers the grouped notifications.
    pub fn observe(&mut self, view: &FleetHealth, now: Timestamp) -> AlertBatch {
        let mut batch = AlertBatch::default();
        let active = active_findings(view);
        let mut transitions: Vec<(NotificationReason, Alert)> = Vec::new();

        // Cleared findings start the hysteresis clock; they resolve only after the delay.
        let mut resolved_keys: Vec<AlertKey> = Vec::new();
        for (key, alert) in &mut self.open {
            if active.contains_key(key) {
                continue;
            }
            match alert.clear_since {
                None => alert.clear_since = Some(now),
                Some(clear_since)
                    if clear_since.age(now) >= self.settings.resolve_after_seconds =>
                {
                    resolved_keys.push(key.clone());
                }
                Some(_) => {}
            }
        }
        for key in resolved_keys {
            if let Some(alert) = self.open.remove(&key) {
                if !alert.silenced {
                    transitions.push((NotificationReason::Resolved, alert.clone()));
                }
                batch.resolved.push(alert);
            }
        }

        // New findings open an alert; repeats update it without notifying again.
        for (key, finding) in &active {
            if let Some(alert) = self.open.get_mut(key) {
                let was_cleared = alert.clear_since.take().is_some();
                let became_red = alert.health != Health::Red && finding.health == Health::Red;
                alert.health = finding.health;
                alert.sentence.clone_from(&finding.sentence);
                alert.action.clone_from(&finding.action);
                alert.updated_at = now;
                let silenced_now = covers_any(&self.silences, key, now);
                let was_silenced = alert.silenced;
                alert.silenced = silenced_now;
                if was_silenced && !silenced_now {
                    transitions.push((NotificationReason::Reopened, alert.clone()));
                } else if became_red && !silenced_now && !was_cleared {
                    transitions.push((NotificationReason::Escalated, alert.clone()));
                }
            } else {
                let silenced = covers_any(&self.silences, key, now);
                let alert = Alert {
                    key: key.clone(),
                    health: finding.health,
                    sentence: finding.sentence.clone(),
                    action: finding.action.clone(),
                    opened_at: now,
                    updated_at: now,
                    since: finding.since,
                    silenced,
                    escalated_steps: 0,
                    clear_since: None,
                };
                if silenced {
                    batch.suppressed.push(key.clone());
                } else {
                    transitions.push((NotificationReason::Opened, alert.clone()));
                }
                self.open.insert(key.clone(), alert);
            }
        }

        // Escalation steps are data; each fires once for a red alert that stays open.
        let steps = self.settings.escalation.clone();
        for alert in self.open.values_mut() {
            if alert.health != Health::Red || alert.silenced {
                continue;
            }
            while let Some(step) = steps.get(alert.escalated_steps) {
                if alert.opened_at.age(now) < step.after_seconds {
                    break;
                }
                alert.escalated_steps += 1;
                let mut escalated = alert.clone();
                escalated.sentence = format!("{} {}", escalated.sentence, step.note);
                transitions.push((NotificationReason::Escalated, escalated));
            }
        }

        batch.opened = transitions
            .iter()
            .filter(|(reason, _)| *reason == NotificationReason::Opened)
            .map(|(_, alert)| alert.clone())
            .collect();
        batch.escalated = transitions
            .iter()
            .filter(|(reason, _)| *reason == NotificationReason::Escalated)
            .map(|(_, alert)| alert.clone())
            .collect();
        batch.notifications = group_notifications(transitions);

        for notification in &batch.notifications {
            for sink in &self.sinks {
                if let Err(SinkError { message }) = sink.deliver(notification) {
                    batch.sink_failures.push(SinkFailure {
                        sink: sink.name().to_owned(),
                        message,
                    });
                }
            }
        }
        batch
    }
}

fn covers_any(silences: &[Silence], key: &AlertKey, now: Timestamp) -> bool {
    silences.iter().any(|silence| silence.covers(key, now))
}

impl Default for AlertManager {
    fn default() -> Self {
        Self {
            settings: AlertSettings::defaults(),
            open: BTreeMap::new(),
            silences: Vec::new(),
            next_silence_id: 1,
            sinks: Vec::new(),
        }
    }
}

fn active_findings(view: &FleetHealth) -> BTreeMap<AlertKey, Finding> {
    let mut active: BTreeMap<AlertKey, Finding> = BTreeMap::new();
    for finding in view.findings() {
        let key = AlertKey::of(finding);
        active
            .entry(key)
            .and_modify(|current| {
                if finding.health > current.health {
                    current.clone_from(finding);
                }
            })
            .or_insert_with(|| finding.clone());
    }
    active
}

fn group_notifications(transitions: Vec<(NotificationReason, Alert)>) -> Vec<AlertNotification> {
    let mut groups: BTreeMap<Option<TenantId>, Vec<(NotificationReason, Alert)>> = BTreeMap::new();
    for (reason, alert) in transitions {
        groups
            .entry(alert.key.subject.tenant().cloned())
            .or_default()
            .push((reason, alert));
    }
    groups
        .into_iter()
        .map(|(tenant, items)| {
            let reasons: BTreeSet<NotificationReason> =
                items.iter().map(|(reason, _)| *reason).collect();
            let reason = if reasons.len() == 1 {
                reasons
                    .into_iter()
                    .next()
                    .unwrap_or(NotificationReason::Mixed)
            } else {
                NotificationReason::Mixed
            };
            let mut summaries: BTreeMap<AlertKey, AlertSummary> = BTreeMap::new();
            for (_, alert) in &items {
                summaries.insert(alert.key.clone(), AlertSummary::from(alert));
            }
            let mut alerts: Vec<AlertSummary> = summaries.into_values().collect();
            alerts.sort_by(|a, b| {
                b.health
                    .cmp(&a.health)
                    .then_with(|| a.subject.cmp(&b.subject))
                    .then_with(|| a.rule_id.cmp(&b.rule_id))
            });
            AlertNotification {
                tenant,
                reason,
                alerts,
            }
        })
        .collect()
}
