//! The learning gate from outside the crate: a repeated successful procedure becomes a candidate,
//! a candidate is unusable until CI passes, and promotion records the CI run id.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_agent_memory::testing::{RETRY_FAILED, embedder, sanitiser};
use spot_agent_memory::{
    CandidateQueue, CandidateStatus, CiOutcome, CiPromotion, CiRunId, Confidence, EpisodeId,
    LearningPolicy, ParamValue, Procedure, ProcedureInput, ProcedureStep, PromotionPort, RemedyId,
    ScenarioSkeleton, SourceKind, Symptom, TenantId, Tier, UsableRemedy, can_use, propose_remedy,
};

/// A procedure with `successes` resolved incidents and `harms` harmful ones.
fn procedure(successes: u32, harms: u32) -> Procedure {
    let mut input = ProcedureInput {
        id: spot_agent_memory::ProcedureId::parse("procedure.job_stuck").unwrap(),
        title: "retry the stuck job",
        symptoms: vec![Symptom::parse("job.stuck_running").unwrap()],
        steps: vec![
            ProcedureStep::new(RemedyId::new(RETRY_FAILED))
                .with("attempts", ParamValue::Integer(3)),
        ],
        confirmations: Vec::new(),
        meta: spot_agent_memory::EntryMeta::new(
            Tier::T2,
            TenantId::parse("tenant_a").unwrap(),
            1_000,
            SourceKind::RemedyRun,
        )
        .unwrap()
        .with_confidence(Confidence::FULL),
    };
    for n in 0..successes {
        input.confirmations.push(
            spot_agent_memory::Confirmation::new(
                EpisodeId::parse(&format!("episode.ok{n}")).unwrap(),
                spot_agent_memory::Outcome::Resolved,
                1_000 + i64::from(n),
            )
            .unwrap(),
        );
    }
    for n in 0..harms {
        input.confirmations.push(
            spot_agent_memory::Confirmation::new(
                EpisodeId::parse(&format!("episode.harm{n}")).unwrap(),
                spot_agent_memory::Outcome::Harm,
                2_000 + i64::from(n),
            )
            .unwrap(),
        );
    }
    Procedure::record(&sanitiser(), &embedder(), input).expect("the test procedure records")
}

#[test]
fn a_repeated_success_becomes_a_proposed_candidate_in_the_remedy_format() {
    let candidate = propose_remedy(&procedure(3, 0), &LearningPolicy::default()).unwrap();
    assert_eq!(candidate.status(), CandidateStatus::Proposed);
    assert_eq!(candidate.remedy().as_str(), "learned.job_stuck");
    assert_eq!(candidate.meta().tier, Tier::T2);
    assert_eq!(candidate.meta().risk.as_str(), "medium");
    assert_eq!(candidate.confirmations(), 3);
    assert!(candidate.meta().undo.is_reversible());
    assert_eq!(
        candidate.meta().params.properties.get("attempts"),
        Some(&spot_remedies::ParamType::Integer)
    );
    assert_eq!(candidate.steps().len(), 1);
    assert!(!can_use(&candidate), "a proposed candidate is not usable");
}

#[test]
fn too_few_confirmations_or_any_harm_blocks_the_candidate() {
    let unproven = propose_remedy(&procedure(2, 0), &LearningPolicy::default()).unwrap_err();
    assert_eq!(unproven.code(), "agent_memory.unproven");
    assert!(unproven.to_string().contains("needs 3"));

    let harmful = propose_remedy(&procedure(5, 1), &LearningPolicy::default()).unwrap_err();
    assert_eq!(harmful.code(), "agent_memory.harmful");

    // The policy's minimum is configurable, and a lower one admits two successes.
    let lenient = LearningPolicy {
        min_confirmations: 2,
        ..LearningPolicy::default()
    };
    assert!(propose_remedy(&procedure(2, 0), &lenient).is_ok());
}

/// Runs `candidate` through CI on `ci`, with the outcome CI reports for `run`.
fn through_ci(
    ci: &CiPromotion,
    mut candidate: spot_agent_memory::CandidateRemedy,
    run: &CiRunId,
    outcome: CiOutcome,
) -> spot_agent_memory::CandidateRemedy {
    ci.start(&mut candidate, run.clone()).unwrap();
    ci.report(run.clone(), outcome);
    ci.settle(&mut candidate).unwrap();
    candidate
}

#[test]
fn a_candidate_is_unusable_until_a_promotion_makes_it_a_usable_remedy() {
    let ci = CiPromotion::new();
    let candidate = propose_remedy(&procedure(3, 0), &LearningPolicy::default()).unwrap();
    let run = CiRunId::parse("ci-run-2026-04-17").unwrap();

    // Proposed: unusable.
    assert!(!can_use(&candidate));

    // In CI: unusable.
    let mut in_ci = candidate.clone();
    ci.start(&mut in_ci, run.clone()).unwrap();
    assert_eq!(in_ci.status(), CandidateStatus::InCi);
    assert!(!can_use(&in_ci));

    // CI has not finished: the candidate does not settle.
    assert_eq!(
        ci.settle(&mut in_ci.clone()).unwrap_err().code(),
        "agent_memory.ci_run_missing"
    );

    // Passed: still unusable, because only promotion makes a remedy usable.
    ci.report(run.clone(), CiOutcome::Passed);
    let mut passed = in_ci.clone();
    ci.settle(&mut passed).unwrap();
    assert_eq!(passed.status(), CandidateStatus::Passed);
    assert!(!can_use(&passed));

    // Promoted: the only path to `UsableRemedy`.
    let usable: UsableRemedy = ci.promote(passed, run.clone()).unwrap();
    assert_eq!(usable.ci_run(), &run);
    assert!(can_use(usable.candidate()));
    assert_eq!(usable.candidate().status(), CandidateStatus::Promoted);
    assert_eq!(usable.meta().title, "retry the stuck job");
}

#[test]
fn promotion_requires_the_ci_run_that_passed() {
    // A run id must be a real token.
    assert!(CiRunId::parse("").is_err());
    assert!(CiRunId::parse("with space").is_err());

    let ci = CiPromotion::new();

    // A candidate that never passed cannot be promoted.
    let candidate = propose_remedy(&procedure(3, 0), &LearningPolicy::default()).unwrap();
    assert_eq!(
        ci.promote(candidate, CiRunId::parse("run-1").unwrap())
            .unwrap_err()
            .code(),
        "agent_memory.status_invalid"
    );

    // A passed candidate promoted under a different run is refused: the record would be a lie.
    let run = CiRunId::parse("run-1").unwrap();
    let passed = through_ci(&ci, candidate_from(3), &run, CiOutcome::Passed);
    ci.report(CiRunId::parse("run-2").unwrap(), CiOutcome::Passed);
    assert_eq!(
        ci.promote(passed, CiRunId::parse("run-2").unwrap())
            .unwrap_err()
            .code(),
        "agent_memory.ci_run_missing"
    );

    // A rejected candidate never promotes.
    let run = CiRunId::parse("run-3").unwrap();
    let rejected = through_ci(
        &ci,
        candidate_from(3),
        &run,
        CiOutcome::Failed("the self-test failed".to_owned()),
    );
    assert_eq!(rejected.status(), CandidateStatus::Rejected);
    assert_eq!(rejected.rejection(), Some("the self-test failed"));
    assert!(ci.promote(rejected, run).is_err());
}

#[test]
fn a_port_that_does_not_record_the_run_as_passed_promotes_nothing() {
    // Two ports: the candidate passed on one, and the other has no record of the run.
    let run = CiRunId::parse("run-1").unwrap();
    let ci = CiPromotion::new();
    let passed = through_ci(&ci, candidate_from(3), &run, CiOutcome::Passed);
    let other = CiPromotion::new();
    assert_eq!(
        other.promote(passed, run).unwrap_err().code(),
        "agent_memory.ci_run_missing"
    );
}

#[test]
fn the_only_public_way_to_a_usable_remedy_is_the_port() {
    // The candidate's transitions, `Promoted::new` and `UsableRemedy::from_promoted` are
    // crate-private: the `compile_fail` doctests in `learning.rs` prove an external crate cannot
    // call them. What an external crate can do is go through the port, and the port needs CI's
    // own record: with none, nothing a caller does moves a candidate.
    let ci = CiPromotion::new();
    let mut candidate = candidate_from(3);
    ci.start(&mut candidate, CiRunId::parse("made-up").unwrap())
        .unwrap();
    assert!(ci.settle(&mut candidate).is_err());
    assert_eq!(candidate.status(), CandidateStatus::InCi);
    assert!(!can_use(&candidate));
}

fn candidate_from(successes: u32) -> spot_agent_memory::CandidateRemedy {
    propose_remedy(&procedure(successes, 0), &LearningPolicy::default()).unwrap()
}

#[test]
fn the_queue_writes_a_spec_and_a_scenario_skeleton_under_candidates() {
    let queue = spot_agent_memory::InMemoryCandidateQueue::new();
    let candidate = candidate_from(3);
    let id = candidate.id().clone();
    let bundle = queue.enqueue(candidate).unwrap();

    assert_eq!(bundle.dir, format!("candidates/{id}"));
    let files = bundle.files();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].0, format!("candidates/{id}/spec.toml"));
    assert!(files[1].0.ends_with("/scenario.toml"));

    let spec: toml::Value = toml::from_str(&bundle.spec_toml).unwrap();
    assert_eq!(spec["remedy"].as_str(), Some("learned.job_stuck"));
    assert_eq!(spec["tier"].as_str(), Some("T2"));
    assert_eq!(spec["status"].as_str(), Some("proposed"));
    assert_eq!(spec["confirmations"].as_integer(), Some(3));
    assert_eq!(
        spec["params"]
            .get("attempts")
            .and_then(|value| value.as_str()),
        Some("integer")
    );
    assert_eq!(spec["undo"]["kind"].as_str(), Some("reversible"));

    let scenario: toml::Value = toml::from_str(&bundle.scenario_toml).unwrap();
    assert_eq!(scenario["remedy"].as_str(), Some("learned.job_stuck"));
    assert_eq!(scenario["undo"].as_str(), Some("restored"));
    let must_prove = scenario["must_prove"].as_array().unwrap();
    assert!(must_prove.len() >= 4);
    assert!(
        must_prove.iter().any(|line| line
            .as_str()
            .is_some_and(|line| line.contains("hard floor"))),
        "the skeleton names the hard floor as an invariant"
    );

    assert_eq!(queue.len(), 1);
    assert_eq!(queue.list().len(), 1);
    assert_eq!(
        queue.get(&id).map(|found| found.id().clone()),
        Some(id.clone())
    );
    assert!(queue.bundle(&id).is_some());
}

#[test]
fn the_queue_refuses_a_second_candidate_of_the_same_id_and_a_ci_started_one() {
    let queue = spot_agent_memory::InMemoryCandidateQueue::new();
    let candidate = candidate_from(3);
    queue.enqueue(candidate.clone()).unwrap();
    assert_eq!(
        queue.enqueue(candidate.clone()).unwrap_err().code(),
        "agent_memory.candidate_invalid"
    );

    let mut started = candidate_from(3);
    CiPromotion::new()
        .start(&mut started, CiRunId::parse("run-1").unwrap())
        .unwrap();
    assert_eq!(
        queue.enqueue(started).unwrap_err().code(),
        "agent_memory.status_invalid"
    );
}

#[test]
fn the_scenario_skeleton_is_writable_on_its_own() {
    let candidate = candidate_from(3);
    let skeleton = ScenarioSkeleton::of(&candidate);
    assert_eq!(skeleton.remedy, "learned.job_stuck");
    assert_eq!(skeleton.symptoms, ["job.stuck_running"]);
    assert!(!skeleton.steps.is_empty());
}

#[test]
fn parameters_that_do_not_fit_one_schema_block_the_candidate() {
    // The same parameter is an integer in one step and text in another: a remedy's parameters
    // must be one shape, so this is refused rather than guessed.
    let mut two_faced = procedure(3, 0);
    two_faced.steps = vec![
        ProcedureStep::new(RemedyId::new(RETRY_FAILED)).with("attempts", ParamValue::Integer(3)),
        ProcedureStep::new(RemedyId::new("job.unstick_running"))
            .with("attempts", ParamValue::text(&sanitiser(), "three").unwrap()),
    ];
    let error = propose_remedy(&two_faced, &LearningPolicy::default()).unwrap_err();
    assert_eq!(error.code(), "agent_memory.candidate_invalid");
    assert!(error.to_string().contains("attempts"));
}

#[test]
fn a_procedure_less_confident_than_the_policy_is_not_proposed() {
    let mut unsure = procedure(3, 0);
    unsure.meta.confidence = Confidence::new(4_000).unwrap();
    let policy = LearningPolicy {
        min_confidence: Confidence::new(5_000).unwrap(),
        ..LearningPolicy::default()
    };
    let error = propose_remedy(&unsure, &policy).unwrap_err();
    assert_eq!(error.code(), "agent_memory.candidate_invalid");
    assert!(error.to_string().contains("4000"));

    // At the floor it passes.
    unsure.meta.confidence = Confidence::FULL;
    assert!(propose_remedy(&unsure, &policy).is_ok());
}

#[test]
fn the_queue_only_moves_a_candidate_forward() {
    let queue = spot_agent_memory::InMemoryCandidateQueue::new();
    let ci = CiPromotion::new();
    let run = CiRunId::parse("run-1").unwrap();
    let proposed = candidate_from(3);
    queue.enqueue(proposed.clone()).unwrap();

    let mut in_ci = proposed.clone();
    ci.start(&mut in_ci, run.clone()).unwrap();
    queue.replace(in_ci.clone()).unwrap();
    let passed = through_ci(&ci, proposed.clone(), &run, CiOutcome::Passed);
    queue.replace(passed.clone()).unwrap();
    let usable = ci.promote(passed.clone(), run).unwrap();
    queue.replace(usable.candidate().clone()).unwrap();

    // A promoted entry is not overwritten by an earlier-status value.
    for earlier in [proposed, in_ci, passed] {
        assert_eq!(
            queue.replace(earlier).unwrap_err().code(),
            "agent_memory.candidate_invalid"
        );
    }
    let id = usable.candidate().id().clone();
    assert_eq!(
        queue.get(&id).map(|found| found.status()),
        Some(CandidateStatus::Promoted)
    );
}
