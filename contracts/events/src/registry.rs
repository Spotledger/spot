//! The event class registry (ADR-061 section 2, ADR-056; decisions B1 to B9 of 31 Oct 2026).
//!
//! Every platform topic has exactly one [`Class`] and one [`Retention`], recorded here and
//! nowhere else. Before 0.7.1 the class was guessed from the topic's prefix by the rebuild
//! executor, and two `is_audit` functions plus `system::retention_of` held three partial answers.
//!
//! # Rules
//!
//! 1. **A class never changes after release** and **an entry is never removed**: a topic that is
//!    no longer emitted is marked `retired`, so an old journal still classifies. A changed class
//!    is a new topic name. A unit test compares [`REGISTRY`] with the committed
//!    `registry.snapshot.json` and fails on any class change or removal.
//! 2. **A platform topic (`doc_*`, `sys_*`, `identity_*`) that has no entry is refused** by
//!    [`check_publishable`]. An **app topic** that has no entry is allowed with
//!    [`Publishable::Warn`] for one minor version after 0.7.1; the app manifest declares its
//!    class (ADR-056) and a later version refuses it.
//! 3. A rebuild **applies** State and Audit events and makes Domain events available to a
//!    consumer without applying them (ADR-061 amendment A.0.1): replaying a Domain event would run
//!    the consumers' reactions twice.
//! 4. Each entry names the crate that **owns** the topic (defines or emits it). The
//!    `event-topics` lint (`tools/lints/event_topics.py`) fails when a platform topic constant is
//!    not here or when the owner crate does not contain the topic.
//!
//! The publish path adopts [`check_publishable`] in a follow-up task; nothing calls it yet.

use std::fmt;

use serde::Serialize;

use crate::names::Topic;

/// The class of an event (ADR-061 section 2). Telemetry never reaches the outbox, so it has no
/// variant: a telemetry signal is not an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// The full state of one record after one committed transaction: applied by a rebuild.
    State,
    /// A business or app fact for consumers to react to: in the journal and available to a
    /// consumer, **not applied** by a rebuild (it would run the reactions twice).
    Domain,
    /// A system fact that matters for proof or rebuild: applied by a rebuild.
    Audit,
}

impl Class {
    /// Whether a rebuild applies events of this class.
    pub const fn applied_by_rebuild(self) -> bool {
        matches!(self, Class::State | Class::Audit)
    }
}

/// How long a topic's events are kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    /// The audit retention period (years), not the outbox retention.
    Audit,
    /// The ordinary outbox retention: the event is a projection input or a notification.
    Outbox,
}

/// Whether a Domain topic collapses to the latest event per subject when a lagging replica
/// catches up (ADR-066 section 2). No platform topic declares coalescing today.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Coalescing {
    /// Every event is delivered.
    KeepAll,
    /// Only the latest event per subject is delivered. Valid for Domain topics only.
    LatestPerSubject,
}

/// One registered topic. The topic is the identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// The topic name.
    pub topic: &'static str,
    /// The class; never changes after release.
    pub class: Class,
    /// The retention.
    pub retention: Retention,
    /// The coalescing declaration.
    pub coalescing: Coalescing,
    /// The crate that defines or emits the topic.
    pub owner: &'static str,
    /// The contract version the entry first appeared in; never rewritten.
    pub since: &'static str,
    /// True when the topic is no longer emitted. The entry stays so old journals classify.
    pub retired: bool,
}

const fn entry(
    topic: &'static str,
    class: Class,
    retention: Retention,
    coalescing: Coalescing,
    owner: &'static str,
    since: &'static str,
) -> Entry {
    Entry {
        topic,
        class,
        retention,
        coalescing,
        owner,
        since,
        retired: false,
    }
}

const fn entry_retired(
    topic: &'static str,
    class: Class,
    retention: Retention,
    coalescing: Coalescing,
    owner: &'static str,
    since: &'static str,
) -> Entry {
    Entry {
        retired: true,
        ..entry(topic, class, retention, coalescing, owner, since)
    }
}

/// How many topics the registry holds.
pub const REGISTRY_LEN: usize = 61;

/// Every registered platform topic. One line per entry: the `event-topics` lint reads this file.
#[rustfmt::skip]
pub const REGISTRY: [Entry; REGISTRY_LEN] = [
    entry("doc_created", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-events-contract", "0.1.0"),
    entry("doc_updated", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-events-contract", "0.1.0"),
    entry("doc_posted", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-events-contract", "0.1.0"),
    entry("doc_cancelled", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-events-contract", "0.1.0"),
    entry("doc_deleted", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-events-contract", "0.1.0"),
    entry("doc_renamed", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-events-contract", "0.1.0"),
    entry("doc_updated_posted", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-events-contract", "0.1.0"),
    entry("doc_state", Class::State, Retention::Outbox, Coalescing::KeepAll, "spot-journal-rebuild", "0.7.1"),
    entry_retired("doc_edge", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-graph-projection", "0.7.1"),
    entry("identity_user_created", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_user_enabled", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_user_disabled", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_user_anonymised", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_sign_in_succeeded", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_sign_in_failed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_account_locked", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_account_unlocked", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_recovery_requested", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_recovery_completed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_session_ended", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_api_key_created", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_api_key_revoked", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_role_changed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_duty_changed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_assignment_created", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_assignment_changed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_assignment_ended", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_sso_policy_changed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_sod_override", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_emergency_started", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_emergency_ended", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_emergency_reviewed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_impersonation_started", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_impersonation_ended", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_field_accessed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_node_changed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_snapshot_published", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.2.0"),
    entry("identity_session_started", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.3.0"),
    entry("identity_password_changed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.3.0"),
    entry("identity_second_factor_enrolled", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.3.0"),
    entry("identity_second_factor_removed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.3.0"),
    entry("identity_recovery_codes_regenerated", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.3.0"),
    entry("identity_sign_in_failed_burst", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.3.0"),
    entry("identity_access_review_completed", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.3.0"),
    entry("sys_doc_audit", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-doc-audit", "0.4.0"),
    entry("sys_graph_edge", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-events-contract", "0.4.0"),
    entry("sys_secret_audit", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-document", "0.5.0"),
    entry("sys_job_enqueued", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-document", "0.7.1"),
    entry("sys_user_notice", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-document", "0.7.1"),
    entry("sys_operation_receipt", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-document", "0.7.1"),
    entry("sys_operation_receipt_effect", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-document", "0.7.1"),
    entry("sys_partition_range_low", Class::Domain, Retention::Outbox, Coalescing::KeepAll, "spot-partition", "0.7.1"),
    entry("sys_bpm_governance", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-bpm-governance", "0.7.1"),
    entry("sys_bpm_case_migrated", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-bpm-versioning", "0.7.1"),
    entry("sys_condition_raised", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-conditions", "0.7.1"),
    entry("sys_condition_cleared", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-conditions", "0.7.1"),
    entry("sys_journal_access", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.7.6"),
    entry("sys_journal_grant", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.7.6"),
    entry("sys_journal_admin", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.7.6"),
    entry("sys_sync_node_admin", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.7.6"),
    entry("sys_sync_decision", Class::Audit, Retention::Audit, Coalescing::KeepAll, "spot-events-contract", "0.7.6"),
];

/// The entry of `topic`, or `None` when it is not registered.
pub fn lookup(topic: &str) -> Option<&'static Entry> {
    REGISTRY.iter().find(|e| e.topic == topic)
}

/// The class of `topic`, or `None` when it is not registered.
pub fn class_of(topic: &str) -> Option<Class> {
    lookup(topic).map(|e| e.class)
}

/// The retention of `topic`, or `None` when it is not registered.
pub fn retention_of(topic: &str) -> Option<Retention> {
    lookup(topic).map(|e| e.retention)
}

/// Whether `topic` is registered as an audit record.
pub fn is_audit(topic: &str) -> bool {
    class_of(topic) == Some(Class::Audit)
}

/// What [`check_publishable`] decided for a topic that may be published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Publishable {
    /// The topic is registered.
    Registered(Class),
    /// An app topic with no registry entry: allowed for one minor version after 0.7.1, with a
    /// warning. A later version refuses it.
    Warn,
}

/// The topic is a platform topic (or not a valid topic at all) with no registry entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnregisteredTopic {
    /// The topic.
    pub topic: String,
}

impl fmt::Display for UnregisteredTopic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the topic `{}` has no registered event class, so it cannot be published",
            self.topic
        )
    }
}

impl std::error::Error for UnregisteredTopic {}

/// Whether `topic` may be published. A registered topic is allowed (a retired one too: it is
/// refused by its producer, not by the class table). An unregistered platform topic, or a name
/// that is not a valid topic, is refused. An unregistered app topic is allowed with a warning.
pub fn check_publishable(topic: &str) -> Result<Publishable, UnregisteredTopic> {
    if let Some(entry) = lookup(topic) {
        return Ok(Publishable::Registered(entry.class));
    }
    match Topic::any(topic) {
        Ok(t) if !t.is_platform() => Ok(Publishable::Warn),
        _ => Err(UnregisteredTopic {
            topic: topic.to_owned(),
        }),
    }
}
