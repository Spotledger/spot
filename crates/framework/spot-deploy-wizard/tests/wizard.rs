//! The engine's acceptance shape: a reference three-node cluster, its budget, and a resume
//! at every step index (WP-ADM-13.T1).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_deploy_wizard::{
    AgentFacts, Authorised, CommandWindow, DeployPlan, Journal, MemoryAudit, MemoryJournal,
    NodeSpec, OsFacts, OsFamily, PeerFacts, Preflight, Prepared, Profile, Role, RunEnv, RunReport,
    ServerFacts, SimulatedExecutor, Speed, Topology, WizardError, estimate, plan, prepare,
    progress,
};
use spot_mgmt_ops::{Catalogue, CatalogueBuilder, OperationModule};
use spot_signed_command::SoftwareSigner;

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

fn agent_facts(topology: &Topology) -> Vec<AgentFacts> {
    topology
        .nodes
        .iter()
        .map(|node| {
            let peers = topology
                .nodes
                .iter()
                .filter(|peer| peer.host != node.host)
                .map(|peer| PeerFacts {
                    host: peer.host.clone(),
                    reachable: true,
                })
                .collect();
            AgentFacts::new(
                node.host.clone(),
                OsFacts {
                    family: OsFamily::Linux,
                    release: "Ubuntu 24.04 LTS".to_owned(),
                    supported: true,
                },
            )
            .with_peers(peers)
        })
        .collect()
}

/// Facts observed just before the test clock starts.
fn facts(topology: &Topology) -> Vec<ServerFacts> {
    agent_facts(topology)
        .into_iter()
        .map(|facts| ServerFacts::for_testing(facts, 1_700_000_000))
        .collect()
}

fn run(
    prepared: &Prepared,
    journal: &mut MemoryJournal,
    executor: &mut SimulatedExecutor,
) -> Result<RunReport, WizardError> {
    let authorised = Authorised::for_testing("admin@example.com", prepared, i64::MAX);
    let mut audit = MemoryAudit::new();
    prepared.run(
        &authorised,
        &mut RunEnv {
            catalogue: &catalogue(),
            journal,
            executor,
            signer: &signer(),
            window: window(),
            clock: &mut clock(),
            audit: &mut audit,
        },
    )
}

fn catalogue() -> Catalogue {
    let mut builder = CatalogueBuilder::new();
    spot_deploy_wizard::WizardCatalogue.register(&mut builder);
    spot_agent_runtime::RuntimeCatalogue.register(&mut builder);
    builder.build().unwrap()
}

fn signer() -> SoftwareSigner {
    SoftwareSigner::from_seed(&[9_u8; 32]).unwrap()
}

fn clock() -> impl FnMut() -> i64 {
    let mut now = 1_700_000_000;
    move || {
        now += 5;
        now
    }
}

fn window() -> CommandWindow {
    CommandWindow::new(120).unwrap()
}

#[test]
fn the_reference_three_node_cluster_finishes_under_thirty_minutes() {
    let topology = reference();
    let prepared = prepare(&topology, &facts(&topology), &catalogue()).unwrap();
    let budget = estimate(prepared.plan(), Speed::Reference).unwrap();
    assert!(
        budget.total_secs < 30 * 60,
        "estimated {} seconds, over the 30-minute acceptance",
        budget.total_secs
    );
    assert!(
        budget.parallel(),
        "the three nodes run their phases in parallel"
    );
    assert!(budget.serial_secs > budget.total_secs);
}

#[test]
fn a_clean_run_finishes_and_reports_full_progress() {
    let topology = reference();
    let prepared = prepare(&topology, &facts(&topology), &catalogue()).unwrap();
    let plan = prepared.plan();
    let mut journal = MemoryJournal::new(plan.id().clone());
    let mut executor = SimulatedExecutor::new();
    let report = run(&prepared, &mut journal, &mut executor).unwrap();
    assert!(report.is_complete());
    assert!(report.failed.is_none());
    assert_eq!(executor.submitted().len(), plan.len());
    assert!(
        executor
            .submitted()
            .iter()
            .all(|command| command.signatures == 1)
    );

    let progress = progress(plan, &journal, Speed::Reference).unwrap();
    assert_eq!(progress.done_steps, plan.len());
    assert_eq!(progress.percent_bp, 10_000);
    assert_eq!(progress.remaining_secs, 0);
    assert_eq!(progress.current, None);
}

#[test]
fn a_failure_at_every_step_index_resumes_to_completion() {
    let topology = reference();
    let prepared = prepare(&topology, &facts(&topology), &catalogue()).unwrap();
    let plan: &DeployPlan = prepared.plan();
    for index in 0..plan.len() {
        let mut journal = MemoryJournal::new(plan.id().clone());
        let mut executor = SimulatedExecutor::new();
        executor.fail_once(index, "the node refused the step");

        let first = run(&prepared, &mut journal, &mut executor).unwrap();
        let failed = first
            .failed
            .as_ref()
            .unwrap_or_else(|| panic!("a failure at step {index} must stop the run"));
        assert_eq!(failed.step, index, "the run stops at the failing step");
        assert_eq!(failed.code, spot_deploy_wizard::error::STEP_FAILED);
        assert!(!first.is_complete());
        assert_eq!(
            failed.cleanup_required,
            !plan.step(index).unwrap().idempotent
        );

        if !plan.step(index).unwrap().idempotent {
            // A half-applied change is never repeated blindly: the operator's clean-up
            // comes first.
            let error = run(&prepared, &mut journal, &mut executor).unwrap_err();
            assert_eq!(error.code(), spot_deploy_wizard::error::JOURNAL_MISMATCH);
            journal.rolled_back(index, 1_700_000_900).unwrap();
        }
        let resumed = run(&prepared, &mut journal, &mut executor).unwrap();
        assert!(
            resumed.is_complete(),
            "a retry after a failure at step {index} completes the plan"
        );
        assert_eq!(resumed.done.len(), plan.len());
        let progress = progress(plan, &journal, Speed::Reference).unwrap();
        assert_eq!(progress.percent_bp, 10_000);
    }
}

#[test]
fn a_crash_after_starting_a_non_idempotent_step_needs_its_cleanup_first() {
    let topology = reference();
    let prepared = prepare(&topology, &facts(&topology), &catalogue()).unwrap();
    let plan = prepared.plan().clone();
    let index = plan
        .steps()
        .iter()
        .position(|step| !step.idempotent)
        .unwrap();
    let mut journal = MemoryJournal::new(plan.id().clone());
    journal.started(index, 1_000).unwrap();

    let mut executor = SimulatedExecutor::new();
    let error = run(&prepared, &mut journal, &mut executor).unwrap_err();
    assert_eq!(error.code(), spot_deploy_wizard::error::JOURNAL_MISMATCH);

    journal.rolled_back(index, 1_100).unwrap();
    let resumed = run(&prepared, &mut journal, &mut executor).unwrap();
    assert!(resumed.is_complete());
}

#[test]
fn blocking_preflight_stops_the_plan_before_it_starts() {
    let topology = reference();
    let mut facts = agent_facts(&topology);
    facts[0].time.synchronized = false;
    let signed: Vec<ServerFacts> = facts
        .iter()
        .cloned()
        .map(|facts| ServerFacts::for_testing(facts, 1_700_000_000))
        .collect();
    let error = prepare(&topology, &signed, &catalogue()).unwrap_err();
    assert_eq!(error.code(), spot_deploy_wizard::error::PREFLIGHT_BLOCKED);

    let report = Preflight::for_topology(&topology, &facts);
    assert!(!report.is_ready());
    assert!(report.blocking_count() >= 1);
}

#[test]
fn a_topology_that_names_a_branch_edge_alongside_web_is_refused() {
    let topology = Topology::new(
        vec![
            node("app1.example.com", &[Role::Web, Role::BranchEdge]),
            node("db1.example.com", &[Role::Database]),
        ],
        Profile::ClusterBasic,
    )
    .with_web_address("erp.example.com");
    let error = plan(&topology).unwrap_err();
    assert_eq!(error.code(), spot_deploy_wizard::error::TOPOLOGY_INVALID);

    let no_database = Topology::new(
        vec![node(
            "app1.example.com",
            &[Role::Web, Role::Jobs, Role::Realtime],
        )],
        Profile::Standalone,
    )
    .with_web_address("erp.example.com");
    let error = plan(&no_database).unwrap_err();
    assert_eq!(error.code(), spot_deploy_wizard::error::TOPOLOGY_INVALID);
}

#[test]
fn an_unknown_operation_in_a_narrow_catalogue_is_refused() {
    let topology = reference();
    let plan = plan(&topology).unwrap();
    let mut builder = CatalogueBuilder::new();
    spot_deploy_wizard::WizardCatalogue.register(&mut builder);
    let only_wizard = builder.build().unwrap();
    let error = plan.validate_catalogue(&only_wizard).unwrap_err();
    assert_eq!(error.code(), spot_deploy_wizard::error::PLAN_INVALID);
}

#[test]
fn errors_are_plain_language_and_never_carry_a_secret() {
    let error = WizardError::StepFailed {
        step: 3,
        title: "Install the runtime bundle on db1.example.com".to_owned(),
        detail: "the bundle signature is not trusted".to_owned(),
    };
    let text = format!("{error}");
    assert!(text.contains("step 3"));
    assert!(!text.contains("password"));
    assert!(error.code().starts_with("deploy_wizard."));
}

#[test]
fn a_journal_is_persistable_as_json() {
    let plan = plan(&reference()).unwrap();
    let mut journal = MemoryJournal::new(plan.id().clone());
    journal.started(0, 1_700_000_000).unwrap();
    journal.failed(0, 1_700_000_010, "refused").unwrap();
    let entries = journal.entries().to_vec();
    let json = serde_json::to_string(&entries).unwrap();
    let back: Vec<spot_deploy_wizard::JournalEntry> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, entries);
}
