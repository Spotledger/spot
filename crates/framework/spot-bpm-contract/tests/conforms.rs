//! Conformance of a process half against a contract: a requester half that
//! conforms, and one failure for each problem kind (ADR-080 section 5.6).

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use spot_bpm_contract::{
    ParticipantKind, ProcessContract, codes, conforms, conforms_process, contracts, from_text,
};
use spot_bpm_model::{
    DecisionConfig, Edge, Limits, Node, NodeId, NodeKind, Participants, Process, ProcessVersion,
    RejectAction, SendMessageConfig, TimeoutAction, TriggerConfig, TriggerType, VersionStatus,
    WaitMessageConfig, validate_version,
};

/// A small extension trait so the test reads as a chain without importing
/// free functions twice.
trait ValidateExt: Sized {
    fn problems(&self) -> Vec<spot_bpm_model::Problem>;
}

impl ValidateExt for ProcessVersion {
    fn problems(&self) -> Vec<spot_bpm_model::Problem> {
        validate_version(self, &spot_bpm_model::AllowAllMeta, &Limits::new())
    }
}

fn id(name: &str) -> NodeId {
    NodeId::new(name).unwrap()
}

fn contract() -> ProcessContract {
    from_text(contracts::REQUEST_APPROVE).unwrap()
}

/// The requester's half of `request_approve`: send the request, then wait for
/// approve or (on timeout) for reject, or cancel instead.
fn requester_half() -> ProcessVersion {
    let terminal =
        |name: &str| Node::new(id(name), NodeKind::Logic(Default::default())).as_terminal();
    let nodes = vec![
        Node::new(
            id("start"),
            NodeKind::Trigger(TriggerConfig {
                trigger: TriggerType::Manual,
                ..TriggerConfig::default()
            }),
        ),
        Node::new(
            id("enter"),
            NodeKind::Form(spot_bpm_model::FormConfig {
                record_type: Some("Expense Claim".to_owned()),
                fields: vec!["reference".to_owned(), "amount".to_owned()],
                assignee: Some("role:requester".to_owned()),
                draft_mode: spot_bpm_model::DraftMode::Create,
            }),
        ),
        Node::new(
            id("send_request"),
            NodeKind::SendMessage(SendMessageConfig {
                contract: "request_approve".to_owned(),
                message: "request".to_owned(),
                mapping: Some("request_payload".to_owned()),
                on_reject: RejectAction::ErrorBranch,
            }),
        )
        .with_error(id("send_failed")),
        Node::new(
            id("choose"),
            NodeKind::Decision(DecisionConfig {
                title: "Wait for an answer or cancel?".to_owned(),
                assignee: Some("role:requester".to_owned()),
                outcomes: vec!["wait".to_owned(), "cancel".to_owned()],
            }),
        ),
        Node::new(
            id("wait_approve"),
            NodeKind::WaitMessage(WaitMessageConfig {
                contract: "request_approve".to_owned(),
                message: "approve".to_owned(),
                on_timeout: TimeoutAction::TimeoutEdge,
            }),
        )
        .with_timeout(3_600),
        Node::new(
            id("wait_reject"),
            NodeKind::WaitMessage(WaitMessageConfig {
                contract: "request_approve".to_owned(),
                message: "reject".to_owned(),
                on_timeout: TimeoutAction::TimeoutEdge,
            }),
        )
        .with_timeout(3_600),
        Node::new(
            id("send_cancel"),
            NodeKind::SendMessage(SendMessageConfig {
                contract: "request_approve".to_owned(),
                message: "cancel".to_owned(),
                mapping: None,
                on_reject: RejectAction::Continue,
            }),
        ),
        terminal("approved"),
        terminal("rejected"),
        terminal("expired"),
        terminal("cancelled"),
        terminal("send_failed"),
    ];
    let edges = vec![
        Edge::new(id("start"), id("enter")),
        Edge::new(id("enter"), id("send_request")),
        Edge::new(id("send_request"), id("choose")),
        Edge::new(id("choose"), id("wait_approve")).with_condition("doc.wait == true"),
        Edge::new(id("choose"), id("send_cancel")).as_default(),
        Edge::new(id("wait_approve"), id("approved")).as_default(),
        Edge::new(id("wait_approve"), id("wait_reject")).with_label("timeout"),
        Edge::new(id("wait_reject"), id("rejected")).as_default(),
        Edge::new(id("wait_reject"), id("expired")).with_label("timeout"),
        Edge::new(id("send_cancel"), id("cancelled")),
    ];
    ProcessVersion {
        name: "request_approval".to_owned(),
        app: "demo".to_owned(),
        title: None,
        version: 1,
        status: VersionStatus::Draft,
        nodes,
        edges,
        rules: vec![],
    }
}

/// The process around the requester half.
fn requester_process() -> Process {
    Process::new("request_approval", "demo", vec![requester_half()])
        .unwrap()
        .with_participants(Participants::new(
            vec![
                model_participant("requester", ParticipantKind::Tenant),
                model_participant("approver", ParticipantKind::External),
            ],
            "requester",
        ))
}

/// The codes a conformance check produced.
fn found(version: &ProcessVersion, role: &str) -> BTreeSet<&'static str> {
    conforms(version, &contract(), role)
        .into_iter()
        .map(|p| p.code)
        .collect()
}

#[test]
fn the_requester_half_conforms() {
    let version = requester_half();
    assert!(
        version.problems().is_empty(),
        "the half must be a valid process too: {:#?}",
        version.problems()
    );
    let problems = conforms(&version, &contract(), "requester");
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn the_whole_process_conforms_and_plays_its_role() {
    let problems = conforms_process(&requester_process(), &contract());
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn a_role_the_contract_does_not_name_is_reported() {
    let version = requester_half();
    assert!(found(&version, "supplier").contains(codes::CONFORMANCE_ROLE));
}

#[test]
fn a_contract_the_step_does_not_name_is_reported() {
    let mut version = requester_half();
    if let NodeKind::SendMessage(config) = &mut version.nodes[2].kind {
        config.contract = "other_contract".to_owned();
    }
    assert!(found(&version, "requester").contains(codes::CONFORMANCE_CONTRACT));
}

#[test]
fn a_message_the_contract_does_not_define_is_reported() {
    let mut version = requester_half();
    if let NodeKind::SendMessage(config) = &mut version.nodes[2].kind {
        config.message = "nope".to_owned();
    }
    assert!(found(&version, "requester").contains(codes::CONFORMANCE_MESSAGE));
}

#[test]
fn a_step_in_the_wrong_direction_is_reported() {
    let mut version = requester_half();
    // The requester sending `approve`, which it receives.
    if let NodeKind::SendMessage(config) = &mut version.nodes[2].kind {
        config.message = "approve".to_owned();
    }
    assert!(found(&version, "requester").contains(codes::CONFORMANCE_DIRECTION));
}

#[test]
fn steps_outside_the_permitted_order_are_reported() {
    let mut version = requester_half();
    // Send the cancellation before the request: the contract allows `cancel`
    // only after `request`.
    version.edges = vec![
        Edge::new(id("start"), id("enter")),
        Edge::new(id("enter"), id("send_cancel")),
        Edge::new(id("send_cancel"), id("send_request")),
        Edge::new(id("send_request"), id("choose")),
        Edge::new(id("choose"), id("wait_approve")).with_condition("doc.wait == true"),
        Edge::new(id("choose"), id("send_cancel")).as_default(),
        Edge::new(id("wait_approve"), id("approved")).as_default(),
        Edge::new(id("wait_approve"), id("wait_reject")).with_label("timeout"),
        Edge::new(id("wait_reject"), id("rejected")).as_default(),
        Edge::new(id("wait_reject"), id("expired")).with_label("timeout"),
    ];
    assert!(found(&version, "requester").contains(codes::CONFORMANCE_ORDER));
}

#[test]
fn an_obligation_the_half_never_reaches_is_reported() {
    let mut version = requester_half();
    // Both waits now wait for `approve`, so nothing waits for `reject`.
    if let NodeKind::WaitMessage(config) = &mut version.nodes[5].kind {
        config.message = "approve".to_owned();
    }
    assert!(found(&version, "requester").contains(codes::CONFORMANCE_OBLIGATION));
}

#[test]
fn a_wait_without_a_limit_or_a_timeout_branch_is_reported() {
    let mut version = requester_half();
    version.nodes[4].timeout_secs = None;
    assert!(found(&version, "requester").contains(codes::CONFORMANCE_TIMEOUT));

    let mut version = requester_half();
    // The contract allows 86 400s; the process may wait less, never more.
    version.nodes[4].timeout_secs = Some(86_401);
    assert!(found(&version, "requester").contains(codes::CONFORMANCE_TIMEOUT));

    let mut version = requester_half();
    // Drop the timeout branch: the timeout edge loses its label.
    for edge in &mut version.edges {
        if edge.from == id("wait_approve") && edge.to == id("wait_reject") {
            edge.label = None;
            edge.default = false;
        }
    }
    assert!(found(&version, "requester").contains(codes::CONFORMANCE_TIMEOUT));

    let mut version = requester_half();
    if let NodeKind::WaitMessage(config) = &mut version.nodes[4].kind {
        config.on_timeout = TimeoutAction::ErrorBranch;
    }
    assert!(found(&version, "requester").contains(codes::CONFORMANCE_TIMEOUT));
}

#[test]
fn participants_that_do_not_match_the_contract_are_reported() {
    let process = Process::new("request_approval", "demo", vec![requester_half()]).unwrap();
    let problems: BTreeSet<&str> = conforms_process(&process, &contract())
        .into_iter()
        .map(|p| p.code)
        .collect();
    assert!(problems.contains(codes::CONFORMANCE_PARTICIPANTS));

    let wrong_kind = Process::new("request_approval", "demo", vec![requester_half()])
        .unwrap()
        .with_participants(Participants::new(
            vec![
                model_participant("requester", ParticipantKind::Tenant),
                model_participant("approver", ParticipantKind::Tenant),
            ],
            "requester",
        ));
    let problems: BTreeSet<&str> = conforms_process(&wrong_kind, &contract())
        .into_iter()
        .map(|p| p.code)
        .collect();
    assert!(problems.contains(codes::CONFORMANCE_PARTICIPANTS));

    let missing_role = Process::new("request_approval", "demo", vec![requester_half()])
        .unwrap()
        .with_participants(Participants::new(
            vec![model_participant("requester", ParticipantKind::Tenant)],
            "requester",
        ));
    let problems: BTreeSet<&str> = conforms_process(&missing_role, &contract())
        .into_iter()
        .map(|p| p.code)
        .collect();
    assert!(problems.contains(codes::CONFORMANCE_PARTICIPANTS));
}

#[test]
fn the_conformance_check_reports_every_problem_at_once() {
    let mut version = requester_half();
    if let NodeKind::SendMessage(config) = &mut version.nodes[2].kind {
        config.contract = "other".to_owned();
        config.message = "nope".to_owned();
    }
    version.nodes[4].timeout_secs = None;
    let problems: BTreeSet<&str> = conforms(&version, &contract(), "requester")
        .into_iter()
        .map(|p| p.code)
        .collect();
    assert!(
        problems.contains(codes::CONFORMANCE_CONTRACT),
        "{problems:?}"
    );
    assert!(
        problems.contains(codes::CONFORMANCE_MESSAGE),
        "{problems:?}"
    );
    assert!(
        problems.contains(codes::CONFORMANCE_TIMEOUT),
        "{problems:?}"
    );
}

#[test]
fn an_incomplete_form_step_is_reported() {
    let mut version = requester_half();
    if let NodeKind::Form(config) = &mut version.nodes[1].kind {
        config.fields.clear();
        config.assignee = None;
        config.record_type = None;
    }
    let problems: BTreeSet<&str> = conforms(&version, &contract(), "requester")
        .into_iter()
        .map(|p| p.code)
        .collect();
    assert!(problems.contains(codes::CONFORMANCE_FORM), "{problems:?}");
}

/// A process-model participant for a contract role, so the test builders keep
/// one role list and no parallel types.
fn model_participant(role: &str, kind: ParticipantKind) -> spot_bpm_model::Participant {
    let kind = match kind {
        ParticipantKind::Tenant => spot_bpm_model::ParticipantKind::Tenant,
        ParticipantKind::External => spot_bpm_model::ParticipantKind::External,
    };
    spot_bpm_model::Participant::new(role, kind)
}

/// A version of `diamonds` chained branch/join diamonds, each with a send on
/// one arm: the number of simple paths is 2^diamonds, the number of
/// (node, position) states is linear.
fn chained_diamonds(diamonds: usize) -> ProcessVersion {
    let plain = |name: &str| Node::new(id(name), NodeKind::Logic(Default::default()));
    let mut nodes = vec![plain("join0")];
    let mut edges = Vec::new();
    for i in 0..diamonds {
        let (left, right, next) = (format!("l{i}"), format!("r{i}"), format!("join{}", i + 1));
        nodes.push(plain(&left));
        nodes.push(Node::new(
            id(&right),
            NodeKind::SendMessage(SendMessageConfig {
                contract: "request_approve".to_owned(),
                message: "request".to_owned(),
                mapping: None,
                on_reject: RejectAction::Continue,
            }),
        ));
        nodes.push(plain(&next));
        for arm in [&left, &right] {
            edges.push(Edge::new(id(&format!("join{i}")), id(arm)));
            edges.push(Edge::new(id(arm), id(&next)));
        }
    }
    let mut version = requester_half();
    version.nodes = nodes;
    version.edges = edges;
    version
}

/// Runs a conformance check on its own thread and fails the test when it does
/// not finish in time, instead of hanging the whole run.
fn finishes_within(seconds: u64, version: ProcessVersion) -> Vec<spot_bpm_contract::Problem> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(conforms(&version, &contract(), "requester"));
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(seconds))
        .expect("conformance must finish in polynomial time")
}

#[test]
fn a_long_chain_of_diamonds_is_checked_in_polynomial_time() {
    // 2^30 simple paths: enumerating them would hang the publish gate.
    let problems = finishes_within(20, chained_diamonds(30));
    assert!(
        problems.iter().all(|p| p.code != "bpm_contract.too_large"),
        "{problems:#?}"
    );
}

#[test]
fn the_order_check_still_sees_a_bad_step_on_one_arm_of_many_diamonds() {
    // The send of `request` repeats on the right arms: after the first one the
    // contract allows no second `request`, so the order problem must still be
    // found when the paths are not enumerated.
    let problems = finishes_within(20, chained_diamonds(30));
    assert!(
        problems.iter().any(|p| p.code == codes::CONFORMANCE_ORDER),
        "{problems:#?}"
    );
}
