//! `spot-sync-conditions`: the conditions catalogue for the journal and sync (WP-SYN-09.T3,
//! ADR-067 section 2).
//!
//! `spot-conditions` is the framework; this crate is the data: one [`CatalogueEntry`] per
//! condition of the ADR table, so a detector in another crate raises a name from here, never a
//! string it invented. A pure library: no clock, no I/O, no randomness, no prose (every text is
//! a message key, listed by [`message_keys`] for the interface and translation tasks).
//!
//! * [`catalogue`] builds the [`Registry`] the engine loads.
//! * [`entries`] gives the full entries: the definition plus what the framework's definition
//!   does not carry (detector and clear-check keys, the debounce, the reason for a severity
//!   above `info`).
//! * [`validate_catalogue`] checks the catalogue's own rules (calm by default, escalation
//!   strictly increasing and upwards, a response for every red condition, well-formed keys).
//! * [`conditions_markdown`] renders the table for `docs/`.
//!
//! # Parameters
//!
//! Parameters are names only; values are always text the raiser formats from typed values (ids,
//! counts, durations), never free text, so a raiser cannot inject content into an alert and a
//! subject carries ids, never tenant data.
//! `JOURNAL_AUDIT_MISMATCH` has the subject kind `tenant` (the audit job rebuilds one tenant's
//! database), with the parameters `first_differing_record` and `difference`.
//!
//! # Magnitude bands
//!
//! A definition has one severity and an escalation schedule by *elapsed open time*. ADR-067's
//! `CLOCK_SKEW` (red over 5 minutes) and `CERT_EXPIRING` (red at 3 days) escalate by a measured
//! value, not by time open, so their definitions are amber and the detector owns the red band.
//! `OBLIGATION_RED` and `OBLIGATION_DUE_SOON` are the ladder rungs the acknowledgement ledger
//! raises beside `OBLIGATION_OVERDUE` (ADR-065); they have no ADR row of their own.

mod error;
mod markdown;
mod spec;
mod table;

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use spot_conditions::{
    ConditionDef, ConditionsError, Escalation, Millis, Registry, ResponseStep, Severity,
};

pub use error::SyncConditionsError;
pub use markdown::conditions_markdown;

/// One catalogued condition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogueEntry {
    /// The framework definition: id, severity, subject kind, explanation key, parameters,
    /// response steps, escalation and operator commands.
    pub def: ConditionDef,
    /// The message key describing the check that raises it.
    pub detector_key: String,
    /// The message key describing the check that clears it.
    pub clear_key: String,
    /// The message key of the short title.
    pub title_key: String,
    /// The debounce for this condition: a clear that returns inside it is one condition.
    pub debounce: Millis,
    /// Why this condition is above `info` (empty for `info`). Developer documentation, not
    /// shown to users.
    pub reason: String,
    /// The name of the remedy to show first on the `needs you` list: one of this entry's
    /// automatic response actions or one of its operator commands. `None` means the default
    /// order: the first operator command, else the last automatic step.
    #[serde(default)]
    pub primary_remedy: Option<String>,
}

impl CatalogueEntry {
    /// The message key of the remedy to show first for this entry: the primary remedy when set
    /// (`sync.remedy.<action>` for a response action, `sync.operator.<command>` for an operator
    /// command), else the first operator command, else the last automatic step; `None` when the
    /// entry names no action at all.
    #[must_use]
    pub fn remedy_message_key(&self) -> Option<String> {
        if let Some(name) = &self.primary_remedy {
            return Some(self.key_of_remedy(name));
        }
        if let Some(command) = self.def.actions.first() {
            return Some(operator_key(command));
        }
        self.def
            .response
            .last()
            .map(|step| remedy_key(&step.action))
    }

    fn key_of_remedy(&self, name: &str) -> String {
        if self.def.response.iter().any(|step| step.action == name) {
            remedy_key(name)
        } else {
            operator_key(name)
        }
    }
}

/// The message key of `part` (`title`, `explanation`, `detector`, `clear`) of condition `id`.
fn key(id: &str, part: &str) -> String {
    format!("sync.condition.{}.{part}", id.to_ascii_lowercase())
}

/// The message key of an automatic response action.
fn remedy_key(action: &str) -> String {
    format!("sync.remedy.{action}")
}

/// The message key of an operator command.
fn operator_key(command: &str) -> String {
    format!("sync.operator.{command}")
}

fn build(spec: &spec::Spec) -> CatalogueEntry {
    let mut def = ConditionDef::new(
        spec.id,
        spec.severity,
        spec.subject,
        &key(spec.id, "explanation"),
    );
    for name in spec.params {
        def = def.with_param(name);
    }
    for (action, limit) in spec.response {
        def = def.with_response(ResponseStep::new(action, *limit));
    }
    for (after, to) in spec.escalation {
        def = def.with_escalation(Escalation::new(Millis::new(*after), *to));
    }
    for command in spec.actions {
        def = def.with_action(command);
    }
    CatalogueEntry {
        def,
        detector_key: key(spec.id, "detector"),
        clear_key: key(spec.id, "clear"),
        title_key: key(spec.id, "title"),
        debounce: Millis::new(spec.debounce_ms),
        reason: spec.reason.to_owned(),
        primary_remedy: spec.primary_remedy.map(str::to_owned),
    }
}

/// Every catalogued condition, in the ADR's table order.
#[must_use]
pub fn entries() -> Vec<CatalogueEntry> {
    table::TABLE.iter().map(build).collect()
}

/// The entry for `id`, if catalogued.
#[must_use]
pub fn entry(id: &str) -> Option<CatalogueEntry> {
    table::TABLE.iter().find(|spec| spec.id == id).map(build)
}

/// The registry of every catalogued condition, ready for a `ConditionEngine`.
///
/// # Errors
///
/// A [`ConditionsError`] only if the table itself is malformed (a duplicate or badly spelled
/// id); the tests keep that from ever shipping.
pub fn catalogue() -> Result<Registry, ConditionsError> {
    let mut registry = Registry::new();
    for entry in entries() {
        registry.register(entry.def)?;
    }
    Ok(registry)
}

/// Every message key the catalogue uses, sorted and without duplicates: titles, explanations,
/// detector and clear checks, one key per automatic response action and one per operator
/// command.
#[must_use]
pub fn message_keys() -> Vec<String> {
    let mut keys = BTreeSet::new();
    for entry in entries() {
        let primary = entry
            .primary_remedy
            .as_deref()
            .map(|name| entry.key_of_remedy(name));
        keys.extend(primary);
        keys.insert(entry.title_key);
        keys.insert(entry.def.explanation_key.clone());
        keys.insert(entry.detector_key);
        keys.insert(entry.clear_key);
        for step in &entry.def.response {
            keys.insert(remedy_key(&step.action));
        }
        for command in &entry.def.actions {
            keys.insert(operator_key(command));
        }
    }
    keys.into_iter().collect()
}

fn is_snake(text: &str) -> bool {
    !text.is_empty()
        && text.starts_with(|c: char| c.is_ascii_lowercase())
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_message_key(text: &str) -> bool {
    !text.is_empty() && text.split('.').all(is_snake) && text.starts_with("sync.")
}

fn rule(entry: &CatalogueEntry) -> Option<&'static str> {
    let def = &entry.def;
    if def.severity > Severity::Info && entry.reason.trim().is_empty() {
        return Some("reason_missing");
    }
    let mut previous: Option<Millis> = None;
    let mut level = def.severity;
    for step in &def.escalation {
        if previous.is_some_and(|before| step.after_millis <= before) {
            return Some("escalation_order");
        }
        if step.after_millis == Millis::ZERO || step.to <= level {
            return Some("escalation_not_upwards");
        }
        previous = Some(step.after_millis);
        level = step.to;
    }
    let reaches_red = def.severity == Severity::Red || level == Severity::Red;
    if reaches_red && def.response.is_empty() {
        return Some("red_without_response");
    }
    if def.response.iter().any(|step| step.limit == 0) {
        return Some("response_limit_zero");
    }
    if entry.debounce == Millis::ZERO {
        return Some("debounce_zero");
    }
    let keys_ok = [
        &def.explanation_key,
        &entry.detector_key,
        &entry.clear_key,
        &entry.title_key,
    ]
    .into_iter()
    .all(|text| is_message_key(text));
    if !keys_ok {
        return Some("message_key");
    }
    let names_ok = def.params.iter().all(|name| is_snake(name))
        && def.response.iter().all(|step| is_snake(&step.action))
        && def.actions.iter().all(|command| is_snake(command));
    if !names_ok {
        return Some("name_form");
    }
    if let Some(name) = entry.primary_remedy.as_deref() {
        let known = def.response.iter().any(|step| step.action == name)
            || def.actions.iter().any(|command| command == name);
        if !known {
            return Some("primary_remedy_unknown");
        }
    }
    if !matches!(
        def.subject_kind.as_str(),
        "tenant" | "node" | "source" | "record"
    ) {
        return Some("subject_kind");
    }
    None
}

/// Check the catalogue's own rules over every entry.
///
/// * a severity above `info` has a documented reason (calm by default, ADR-067 section 6);
/// * escalation thresholds are positive, strictly increasing, and each step goes up;
/// * a condition that is or becomes red has at least one automatic response step;
/// * response limits and the debounce are positive;
/// * message keys are dotted lower case under `sync.`, parameter and action names are snake
///   case, a primary remedy names one of the entry's response actions or operator commands, and the subject kind is `tenant`, `node`, `source` or `record`.
///
/// # Errors
///
/// [`SyncConditionsError::InvalidEntry`] for the first entry that breaks a rule.
pub fn validate_catalogue() -> Result<(), SyncConditionsError> {
    for entry in entries() {
        if let Some(rule) = rule(&entry) {
            return Err(SyncConditionsError::InvalidEntry {
                id: entry.def.id,
                rule,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn sample() -> CatalogueEntry {
        entry("NODE_SILENT").expect("catalogued")
    }

    #[test]
    fn each_rule_refuses_its_violation() {
        let mut e = sample();
        e.reason.clear();
        assert_eq!(rule(&e), Some("reason_missing"));

        let mut e = sample();
        e.def
            .escalation
            .push(Escalation::new(Millis::new(1), Severity::Red));
        assert_eq!(rule(&e), Some("escalation_order"));

        let mut e = sample();
        e.def.escalation = vec![Escalation::new(Millis::new(5), Severity::Info)];
        assert_eq!(rule(&e), Some("escalation_not_upwards"));

        let mut e = entry("NODE_TOO_OLD").expect("catalogued");
        e.def.response.clear();
        assert_eq!(rule(&e), Some("red_without_response"));

        let mut e = sample();
        e.def.explanation_key = "Not A Key".to_owned();
        assert_eq!(rule(&e), Some("message_key"));

        let mut e = sample();
        e.debounce = Millis::ZERO;
        assert_eq!(rule(&e), Some("debounce_zero"));

        let mut e = sample();
        e.def.subject_kind = "branch".to_owned();
        assert_eq!(rule(&e), Some("subject_kind"));

        let mut e = entry("NODE_TOO_OLD").expect("catalogued");
        e.primary_remedy = Some("no_such_remedy".to_owned());
        assert_eq!(rule(&e), Some("primary_remedy_unknown"));

        assert_eq!(rule(&sample()), None);
    }

    #[test]
    fn node_too_old_offers_the_upgrade_first_and_keeps_suspend_reachable() {
        let e = entry("NODE_TOO_OLD").expect("catalogued");
        assert_eq!(e.primary_remedy.as_deref(), Some("offer_upgrade"));
        assert_eq!(
            e.remedy_message_key().as_deref(),
            Some("sync.remedy.offer_upgrade")
        );
        assert!(e.def.actions.iter().any(|c| c == "suspend_node"));
        assert!(message_keys().contains(&"sync.remedy.offer_upgrade".to_owned()));
        assert!(message_keys().contains(&"sync.operator.suspend_node".to_owned()));
    }

    #[test]
    fn a_primary_remedy_that_is_an_operator_command_keys_to_the_operator_message() {
        let mut e = sample();
        // `resume_node` is only an operator command; `notify_contact` is also a response step,
        // which the key then resolves to `sync.remedy.*`.
        let command = e.def.actions.last().cloned().expect("has a command");
        assert_eq!(command, "resume_node");
        e.primary_remedy = Some(command.clone());
        assert_eq!(rule(&e), None);
        assert_eq!(
            e.remedy_message_key(),
            Some(format!("sync.operator.{command}"))
        );
    }

    #[test]
    fn only_node_too_old_sets_a_primary_remedy() {
        for e in entries() {
            assert_eq!(
                e.primary_remedy.is_some(),
                e.def.id == "NODE_TOO_OLD",
                "{}",
                e.def.id
            );
            if let Some(name) = &e.primary_remedy {
                assert!(message_keys().contains(&e.key_of_remedy(name)));
            }
        }
    }

    #[test]
    fn the_error_has_a_stable_code() {
        let err = SyncConditionsError::InvalidEntry {
            id: "X".to_owned(),
            rule: "reason_missing",
        };
        assert_eq!(err.code(), "sync_conditions.invalid_entry");
    }
}
