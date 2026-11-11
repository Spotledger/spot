//! Unit behaviour of the two machines.

use spot_sync_adoption::{
    AdoptionError, HubInput, HubOutput, HubSide, NodeInput, NodeOutput, NodeSide, Ownership,
    RecordState,
};
use spot_sync_policy::preset::AdoptMode;

fn hub(mode: AdoptMode, inputs: &[HubInput]) -> (HubSide, Vec<HubOutput>) {
    let mut h = HubSide::new(mode);
    let mut all = Vec::new();
    for i in inputs {
        let (n, o) = h.step(*i);
        h = n;
        all.extend(o);
    }
    (h, all)
}

const ARRIVE_1: HubInput = HubInput::RecordArrived { version: 1 };

#[test]
fn auto_requests_on_arrival_once() {
    let (h, out) = hub(AdoptMode::Auto, &[ARRIVE_1, ARRIVE_1, ARRIVE_1]);
    assert_eq!(out, vec![HubOutput::WriteRequest { seen_version: 1 }]);
    assert_eq!(
        h.state(),
        RecordState::AdoptionPending {
            requested_version: 1
        }
    );
}

#[test]
fn later_versions_do_not_issue_a_second_request() {
    let (h, out) = hub(
        AdoptMode::Auto,
        &[
            ARRIVE_1,
            HubInput::RecordArrived { version: 3 },
            HubInput::ProcessStepFired,
        ],
    );
    assert_eq!(out.len(), 1);
    assert_eq!(h.seen_version(), Some(3));
}

#[test]
fn manual_waits_for_the_process_step_or_an_administrator() {
    let (h, out) = hub(AdoptMode::Manual, &[ARRIVE_1]);
    assert!(out.is_empty());
    assert_eq!(h.state(), RecordState::Provisional);
    let (_, out) = h.step(HubInput::ProcessStepFired);
    assert_eq!(out, vec![HubOutput::WriteRequest { seen_version: 1 }]);
    let (_, out) = h.step(HubInput::AdminAdopt);
    assert_eq!(out, vec![HubOutput::WriteRequest { seen_version: 1 }]);
}

#[test]
fn a_trigger_before_the_record_arrived_is_refused() {
    let (h, out) = hub(AdoptMode::Manual, &[HubInput::ProcessStepFired]);
    assert_eq!(
        out,
        vec![HubOutput::Rejected(AdoptionError::RecordNotArrived)]
    );
    assert_eq!(h.state(), RecordState::Provisional);
}

#[test]
fn never_never_requests() {
    let (h, out) = hub(
        AdoptMode::Never,
        &[ARRIVE_1, HubInput::ProcessStepFired, HubInput::AdminAdopt],
    );
    assert!(out.is_empty());
    assert_eq!(h.state(), RecordState::NeverAdopted);
    let (_, out) = h.step(HubInput::AckReceived { final_version: 1 });
    assert_eq!(out, vec![HubOutput::Rejected(AdoptionError::NoRequest)]);
}

#[test]
fn changing_the_setting_to_auto_requests_for_a_record_already_seen() {
    let (h, _) = hub(AdoptMode::Manual, &[ARRIVE_1]);
    let (_, out) = h.step(HubInput::AdoptMode(AdoptMode::Auto));
    assert_eq!(out, vec![HubOutput::WriteRequest { seen_version: 1 }]);
}

#[test]
fn offline_node_leaves_it_pending_with_the_condition_and_edits_still_flow() {
    let (h, out) = hub(
        AdoptMode::Auto,
        &[ARRIVE_1, HubInput::NodeOffline, HubInput::NodeOffline],
    );
    assert_eq!(
        out,
        vec![
            HubOutput::WriteRequest { seen_version: 1 },
            HubOutput::RaiseCondition
        ]
    );
    assert!(h.condition_raised());
    // An edit made at the node still applies at the hub while pending.
    let (h, out) = h.step(HubInput::RecordArrived { version: 2 });
    assert!(out.is_empty());
    assert_eq!(h.seen_version(), Some(2));
    assert!(!h.owns());
    // The condition clears with the adoption.
    let (h, out) = h.step(HubInput::AckReceived { final_version: 2 });
    assert_eq!(
        out,
        vec![
            HubOutput::TakeOwnership,
            HubOutput::WriteAdopted { version: 3 },
            HubOutput::ClearCondition
        ]
    );
    assert!(h.owns() && !h.condition_raised());
}

#[test]
fn offline_before_the_request_raises_the_condition_with_the_request() {
    let (_, out) = hub(AdoptMode::Auto, &[HubInput::NodeOffline, ARRIVE_1]);
    assert_eq!(
        out,
        vec![
            HubOutput::WriteRequest { seen_version: 1 },
            HubOutput::RaiseCondition
        ]
    );
}

#[test]
fn duplicate_ack_is_idempotent_and_a_different_one_is_refused() {
    let (h, _) = hub(
        AdoptMode::Auto,
        &[ARRIVE_1, HubInput::AckReceived { final_version: 1 }],
    );
    assert!(h.owns());
    let (h2, out) = h.step(HubInput::AckReceived { final_version: 1 });
    assert!(out.is_empty());
    assert_eq!(h2, h);
    let (_, out) = h.step(HubInput::AckReceived { final_version: 2 });
    assert_eq!(out, vec![HubOutput::Rejected(AdoptionError::AckMismatch)]);
    // A late re-delivery of a version after adoption changes nothing.
    let (h3, out) = h.step(HubInput::RecordArrived { version: 1 });
    assert!(out.is_empty());
    assert_eq!(h3, h);
}

#[test]
fn stale_ack_is_rejected_and_the_hub_stays_pending() {
    let (h, _) = hub(
        AdoptMode::Auto,
        &[ARRIVE_1, HubInput::RecordArrived { version: 3 }],
    );
    let (h2, out) = h.step(HubInput::AckReceived { final_version: 2 });
    assert_eq!(
        out,
        vec![HubOutput::Rejected(AdoptionError::StaleAck {
            final_version: 2,
            seen_version: 3
        })]
    );
    assert_eq!(h2, h);
}

#[test]
fn an_ack_naming_versions_the_hub_lacks_is_rejected() {
    let (h, _) = hub(AdoptMode::Auto, &[ARRIVE_1]);
    let (h2, out) = h.step(HubInput::AckReceived { final_version: 4 });
    assert_eq!(
        out,
        vec![HubOutput::Rejected(AdoptionError::MissingVersions {
            final_version: 4,
            seen_version: 1
        })]
    );
    assert!(!h2.owns());
}

#[test]
fn ack_without_a_request_is_refused() {
    let (_, out) = hub(
        AdoptMode::Manual,
        &[ARRIVE_1, HubInput::AckReceived { final_version: 1 }],
    );
    assert_eq!(out, vec![HubOutput::Rejected(AdoptionError::NoRequest)]);
}

#[test]
fn version_counter_exhaustion_is_refused_not_wrapped() {
    let (h, _) = hub(
        AdoptMode::Auto,
        &[HubInput::RecordArrived { version: u64::MAX }],
    );
    let (h2, out) = h.step(HubInput::AckReceived {
        final_version: u64::MAX,
    });
    assert_eq!(
        out,
        vec![HubOutput::Rejected(AdoptionError::VersionExhausted)]
    );
    assert!(!h2.owns());
    let (n, out) = NodeSide::new(u64::MAX).step(NodeInput::LocalEdit);
    assert_eq!(
        out,
        vec![NodeOutput::Rejected(AdoptionError::VersionExhausted)]
    );
    assert_eq!(n.version(), u64::MAX);
}

#[test]
fn node_edits_before_the_request_flow_and_after_it_are_refused() {
    let n = NodeSide::new(1);
    let (n, out) = n.step(NodeInput::LocalEdit);
    assert_eq!(out, vec![NodeOutput::EditWritten { version: 2 }]);
    let (n, out) = n.step(NodeInput::RequestApplied { seen_version: 1 });
    assert_eq!(
        out,
        vec![
            NodeOutput::StopOwning,
            NodeOutput::WriteAck { final_version: 2 }
        ]
    );
    assert_eq!(n.ownership(), Ownership::Mirror);
    let (n2, out) = n.step(NodeInput::LocalEdit);
    assert_eq!(out, vec![NodeOutput::Rejected(AdoptionError::NotOwner)]);
    assert_eq!(n2, n);
    assert_eq!(AdoptionError::NotOwner.code(), "sync_adoption.not_owner");
    assert!(AdoptionError::NotOwner.to_string().contains("Head Office"));
}

#[test]
fn node_request_applied_twice_is_idempotent_and_tick_reoffers_the_ack_until_durable() {
    let n = NodeSide::new(1);
    let (n, _) = n.step(NodeInput::RequestApplied { seen_version: 1 });
    let (n, out) = n.step(NodeInput::RequestApplied { seen_version: 1 });
    assert!(out.is_empty());
    let (n, out) = n.step(NodeInput::Tick);
    assert_eq!(out, vec![NodeOutput::WriteAck { final_version: 1 }]);
    let (n, _) = n.step(NodeInput::AckDurable);
    let (_, out) = n.step(NodeInput::Tick);
    assert!(out.is_empty());
}

#[test]
fn a_request_beyond_the_nodes_versions_is_refused() {
    let n = NodeSide::new(2);
    let (n2, out) = n.step(NodeInput::RequestApplied { seen_version: 5 });
    assert_eq!(
        out,
        vec![NodeOutput::Rejected(AdoptionError::RequestAhead {
            seen_version: 5,
            node_version: 2
        })]
    );
    assert!(n2.owns());
}

#[test]
fn every_error_code_is_in_the_catalogue_form() {
    let errs = [
        AdoptionError::NotOwner,
        AdoptionError::StaleAck {
            final_version: 1,
            seen_version: 2,
        },
        AdoptionError::MissingVersions {
            final_version: 2,
            seen_version: 1,
        },
        AdoptionError::NoRequest,
        AdoptionError::AckMismatch,
        AdoptionError::RecordNotArrived,
        AdoptionError::RequestAhead {
            seen_version: 2,
            node_version: 1,
        },
        AdoptionError::VersionExhausted,
    ];
    for e in errs {
        assert!(e.code().starts_with("sync_adoption."));
        assert!(!e.to_string().is_empty());
    }
}
