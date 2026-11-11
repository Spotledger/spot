#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::topology::Profile;

fn node(host: &str, roles: &[Role]) -> NodeSpec {
    NodeSpec::new(host, roles).unwrap()
}

fn reference() -> Topology {
    Topology::new(
        vec![
            node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
            node("db1.example.com", &[Role::Database]),
            node("db2.example.com", &[Role::Database]),
        ],
        Profile::ClusterBasic,
    )
    .with_web_address("erp.example.com")
}

#[test]
fn a_reference_plan_has_every_phase_in_order() {
    let plan = plan(&reference()).unwrap();
    assert!(plan.validate().is_ok());
    for (position, step) in plan.steps().iter().enumerate() {
        assert_eq!(step.index, position);
    }
    // Every node is enrolled and hardened.
    for host in ["app1.example.com", "db1.example.com", "db2.example.com"] {
        assert!(
            plan.steps()
                .iter()
                .any(|step| step.kind == StepKind::EnrolAgent
                    && step.host.as_ref().map(Host::as_str) == Some(host))
        );
        assert!(
            plan.steps()
                .iter()
                .any(|step| step.kind == StepKind::HardenBaseline
                    && step.host.as_ref().map(Host::as_str) == Some(host))
        );
    }
    // The replica comes after the primary database exists.
    let init = plan
        .steps()
        .iter()
        .find(|step| step.kind == StepKind::InitialiseDatabase)
        .unwrap();
    let replica = plan
        .steps()
        .iter()
        .find(|step| step.kind == StepKind::CreateReplica)
        .unwrap();
    assert!(replica.depends_on.contains(&init.index));
    // Verification is the last step on a node.
    let verify = plan
        .steps()
        .iter()
        .find(|step| step.kind == StepKind::VerifyHealth)
        .unwrap();
    assert!(verify.depends_on.iter().all(|dep| dep < &verify.index));
}

#[test]
fn the_plan_is_independent_of_node_order() {
    let topology = reference();
    let mut reversed = topology.clone();
    reversed.nodes.reverse();
    let first = plan(&topology).unwrap();
    let second = plan(&reversed).unwrap();
    assert_eq!(first.id(), second.id());
}

#[test]
fn the_plan_changes_when_the_topology_does() {
    let one = plan(&reference()).unwrap();
    let mut changed = reference();
    changed.nodes[1].primary = true;
    changed.nodes[2].primary = false;
    let two = plan(&changed).unwrap();
    assert_ne!(one.id(), two.id());
}

#[test]
fn an_invalid_topology_is_refused() {
    let invalid = Topology::new(
        vec![node("app1.example.com", &[Role::Web])],
        Profile::ClusterBasic,
    );
    let error = plan(&invalid).unwrap_err();
    assert_eq!(error.code(), crate::error::TOPOLOGY_INVALID);
}

#[test]
fn every_step_names_a_catalogued_operation() {
    use spot_mgmt_ops::{CatalogueBuilder, OperationModule};
    let plan = plan(&reference()).unwrap();
    let mut builder = CatalogueBuilder::new();
    crate::catalogue::WizardCatalogue.register(&mut builder);
    spot_agent_runtime::RuntimeCatalogue.register(&mut builder);
    let catalogue = builder.build().unwrap();
    plan.validate_catalogue(&catalogue).unwrap();
}

#[test]
fn every_non_idempotent_step_has_a_rollback_and_notes_are_never_empty() {
    for topology in [reference(), standalone()] {
        let plan = plan(&topology).unwrap();
        for step in plan.steps() {
            if !step.idempotent {
                assert!(step.rollback.is_some(), "step {} has none", step.index);
            }
            if let Some(Rollback::Note(note)) = &step.rollback {
                assert!(!note.trim().is_empty());
            }
        }
    }
}

fn standalone() -> Topology {
    Topology::new(
        vec![node(
            "all1.example.com",
            &[Role::Web, Role::Jobs, Role::Realtime, Role::Database],
        )],
        Profile::Standalone,
    )
    .with_web_address("erp.example.com")
}

#[test]
fn standalone_single_node_has_no_replica() {
    let plan = plan(&standalone()).unwrap();
    assert!(
        !plan
            .steps()
            .iter()
            .any(|step| step.kind == StepKind::CreateReplica)
    );
    assert!(
        plan.steps()
            .iter()
            .any(|step| step.kind == StepKind::InitialiseDatabase)
    );
}

#[test]
fn a_branch_edge_gets_its_own_plan_node_but_no_database() {
    let topology = Topology::new(
        vec![
            node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
            node("db1.example.com", &[Role::Database]),
            node("edge1.example.com", &[Role::BranchEdge]),
        ],
        Profile::ClusterBasic,
    )
    .with_web_address("erp.example.com");
    let plan = plan(&topology).unwrap();
    assert!(
        plan.steps()
            .iter()
            .any(|step| step.kind == StepKind::ConfigureRoles
                && step.host.as_ref().map(Host::as_str) == Some("edge1.example.com"))
    );
    assert!(
        !plan
            .steps()
            .iter()
            .any(|step| step.kind == StepKind::InitialiseDatabase
                && step.host.as_ref().map(Host::as_str) == Some("edge1.example.com"))
    );
}

#[test]
fn a_tampered_plan_is_refused() {
    let mut plan = plan(&reference()).unwrap();
    plan.steps[0].depends_on = vec![1];
    let error = plan.validate().unwrap_err();
    assert_eq!(error.code(), crate::error::PLAN_INVALID);
    assert!(format!("{error}").contains("does not run before it"));
}

#[test]
fn a_missing_rollback_is_refused() {
    let mut plan = plan(&reference()).unwrap();
    let index = plan
        .steps()
        .iter()
        .position(|step| !step.idempotent)
        .unwrap();
    plan.steps[index].rollback = None;
    let error = plan.validate().unwrap_err();
    assert!(format!("{error}").contains("no rollback"));
}

#[test]
fn removing_the_hardening_step_is_refused() {
    let mut plan = plan(&reference()).unwrap();
    let index = plan
        .steps()
        .iter()
        .position(|step| step.kind == StepKind::HardenBaseline)
        .unwrap();
    plan.steps.remove(index);
    for (position, step) in plan.steps.iter_mut().enumerate() {
        step.index = position;
    }
    let error = plan.validate().unwrap_err();
    assert!(format!("{error}").contains("hardening"));
}

#[test]
fn ready_steps_are_those_whose_dependencies_ran() {
    let plan = plan(&reference()).unwrap();
    let done = BTreeSet::new();
    let ready = plan.ready(&done);
    assert_eq!(ready.len(), 3, "the three enrolments are independent");
    let mut done = BTreeSet::from([0, 1, 2]);
    let ready = plan.ready(&done);
    assert_eq!(ready.len(), 3, "the three hardening steps are independent");
    done.extend(ready);
    assert!(plan.ready(&done).iter().all(|index| {
        plan.step(*index)
            .unwrap()
            .depends_on
            .iter()
            .all(|dep| done.contains(dep))
    }));
}

#[test]
fn a_plan_round_trips_through_json() {
    let plan = plan(&reference()).unwrap();
    let json = serde_json::to_string(&plan).unwrap();
    assert_eq!(serde_json::from_str::<DeployPlan>(&json).unwrap(), plan);
}

#[test]
fn role_ports_match_the_preflight_table() {
    for role in Role::ALL {
        assert_eq!(role_ports(role), crate::preflight::required_ports(role));
    }
}
