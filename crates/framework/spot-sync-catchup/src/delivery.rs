//! Which audit events a catch-up delivers (ADR-066 section 2; WP-SYN-07.T5 and T6).
//!
//! The two questions the research note raised are two explicit options, so the architecture owner's answer is a
//! value, not a rewrite. The defaults are the note's recommendation.

use crate::audit::ScopeOracle;
use crate::event::Event;
use serde::{Deserialize, Serialize};

/// What happens to an audit event nobody addressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Unaddressed {
    /// Delivered when its subject is in scope or it is covered by reference (the behaviour before
    /// WP-SYN-07.T6).
    Deliver,
    /// Never delivered: ADR-066 section 2, "never as a broadcast". **The default.**
    Drop,
}

/// What happens to an audit event addressed to this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Addressed {
    /// Delivered only when in scope or covered by reference (the behaviour before
    /// WP-SYN-07.T6).
    ScopeChecked,
    /// Delivered whatever its subject: the hub chose the address. A `sys_sync_missing` event
    /// names records the node does not hold yet. **The default.**
    Always,
}

/// The audit-delivery option pair of [`crate::plan_with_audit_options`].
///
/// The default is the recommendation of `docs/research/sync-audit-event-delivery.md`: drop the
/// unaddressed, deliver the node-addressed regardless of scope. The pre-T6 behaviour is
/// [`AuditDelivery::LEGACY`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AuditDelivery {
    /// What happens to an unaddressed audit event.
    pub unaddressed: Unaddressed,
    /// What happens to an audit event addressed to this node.
    pub addressed: Addressed,
}

impl AuditDelivery {
    /// The behaviour of `plan_with_audit` before WP-SYN-07.T6: deliver an audit event iff in
    /// scope and not addressed to another node.
    pub const LEGACY: Self = Self {
        unaddressed: Unaddressed::Deliver,
        addressed: Addressed::ScopeChecked,
    };

    /// Whether `event` is delivered to `node`.
    ///
    /// | addressed to | `in_scope` (subject or by reference) | `Unaddressed::Deliver` | `Unaddressed::Drop` |
    /// | --- | --- | --- | --- |
    /// | nobody | yes | delivered | dropped |
    /// | nobody | no | dropped | dropped |
    /// | another node | any | dropped | dropped |
    ///
    /// | addressed to | `in_scope` | `Addressed::ScopeChecked` | `Addressed::Always` |
    /// | --- | --- | --- | --- |
    /// | this node | yes | delivered | delivered |
    /// | this node | no | dropped | delivered |
    ///
    /// The `mandatory` flag does not change the answer; a delivered mandatory event joins the
    /// mandatory lane.
    #[must_use]
    pub fn keeps(&self, event: &Event, node: &str, oracle: &impl ScopeOracle) -> bool {
        let in_scope = || {
            event
                .subject
                .as_ref()
                .is_some_and(|record| oracle.in_scope(record))
                || oracle.covers_by_reference(event)
        };
        match event.addressed_to.as_deref() {
            Some(to) if to != node => false,
            Some(_) => self.addressed == Addressed::Always || in_scope(),
            None => self.unaddressed == Unaddressed::Deliver && in_scope(),
        }
    }
}

impl Default for AuditDelivery {
    fn default() -> Self {
        Self {
            unaddressed: Unaddressed::Drop,
            addressed: Addressed::Always,
        }
    }
}
