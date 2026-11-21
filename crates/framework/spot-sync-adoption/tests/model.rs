//! Exhaustive model checking of the exchange.
//!
//! The world: one record, one creating node, the hub, an ordered node-to-hub stream, a
//! hub-to-node request channel and a network that can be cut and healed. Delivery in each
//! direction keeps order; a cut only delays delivery (the stream resumes when it heals). The
//! explorer visits every interleaving of up to four node edits, the hub's request (by arrival,
//! process step or administrator), the node applying the request, the acknowledgement, a
//! duplicate delivery and the cut/heal, and checks the safety properties in every reachable
//! state.

use std::collections::{HashSet, VecDeque};

use spot_sync_adoption::{
    HubInput, HubOutput, HubSide, NodeInput, NodeOutput, NodeSide, RecordState, Version,
};
use spot_sync_policy::preset::AdoptMode;

/// A message on the node-to-hub stream, in the node's write order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Up {
    Version(Version),
    Ack(Version),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct World {
    node: NodeSide,
    hub: HubSide,
    /// Everything the node has put on its stream, in order, and how much the hub consumed.
    up: Vec<Up>,
    delivered: usize,
    /// The request on its way to the node, if any, and whether the node applied it.
    request: Option<Version>,
    request_applied: bool,
    cut: bool,
    cut_used: bool,
    edits: u8,
    trigger_used: bool,
    dup_used: bool,
    all_versions_written: Version,
}

const MAX_EDITS: u8 = 4;

fn start(mode: AdoptMode) -> World {
    World {
        node: NodeSide::new(1),
        hub: HubSide::new(mode),
        up: vec![Up::Version(1)],
        delivered: 0,
        request: None,
        request_applied: false,
        cut: false,
        cut_used: false,
        edits: 0,
        trigger_used: false,
        dup_used: false,
        all_versions_written: 1,
    }
}

fn apply_hub(w: &mut World, input: HubInput) {
    let (h, out) = w.hub.step(input);
    w.hub = h;
    for o in out {
        match o {
            HubOutput::WriteRequest { seen_version } => {
                assert!(w.request.is_none(), "a second request was issued");
                w.request = Some(seen_version);
            }
            HubOutput::Rejected(e) => panic!("the hub rejected an in-order input {input:?}: {e}"),
            _ => {}
        }
    }
}

fn apply_node(w: &mut World, input: NodeInput) {
    let (n, out) = w.node.step(input);
    w.node = n;
    for o in out {
        match o {
            NodeOutput::EditWritten { version } => {
                w.up.push(Up::Version(version));
                w.all_versions_written = version;
            }
            NodeOutput::WriteAck { final_version } => w.up.push(Up::Ack(final_version)),
            NodeOutput::StopOwning | NodeOutput::Rejected(_) => {}
        }
    }
}

fn successors(w: &World, mode: AdoptMode) -> Vec<World> {
    let mut next = Vec::new();
    // A node edit (refused after the node stopped owning, so it is only offered while owner;
    // the refusal itself is covered by the unit tests).
    if w.edits < MAX_EDITS && w.node.owns() {
        let mut n = w.clone();
        n.edits += 1;
        apply_node(&mut n, NodeInput::LocalEdit);
        next.push(n);
    }
    // The hub consumes the next message of the stream.
    if !w.cut && w.delivered < w.up.len() {
        let mut n = w.clone();
        let msg = n.up[n.delivered];
        n.delivered += 1;
        match msg {
            Up::Version(version) => apply_hub(&mut n, HubInput::RecordArrived { version }),
            Up::Ack(final_version) => apply_hub(&mut n, HubInput::AckReceived { final_version }),
        }
        next.push(n);
    }
    // A duplicate delivery of the message just consumed (the stream may re-deliver).
    if !w.cut && w.delivered > 0 && !w.dup_used {
        let mut n = w.clone();
        n.dup_used = true;
        match n.up[n.delivered - 1] {
            Up::Version(version) => apply_hub(&mut n, HubInput::RecordArrived { version }),
            Up::Ack(final_version) => apply_hub(&mut n, HubInput::AckReceived { final_version }),
        }
        next.push(n);
    }
    // A manual trigger (process step, once the hub has seen the record).
    if mode == AdoptMode::Manual && !w.trigger_used && w.hub.seen_version().is_some() {
        let mut n = w.clone();
        n.trigger_used = true;
        apply_hub(&mut n, HubInput::ProcessStepFired);
        next.push(n);
    }
    // The node applies the request (needs the network).
    if let Some(seen) = w.request
        && !w.cut
        && !w.request_applied
    {
        let mut n = w.clone();
        n.request_applied = true;
        apply_node(&mut n, NodeInput::RequestApplied { seen_version: seen });
        next.push(n);
    }
    // The network is cut once, and heals.
    if !w.cut && !w.cut_used {
        let mut n = w.clone();
        n.cut = true;
        n.cut_used = true;
        apply_hub(&mut n, HubInput::NodeOffline);
        next.push(n);
    }
    if w.cut {
        let mut n = w.clone();
        n.cut = false;
        next.push(n);
    }
    next
}

fn check(w: &World) {
    let node_owns = w.node.owns();
    let hub_owns = w.hub.owns();
    // Never two owners.
    assert!(!(node_owns && hub_owns), "two owners: {w:?}");
    // Never none, except between the node stopping and the hub taking ownership; and a hub
    // that has not requested never leaves the node without ownership.
    if !node_owns && !hub_owns {
        assert!(
            matches!(w.hub.state(), RecordState::AdoptionPending { .. }),
            "no owner outside the exchange: {w:?}"
        );
        assert!(
            w.request_applied,
            "the node stopped owning without a request: {w:?}"
        );
    }
    // When the hub takes ownership it holds every version the node ever wrote.
    if hub_owns {
        assert_eq!(
            w.hub.seen_version(),
            Some(w.all_versions_written),
            "a node version was lost: {w:?}"
        );
        assert!(!node_owns);
        // The node cannot have written anything after the final version it acknowledged.
        assert_eq!(w.node.version(), w.all_versions_written);
    }
    // The node stops owning exactly when it applied the request.
    assert_eq!(
        node_owns, !w.request_applied,
        "ownership/application mismatch: {w:?}"
    );
    // The hub's seen version is a prefix of what the node wrote.
    if let Some(s) = w.hub.seen_version() {
        assert!(s <= w.all_versions_written);
    }
    // `never` never requests.
    if w.hub.mode() == AdoptMode::Never {
        assert!(w.request.is_none());
    }
}

fn explore(mode: AdoptMode) -> (usize, bool, bool) {
    let mut seen: HashSet<World> = HashSet::new();
    let mut queue = VecDeque::new();
    let s = start(mode);
    seen.insert(s.clone());
    queue.push_back(s);
    let mut adopted_reached = false;
    let mut stuck_pending_reached = false;
    while let Some(w) = queue.pop_front() {
        check(&w);
        if w.hub.owns() {
            adopted_reached = true;
        }
        let succ = successors(&w, mode);
        // A state with nothing left to do that is still pending is a livelock only if the
        // request was never applied; a cut that never heals is excluded by the heal move.
        if succ.is_empty() && matches!(w.hub.state(), RecordState::AdoptionPending { .. }) {
            stuck_pending_reached = true;
        }
        for n in succ {
            if seen.insert(n.clone()) {
                queue.push_back(n);
            }
        }
    }
    (seen.len(), adopted_reached, stuck_pending_reached)
}

#[test]
fn auto_loses_no_version_and_never_has_two_owners() {
    let (states, adopted, stuck) = explore(AdoptMode::Auto);
    assert!(states > 100, "the explorer visited only {states} states");
    assert!(adopted, "no interleaving completed the adoption");
    assert!(
        !stuck,
        "an interleaving ended with the adoption still pending"
    );
}

#[test]
fn manual_with_a_process_step_loses_no_version_and_never_has_two_owners() {
    let (states, adopted, stuck) = explore(AdoptMode::Manual);
    assert!(states > 100, "the explorer visited only {states} states");
    assert!(adopted);
    // Manual may end without the step ever firing, which is `Provisional`, not pending.
    assert!(!stuck);
}

#[test]
fn never_requests_nothing_and_the_node_keeps_the_record() {
    let (_, adopted, _) = explore(AdoptMode::Never);
    assert!(!adopted);
}
