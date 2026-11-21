//! Conformance of a process half against its contract (ADR-080 section 5.6).
//!
//! Each organisation runs its own half of a conversation (ADR-080 section 5.1).
//! [`conforms`] answers the question the designer asks while a half is drawn
//! and the publish gate asks before it goes live:
//!
//! > Does this process, playing this role, exchange only contract messages, in
//! > an order the contract permits, reach every message the contract obliges
//! > it to exchange, and give every wait a timeout branch?
//!
//! The order check works on **local steps**: the send and wait nodes on one
//! path through the process. Two local steps are in a permitted order when the
//! contract lets the second follow the first, passing only through messages
//! that involve other roles — a message that involves *this* role may not be
//! skipped, because the process has no node for it.
//!
//! [`conforms_process`] checks the whole process: that it declares the
//! contract's participants and plays one of the roles, then that every version
//! conforms.

use std::collections::{BTreeMap, BTreeSet};

use spot_bpm_model::{DraftMode, Node, NodeId, NodeKind, Process, ProcessVersion};

use crate::check::{Problem, codes};
use crate::model::{ParticipantKind, ProcessContract};

/// Checks one process version — one half of the conversation — against the
/// contract, for the role it plays.
///
/// Every problem is reported, deterministically sorted and de-duplicated.
#[must_use]
pub fn conforms(version: &ProcessVersion, contract: &ProcessContract, role: &str) -> Vec<Problem> {
    let mut problems = crate::limits::exceeded(contract);
    if !problems.is_empty() {
        return problems;
    }
    if contract.participant(role).is_none() {
        problems.push(Problem::new(
            codes::CONFORMANCE_ROLE,
            role,
            format!(
                "process `{}` plays role `{role}`, which contract `{}` does not declare",
                version.name, contract.id
            ),
        ));
        return problems;
    }
    let steps = message_steps(version);
    for step in &steps {
        check_step(step, version, contract, role, &mut problems);
    }
    check_forms(version, &mut problems);
    check_order_of_steps(&local_steps(version), contract, role, &mut problems);
    check_obligations(version, contract, role, &mut problems);
    problems.sort();
    problems.dedup();
    problems
}

/// Checks a whole process: its declared participants against the contract's,
/// then every version for the role it plays.
#[must_use]
pub fn conforms_process(process: &Process, contract: &ProcessContract) -> Vec<Problem> {
    let mut problems = Vec::new();
    let Some(participants) = &process.participants else {
        problems.push(Problem::new(
            codes::CONFORMANCE_PARTICIPANTS,
            process.name.clone(),
            format!(
                "process `{}` declares no participants, so it cannot play a role of contract `{}`",
                process.name, contract.id
            ),
        ));
        return problems;
    };
    if participants.participant(&participants.plays).is_none() {
        problems.push(Problem::new(
            codes::CONFORMANCE_PARTICIPANTS,
            process.name.clone(),
            format!(
                "process `{}` plays role `{}`, which its own participants do not name",
                process.name, participants.plays
            ),
        ));
    }
    for participant in &participants.roles {
        match contract.participant(&participant.role) {
            None => problems.push(Problem::new(
                codes::CONFORMANCE_PARTICIPANTS,
                participant.role.clone(),
                format!(
                    "process `{}` names role `{}`, which contract `{}` does not declare",
                    process.name, participant.role, contract.id
                ),
            )),
            Some(expected) if !same_kind(participant.kind, expected.kind) => {
                problems.push(Problem::new(
                    codes::CONFORMANCE_PARTICIPANTS,
                    participant.role.clone(),
                    format!(
                        "process `{}` says role `{}` is {} but the contract says it is {}",
                        process.name,
                        participant.role,
                        model_kind(participant.kind),
                        expected.kind.as_str()
                    ),
                ));
            }
            Some(_) => {}
        }
    }
    for participant in &contract.participants {
        if participants
            .roles
            .iter()
            .all(|own| own.role != participant.role)
        {
            problems.push(Problem::new(
                codes::CONFORMANCE_PARTICIPANTS,
                participant.role.clone(),
                format!(
                    "process `{}` does not name role `{}`, which contract `{}` declares",
                    process.name, participant.role, contract.id
                ),
            ));
        }
    }
    let oversize = crate::limits::exceeded(contract);
    if !oversize.is_empty() {
        problems.extend(oversize);
        problems.sort();
        problems.dedup();
        return problems;
    }
    for version in &process.versions {
        problems.extend(conforms(version, contract, &participants.plays));
    }
    problems.sort();
    problems.dedup();
    problems
}

/// The kind of message step a node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Send,
    Wait,
}

/// One message step on a path: the node, its message and its direction.
#[derive(Clone)]
struct Step<'a> {
    node: &'a Node,
    message: &'a str,
    direction: Direction,
}

/// One message step as it occurs on a path, with the contract position it
/// occurs at: the last message actually exchanged before it.
///
/// A step on a timeout (or error) branch did not happen — the message never
/// arrived and the send failed — so the position after it is the position
/// before it. That is what lets a process wait for `approve` and fall back to
/// waiting for `reject` on timeout while still following the contract: after
/// `request`, both are allowed.
#[derive(Clone)]
struct LocalStep<'a> {
    step: Step<'a>,
    position_before: Option<String>,
}

impl Step<'_> {
    /// The contract the step names.
    fn contract(&self) -> &str {
        match &self.node.kind {
            NodeKind::SendMessage(config) => &config.contract,
            NodeKind::WaitMessage(config) => &config.contract,
            _ => "",
        }
    }
}

/// Every message step of a version, in definition order.
fn message_steps(version: &ProcessVersion) -> Vec<Step<'_>> {
    let mut steps = Vec::new();
    for node in &version.nodes {
        match &node.kind {
            NodeKind::SendMessage(config) => steps.push(Step {
                node,
                message: &config.message,
                direction: Direction::Send,
            }),
            NodeKind::WaitMessage(config) => steps.push(Step {
                node,
                message: &config.message,
                direction: Direction::Wait,
            }),
            _ => {}
        }
    }
    steps
}

/// Every form step of the half is complete enough to run (ADR-080 section
/// 2.2): it names fields, who is asked, and the record type its draft mode
/// needs.
///
/// The process model knows the form kind and checks its record type against
/// metadata; the step's completeness is checked here, with the contract the
/// half belongs to, because this is the check the designer and the publish gate
/// run.
fn check_forms(version: &ProcessVersion, problems: &mut Vec<Problem>) {
    for node in &version.nodes {
        let NodeKind::Form(config) = &node.kind else {
            continue;
        };
        if config.fields.is_empty() {
            problems.push(Problem::new(
                codes::CONFORMANCE_FORM,
                node.id.as_str(),
                format!("form step `{}` names no fields", node.id),
            ));
        }
        let mut seen = BTreeSet::new();
        for field in &config.fields {
            if !crate::check::is_field_name(field) {
                problems.push(Problem::new(
                    codes::CONFORMANCE_FORM,
                    node.id.as_str(),
                    format!("form step `{}` names invalid field `{field}`", node.id),
                ));
            } else if !seen.insert(field.as_str()) {
                problems.push(Problem::new(
                    codes::CONFORMANCE_FORM,
                    node.id.as_str(),
                    format!("form step `{}` names field `{field}` twice", node.id),
                ));
            }
        }
        if config
            .assignee
            .as_deref()
            .is_none_or(|assignee| assignee.trim().is_empty())
        {
            problems.push(Problem::new(
                codes::CONFORMANCE_FORM,
                node.id.as_str(),
                format!("form step `{}` names no assignee", node.id),
            ));
        }
        if config.draft_mode != DraftMode::Collect && config.record_type.is_none() {
            problems.push(Problem::new(
                codes::CONFORMANCE_FORM,
                node.id.as_str(),
                format!(
                    "form step `{}` creates or updates a draft but names no record type",
                    node.id
                ),
            ));
        }
    }
}

/// One message step's contract, message, direction and timeout.
fn check_step(
    step: &Step<'_>,
    version: &ProcessVersion,
    contract: &ProcessContract,
    role: &str,
    problems: &mut Vec<Problem>,
) {
    if step.contract() != contract.id {
        problems.push(Problem::new(
            codes::CONFORMANCE_CONTRACT,
            step.node.id.as_str(),
            format!(
                "node `{}` names contract `{}`, but this half is checked against `{}`",
                step.node.id,
                step.contract(),
                contract.id
            ),
        ));
    }
    let Some(message) = contract.message(step.message) else {
        problems.push(Problem::new(
            codes::CONFORMANCE_MESSAGE,
            step.node.id.as_str(),
            format!(
                "node `{}` names message `{}`, which contract `{}` does not define",
                step.node.id, step.message, contract.id
            ),
        ));
        return;
    };
    match step.direction {
        Direction::Send if message.from != role => problems.push(Problem::new(
            codes::CONFORMANCE_DIRECTION,
            step.node.id.as_str(),
            format!(
                "node `{}` sends `{}`, but the contract sends it from `{}`",
                step.node.id, message.name, message.from
            ),
        )),
        Direction::Wait if message.to != role => problems.push(Problem::new(
            codes::CONFORMANCE_DIRECTION,
            step.node.id.as_str(),
            format!(
                "node `{}` waits for `{}`, but the contract sends it to `{}`",
                step.node.id, message.name, message.to
            ),
        )),
        _ => {}
    }
    if step.direction == Direction::Wait {
        check_wait_timeout(step.node, version, step.message, contract, role, problems);
    }
}

/// A wait's time limit is the contract's, and its timeout branch exists
/// (ADR-080 section 5.5).
fn check_wait_timeout(
    node: &Node,
    version: &ProcessVersion,
    message: &str,
    contract: &ProcessContract,
    role: &str,
    problems: &mut Vec<Problem>,
) {
    let Some(limit) = node.timeout_secs.filter(|seconds| *seconds > 0) else {
        problems.push(Problem::new(
            codes::CONFORMANCE_TIMEOUT,
            node.id.as_str(),
            format!("wait node `{}` has no time limit", node.id),
        ));
        return;
    };
    match contract.timeout(message, role) {
        None => problems.push(Problem::new(
            codes::CONFORMANCE_TIMEOUT,
            node.id.as_str(),
            format!(
                "wait node `{}` waits for `{message}`, but the contract sets no time limit for it",
                node.id
            ),
        )),
        Some(rule) if limit > rule.seconds => problems.push(Problem::new(
            codes::CONFORMANCE_TIMEOUT,
            node.id.as_str(),
            format!(
                "wait node `{}` waits {limit}s, longer than the contract's {}s",
                node.id, rule.seconds
            ),
        )),
        Some(_) => {}
    }
    let has_branch = match &node.kind {
        NodeKind::WaitMessage(config) => match config.on_timeout {
            spot_bpm_model::TimeoutAction::ErrorBranch => node.error.is_some(),
            spot_bpm_model::TimeoutAction::TimeoutEdge => has_timeout_edge(version, node),
        },
        _ => false,
    };
    if !has_branch {
        problems.push(Problem::new(
            codes::CONFORMANCE_TIMEOUT,
            node.id.as_str(),
            format!(
                "wait node `{}` has no timeout branch, so a peer that never answers stalls the request",
                node.id
            ),
        ));
    }
}

/// Whether the wait has an outgoing edge labelled as its timeout branch.
fn has_timeout_edge(version: &ProcessVersion, node: &Node) -> bool {
    version.edges.iter().any(|edge| {
        edge.from == node.id
            && edge.label.as_deref() == Some(spot_bpm_model::WaitMessageConfig::TIMEOUT_EDGE_LABEL)
    })
}

/// Every message step of the half with each contract position it can occur at.
///
/// The search is over states `(node, position)`, each visited once, so it is
/// polynomial: at most `nodes x (messages + 1)` states however branched the
/// process is. (Enumerating paths is exponential in the number of branches and
/// a denial of service at publish time.) A cycle ends because its states
/// repeat, which is validation's problem to report, not conformance's.
fn local_steps(version: &ProcessVersion) -> Vec<LocalStep<'_>> {
    let mut outgoing: BTreeMap<&str, Vec<&spot_bpm_model::Edge>> = BTreeMap::new();
    for edge in &version.edges {
        outgoing.entry(edge.from.as_str()).or_default().push(edge);
    }
    let mut seen: BTreeSet<(&str, Option<&str>)> = BTreeSet::new();
    let mut steps: Vec<LocalStep<'_>> = Vec::new();
    let mut stack: Vec<(&NodeId, Option<&str>)> = entry_ids_ref(version)
        .into_iter()
        .map(|entry| (entry, None))
        .collect();
    while let Some((id, position)) = stack.pop() {
        if !seen.insert((id.as_str(), position)) {
            continue;
        }
        let Some(node) = version.node(id) else {
            continue;
        };
        let message = match &node.kind {
            NodeKind::SendMessage(config) => Some((config.message.as_str(), Direction::Send)),
            NodeKind::WaitMessage(config) => Some((config.message.as_str(), Direction::Wait)),
            _ => None,
        };
        if let Some((name, direction)) = message {
            steps.push(LocalStep {
                step: Step {
                    node,
                    message: name,
                    direction,
                },
                position_before: position.map(str::to_owned),
            });
        }
        let next_position = message.map(|(name, _)| name).or(position);
        for edge in outgoing.get(id.as_str()).into_iter().flatten() {
            // A timeout or error branch did not exchange the message.
            let failed = edge.label.as_deref()
                == Some(spot_bpm_model::WaitMessageConfig::TIMEOUT_EDGE_LABEL)
                || node.error.as_ref() == Some(&edge.to);
            stack.push((&edge.to, if failed { position } else { next_position }));
        }
    }
    steps
}

/// Checks every message step against the contract's order.
fn check_order_of_steps(
    steps: &[LocalStep<'_>],
    contract: &ProcessContract,
    role: &str,
    problems: &mut Vec<Problem>,
) {
    let mut memo: BTreeMap<(Option<&str>, &str), bool> = BTreeMap::new();
    for local in steps {
        if contract.message(local.step.message).is_none() {
            continue;
        }
        let key = (local.position_before.as_deref(), local.step.message);
        let permitted = *memo
            .entry(key)
            .or_insert_with(|| permitted_next(contract, role, key.0, key.1));
        if !permitted {
            let at = local.position_before.as_deref().unwrap_or("the start");
            problems.push(Problem::new(
                codes::CONFORMANCE_ORDER,
                local.step.node.id.as_str(),
                format!(
                    "message step `{}` exchanges `{}` at a point where the contract allows no such message after `{at}`",
                    local.step.node.id, local.step.message
                ),
            ));
        }
    }
}

/// Whether `next` may be the role's next step after `after` (or the first
/// step), passing only through messages that involve other roles.
fn permitted_next(contract: &ProcessContract, role: &str, after: Option<&str>, next: &str) -> bool {
    let mut queue: Vec<&str> = match after {
        Some(after) => contract
            .outgoing(after)
            .iter()
            .filter_map(|transition| transition.next_message())
            .collect(),
        None => contract
            .starts()
            .iter()
            .filter_map(|transition| transition.next_message())
            .collect(),
    };
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    while let Some(candidate) = queue.pop() {
        if !seen.insert(candidate) {
            continue;
        }
        if candidate == next {
            return true;
        }
        // A message involving this role is a local step: it cannot be passed
        // through without a node.
        if contract.involves(role, candidate) {
            continue;
        }
        for transition in contract.outgoing(candidate) {
            if let Some(following) = transition.next_message() {
                queue.push(following);
            }
        }
    }
    false
}

/// Every reachable message that involves the role needs a node.
fn check_obligations(
    version: &ProcessVersion,
    contract: &ProcessContract,
    role: &str,
    problems: &mut Vec<Problem>,
) {
    let steps = message_steps(version);
    let reachable_nodes = reachable_ids(version);
    for message_name in contract.reachable_messages() {
        let Some(message) = contract.message(&message_name) else {
            continue;
        };
        if message.from != role && message.to != role {
            continue;
        }
        let direction = if message.from == role {
            Direction::Send
        } else {
            Direction::Wait
        };
        let covered = steps.iter().any(|step| {
            step.message == message_name
                && step.direction == direction
                && reachable_nodes.contains(step.node.id.as_str())
        });
        if !covered {
            let verb = if direction == Direction::Send {
                "send"
            } else {
                "wait for"
            };
            problems.push(Problem::new(
                codes::CONFORMANCE_OBLIGATION,
                message_name.clone(),
                format!(
                    "the contract obliges role `{role}` to {verb} `{message_name}`, but no reachable step of process `{}` does",
                    version.name
                ),
            ));
        }
    }
}

/// The ids reachable from the version's entry nodes, following edges and error
/// branches.
fn reachable_ids(version: &ProcessVersion) -> BTreeSet<&str> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut stack: Vec<&NodeId> = entry_ids_ref(version);
    while let Some(id) = stack.pop() {
        if !seen.insert(id.as_str()) {
            continue;
        }
        for edge in version.edges.iter().filter(|edge| &edge.from == id) {
            stack.push(&edge.to);
        }
        if let Some(node) = version.node(id)
            && let Some(error) = &node.error
        {
            stack.push(error);
        }
    }
    seen
}

/// The nodes nothing leads into; a version with no entry at all starts at every
/// node.
fn entry_ids_ref(version: &ProcessVersion) -> Vec<&NodeId> {
    let targets: BTreeSet<&str> = version
        .edges
        .iter()
        .map(|edge| edge.to.as_str())
        .chain(
            version
                .nodes
                .iter()
                .filter_map(|node| node.error.as_ref().map(NodeId::as_str)),
        )
        .collect();
    let entries: Vec<&NodeId> = version
        .nodes
        .iter()
        .filter(|node| !targets.contains(node.id.as_str()))
        .map(|node| &node.id)
        .collect();
    if entries.is_empty() {
        version.nodes.iter().map(|node| &node.id).collect()
    } else {
        entries
    }
}

/// Whether a model participant kind and a contract participant kind agree.
fn same_kind(model: spot_bpm_model::ParticipantKind, contract: ParticipantKind) -> bool {
    matches!(
        (model, contract),
        (
            spot_bpm_model::ParticipantKind::Tenant,
            ParticipantKind::Tenant
        ) | (
            spot_bpm_model::ParticipantKind::External,
            ParticipantKind::External
        )
    )
}

/// The text token of a model participant kind.
fn model_kind(kind: spot_bpm_model::ParticipantKind) -> &'static str {
    match kind {
        spot_bpm_model::ParticipantKind::Tenant => "a tenant",
        spot_bpm_model::ParticipantKind::External => "an external system",
    }
}
