//! Recorded security findings: a plan runs only for an authenticated, stepped-up administrator with
//! an approval of that exact plan, every step is audited, and the facts behind the plan are
//! signed by the enrolled agent and fresh.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use spot_deploy_wizard::error::{
    APPROVAL_EXPIRED, APPROVAL_MISMATCH, APPROVAL_MISSING, APPROVAL_SELF, AUDIT_FAILED,
    AUTHORISATION_EXPIRED, FACTS_REPLAYED, FACTS_STALE, FACTS_UNTRUSTED, FACTS_WRONG_AGENT,
    FORBIDDEN, STEP_UP_REQUIRED, UNAUTHENTICATED,
};
use spot_deploy_wizard::{
    APPROVAL_WINDOW_SECS, AdminIdentity, AgentFacts, Approval, AuditOutcome, Authorised,
    CommandWindow, DEPLOY_PERMISSION, EnrolledAgents, FactsPolicy, Host, Journal, MemoryAudit,
    MemoryFactsLedger, MemoryJournal, NodeSpec, OsFacts, OsFamily, PeerFacts, Prepared, Profile,
    Role, RunEnv, RunReport, STEP_UP_WINDOW_SECS, ServerFacts, SignedFacts, SimulatedExecutor,
    StepUp, Topology, WizardError, authorise, prepare,
};
use spot_mgmt_ops::{Catalogue, CatalogueBuilder, OperationModule};
use spot_signed_command::{Signer, SoftwareSigner};

const NOW: i64 = 1_700_000_000;

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

fn other_topology() -> Topology {
    Topology::new(
        vec![
            node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
            node("db1.example.com", &[Role::Database]),
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

fn catalogue() -> Catalogue {
    let mut builder = CatalogueBuilder::new();
    spot_deploy_wizard::WizardCatalogue.register(&mut builder);
    spot_agent_runtime::RuntimeCatalogue.register(&mut builder);
    builder.build().unwrap()
}

fn prepared_at(topology: &Topology, observed_at: i64) -> Prepared {
    let facts: Vec<ServerFacts> = agent_facts(topology)
        .into_iter()
        .map(|facts| ServerFacts::for_testing(facts, observed_at))
        .collect();
    prepare(topology, &facts, &catalogue()).unwrap()
}

fn prepared() -> Prepared {
    prepared_at(&reference(), NOW - 10)
}

fn admin() -> AdminIdentity {
    AdminIdentity::new("alice@example.com", &[DEPLOY_PERMISSION], NOW + 3_600)
}

fn approval_for(prepared: &Prepared) -> Approval {
    Approval::new(
        prepared.plan().id().clone(),
        prepared.plan_hash().clone(),
        "bob@example.com",
        NOW - 60,
    )
}

fn step_up() -> StepUp {
    StepUp::new("alice@example.com", NOW - 30)
}

fn token(prepared: &Prepared) -> Authorised {
    authorise(
        prepared,
        Some(&admin()),
        Some(&step_up()),
        Some(&approval_for(prepared)),
        NOW,
    )
    .unwrap()
}

struct Fixture {
    journal: MemoryJournal,
    executor: SimulatedExecutor,
    audit: MemoryAudit,
}

impl Fixture {
    fn new(prepared: &Prepared) -> Self {
        Self {
            journal: MemoryJournal::new(prepared.plan().id().clone()),
            executor: SimulatedExecutor::new(),
            audit: MemoryAudit::new(),
        }
    }

    fn run(
        &mut self,
        prepared: &Prepared,
        authorised: &Authorised,
        start: i64,
    ) -> Result<RunReport, WizardError> {
        let mut now = start;
        let mut clock = move || {
            now += 1;
            now
        };
        let signer = SoftwareSigner::from_seed(&[9_u8; 32]).unwrap();
        prepared.run(
            authorised,
            &mut RunEnv {
                catalogue: &catalogue(),
                journal: &mut self.journal,
                executor: &mut self.executor,
                signer: &signer,
                window: CommandWindow::new(120).unwrap(),
                clock: &mut clock,
                audit: &mut self.audit,
            },
        )
    }

    fn nothing_happened(&self) -> bool {
        self.executor.submitted().is_empty() && self.audit.entries().is_empty()
    }
}

fn code(result: Result<impl std::fmt::Debug, WizardError>) -> &'static str {
    result.unwrap_err().code()
}

// ---------------------------------------------------------------------------------------
// Recorded security finding: authorisation
// ---------------------------------------------------------------------------------------

#[test]
fn an_authorised_run_completes_and_audits_every_step_before_and_after() {
    let prepared = prepared();
    let authorised = token(&prepared);
    let mut fixture = Fixture::new(&prepared);
    let report = fixture.run(&prepared, &authorised, NOW).unwrap();
    assert!(report.is_complete());

    let entries = fixture.audit.entries();
    assert_eq!(entries.len(), prepared.plan().len() * 2);
    for step in prepared.plan().steps() {
        let mine: Vec<_> = entries.iter().filter(|e| e.step == step.index).collect();
        assert_eq!(mine.len(), 2, "step {}", step.index);
        assert_eq!(mine[0].outcome, AuditOutcome::Started);
        assert_eq!(mine[1].outcome, AuditOutcome::Succeeded);
        assert!(mine[0].at <= mine[1].at);
        for entry in mine {
            assert_eq!(entry.actor, "alice@example.com");
            assert_eq!(entry.plan_id, prepared.plan().id().as_str());
            assert_eq!(entry.step_id, step.operation.id.as_str());
        }
    }
}

#[test]
fn a_run_without_an_identity_is_refused_and_executes_nothing() {
    let prepared = prepared();
    let refused = authorise(
        &prepared,
        None,
        Some(&step_up()),
        Some(&approval_for(&prepared)),
        NOW,
    );
    assert_eq!(code(refused), UNAUTHENTICATED);
    let anonymous = AdminIdentity::new("  ", &[DEPLOY_PERMISSION], NOW + 100);
    let refused = authorise(
        &prepared,
        Some(&anonymous),
        Some(&step_up()),
        Some(&approval_for(&prepared)),
        NOW,
    );
    assert_eq!(code(refused), UNAUTHENTICATED);
    let ended = AdminIdentity::new("alice@example.com", &[DEPLOY_PERMISSION], NOW);
    let refused = authorise(
        &prepared,
        Some(&ended),
        Some(&step_up()),
        Some(&approval_for(&prepared)),
        NOW,
    );
    assert_eq!(code(refused), UNAUTHENTICATED);
    assert!(Fixture::new(&prepared).nothing_happened());
}

#[test]
fn an_administrator_without_the_permission_is_forbidden() {
    let prepared = prepared();
    let reader = AdminIdentity::new("alice@example.com", &["deploy.view"], NOW + 100);
    let refused = authorise(
        &prepared,
        Some(&reader),
        Some(&step_up()),
        Some(&approval_for(&prepared)),
        NOW,
    );
    assert_eq!(code(refused), FORBIDDEN);
}

#[test]
fn a_missing_stale_future_or_foreign_step_up_is_refused() {
    let prepared = prepared();
    let approval = approval_for(&prepared);
    let try_with = |step_up: Option<&StepUp>| {
        authorise(&prepared, Some(&admin()), step_up, Some(&approval), NOW)
    };
    assert_eq!(code(try_with(None)), STEP_UP_REQUIRED);
    let stale = StepUp::new("alice@example.com", NOW - STEP_UP_WINDOW_SECS - 1);
    assert_eq!(code(try_with(Some(&stale))), STEP_UP_REQUIRED);
    let future = StepUp::new("alice@example.com", NOW + 1);
    assert_eq!(code(try_with(Some(&future))), STEP_UP_REQUIRED);
    let someone_else = StepUp::new("carol@example.com", NOW - 5);
    assert_eq!(code(try_with(Some(&someone_else))), STEP_UP_REQUIRED);
    let edge = StepUp::new("alice@example.com", NOW - STEP_UP_WINDOW_SECS);
    assert!(try_with(Some(&edge)).is_ok());
}

#[test]
fn a_missing_approval_is_refused() {
    let prepared = prepared();
    let refused = authorise(&prepared, Some(&admin()), Some(&step_up()), None, NOW);
    assert_eq!(code(refused), APPROVAL_MISSING);
}

#[test]
fn an_approval_for_plan_a_does_not_run_plan_b() {
    let plan_a = prepared();
    let plan_b = prepared_at(&other_topology(), NOW - 10);
    assert_ne!(plan_a.plan().id(), plan_b.plan().id());
    let refused = authorise(
        &plan_b,
        Some(&admin()),
        Some(&step_up()),
        Some(&approval_for(&plan_a)),
        NOW,
    );
    assert_eq!(code(refused), APPROVAL_MISMATCH);
}

#[test]
fn an_approval_names_the_hash_so_a_plan_built_from_other_facts_needs_a_new_one() {
    let first = prepared_at(&reference(), NOW - 10);
    let second = prepared_at(&reference(), NOW - 5);
    assert_eq!(
        first.plan().id(),
        second.plan().id(),
        "same topology, same id"
    );
    assert_ne!(
        first.plan_hash(),
        second.plan_hash(),
        "different facts, different hash"
    );
    let refused = authorise(
        &second,
        Some(&admin()),
        Some(&step_up()),
        Some(&approval_for(&first)),
        NOW,
    );
    assert_eq!(code(refused), APPROVAL_MISMATCH);
}

#[test]
fn a_token_for_plan_a_does_not_run_plan_b() {
    let plan_a = prepared();
    let plan_b = prepared_at(&other_topology(), NOW - 10);
    let token_a = token(&plan_a);
    let mut fixture = Fixture::new(&plan_b);
    assert_eq!(code(fixture.run(&plan_b, &token_a, NOW)), APPROVAL_MISMATCH);
    assert!(fixture.nothing_happened());
}

#[test]
fn nobody_approves_their_own_run() {
    let prepared = prepared();
    for approver in [
        "alice@example.com",
        "ALICE@example.com",
        " alice@example.com ",
        "",
    ] {
        let approval = Approval::new(
            prepared.plan().id().clone(),
            prepared.plan_hash().clone(),
            approver,
            NOW - 5,
        );
        let refused = authorise(
            &prepared,
            Some(&admin()),
            Some(&step_up()),
            Some(&approval),
            NOW,
        );
        assert_eq!(code(refused), APPROVAL_SELF, "approver {approver:?}");
    }
}

#[test]
fn an_old_or_future_approval_is_refused() {
    let prepared = prepared();
    for approved_at in [NOW - APPROVAL_WINDOW_SECS - 1, NOW + 1] {
        let approval = Approval::new(
            prepared.plan().id().clone(),
            prepared.plan_hash().clone(),
            "bob@example.com",
            approved_at,
        );
        let refused = authorise(
            &prepared,
            Some(&admin()),
            Some(&step_up()),
            Some(&approval),
            NOW,
        );
        assert_eq!(code(refused), APPROVAL_EXPIRED);
    }
}

#[test]
fn the_token_lapses_with_the_step_up_window() {
    let prepared = prepared();
    let authorised = token(&prepared);
    assert_eq!(authorised.actor(), "alice@example.com");
    // Step-up was 30 seconds old at NOW, so it lapses 570 seconds later.
    assert_eq!(authorised.expires_at(), NOW - 30 + STEP_UP_WINDOW_SECS);
    let mut fixture = Fixture::new(&prepared);
    let late = authorised.expires_at();
    assert_eq!(
        code(fixture.run(&prepared, &authorised, late)),
        AUTHORISATION_EXPIRED
    );
    assert!(fixture.nothing_happened());
}

// ---------------------------------------------------------------------------------------
// Per-step audit
// ---------------------------------------------------------------------------------------

#[test]
fn when_the_start_entry_cannot_be_written_the_step_does_not_run() {
    let prepared = prepared();
    let authorised = token(&prepared);

    let mut fixture = Fixture::new(&prepared);
    fixture.audit = MemoryAudit::failing();
    assert_eq!(code(fixture.run(&prepared, &authorised, NOW)), AUDIT_FAILED);
    assert!(fixture.executor.submitted().is_empty());
    assert!(
        fixture.journal.entries().is_empty(),
        "no journal entry either"
    );

    // Failing in the middle: earlier steps ran, the failing one did not.
    let mut fixture = Fixture::new(&prepared);
    fixture.audit = MemoryAudit::failing_on(2, AuditOutcome::Started);
    assert_eq!(code(fixture.run(&prepared, &authorised, NOW)), AUDIT_FAILED);
    assert!(
        fixture.executor.submitted().iter().all(|s| s.step != 2),
        "step 2 must not have been submitted"
    );
    assert!(fixture.journal.entries().iter().all(|e| e.step != 2));
}

#[test]
fn when_the_end_entry_cannot_be_written_the_run_stops() {
    let prepared = prepared();
    let authorised = token(&prepared);
    let mut fixture = Fixture::new(&prepared);
    fixture.audit = MemoryAudit::failing_on(0, AuditOutcome::Succeeded);
    assert_eq!(code(fixture.run(&prepared, &authorised, NOW)), AUDIT_FAILED);
    assert_eq!(
        fixture.executor.submitted().len(),
        1,
        "no step after the unaudited one"
    );
}

#[test]
fn a_failed_step_is_audited_as_failed_and_the_entries_hold_no_payload() {
    let prepared = prepared();
    let authorised = token(&prepared);
    let mut fixture = Fixture::new(&prepared);
    fixture
        .executor
        .fail_once(1, "token=hunter2 refused by the node");
    let report = fixture.run(&prepared, &authorised, NOW).unwrap();
    assert!(report.failed.is_some());
    let failed: Vec<_> = fixture
        .audit
        .entries()
        .iter()
        .filter(|e| e.step == 1)
        .map(|e| e.outcome)
        .collect();
    assert_eq!(failed, vec![AuditOutcome::Started, AuditOutcome::Failed]);
    let json = serde_json::to_string(fixture.audit.entries()).unwrap();
    assert!(
        !json.contains("hunter2"),
        "executor output never enters the audit"
    );
    assert!(!json.contains("params"));
}

// ---------------------------------------------------------------------------------------
// Recorded security finding: signed and fresh facts
// ---------------------------------------------------------------------------------------

struct Enrolment {
    hosts: BTreeMap<String, String>,
    signers: BTreeMap<String, SoftwareSigner>,
}

impl Enrolment {
    fn new() -> Self {
        let mut hosts = BTreeMap::new();
        let mut signers = BTreeMap::new();
        for (index, host) in ["app1.example.com", "db1.example.com", "db2.example.com"]
            .iter()
            .enumerate()
        {
            let agent = format!("agent-{index}");
            let seed = [u8::try_from(index).unwrap() + 40; 32];
            signers.insert(agent.clone(), SoftwareSigner::from_seed(&seed).unwrap());
            hosts.insert((*host).to_owned(), agent);
        }
        Self { hosts, signers }
    }

    fn sign(&self, agent: &str, sequence: u64, observed_at: i64, facts: AgentFacts) -> SignedFacts {
        let message = SignedFacts::signed_message(agent, sequence, observed_at, &facts).unwrap();
        let signature = self.signers[agent].sign(&message).to_bytes().to_vec();
        SignedFacts {
            agent_id: agent.to_owned(),
            sequence,
            observed_at,
            facts,
            signature,
        }
    }
}

impl EnrolledAgents for Enrolment {
    fn enrolled_agent(&self, host: &Host) -> Option<String> {
        self.hosts.get(host.as_str()).cloned()
    }

    // Ed25519 is deterministic, so re-signing and comparing is a real verification.
    fn verify(&self, agent_id: &str, message: &[u8], signature: &[u8]) -> bool {
        self.signers
            .get(agent_id)
            .is_some_and(|signer| signer.sign(message).to_bytes().as_slice() == signature)
    }
}

fn app_facts() -> AgentFacts {
    agent_facts(&reference()).remove(0)
}

fn verify(
    report: &SignedFacts,
    ledger: &mut MemoryFactsLedger,
) -> Result<ServerFacts, WizardError> {
    report.verify(&Enrolment::new(), ledger, FactsPolicy::default(), NOW)
}

#[test]
fn a_signed_fresh_report_is_accepted_and_carries_its_stamp() {
    let enrolment = Enrolment::new();
    let report = enrolment.sign("agent-0", 5, NOW - 20, app_facts());
    let verified = verify(&report, &mut MemoryFactsLedger::new()).unwrap();
    assert_eq!(verified.agent_id(), "agent-0");
    assert_eq!(verified.sequence(), 5);
    assert_eq!(verified.observed_at(), NOW - 20);
    assert_eq!(verified.facts(), &app_facts());
}

#[test]
fn a_forged_or_edited_report_is_refused() {
    let enrolment = Enrolment::new();
    let mut ledger = MemoryFactsLedger::new();

    let mut forged = enrolment.sign("agent-0", 5, NOW - 20, app_facts());
    forged.signature = vec![7; 64];
    assert_eq!(code(verify(&forged, &mut ledger)), FACTS_UNTRUSTED);

    let mut empty = enrolment.sign("agent-0", 5, NOW - 20, app_facts());
    empty.signature.clear();
    assert_eq!(code(verify(&empty, &mut ledger)), FACTS_UNTRUSTED);

    // Edited after signing: the disk now claims plenty of space.
    let mut edited = enrolment.sign("agent-0", 5, NOW - 20, app_facts());
    edited.facts = edited.facts.with_data_dir_free_gb(9_999);
    assert_eq!(code(verify(&edited, &mut ledger)), FACTS_UNTRUSTED);

    // A different sequence or time than the one signed.
    let mut renumbered = enrolment.sign("agent-0", 5, NOW - 20, app_facts());
    renumbered.sequence = 6;
    assert_eq!(code(verify(&renumbered, &mut ledger)), FACTS_UNTRUSTED);
    let mut retimed = enrolment.sign("agent-0", 5, NOW - 20, app_facts());
    retimed.observed_at = NOW;
    assert_eq!(code(verify(&retimed, &mut ledger)), FACTS_UNTRUSTED);

    // None of the refusals burned the sequence number.
    let genuine = enrolment.sign("agent-0", 5, NOW - 20, app_facts());
    assert!(verify(&genuine, &mut ledger).is_ok());
}

#[test]
fn a_report_from_another_agent_is_refused() {
    let enrolment = Enrolment::new();
    // agent-1 is genuinely enrolled, and signs validly, but not for app1.
    let report = enrolment.sign("agent-1", 5, NOW - 20, app_facts());
    assert_eq!(
        code(verify(&report, &mut MemoryFactsLedger::new())),
        FACTS_WRONG_AGENT
    );
    // An agent nobody enrolled.
    let mut stranger = enrolment.sign("agent-0", 5, NOW - 20, app_facts());
    stranger.agent_id = "agent-9".to_owned();
    assert_eq!(
        code(verify(&stranger, &mut MemoryFactsLedger::new())),
        FACTS_WRONG_AGENT
    );
}

#[test]
fn a_stale_or_future_report_is_refused() {
    let enrolment = Enrolment::new();
    let mut ledger = MemoryFactsLedger::new();
    let stale = enrolment.sign("agent-0", 5, NOW - 301, app_facts());
    assert_eq!(code(verify(&stale, &mut ledger)), FACTS_STALE);
    let future = enrolment.sign("agent-0", 6, NOW + 1, app_facts());
    assert_eq!(code(verify(&future, &mut ledger)), FACTS_STALE);
    let edge = enrolment.sign("agent-0", 7, NOW - 300, app_facts());
    assert!(verify(&edge, &mut ledger).is_ok());
}

#[test]
fn a_replayed_or_older_report_is_refused() {
    let enrolment = Enrolment::new();
    let mut ledger = MemoryFactsLedger::new();
    let first = enrolment.sign("agent-0", 10, NOW - 20, app_facts());
    assert!(verify(&first, &mut ledger).is_ok());
    assert_eq!(
        code(verify(&first, &mut ledger)),
        FACTS_REPLAYED,
        "same report again"
    );
    let older = enrolment.sign("agent-0", 9, NOW - 10, app_facts());
    assert_eq!(
        code(verify(&older, &mut ledger)),
        FACTS_REPLAYED,
        "lower sequence"
    );
    let next = enrolment.sign("agent-0", 11, NOW - 5, app_facts());
    assert!(verify(&next, &mut ledger).is_ok());
}

#[test]
fn a_policy_must_be_a_sane_age() {
    assert!(FactsPolicy::new(0).is_err());
    assert!(FactsPolicy::new(-5).is_err());
    assert!(FactsPolicy::new(3_601).is_err());
    assert_eq!(FactsPolicy::new(60).unwrap().max_age_secs(), 60);
}

#[test]
fn debug_output_of_a_report_does_not_dump_the_signature() {
    let enrolment = Enrolment::new();
    let mut report = enrolment.sign("agent-0", 5, NOW - 20, app_facts());
    report.signature = vec![0xAB; 64];
    let text = format!("{report:?}");
    assert!(text.contains("signature_len"));
    assert!(!text.contains("171"), "no signature bytes in Debug");
}

#[test]
fn facts_that_age_between_prepare_and_run_refuse_the_run() {
    // Observed 10 seconds before NOW; the policy keeps them for 300.
    let prepared = prepared();
    let authorised = authorise(
        &prepared,
        Some(&AdminIdentity::new(
            "alice@example.com",
            &[DEPLOY_PERMISSION],
            NOW + 10_000,
        )),
        Some(&StepUp::new("alice@example.com", NOW + 5_000)),
        Some(&Approval::new(
            prepared.plan().id().clone(),
            prepared.plan_hash().clone(),
            "bob@example.com",
            NOW + 5_000,
        )),
        NOW + 5_001,
    )
    .unwrap();
    let mut fixture = Fixture::new(&prepared);
    // 400 seconds after they were observed.
    assert_eq!(
        code(fixture.run(&prepared, &authorised, NOW + 390)),
        FACTS_STALE
    );
    assert!(fixture.nothing_happened());
    // Inside the window it runs.
    let mut fixture = Fixture::new(&prepared);
    assert!(
        fixture
            .run(&prepared, &authorised, NOW + 100)
            .unwrap()
            .is_complete()
    );
}

#[test]
fn the_plan_embeds_when_and_by_whom_each_fact_was_observed() {
    let prepared = prepared();
    let stamps = prepared.fact_stamps();
    assert_eq!(stamps.len(), 3);
    assert!(stamps.iter().all(|s| s.observed_at == NOW - 10));
    assert!(stamps.windows(2).all(|pair| pair[0].host < pair[1].host));
}
