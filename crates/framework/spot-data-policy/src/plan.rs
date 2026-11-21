//! The planner: what a retention run would do, and why.
//!
//! [`plan_run`] is a pure read: it never changes a record, a hold, a proof or a checkpoint. It
//! answers, for one tenant's store and one instant:
//!
//! - which records have reached their due date under their DocType's rule;
//! - which are suspended by a legal hold, a statutory floor, a rule exception or a retained
//!   referrer, and the exact reason;
//! - how many items each rule would action, by action;
//! - what the plan's identity digest is, so an execution can prove it is running the plan it was
//!   resumed from (ADR-071 section 2).
//!
//! A dry run is the default on the execution side; the plan is what a person approves first.
//! Nothing here reads the wall clock: `now` is an argument.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest as _, Sha256};

use crate::calendar::{Calendar, CivilDate, Period};
use crate::error::DataPolicyError;
use crate::hold::Holds;
use crate::proof::{put_field, to_hex};
use crate::record::{FieldClassMap, Record};
use crate::rule::{Action, RetentionRule, RetentionRules};
use crate::statutory::{FloorLookup, StatutoryFloor};
use crate::store::{RetentionStore, SkipReason};

/// The policies one plan (and one execution) runs under.
#[derive(Clone, Copy)]
pub struct PolicySet<'a> {
    /// The tenant's retention rules.
    pub rules: &'a RetentionRules,
    /// The tenant's legal holds.
    pub holds: &'a Holds,
    /// The statutory floor table (empty by default).
    pub floors: &'a StatutoryFloor,
    /// The DocType field classifications anonymisation uses.
    pub field_classes: &'a FieldClassMap,
    /// The injected calendar.
    pub calendar: &'a dyn Calendar,
}

impl<'a> PolicySet<'a> {
    /// Groups the policies.
    #[must_use]
    pub fn new(
        rules: &'a RetentionRules,
        holds: &'a Holds,
        floors: &'a StatutoryFloor,
        field_classes: &'a FieldClassMap,
        calendar: &'a dyn Calendar,
    ) -> Self {
        Self {
            rules,
            holds,
            floors,
            field_classes,
            calendar,
        }
    }
}

/// One item the planner would action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedAction {
    /// The record.
    pub record_id: String,
    /// The record's DocType.
    pub doctype: String,
    /// The rule that produced it.
    pub rule_id: String,
    /// What would happen.
    pub action: Action,
    /// The rule's trigger instant, in Unix milliseconds.
    pub trigger_at: u64,
    /// The effective due instant, after any statutory floor, in Unix milliseconds.
    pub due_at: u64,
    /// Why it is due, in plain words (`due since 2036-10-19`).
    pub reason: String,
    /// For [`Action::Anonymise`], the fields the anonymise operation replaces.
    pub anonymise_fields: Vec<String>,
}

/// One record the planner left alone, with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedSkip {
    /// The record.
    pub record_id: String,
    /// The record's DocType.
    pub doctype: String,
    /// The rule that considered it.
    pub rule_id: String,
    /// The action the rule would have taken.
    pub action: Action,
    /// Why the record was left alone.
    pub reason: SkipReason,
}

/// How many records one rule would action, by action, and how many it skipped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuleCount {
    /// Items the rule would action.
    pub planned: u64,
    /// Records the rule considered and left alone.
    pub skipped: u64,
    /// Planned deletions.
    pub delete: u64,
    /// Planned anonymisations.
    pub anonymise: u64,
    /// Planned archives.
    pub archive: u64,
}

/// What a retention run would do, with its identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionPlan {
    now: u64,
    calendar: String,
    items: Vec<PlannedAction>,
    skips: Vec<PlannedSkip>,
    counts: BTreeMap<String, RuleCount>,
    identity: [u8; 32],
}

impl RetentionPlan {
    /// The instant the plan was made.
    #[must_use]
    pub fn now(&self) -> u64 {
        self.now
    }

    /// The calendar the plan used.
    #[must_use]
    pub fn calendar(&self) -> &str {
        &self.calendar
    }

    /// The items to action, in stable `(rule, record)` order.
    #[must_use]
    pub fn items(&self) -> &[PlannedAction] {
        &self.items
    }

    /// The records left alone, in stable `(rule, record)` order.
    #[must_use]
    pub fn skips(&self) -> &[PlannedSkip] {
        &self.skips
    }

    /// The per-rule counts, including rules that had no matching records.
    #[must_use]
    pub fn counts(&self) -> &BTreeMap<String, RuleCount> {
        &self.counts
    }

    /// The count for one rule.
    #[must_use]
    pub fn count(&self, rule_id: &str) -> Option<RuleCount> {
        self.counts.get(rule_id).copied()
    }

    /// The identity digest: the policy, the instant and the planned items.
    #[must_use]
    pub fn identity(&self) -> [u8; 32] {
        self.identity
    }

    /// The identity digest in lower-case hexadecimal.
    #[must_use]
    pub fn identity_hex(&self) -> String {
        to_hex(&self.identity)
    }

    /// How many items the plan would action.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.items.len()
    }

    /// Whether the plan would action nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Plans a run over `store` at `now`.
///
/// A record is actioned when its rule matches, its trigger is present, its calendar-aware period
/// has ended, no legal hold covers it, no exception names it, a deletion is allowed by the
/// statutory floor, and no retained record still links to it. Everything else is a
/// [`PlannedSkip`] with its reason.
///
/// # Errors
/// [`DataPolicyError::StoreFailed`] when the store cannot list its records. A plan is never built
/// from a failed read: "the database is down" is not "there is nothing to retain".
pub fn plan_run(
    policy: &PolicySet<'_>,
    store: &dyn RetentionStore,
    now: u64,
) -> Result<RetentionPlan, DataPolicyError> {
    let mut items: Vec<PlannedAction> = Vec::new();
    let mut skips: Vec<PlannedSkip> = Vec::new();
    let records = store.records()?;

    for record in &records {
        let Some(rule) = policy.rules.for_doctype(&record.doctype) else {
            continue;
        };
        if rule.exceptions.applies(record) {
            skips.push(skip(record, rule, SkipReason::Exception));
            continue;
        }
        let Some(trigger_at) = record.trigger_at(&rule.trigger) else {
            skips.push(skip(record, rule, SkipReason::TriggerMissing));
            continue;
        };
        let trigger_date = match CivilDate::from_unix_millis(trigger_at) {
            Ok(date) => date,
            Err(error) => {
                skips.push(skip(
                    record,
                    rule,
                    SkipReason::CalendarRefused {
                        detail: error.to_string(),
                    },
                ));
                continue;
            }
        };
        let rule_due = match policy.calendar.add(trigger_date, rule.period) {
            Ok(date) => date,
            Err(error) => {
                skips.push(skip(
                    record,
                    rule,
                    SkipReason::CalendarRefused {
                        detail: error.to_string(),
                    },
                ));
                continue;
            }
        };
        let rule_due_ms = match rule_due.to_unix_millis() {
            Ok(millis) => millis,
            Err(error) => {
                skips.push(skip(
                    record,
                    rule,
                    SkipReason::CalendarRefused {
                        detail: error.to_string(),
                    },
                ));
                continue;
            }
        };
        if rule_due_ms > now {
            skips.push(skip(record, rule, SkipReason::NotDue));
            continue;
        }
        if let Some(hold) = policy.holds.covering(record) {
            skips.push(skip(
                record,
                rule,
                SkipReason::LegalHold {
                    hold: hold.id.clone(),
                    reason: hold.reason.clone(),
                },
            ));
            continue;
        }

        let mut due_at = rule_due_ms;
        let mut reason = format!("due since {rule_due}");
        if rule.action == Action::Delete {
            match policy.floors.lookup(record, trigger_date, policy.calendar) {
                Ok(FloorLookup::NotSubject) => {}
                Ok(FloorLookup::UnknownJurisdiction) => {
                    skips.push(skip(record, rule, SkipReason::StatutoryFloorUnknown));
                    continue;
                }
                Ok(FloorLookup::Earliest(earliest)) => {
                    if earliest > now {
                        skips.push(skip(
                            record,
                            rule,
                            SkipReason::StatutoryFloorPending { earliest },
                        ));
                        continue;
                    }
                    if earliest > due_at {
                        due_at = earliest;
                        reason = format!(
                            "due since {rule_due}; the statutory floor moved it to {}",
                            date_text(earliest)
                        );
                    }
                }
                Err(error) => {
                    skips.push(skip(
                        record,
                        rule,
                        SkipReason::CalendarRefused {
                            detail: error.to_string(),
                        },
                    ));
                    continue;
                }
            }
        }

        let anonymise_fields = if rule.action == Action::Anonymise {
            if !policy.field_classes.knows(&record.doctype) {
                skips.push(skip(record, rule, SkipReason::Unclassified));
                continue;
            }
            policy.field_classes.anonymise_fields(&record.doctype)
        } else {
            Vec::new()
        };

        items.push(PlannedAction {
            record_id: record.id.clone(),
            doctype: record.doctype.clone(),
            rule_id: rule.id.clone(),
            action: rule.action,
            trigger_at,
            due_at,
            reason,
            anonymise_fields,
        });
    }

    // Referential pass: a record a retained record still links to is not deleted. Another record
    // that is itself being deleted in this plan is not retained.
    let delete_ids: BTreeSet<String> = items
        .iter()
        .filter(|item| item.action == Action::Delete)
        .map(|item| item.record_id.clone())
        .collect();
    let mut blocked = Vec::new();
    for (index, item) in items.iter().enumerate() {
        if item.action != Action::Delete {
            continue;
        }
        let mut referrers: Vec<String> = records
            .iter()
            .filter(|other| other.id != item.record_id)
            .filter(|other| !delete_ids.contains(&other.id))
            .filter(|other| other.references_record(&item.record_id))
            .map(|other| other.id.clone())
            .collect();
        if referrers.is_empty() {
            continue;
        }
        referrers.sort();
        let Some(rule) = policy.rules.get(&item.rule_id) else {
            continue;
        };
        let record = records
            .iter()
            .find(|record| record.id == item.record_id)
            .cloned()
            .unwrap_or_else(|| placeholder(&item.record_id, &item.doctype));
        blocked.push((
            index,
            skip(&record, rule, SkipReason::Referential { referrers }),
        ));
    }
    let blocked_indices: BTreeSet<usize> = blocked.iter().map(|(index, _)| *index).collect();
    let mut kept = Vec::with_capacity(items.len());
    for (index, item) in items.into_iter().enumerate() {
        if !blocked_indices.contains(&index) {
            kept.push(item);
        }
    }
    items = kept;
    skips.extend(blocked.into_iter().map(|(_, skip)| skip));

    items.sort_by(|a, b| {
        a.rule_id
            .cmp(&b.rule_id)
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
    skips.sort_by(|a, b| {
        a.rule_id
            .cmp(&b.rule_id)
            .then_with(|| a.record_id.cmp(&b.record_id))
            .then_with(|| a.reason.as_str().cmp(b.reason.as_str()))
    });

    let counts = count(policy.rules, &items, &skips);
    let identity = identity_of(policy, now, &items);

    Ok(RetentionPlan {
        now,
        calendar: policy.calendar.name().to_owned(),
        items,
        skips,
        counts,
        identity,
    })
}

fn skip(record: &Record, rule: &RetentionRule, reason: SkipReason) -> PlannedSkip {
    PlannedSkip {
        record_id: record.id.clone(),
        doctype: record.doctype.clone(),
        rule_id: rule.id.clone(),
        action: rule.action,
        reason,
    }
}

/// A record that vanished between the first and the referential pass; only its identity is used
/// in a skip entry.
fn placeholder(id: &str, doctype: &str) -> Record {
    Record {
        id: id.to_owned(),
        doctype: doctype.to_owned(),
        doc_class: None,
        jurisdiction: None,
        fields: BTreeMap::new(),
        created: 1,
        closed: None,
        events: BTreeMap::new(),
        references: Vec::new(),
    }
}

fn date_text(millis: u64) -> String {
    CivilDate::from_unix_millis(millis).map_or_else(|_| millis.to_string(), |date| date.to_string())
}

fn count(
    rules: &RetentionRules,
    items: &[PlannedAction],
    skips: &[PlannedSkip],
) -> BTreeMap<String, RuleCount> {
    let mut counts: BTreeMap<String, RuleCount> = rules
        .rules()
        .iter()
        .map(|rule| (rule.id.clone(), RuleCount::default()))
        .collect();
    for item in items {
        let count = counts.entry(item.rule_id.clone()).or_default();
        count.planned += 1;
        match item.action {
            Action::Delete => count.delete += 1,
            Action::Anonymise => count.anonymise += 1,
            Action::Archive => count.archive += 1,
        }
    }
    for skipped in skips {
        counts.entry(skipped.rule_id.clone()).or_default().skipped += 1;
    }
    counts
}

fn identity_of(policy: &PolicySet<'_>, now: u64, items: &[PlannedAction]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"spot.data_policy.retention_plan.v1\0");
    put_field(&mut hasher, policy.calendar.name().as_bytes());
    hasher.update(now.to_be_bytes());

    let mut rules: Vec<&RetentionRule> = policy.rules.rules().iter().collect();
    rules.sort_by(|a, b| a.id.cmp(&b.id));
    for rule in rules {
        put_field(&mut hasher, rule.id.as_bytes());
        put_field(&mut hasher, rule.doctype.as_bytes());
        put_field(&mut hasher, rule.trigger.as_text().as_bytes());
        put_field(&mut hasher, period_text(rule.period).as_bytes());
        put_field(&mut hasher, rule.action.as_str().as_bytes());
        for id in &rule.exceptions.record_ids {
            put_field(&mut hasher, id.as_bytes());
        }
        for class in &rule.exceptions.doc_classes {
            put_field(&mut hasher, class.as_bytes());
        }
    }

    let mut holds: Vec<&crate::hold::Hold> = policy.holds.holds().iter().collect();
    holds.sort_by(|a, b| a.id.cmp(&b.id));
    for hold in holds {
        put_field(&mut hasher, hold.id.as_bytes());
        put_field(&mut hasher, hold.scope.as_text().as_bytes());
        put_field(&mut hasher, hold.reason.as_bytes());
        put_field(&mut hasher, hold.set_by.as_bytes());
    }

    let mut floors: Vec<&crate::statutory::FloorEntry> = policy.floors.entries().iter().collect();
    floors.sort_by(|a, b| {
        a.doc_class
            .cmp(&b.doc_class)
            .then_with(|| a.jurisdiction.cmp(&b.jurisdiction))
    });
    for floor in floors {
        put_field(&mut hasher, floor.doc_class.as_bytes());
        put_field(&mut hasher, floor.jurisdiction.as_bytes());
        put_field(&mut hasher, period_text(floor.period).as_bytes());
    }

    for (doctype, field, spec) in policy.field_classes.entries() {
        put_field(&mut hasher, doctype.as_bytes());
        put_field(&mut hasher, field.as_bytes());
        hasher.update([u8::from(spec.classification.is_personal())]);
        hasher.update([u8::from(spec.ledger_relevant)]);
    }

    for item in items {
        put_field(&mut hasher, item.record_id.as_bytes());
        put_field(&mut hasher, item.rule_id.as_bytes());
        put_field(&mut hasher, item.action.as_str().as_bytes());
        hasher.update(item.due_at.to_be_bytes());
        for field in &item.anonymise_fields {
            put_field(&mut hasher, field.as_bytes());
        }
    }
    hasher.finalize().into()
}

fn period_text(period: Period) -> String {
    period.as_text()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{PolicySet, plan_run};
    use crate::calendar::{CivilDate, Gregorian};
    use crate::hold::Holds;
    use crate::record::{FieldClassMap, Record};
    use crate::rule::RetentionRules;
    use crate::statutory::StatutoryFloor;
    use crate::store::{MemoryStore, SkipReason};

    fn at(year: i32, month: u8, day: u8) -> u64 {
        CivilDate::new(year, month, day)
            .unwrap()
            .to_unix_millis()
            .unwrap()
    }

    fn record(id: &str, created: u64, class: Option<&str>) -> Record {
        Record {
            id: id.to_owned(),
            doctype: "note".to_owned(),
            doc_class: class.map(str::to_owned),
            jurisdiction: Some("de".to_owned()),
            fields: Default::default(),
            created,
            closed: None,
            events: Default::default(),
            references: Vec::new(),
        }
    }

    fn rules() -> RetentionRules {
        RetentionRules::from_toml(
            "[[rule]]\nid = \"notes\"\ndoctype = \"note\"\ntrigger = \"created\"\nperiod = \"1y\"\naction = \"delete\"\n",
        )
        .unwrap()
    }

    #[test]
    fn a_due_record_is_planned_and_a_young_one_is_not_due() {
        let store = MemoryStore::from_records(vec![
            record("old", at(2020, 1, 1), None),
            record("young", at(2025, 12, 31), None),
        ])
        .unwrap();
        let rules = rules();
        let holds = Holds::empty();
        let floors = StatutoryFloor::empty();
        let fields = FieldClassMap::default();
        let plan = plan_run(
            &PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian),
            &store,
            at(2026, 1, 1),
        )
        .unwrap();
        assert_eq!(plan.items().len(), 1);
        assert_eq!(plan.items()[0].record_id, "old");
        assert_eq!(plan.count("notes").unwrap().delete, 1);
        assert!(
            plan.skips().iter().any(|skip| skip.record_id == "young"
                && skip.reason.as_str() == SkipReason::NotDue.as_str())
        );
    }

    #[test]
    fn the_identity_changes_with_time_or_policy() {
        let store = MemoryStore::from_records(vec![record("old", at(2020, 1, 1), None)]).unwrap();
        let rules = rules();
        let holds = Holds::empty();
        let floors = StatutoryFloor::empty();
        let fields = FieldClassMap::default();
        let policy = PolicySet::new(&rules, &holds, &floors, &fields, &Gregorian);
        let first = plan_run(&policy, &store, at(2026, 1, 1)).unwrap();
        let same = plan_run(&policy, &store, at(2026, 1, 1)).unwrap();
        let later = plan_run(&policy, &store, at(2026, 1, 2)).unwrap();
        assert_eq!(first.identity(), same.identity());
        assert_ne!(first.identity(), later.identity());
        assert_eq!(first.identity_hex().len(), 64);
    }
}
