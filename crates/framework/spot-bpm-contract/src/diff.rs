//! Version diffs and what they mean for a running conversation (ADR-080
//! section 5.2, ADR-49's version rule).
//!
//! A change to a contract is a new version, and a conversation stays on the
//! version it started with. That leaves two questions the engine and the
//! designer must answer without guessing:
//!
//! - [`diff`]: what changed between two versions, and is each change
//!   **additive** (a conversation that started on the old version can go on
//!   under the new one) or **breaking** (it cannot)?
//! - [`compatible_range`]: given the versions a contract has had, how far can a
//!   conversation that started at version *n* be carried?
//!
//! The classification is deliberately conservative: when a change removes
//! something a peer may still send, narrows a field, shortens a wait, lowers a
//! repetition bound, tightens a field from optional to required or introduces a
//! new wait limit, it is **breaking**. Adding a message, an outcome, an
//! optional field, an order rule or a participant is **additive**.
//!
//! [`can_continue`] is the per-conversation check for the messages a
//! conversation has not reached yet; it reads [`diff`]'s rules the same way.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{
    Field, MessageDef, Next, Outcome, Participant, ProcessContract, Signature, TimeoutRule,
    Transition,
};

/// What part of the contract a change touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Subject {
    /// The contract id.
    Id,
    /// The version number.
    Version,
    /// The title.
    Title,
    /// A participant.
    Participant,
    /// A message.
    Message,
    /// One field of a message.
    Field,
    /// An order rule.
    Transition,
    /// An outcome.
    Outcome,
    /// A wait limit.
    Timeout,
    /// A signature.
    Signature,
}

impl Subject {
    /// The subject as a stable token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Version => "version",
            Self::Title => "title",
            Self::Participant => "participant",
            Self::Message => "message",
            Self::Field => "field",
            Self::Transition => "transition",
            Self::Outcome => "outcome",
            Self::Timeout => "timeout",
            Self::Signature => "signature",
        }
    }
}

/// How a named part changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// It exists in the new version only.
    Added,
    /// It exists in the old version only.
    Removed,
    /// It exists in both, with a different definition.
    Modified,
}

/// Whether a running conversation can follow the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Compatibility {
    /// A conversation that started on the old version can go on under the new
    /// one.
    Additive,
    /// A conversation that started on the old version cannot be carried over.
    Breaking,
}

/// One difference between two contract versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractChange {
    /// The part of the contract that changed.
    pub subject: Subject,
    /// The name of the part: a role, a message, `message.field`, a rule's
    /// `after -> next`, an outcome or a signature's role.
    pub name: String,
    /// How it changed.
    pub change: Change,
    /// What that means for a running conversation.
    pub compatibility: Compatibility,
    /// A sentence naming what changed, never a payload value.
    pub detail: String,
}

impl ContractChange {
    fn new(
        subject: Subject,
        name: impl Into<String>,
        change: Change,
        compatibility: Compatibility,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            subject,
            name: name.into(),
            change,
            compatibility,
            detail: detail.into(),
        }
    }
}

/// Every change from `a` to `b`, deterministically sorted.
///
/// The two contracts need not share an id; a changed id is reported (and is
/// breaking) rather than refused, so a designer can show what a peer sent even
/// when it is the wrong contract.
#[must_use]
pub fn diff(a: &ProcessContract, b: &ProcessContract) -> Vec<ContractChange> {
    let mut changes = Vec::new();
    if a.id != b.id {
        changes.push(ContractChange::new(
            Subject::Id,
            b.id.clone(),
            Change::Modified,
            Compatibility::Breaking,
            format!("the contract id changed from `{}` to `{}`", a.id, b.id),
        ));
    }
    if a.version != b.version {
        changes.push(ContractChange::new(
            Subject::Version,
            b.version.to_string(),
            Change::Modified,
            Compatibility::Additive,
            format!(
                "the version number changed from {} to {}",
                a.version, b.version
            ),
        ));
    }
    if a.title != b.title {
        changes.push(ContractChange::new(
            Subject::Title,
            "",
            Change::Modified,
            Compatibility::Additive,
            "the title changed".to_owned(),
        ));
    }
    diff_participants(a, b, &mut changes);
    diff_messages(a, b, &mut changes);
    diff_order(a, b, &mut changes);
    diff_outcomes(a, b, &mut changes);
    diff_timeouts(a, b, &mut changes);
    diff_signatures(a, b, &mut changes);
    changes.sort_by(|x, y| {
        (
            x.subject,
            x.name.as_str(),
            x.change,
            x.compatibility,
            x.detail.as_str(),
        )
            .cmp(&(
                y.subject,
                y.name.as_str(),
                y.change,
                y.compatibility,
                y.detail.as_str(),
            ))
    });
    changes.dedup();
    changes
}

/// Whether no change from `a` to `b` is breaking.
#[must_use]
pub fn is_compatible(a: &ProcessContract, b: &ProcessContract) -> bool {
    diff(a, b)
        .iter()
        .all(|change| change.compatibility == Compatibility::Additive)
}

/// The participants.
fn diff_participants(a: &ProcessContract, b: &ProcessContract, changes: &mut Vec<ContractChange>) {
    let old: BTreeMap<&str, &Participant> = a
        .participants
        .iter()
        .map(|p| (p.role.as_str(), p))
        .collect();
    let new: BTreeMap<&str, &Participant> = b
        .participants
        .iter()
        .map(|p| (p.role.as_str(), p))
        .collect();
    for (role, participant) in &new {
        match old.get(role) {
            None => changes.push(ContractChange::new(
                Subject::Participant,
                *role,
                Change::Added,
                Compatibility::Additive,
                format!("role `{role}` is new"),
            )),
            Some(before) if before.kind != participant.kind => changes.push(ContractChange::new(
                Subject::Participant,
                *role,
                Change::Modified,
                Compatibility::Breaking,
                format!(
                    "role `{role}` changed from {} to {}",
                    before.kind.as_str(),
                    participant.kind.as_str()
                ),
            )),
            Some(_) => {}
        }
    }
    for role in old.keys() {
        if !new.contains_key(role) {
            changes.push(ContractChange::new(
                Subject::Participant,
                *role,
                Change::Removed,
                Compatibility::Breaking,
                format!("role `{role}` is gone"),
            ));
        }
    }
}

/// The messages and their fields.
fn diff_messages(a: &ProcessContract, b: &ProcessContract, changes: &mut Vec<ContractChange>) {
    let old: BTreeMap<&str, &MessageDef> =
        a.messages.iter().map(|m| (m.name.as_str(), m)).collect();
    let new: BTreeMap<&str, &MessageDef> =
        b.messages.iter().map(|m| (m.name.as_str(), m)).collect();
    for (name, message) in &new {
        match old.get(name) {
            None => changes.push(ContractChange::new(
                Subject::Message,
                *name,
                Change::Added,
                Compatibility::Additive,
                format!("message `{name}` is new"),
            )),
            Some(before) => diff_message(before, message, changes),
        }
    }
    for name in old.keys() {
        if !new.contains_key(name) {
            changes.push(ContractChange::new(
                Subject::Message,
                *name,
                Change::Removed,
                Compatibility::Breaking,
                format!("message `{name}` is gone"),
            ));
        }
    }
}

/// One message that exists in both versions.
fn diff_message(before: &MessageDef, after: &MessageDef, changes: &mut Vec<ContractChange>) {
    let name = after.name.as_str();
    if before.topic != after.topic {
        changes.push(ContractChange::new(
            Subject::Message,
            name,
            Change::Modified,
            Compatibility::Breaking,
            format!(
                "message `{name}` moved from topic `{}` to `{}`",
                before.topic, after.topic
            ),
        ));
    }
    if before.from != after.from || before.to != after.to {
        changes.push(ContractChange::new(
            Subject::Message,
            name,
            Change::Modified,
            Compatibility::Breaking,
            format!(
                "message `{name}` changed direction from `{}` -> `{}` to `{}` -> `{}`",
                before.from, before.to, after.from, after.to
            ),
        ));
    }
    if before.schema_version != after.schema_version {
        let compatibility = if after.schema_version > before.schema_version {
            Compatibility::Additive
        } else {
            Compatibility::Breaking
        };
        changes.push(ContractChange::new(
            Subject::Message,
            name,
            Change::Modified,
            compatibility,
            format!(
                "message `{name}` schema version changed from {} to {}",
                before.schema_version, after.schema_version
            ),
        ));
    }
    let old_fields: BTreeMap<&str, &Field> =
        before.fields.iter().map(|f| (f.name.as_str(), f)).collect();
    let new_fields: BTreeMap<&str, &Field> =
        after.fields.iter().map(|f| (f.name.as_str(), f)).collect();
    for (field_name, field) in &new_fields {
        let subject = format!("{name}.{field_name}");
        match old_fields.get(field_name) {
            None => {
                let compatibility = if field.required {
                    Compatibility::Breaking
                } else {
                    Compatibility::Additive
                };
                changes.push(ContractChange::new(
                    Subject::Field,
                    subject,
                    Change::Added,
                    compatibility,
                    format!(
                        "field `{field_name}` of message `{name}` is new and {}",
                        if field.required {
                            "required"
                        } else {
                            "optional"
                        }
                    ),
                ));
            }
            Some(before) => diff_field(before, field, &subject, name, changes),
        }
    }
    for field_name in old_fields.keys() {
        if !new_fields.contains_key(field_name) {
            changes.push(ContractChange::new(
                Subject::Field,
                format!("{name}.{field_name}"),
                Change::Removed,
                Compatibility::Breaking,
                format!("field `{field_name}` of message `{name}` is gone"),
            ));
        }
    }
}

/// One field that exists in both versions.
fn diff_field(
    before: &Field,
    after: &Field,
    subject: &str,
    message: &str,
    changes: &mut Vec<ContractChange>,
) {
    if before.kind != after.kind {
        changes.push(ContractChange::new(
            Subject::Field,
            subject,
            Change::Modified,
            Compatibility::Breaking,
            format!(
                "field `{}` of message `{message}` changed from {} to {}",
                after.name,
                before.kind.as_str(),
                after.kind.as_str()
            ),
        ));
    }
    if before.required != after.required {
        let compatibility = if after.required {
            Compatibility::Breaking
        } else {
            Compatibility::Additive
        };
        changes.push(ContractChange::new(
            Subject::Field,
            subject,
            Change::Modified,
            compatibility,
            format!(
                "field `{}` of message `{message}` became {}",
                after.name,
                if after.required {
                    "required"
                } else {
                    "optional"
                }
            ),
        ));
    }
    match (before.max_len, after.max_len) {
        (Some(before_len), Some(after_len)) if before_len != after_len => {
            let compatibility = if after_len < before_len {
                Compatibility::Breaking
            } else {
                Compatibility::Additive
            };
            changes.push(ContractChange::new(
                Subject::Field,
                subject,
                Change::Modified,
                compatibility,
                format!(
                    "field `{}` of message `{message}` changed its text bound from {before_len} to {after_len}",
                    after.name
                ),
            ));
        }
        (Some(before_len), None) => changes.push(ContractChange::new(
            Subject::Field,
            subject,
            Change::Modified,
            Compatibility::Additive,
            format!(
                "field `{}` of message `{message}` lost its text bound of {before_len}",
                after.name
            ),
        )),
        (None, Some(after_len)) => changes.push(ContractChange::new(
            Subject::Field,
            subject,
            Change::Modified,
            Compatibility::Breaking,
            format!(
                "field `{}` of message `{message}` gained a text bound of {after_len}",
                after.name
            ),
        )),
        _ => {}
    }
}

/// The order rules, keyed by what they follow, what they lead to and who acts.
fn diff_order(a: &ProcessContract, b: &ProcessContract, changes: &mut Vec<ContractChange>) {
    let old = transitions_by_key(a);
    let new = transitions_by_key(b);
    for (key, transition) in &new {
        match old.get(key) {
            None => changes.push(ContractChange::new(
                Subject::Transition,
                key.clone(),
                Change::Added,
                Compatibility::Additive,
                format!("order rule `{key}` is new"),
            )),
            Some(before) => {
                if before.bound != transition.bound {
                    let compatibility = match (before.bound, transition.bound) {
                        (None, Some(_)) => Compatibility::Breaking,
                        (Some(_), None) => Compatibility::Additive,
                        (Some(before_bound), Some(after_bound)) if after_bound < before_bound => {
                            Compatibility::Breaking
                        }
                        _ => Compatibility::Additive,
                    };
                    changes.push(ContractChange::new(
                        Subject::Transition,
                        key.clone(),
                        Change::Modified,
                        compatibility,
                        format!("order rule `{key}` changed its repetition bound"),
                    ));
                }
            }
        }
    }
    for key in old.keys() {
        if !new.contains_key(key) {
            changes.push(ContractChange::new(
                Subject::Transition,
                key.clone(),
                Change::Removed,
                Compatibility::Breaking,
                format!("order rule `{key}` is gone"),
            ));
        }
    }
}

/// The order rules keyed by a stable text key.
fn transitions_by_key(contract: &ProcessContract) -> BTreeMap<String, &Transition> {
    contract
        .order
        .iter()
        .map(|transition| (transition_key(transition), transition))
        .collect()
}

/// `after -> next (by role)`, with `start` for the first message.
fn transition_key(transition: &Transition) -> String {
    let after = transition.after.as_deref().unwrap_or("start");
    match &transition.next {
        Next::Message(name) => format!("{after} -> {name} (by {})", transition.by),
        Next::Outcome(name) => format!("{after} -> outcome {name} (by {})", transition.by),
    }
}

/// The outcomes.
fn diff_outcomes(a: &ProcessContract, b: &ProcessContract, changes: &mut Vec<ContractChange>) {
    let old: BTreeMap<&str, &Outcome> = a.outcomes.iter().map(|o| (o.name.as_str(), o)).collect();
    let new: BTreeMap<&str, &Outcome> = b.outcomes.iter().map(|o| (o.name.as_str(), o)).collect();
    for (name, outcome) in &new {
        match old.get(name) {
            None => changes.push(ContractChange::new(
                Subject::Outcome,
                *name,
                Change::Added,
                Compatibility::Additive,
                format!("outcome `{name}` is new"),
            )),
            Some(before) if before.kind != outcome.kind => changes.push(ContractChange::new(
                Subject::Outcome,
                *name,
                Change::Modified,
                Compatibility::Breaking,
                format!(
                    "outcome `{name}` changed from {} to {}",
                    before.kind.as_str(),
                    outcome.kind.as_str()
                ),
            )),
            Some(_) => {}
        }
    }
    for name in old.keys() {
        if !new.contains_key(name) {
            changes.push(ContractChange::new(
                Subject::Outcome,
                *name,
                Change::Removed,
                Compatibility::Breaking,
                format!("outcome `{name}` is gone"),
            ));
        }
    }
}

/// The wait limits.
fn diff_timeouts(a: &ProcessContract, b: &ProcessContract, changes: &mut Vec<ContractChange>) {
    let old: BTreeMap<String, &TimeoutRule> =
        a.timeouts.iter().map(|t| (timeout_key(t), t)).collect();
    let new: BTreeMap<String, &TimeoutRule> =
        b.timeouts.iter().map(|t| (timeout_key(t), t)).collect();
    for (key, rule) in &new {
        match old.get(key) {
            None => changes.push(ContractChange::new(
                Subject::Timeout,
                key.clone(),
                Change::Added,
                Compatibility::Breaking,
                format!("wait limit `{key}` is new"),
            )),
            Some(before) => {
                if before.seconds != rule.seconds {
                    let compatibility = if rule.seconds > before.seconds {
                        Compatibility::Additive
                    } else {
                        Compatibility::Breaking
                    };
                    changes.push(ContractChange::new(
                        Subject::Timeout,
                        key.clone(),
                        Change::Modified,
                        compatibility,
                        format!(
                            "wait limit `{key}` changed from {}s to {}s",
                            before.seconds, rule.seconds
                        ),
                    ));
                }
                if before.outcome != rule.outcome {
                    let compatibility = if rule.outcome.is_some() {
                        Compatibility::Breaking
                    } else {
                        Compatibility::Additive
                    };
                    changes.push(ContractChange::new(
                        Subject::Timeout,
                        key.clone(),
                        Change::Modified,
                        compatibility,
                        format!("wait limit `{key}` changed the outcome it takes"),
                    ));
                }
            }
        }
    }
    for key in old.keys() {
        if !new.contains_key(key) {
            changes.push(ContractChange::new(
                Subject::Timeout,
                key.clone(),
                Change::Removed,
                Compatibility::Additive,
                format!("wait limit `{key}` is gone"),
            ));
        }
    }
}

/// The key of a wait limit.
fn timeout_key(rule: &TimeoutRule) -> String {
    format!("{} waits for {}", rule.by, rule.message)
}

/// The signatures. Signing policy is open (ADR-080 decision 4), so a change in
/// signatures never changes what a conversation may do.
fn diff_signatures(a: &ProcessContract, b: &ProcessContract, changes: &mut Vec<ContractChange>) {
    let old: BTreeMap<&str, &Signature> =
        a.signatures.iter().map(|s| (s.role.as_str(), s)).collect();
    let new: BTreeMap<&str, &Signature> =
        b.signatures.iter().map(|s| (s.role.as_str(), s)).collect();
    for (role, signature) in &new {
        match old.get(role) {
            None => changes.push(ContractChange::new(
                Subject::Signature,
                *role,
                Change::Added,
                Compatibility::Additive,
                format!("role `{role}` signed this version"),
            )),
            Some(before) if before != signature => changes.push(ContractChange::new(
                Subject::Signature,
                *role,
                Change::Modified,
                Compatibility::Additive,
                format!("the signature of role `{role}` changed"),
            )),
            Some(_) => {}
        }
    }
    for role in old.keys() {
        if !new.contains_key(role) {
            changes.push(ContractChange::new(
                Subject::Signature,
                *role,
                Change::Removed,
                Compatibility::Additive,
                format!("the signature of role `{role}` is gone"),
            ));
        }
    }
}

/// The versions a conversation that started at `started_at` may be carried
/// through: the last version reachable by a chain of additive changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompatibleRange {
    /// The version the conversation started on.
    pub from: u32,
    /// The newest version it can be carried to, inclusive.
    pub to: u32,
}

/// Why a compatible range could not be computed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RangeError {
    /// No contract in the history has that version.
    #[error("no contract version {version} is in the history")]
    UnknownVersion {
        /// The version asked for.
        version: u32,
    },
    /// The history holds one version number twice.
    #[error("contract version {version} appears twice in the history")]
    DuplicateVersion {
        /// The repeated version.
        version: u32,
    },
    /// The history mixes two contracts.
    #[error("the history holds contract `{found}` as well as `{expected}`")]
    ForeignContract {
        /// The contract the range is about.
        expected: String,
        /// The other contract in the history.
        found: String,
    },
}

/// The range of versions a conversation started at `started_at` may be carried
/// through.
///
/// The walk is pairwise: version *n* to *n+1* must be an additive change, and
/// the range ends at the first breaking one. A history need not be complete; it
/// is the versions the caller holds, in any order.
///
/// # Errors
/// [`RangeError`] when the started version is absent, a version repeats or the
/// history mixes contracts.
pub fn compatible_range(
    history: &[ProcessContract],
    started_at: u32,
) -> Result<CompatibleRange, RangeError> {
    let Some(first) = history.first() else {
        return Err(RangeError::UnknownVersion {
            version: started_at,
        });
    };
    let mut versions: Vec<&ProcessContract> = history.iter().collect();
    versions.sort_by_key(|contract| contract.version);
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    for contract in &versions {
        if !seen.insert(contract.version) {
            return Err(RangeError::DuplicateVersion {
                version: contract.version,
            });
        }
        if contract.id != first.id {
            return Err(RangeError::ForeignContract {
                expected: first.id.clone(),
                found: contract.id.clone(),
            });
        }
    }
    let Some(start) = versions
        .iter()
        .position(|contract| contract.version == started_at)
    else {
        return Err(RangeError::UnknownVersion {
            version: started_at,
        });
    };
    let mut range = CompatibleRange {
        from: started_at,
        to: started_at,
    };
    let mut current = versions[start];
    for next in &versions[start + 1..] {
        if !is_compatible(current, next) {
            break;
        }
        range.to = next.version;
        current = next;
    }
    Ok(range)
}

/// Whether a conversation that has reached `last` (or has not started yet, for
/// `None`) under `old` can be completed under `new`.
///
/// The check is conservative and covers what the conversation has **not** done
/// yet: every message it may still exchange, every order rule it may still
/// take, every outcome it may still reach and every field of those messages
/// must still exist in the new version with a compatible shape. A shorter wait
/// limit is not consulted here: a wait's limit is fixed when the wait starts,
/// so it does not change a conversation already under way.
#[must_use]
pub fn can_continue(old: &ProcessContract, new: &ProcessContract, last: Option<&str>) -> bool {
    if old.id != new.id {
        return false;
    }
    let remaining = remaining_messages(old, last);
    for name in &remaining {
        let Some(before) = old.message(name) else {
            continue;
        };
        let Some(after) = new.message(name) else {
            return false;
        };
        if before.topic != after.topic
            || before.from != after.from
            || before.to != after.to
            || after.schema_version < before.schema_version
        {
            return false;
        }
        for field in &before.fields {
            match after.field(&field.name) {
                None => return false,
                Some(after_field) if after_field.kind != field.kind => return false,
                Some(after_field) if after_field.required && !field.required => {}
                Some(_) => {}
            }
        }
    }
    let mut roots = remaining;
    if let Some(last) = last {
        roots.insert(last.to_owned());
    }
    for root in &roots {
        for transition in old.outgoing(root) {
            let key = transition_key(transition);
            let Some(after) = new
                .order
                .iter()
                .find(|candidate| transition_key(candidate) == key)
            else {
                return false;
            };
            match (transition.bound, after.bound) {
                (Some(before), Some(after)) if after < before => return false,
                (Some(_), None) => {}
                _ => {}
            }
            if let Some(outcome) = transition.next_outcome()
                && new.outcome(outcome).is_none()
            {
                return false;
            }
        }
    }
    true
}

/// The messages a conversation has not reached yet, starting after `last`.
fn remaining_messages(contract: &ProcessContract, last: Option<&str>) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut queue: Vec<String> = match last {
        Some(last) => contract
            .outgoing(last)
            .iter()
            .filter_map(|transition| transition.next_message().map(str::to_owned))
            .collect(),
        None => contract.reachable_messages().into_iter().collect(),
    };
    while let Some(name) = queue.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        for transition in contract.outgoing(&name) {
            if let Some(next) = transition.next_message() {
                queue.push(next.to_owned());
            }
        }
    }
    seen
}
