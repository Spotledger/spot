//! The contract checker (ADR-080 section 5.2, section 5.6): every problem with
//! a contract, reported at once.
//!
//! [`check`] answers "is this contract one a conversation can actually run
//! under?". It reports a stable `bpm_contract.*` code, the role, message,
//! outcome or signature the problem anchors on, and a sentence that names no
//! value — a contract's field names are safe to name, a payload's values are
//! not.
//!
//! What it refuses, and why:
//!
//! - a **message nobody can reach**: no order rule starts with it and no rule
//!   leads to it, so it is dead weight in a signed document;
//! - a **role that can never act**: it sends nothing, receives nothing and
//!   waits for nothing, so naming it claims a participation that does not
//!   exist;
//! - a **dead end without an outcome**: a message from which no path reaches an
//!   outcome, which is a conversation that can never be declared finished;
//! - a **message from a role to itself**: a tenant does not send to itself over
//!   a link;
//! - **schema errors**: a field that is not a valid name, a duplicate field, a
//!   text bound on a non-text field, a zero schema version;
//! - a **cyclic order without a bound**: a cycle is how a conversation repeats
//!   (a revision loop, a counter-offer), but each repetition must be bounded or
//!   the conversation can run for ever;
//! - a **field the metadata classifies as secret**, through [`FieldMetaView`];
//! - a **signature that does not cover this version**, names an unknown role or
//!   repeats a signer.

use std::collections::{BTreeMap, BTreeSet};

use crate::meta::{FieldClass, FieldMetaView, PublicFields};
use crate::model::{Field, MessageDef, Next, ProcessContract, Signature, TimeoutRule, Transition};

/// The stable `bpm_contract.*` codes this crate reports.
pub mod codes {
    /// The contract is larger than the bounds of [`limits`](crate::limits).
    pub const TOO_LARGE: &str = "bpm_contract.too_large";
    /// The contract id is not a valid name.
    pub const INVALID_ID: &str = "bpm_contract.invalid_id";
    /// The version number is 0; versions start at 1.
    pub const VERSION_ZERO: &str = "bpm_contract.version_zero";
    /// The contract names no participants.
    pub const NO_PARTICIPANTS: &str = "bpm_contract.no_participants";
    /// A role is not a valid name.
    pub const INVALID_ROLE: &str = "bpm_contract.invalid_role";
    /// The same role appears twice.
    pub const DUPLICATE_ROLE: &str = "bpm_contract.duplicate_role";
    /// A role neither sends, receives nor waits for anything.
    pub const ROLE_INACTIVE: &str = "bpm_contract.role_inactive";
    /// A message name is not a valid name.
    pub const INVALID_MESSAGE: &str = "bpm_contract.invalid_message";
    /// The same message name appears twice.
    pub const DUPLICATE_MESSAGE: &str = "bpm_contract.duplicate_message";
    /// A message, transition or timeout names a role the contract does not
    /// declare.
    pub const UNKNOWN_ROLE: &str = "bpm_contract.unknown_role";
    /// A message topic is not a valid tenant-link topic.
    pub const INVALID_TOPIC: &str = "bpm_contract.invalid_topic";
    /// A message's sender and receiver are the same role.
    pub const SELF_MESSAGE: &str = "bpm_contract.self_message";
    /// The message schema is wrong: a zero schema version, or a text bound on a
    /// field that is not text.
    pub const SCHEMA: &str = "bpm_contract.schema";
    /// A field name is not a valid field name.
    pub const INVALID_FIELD: &str = "bpm_contract.invalid_field";
    /// The same field appears twice in one message.
    pub const DUPLICATE_FIELD: &str = "bpm_contract.duplicate_field";
    /// A field list names a field the metadata classifies as secret.
    pub const SECRET_FIELD: &str = "bpm_contract.secret_field";
    /// A transition names a message the contract does not define.
    pub const UNKNOWN_MESSAGE: &str = "bpm_contract.unknown_message";
    /// An outcome name is not a valid name.
    pub const INVALID_OUTCOME: &str = "bpm_contract.invalid_outcome";
    /// The same outcome appears twice.
    pub const DUPLICATE_OUTCOME: &str = "bpm_contract.duplicate_outcome";
    /// An order rule or timeout names an outcome the contract does not declare.
    pub const UNKNOWN_OUTCOME: &str = "bpm_contract.unknown_outcome";
    /// An order rule names a sender that is not the message's sender.
    pub const ORDER_MISMATCH: &str = "bpm_contract.order_mismatch";
    /// The same order rule appears twice.
    pub const DUPLICATE_TRANSITION: &str = "bpm_contract.duplicate_transition";
    /// An order rule has a bound of 0.
    pub const INVALID_BOUND: &str = "bpm_contract.invalid_bound";
    /// No order rule starts with this message or leads to it.
    pub const MESSAGE_UNREACHABLE: &str = "bpm_contract.message_unreachable";
    /// From this message no path reaches an outcome.
    pub const DEAD_END: &str = "bpm_contract.dead_end";
    /// An order cycle has no bounded transition.
    pub const UNBOUNDED_CYCLE: &str = "bpm_contract.unbounded_cycle";
    /// No order rule ever ends in this outcome.
    pub const OUTCOME_UNREACHABLE: &str = "bpm_contract.outcome_unreachable";
    /// A wait limit is wrong: unknown message or role, a waiting role that does
    /// not receive the message, zero seconds, a duplicate or an unknown
    /// outcome.
    pub const TIMEOUT: &str = "bpm_contract.timeout";
    /// A signature does not cover this version, names an unknown role or
    /// repeats a signer.
    pub const SIGNATURE: &str = "bpm_contract.signature";
    /// The role the process half plays is not a participant of the contract.
    pub const CONFORMANCE_ROLE: &str = "bpm_contract.conformance_role";
    /// The process's declared participants do not match the contract's.
    pub const CONFORMANCE_PARTICIPANTS: &str = "bpm_contract.conformance_participants";
    /// A message step names a different contract.
    pub const CONFORMANCE_CONTRACT: &str = "bpm_contract.conformance_contract";
    /// A message step names a message the contract does not define.
    pub const CONFORMANCE_MESSAGE: &str = "bpm_contract.conformance_message";
    /// A send step sends a message the role receives, or a wait waits for one
    /// the role sends.
    pub const CONFORMANCE_DIRECTION: &str = "bpm_contract.conformance_direction";
    /// The process's message steps are not in an order the contract permits.
    pub const CONFORMANCE_ORDER: &str = "bpm_contract.conformance_order";
    /// The process does not reach a message the contract obliges the role to
    /// exchange.
    pub const CONFORMANCE_OBLIGATION: &str = "bpm_contract.conformance_obligation";
    /// A wait has no timeout branch, or its limit is not the contract's.
    pub const CONFORMANCE_TIMEOUT: &str = "bpm_contract.conformance_timeout";
    /// A form step in the half is not complete: no fields, no assignee, or no
    /// record type while it creates or updates a draft.
    pub const CONFORMANCE_FORM: &str = "bpm_contract.conformance_form";
}

/// One problem with a contract, or with a process half checked against one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Problem {
    /// The stable machine-readable code (`bpm_contract.*`).
    pub code: &'static str,
    /// The role, message, outcome, node or signature the problem anchors on;
    /// empty when it is about the contract as a whole.
    pub subject: String,
    /// A human-readable sentence that names no value.
    pub message: String,
}

impl Problem {
    /// Builds a problem.
    #[must_use]
    pub fn new(code: &'static str, subject: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code,
            subject: subject.into(),
            message: message.into(),
        }
    }
}

/// Checks a contract without field metadata: every field is treated as public.
#[must_use]
pub fn check(contract: &ProcessContract) -> Vec<Problem> {
    check_with(contract, &PublicFields)
}

/// Checks a contract against the metadata's field classes.
///
/// Every problem is reported, deterministically sorted by code, subject and
/// message, and de-duplicated.
#[must_use]
pub fn check_with(contract: &ProcessContract, fields: &dyn FieldMetaView) -> Vec<Problem> {
    // An oversize contract is refused before any other work: the other checks
    // are polynomial in the contract's size, and the size is untrusted.
    let mut problems = crate::limits::exceeded(contract);
    if !problems.is_empty() {
        return problems;
    }
    check_identity(contract, &mut problems);
    check_participants(contract, &mut problems);
    check_messages(contract, fields, &mut problems);
    check_order(contract, &mut problems);
    check_outcomes(contract, &mut problems);
    check_timeouts(contract, &mut problems);
    check_reachability(contract, &mut problems);
    check_cycles(contract, &mut problems);
    check_signatures(contract, &mut problems);
    problems.sort();
    problems.dedup();
    problems
}

/// The id and version number.
fn check_identity(contract: &ProcessContract, problems: &mut Vec<Problem>) {
    if !is_name(&contract.id) {
        problems.push(Problem::new(
            codes::INVALID_ID,
            "",
            format!("`{}` is not a valid contract id", contract.id),
        ));
    }
    if contract.version == 0 {
        problems.push(Problem::new(
            codes::VERSION_ZERO,
            "",
            "the version is 0; contract versions start at 1".to_owned(),
        ));
    }
}

/// The participants: at least one, well named, distinct, and each able to act.
fn check_participants(contract: &ProcessContract, problems: &mut Vec<Problem>) {
    if contract.participants.is_empty() {
        problems.push(Problem::new(
            codes::NO_PARTICIPANTS,
            "",
            "the contract names no participants".to_owned(),
        ));
        return;
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for participant in &contract.participants {
        if !is_name(&participant.role) {
            problems.push(Problem::new(
                codes::INVALID_ROLE,
                participant.role.clone(),
                format!("`{}` is not a valid role name", participant.role),
            ));
        } else if !seen.insert(participant.role.as_str()) {
            problems.push(Problem::new(
                codes::DUPLICATE_ROLE,
                participant.role.clone(),
                format!("role `{}` appears twice", participant.role),
            ));
        }
        if !can_act(contract, &participant.role) {
            problems.push(Problem::new(
                codes::ROLE_INACTIVE,
                participant.role.clone(),
                format!(
                    "role `{}` neither sends, receives nor waits for anything",
                    participant.role
                ),
            ));
        }
    }
}

/// Whether the role takes part in any message exchange.
fn can_act(contract: &ProcessContract, role: &str) -> bool {
    let sends = contract
        .messages
        .iter()
        .any(|m| m.from == role || m.to == role);
    let acts = contract.order.iter().any(|t| t.by == role);
    let waits = contract.timeouts.iter().any(|t| t.by == role);
    sends || acts || waits
}

/// The messages: names, topics, directions, schema versions and fields.
fn check_messages(
    contract: &ProcessContract,
    fields: &dyn FieldMetaView,
    problems: &mut Vec<Problem>,
) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for message in &contract.messages {
        if !is_name(&message.name) {
            problems.push(Problem::new(
                codes::INVALID_MESSAGE,
                message.name.clone(),
                format!("`{}` is not a valid message name", message.name),
            ));
        } else if !seen.insert(message.name.as_str()) {
            problems.push(Problem::new(
                codes::DUPLICATE_MESSAGE,
                message.name.clone(),
                format!("message `{}` appears twice", message.name),
            ));
        }
        if !is_topic(&message.topic) {
            problems.push(Problem::new(
                codes::INVALID_TOPIC,
                message.name.clone(),
                format!(
                    "message `{}` names topic `{}`, which is not a valid link topic",
                    message.name, message.topic
                ),
            ));
        }
        for role in [&message.from, &message.to] {
            if contract.participant(role).is_none() {
                problems.push(Problem::new(
                    codes::UNKNOWN_ROLE,
                    message.name.clone(),
                    format!(
                        "message `{}` names role `{role}`, which the contract does not declare",
                        message.name
                    ),
                ));
            }
        }
        if message.from == message.to {
            problems.push(Problem::new(
                codes::SELF_MESSAGE,
                message.name.clone(),
                format!(
                    "message `{}` is sent from `{}` to itself",
                    message.name, message.from
                ),
            ));
        }
        if message.schema_version == 0 {
            problems.push(Problem::new(
                codes::SCHEMA,
                message.name.clone(),
                format!(
                    "message `{}` has schema version 0; message schemas start at 1",
                    message.name
                ),
            ));
        }
        check_fields(message, fields, problems);
    }
}

/// One message's field list.
fn check_fields(message: &MessageDef, fields: &dyn FieldMetaView, problems: &mut Vec<Problem>) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for field in &message.fields {
        let subject = format!("{}.{}", message.name, field.name);
        if !is_field_name(&field.name) {
            problems.push(Problem::new(
                codes::INVALID_FIELD,
                subject.clone(),
                format!("`{}` is not a valid field name", field.name),
            ));
        } else if !seen.insert(field.name.as_str()) {
            problems.push(Problem::new(
                codes::DUPLICATE_FIELD,
                subject.clone(),
                format!("field `{}` appears twice", field.name),
            ));
        }
        check_field_shape(message, field, &subject, problems);
        if fields.class(&field.name) == FieldClass::Secret {
            problems.push(Problem::new(
                codes::SECRET_FIELD,
                subject,
                format!(
                    "message `{}` lists field `{}`, which the metadata classifies as secret",
                    message.name, field.name
                ),
            ));
        }
    }
}

/// A field's text bound belongs to text fields only.
fn check_field_shape(
    message: &MessageDef,
    field: &Field,
    subject: &str,
    problems: &mut Vec<Problem>,
) {
    if let Some(max_len) = field.max_len {
        if field.kind != crate::model::FieldKind::String {
            problems.push(Problem::new(
                codes::SCHEMA,
                subject.to_owned(),
                format!(
                    "field `{}` of message `{}` has a text bound but is not text",
                    field.name, message.name
                ),
            ));
        } else if max_len == 0 {
            problems.push(Problem::new(
                codes::SCHEMA,
                subject.to_owned(),
                format!(
                    "field `{}` of message `{}` allows no text at all",
                    field.name, message.name
                ),
            ));
        }
    }
}

/// The order rules: names, senders, duplicates and bounds.
fn check_order(contract: &ProcessContract, problems: &mut Vec<Problem>) {
    let mut seen: BTreeSet<(Option<&str>, &str, &str)> = BTreeSet::new();
    for transition in &contract.order {
        let subject = transition_subject(transition);
        if let Some(after) = &transition.after
            && contract.message(after).is_none()
        {
            problems.push(Problem::new(
                codes::UNKNOWN_MESSAGE,
                subject.clone(),
                format!("order rule follows `{after}`, which is not a message of the contract"),
            ));
        }
        match &transition.next {
            Next::Message(name) => match contract.message(name) {
                None => problems.push(Problem::new(
                    codes::UNKNOWN_MESSAGE,
                    subject.clone(),
                    format!("order rule leads to `{name}`, which is not a message of the contract"),
                )),
                Some(message) if message.from != transition.by => problems.push(Problem::new(
                    codes::ORDER_MISMATCH,
                    subject.clone(),
                    format!(
                        "order rule says `{}` may send `{name}`, but the message's sender is `{}`",
                        transition.by, message.from
                    ),
                )),
                Some(_) => {}
            },
            Next::Outcome(name) => {
                if contract.outcome(name).is_none() {
                    problems.push(Problem::new(
                        codes::UNKNOWN_OUTCOME,
                        subject.clone(),
                        format!(
                            "order rule leads to `{name}`, which is not an outcome of the contract"
                        ),
                    ));
                }
            }
        }
        if contract.participant(&transition.by).is_none() {
            problems.push(Problem::new(
                codes::UNKNOWN_ROLE,
                subject.clone(),
                format!(
                    "order rule names role `{}`, which the contract does not declare",
                    transition.by
                ),
            ));
        }
        if transition.bound == Some(0) {
            problems.push(Problem::new(
                codes::INVALID_BOUND,
                subject.clone(),
                "order rule may be taken 0 times; a bound is at least 1".to_owned(),
            ));
        }
        let key = (
            transition.after.as_deref(),
            transition.next_message().unwrap_or(""),
            transition.by.as_str(),
        );
        let outcome_key = (
            transition.after.as_deref(),
            transition.next_outcome().unwrap_or(""),
            transition.by.as_str(),
        );
        let duplicate = match &transition.next {
            Next::Message(_) => !seen.insert(key),
            Next::Outcome(_) => !seen.insert(outcome_key),
        };
        if duplicate {
            problems.push(Problem::new(
                codes::DUPLICATE_TRANSITION,
                subject,
                "the same order rule appears twice".to_owned(),
            ));
        }
    }
}

/// The outcome names and duplicates.
fn check_outcomes(contract: &ProcessContract, problems: &mut Vec<Problem>) {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for outcome in &contract.outcomes {
        if !is_name(&outcome.name) {
            problems.push(Problem::new(
                codes::INVALID_OUTCOME,
                outcome.name.clone(),
                format!("`{}` is not a valid outcome name", outcome.name),
            ));
        } else if !seen.insert(outcome.name.as_str()) {
            problems.push(Problem::new(
                codes::DUPLICATE_OUTCOME,
                outcome.name.clone(),
                format!("outcome `{}` appears twice", outcome.name),
            ));
        }
    }
}

/// The wait limits.
fn check_timeouts(contract: &ProcessContract, problems: &mut Vec<Problem>) {
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for timeout in &contract.timeouts {
        check_timeout(contract, timeout, problems);
        if !seen.insert((timeout.message.as_str(), timeout.by.as_str())) {
            problems.push(Problem::new(
                codes::TIMEOUT,
                timeout_subject(timeout),
                format!(
                    "the wait of `{}` for `{}` has more than one limit",
                    timeout.by, timeout.message
                ),
            ));
        }
    }
}

/// One wait limit.
fn check_timeout(contract: &ProcessContract, timeout: &TimeoutRule, problems: &mut Vec<Problem>) {
    let subject = timeout_subject(timeout);
    match contract.message(&timeout.message) {
        None => problems.push(Problem::new(
            codes::TIMEOUT,
            subject.clone(),
            format!(
                "a wait limit names `{}`, which is not a message of the contract",
                timeout.message
            ),
        )),
        Some(message) if message.to != timeout.by => problems.push(Problem::new(
            codes::TIMEOUT,
            subject.clone(),
            format!(
                "a wait limit says `{}` waits for `{}`, but the message's receiver is `{}`",
                timeout.by, timeout.message, message.to
            ),
        )),
        Some(_) => {}
    }
    if contract.participant(&timeout.by).is_none() {
        problems.push(Problem::new(
            codes::TIMEOUT,
            subject.clone(),
            format!(
                "a wait limit names role `{}`, which the contract does not declare",
                timeout.by
            ),
        ));
    }
    if timeout.seconds == 0 {
        problems.push(Problem::new(
            codes::TIMEOUT,
            subject.clone(),
            "a wait limit of 0 seconds leaves no time to answer".to_owned(),
        ));
    }
    if let Some(outcome) = &timeout.outcome
        && contract.outcome(outcome).is_none()
    {
        problems.push(Problem::new(
            codes::TIMEOUT,
            subject,
            format!("a wait limit ends in `{outcome}`, which is not an outcome of the contract"),
        ));
    }
}

/// Every message is reachable, every outcome ends a conversation, and no
/// reachable message is a dead end.
fn check_reachability(contract: &ProcessContract, problems: &mut Vec<Problem>) {
    let reachable = contract.reachable_messages();
    for message in &contract.messages {
        if !reachable.contains(&message.name) {
            problems.push(Problem::new(
                codes::MESSAGE_UNREACHABLE,
                message.name.clone(),
                format!(
                    "message `{}` is not reachable from any starting message",
                    message.name
                ),
            ));
        }
    }
    for outcome in &contract.outcomes {
        if !contract
            .order
            .iter()
            .any(|t| t.next_outcome() == Some(&outcome.name))
        {
            problems.push(Problem::new(
                codes::OUTCOME_UNREACHABLE,
                outcome.name.clone(),
                format!("no order rule ends the conversation in `{}`", outcome.name),
            ));
        }
    }
    let can_end = messages_reaching_an_outcome(contract);
    for name in &reachable {
        if !can_end.contains(name) {
            problems.push(Problem::new(
                codes::DEAD_END,
                name.clone(),
                format!("no path from `{name}` reaches an outcome"),
            ));
        }
    }
}

/// The messages from which some path reaches an outcome.
fn messages_reaching_an_outcome(contract: &ProcessContract) -> BTreeSet<String> {
    let mut reaching: BTreeSet<String> = contract
        .order
        .iter()
        .filter(|t| t.next_outcome().is_some())
        .filter_map(|t| t.after.clone())
        .collect();
    loop {
        let mut grew = false;
        for transition in &contract.order {
            let Some(next) = transition.next_message() else {
                continue;
            };
            if reaching.contains(next)
                && let Some(after) = &transition.after
                && reaching.insert(after.clone())
            {
                grew = true;
            }
        }
        if !grew {
            return reaching;
        }
    }
}

/// A cycle in the order graph must carry a bound (ADR-080 section 5.2).
fn check_cycles(contract: &ProcessContract, problems: &mut Vec<Problem>) {
    let names: Vec<String> = contract.messages.iter().map(|m| m.name.clone()).collect();
    let reach = message_reachability(contract, &names);
    let mut assigned: BTreeSet<String> = BTreeSet::new();
    for name in &names {
        if assigned.contains(name) {
            continue;
        }
        let cyclic = reach.get(name).is_some_and(|set| set.contains(name));
        let mut component: BTreeSet<String> = BTreeSet::new();
        component.insert(name.clone());
        for other in &names {
            if other != name
                && reach.get(name).is_some_and(|set| set.contains(other))
                && reach.get(other).is_some_and(|set| set.contains(name))
            {
                component.insert(other.clone());
            }
        }
        if !cyclic && component.len() == 1 {
            continue;
        }
        assigned.extend(component.iter().cloned());
        let bounded = contract.order.iter().any(|transition| {
            transition.bound.is_some()
                && transition
                    .after
                    .as_ref()
                    .is_some_and(|after| component.contains(after))
                && transition
                    .next_message()
                    .is_some_and(|next| component.contains(next))
        });
        if !bounded {
            problems.push(Problem::new(
                codes::UNBOUNDED_CYCLE,
                component.iter().next().cloned().unwrap_or_default(),
                format!(
                    "the order rules loop through {} without a bound",
                    component.into_iter().collect::<Vec<_>>().join(", ")
                ),
            ));
        }
    }
}

/// For each message, the messages reachable from it by following order rules.
fn message_reachability(
    contract: &ProcessContract,
    names: &[String],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut map = BTreeMap::new();
    for name in names {
        let mut seen = BTreeSet::new();
        let mut queue = vec![name.clone()];
        while let Some(current) = queue.pop() {
            for transition in contract.outgoing(&current) {
                if let Some(next) = transition.next_message()
                    && seen.insert(next.to_owned())
                {
                    queue.push(next.to_owned());
                }
            }
        }
        map.insert(name.clone(), seen);
    }
    map
}

/// Signatures: structurally sound, over this version, one per signer.
fn check_signatures(contract: &ProcessContract, problems: &mut Vec<Problem>) {
    let hash = contract.content_hash().as_hex().to_owned();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for signature in &contract.signatures {
        check_signature(contract, signature, &hash, problems);
        if !seen.insert(signature.role.as_str()) {
            problems.push(Problem::new(
                codes::SIGNATURE,
                signature.role.clone(),
                format!("role `{}` signed this version twice", signature.role),
            ));
        }
    }
}

/// One signature.
fn check_signature(
    contract: &ProcessContract,
    signature: &Signature,
    hash: &str,
    problems: &mut Vec<Problem>,
) {
    if contract.participant(&signature.role).is_none() {
        problems.push(Problem::new(
            codes::SIGNATURE,
            signature.role.clone(),
            format!(
                "a signature names role `{}`, which the contract does not declare",
                signature.role
            ),
        ));
    }
    if signature.algorithm.is_empty()
        || signature.key_id.is_empty()
        || signature.signature.is_empty()
    {
        problems.push(Problem::new(
            codes::SIGNATURE,
            signature.role.clone(),
            format!(
                "the signature of `{}` does not name an algorithm, a key and a value",
                signature.role
            ),
        ));
    }
    if signature.signed_hash != hash {
        problems.push(Problem::new(
            codes::SIGNATURE,
            signature.role.clone(),
            format!(
                "the signature of `{}` covers a different version of the contract",
                signature.role
            ),
        ));
    }
}

/// The subject of an order rule, stable across renderings.
fn transition_subject(transition: &Transition) -> String {
    let after = transition.after.as_deref().unwrap_or("start");
    let next = match &transition.next {
        Next::Message(name) => format!("message {name}"),
        Next::Outcome(name) => format!("outcome {name}"),
    };
    format!("{after} -> {next}")
}

/// The subject of a wait limit.
fn timeout_subject(timeout: &TimeoutRule) -> String {
    format!("{} waits for {}", timeout.by, timeout.message)
}

/// Whether `raw` matches `[a-z][a-z0-9_]{0,63}`.
#[must_use]
pub fn is_name(raw: &str) -> bool {
    spot_bpm_model::id::is_name(raw)
}

/// Whether `raw` is a tenant-link topic: at least two dotted lower-case
/// segments, at most 128 characters (ADR-058 rule 7).
#[must_use]
pub fn is_topic(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > 128 {
        return false;
    }
    let segments: Vec<&str> = raw.split('.').collect();
    segments.len() >= 2 && segments.iter().all(|segment| is_segment(segment))
}

/// Whether `raw` is a field name: dotted lower-case segments (ADR-058 rule 6).
#[must_use]
pub fn is_field_name(raw: &str) -> bool {
    spot_bpm_model::id::is_field_name(raw)
}

/// One lower-case segment of a topic.
fn is_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
