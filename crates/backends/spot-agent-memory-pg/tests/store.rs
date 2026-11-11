//! The [`MemoryStore`] contract over PostgreSQL: the five trait calls, the tenant gate, decoding
//! through the domain constructors, the export and the candidate queue.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{fixture, tenant};
use spot_agent_memory::testing::{embedder, episode, episode_saying, fact, procedure, sanitiser};
use spot_agent_memory::{
    CandidateId, Confirmation, Entry, EpisodeId, LearningPolicy, MemoryKind, MemoryStore, Outcome,
    Procedure, ProcedureId, ProcedureInput, ProcedureStep, RemedyId, SourceKind, Symptom, Tier,
    propose_remedy,
};
use spot_agent_memory_pg::StoredCandidate;

#[test]
fn every_kind_round_trips_through_the_memory_database() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    let owner = &fixture.tenant;
    let entries = vec![
        Entry::Episode(episode("tenant_a", 1)),
        Entry::Procedure(procedure("tenant_a", 1)),
        Entry::Fact(fact("tenant_a", 1)),
    ];
    for entry in &entries {
        store.put(entry.clone()).expect("the entry is written");
    }

    for entry in &entries {
        let read = store
            .get(owner, entry.kind(), entry.id_str())
            .expect("the entry is there");
        assert_eq!(&read, entry, "{} survives the database", entry.id_str());
        assert_eq!(read.vector(), entry.vector());
        assert_eq!(read.meta(), entry.meta());
    }
    assert_eq!(store.list(owner, MemoryKind::Episode).len(), 1);
    assert_eq!(store.tenants(), std::slice::from_ref(owner));
    assert_eq!(store.count(MemoryKind::Procedure).unwrap(), 1);
}

#[test]
fn a_write_replaces_the_entry_of_the_same_tenant_and_id() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    let owner = &fixture.tenant;
    store
        .put(Entry::Episode(episode_saying(
            "tenant_a",
            1,
            "first words",
            1_000,
        )))
        .expect("the first write");
    store
        .put(Entry::Episode(episode_saying(
            "tenant_a",
            1,
            "second words",
            2_000,
        )))
        .expect("the replacement");
    assert_eq!(store.count(MemoryKind::Episode).unwrap(), 1);
    let read = store.get(owner, MemoryKind::Episode, "episode.e1").unwrap();
    assert_eq!(read.meta().created_ms, 2_000);
    assert_eq!(read.text(), "second words");
}

#[test]
fn a_listing_is_ordered_by_id_and_a_removal_reports_whether_it_was_there() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    let owner = &fixture.tenant;
    for n in [2, 1] {
        store
            .put(Entry::Episode(episode("tenant_a", n)))
            .expect("the write");
    }
    let ids: Vec<String> = store
        .list(owner, MemoryKind::Episode)
        .iter()
        .map(|entry| entry.id_str().to_owned())
        .collect();
    assert_eq!(ids, ["episode.e1", "episode.e2"]);

    assert!(
        store
            .remove(owner, MemoryKind::Episode, "episode.e1")
            .expect("the removal")
    );
    assert!(
        !store
            .remove(owner, MemoryKind::Episode, "episode.e1")
            .expect("the second removal")
    );
    assert_eq!(store.count(MemoryKind::Episode).unwrap(), 1);
}

#[test]
fn an_invalid_entry_never_reaches_the_database() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    let mut invalid = episode("tenant_a", 1);
    invalid.symptoms.clear();
    let error = store
        .put(Entry::Episode(invalid))
        .expect_err("an episode without a symptom is refused");
    assert_eq!(error.code(), "agent_memory.entry_invalid");
    assert_eq!(store.count(MemoryKind::Episode).unwrap(), 0);
}

#[test]
fn a_store_bound_to_one_tenant_refuses_another_and_reads_nothing_of_it() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    let stranger = tenant("tenant_b");
    let error = store
        .put(Entry::Episode(episode("tenant_b", 1)))
        .expect_err("another tenant's entry is refused");
    assert_eq!(error.code(), "agent_memory.store_failed");
    assert!(
        store
            .get(&stranger, MemoryKind::Episode, "episode.e1")
            .is_none()
    );
    assert!(store.list(&stranger, MemoryKind::Episode).is_empty());
    assert_eq!(
        store
            .remove(&stranger, MemoryKind::Episode, "episode.e1")
            .expect_err("another tenant's removal is refused")
            .code(),
        "agent_memory.store_failed"
    );
    // The row is stored under one tenant, so it is invisible to the other even by id.
    store
        .put(Entry::Episode(episode("tenant_a", 1)))
        .expect("the write");
    assert!(
        store
            .get(&stranger, MemoryKind::Episode, "episode.e1")
            .is_none()
    );
}

#[test]
fn an_export_carries_every_entry_of_every_kind() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    store
        .put(Entry::Episode(episode("tenant_a", 1)))
        .expect("the episode");
    store
        .put(Entry::Procedure(procedure("tenant_a", 1)))
        .expect("the procedure");
    store
        .put(Entry::Fact(fact("tenant_a", 1)))
        .expect("the fact");

    let export = store.export().expect("the export");
    assert_eq!(export.len(), 3);
    assert_eq!(export.tenant, "tenant_a");
    assert_eq!(export.episodes.len(), 1);
    assert_eq!(export.procedures.len(), 1);
    assert_eq!(export.facts.len(), 1);
    let json = export.to_json().expect("the export renders");
    assert!(json.contains("episode.e1"));
    assert!(!json.contains("postgres://"), "an export holds no secret");
}

#[test]
fn the_database_refuses_a_stored_row_whose_text_no_longer_passes_the_sanitiser() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    // A row written by hand (or by an older version) carrying a record value in its summary: the
    // store re-scans on the way out rather than handing the value to a caller.
    let client = fixture.rt.block_on(async { fixture.db.raw_client().await });
    let vector = vec![0.0f32; spot_agent_memory::EMBEDDING_DIM];
    let vector_text = format!(
        "[{}]",
        vector
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<String>>()
            .join(",")
    );
    fixture.rt.block_on(async {
        client
            .execute(
                "INSERT INTO spot_memory_episode (tenant, id, incident_doctype, incident_id, \
                 symptoms, actions, outcome, summary, audits, remedies, tier, created_ms, ttl_ms, \
                 confidence_bp, source, embedding) \
                 VALUES ('tenant_a', 'episode.hand_made', 'job', 'j-1', ARRAY['job.stuck'], \
                 '[]'::jsonb, 'resolved', 'the invoice total was Rs 12,500', ARRAY['audit-1'], \
                 ARRAY[]::text[], 0, 1000, NULL, 10000, 'incident', $1::text::vector)",
                &[&vector_text],
            )
            .await
            .expect("the hand-made row is inserted");
    });
    let error = store
        .try_get(MemoryKind::Episode, "episode.hand_made")
        .expect_err("a record value in a stored summary is refused");
    assert_eq!(error.code(), "agent_memory.store_failed");
    assert!(!error.to_string().contains("12,500"), "{error}");
}

#[test]
fn a_candidate_is_queued_with_its_self_test_skeleton_and_can_never_be_promoted_without_ci() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    let procedure = three_times_confirmed();
    store
        .put(Entry::Procedure(procedure.clone()))
        .expect("the procedure is stored");
    let candidate =
        propose_remedy(&procedure, &LearningPolicy::default()).expect("the candidate is proposed");
    store
        .record_candidate(&candidate, 5_000)
        .expect("the candidate is queued");

    let stored: StoredCandidate = store
        .candidate(candidate.id())
        .expect("the queue is read")
        .expect("the candidate is there");
    assert_eq!(stored.status, spot_agent_memory::CandidateStatus::Proposed);
    assert_eq!(stored.confirmations, 3);
    assert_eq!(stored.remedy, candidate.remedy().to_string());
    assert_eq!(stored.procedure, "procedure.stuck");
    assert_eq!(stored.undo.0, "reversible");
    assert!(!stored.is_usable());
    assert_eq!(store.candidates().expect("the queue").len(), 1);
    assert!(
        store
            .candidate(&CandidateId::parse("candidate.absent").unwrap())
            .expect("the read")
            .is_none()
    );

    // The table's own constraint: a row that claims a promotion must name the CI run, so a
    // promotion the domain type could not have produced cannot be written even by hand.
    let client = fixture.rt.block_on(async { fixture.db.raw_client().await });
    let refused = fixture.rt.block_on(async {
        client
            .execute(
                "INSERT INTO spot_memory_candidate (tenant, id, remedy, procedure_id, status, \
                 confirmations, symptoms, steps, title, tier, risk, affects, preconditions, \
                 undo_kind, undo_detail, params, scenario, updated_ms) \
                 VALUES ('tenant_a', 'candidate.forged', 'learned.forged', 'procedure.stuck', \
                 'promoted', 3, ARRAY['job.stuck'], '[]'::jsonb, 'forged', 2, 'medium', \
                 ARRAY[]::text[], 'none', 'reversible', 'restores', '{}'::jsonb, '{}'::jsonb, 1000)",
                &[],
            )
            .await
    });
    let error = refused.expect_err("a promoted row with no CI run is refused");
    assert!(
        error
            .as_db_error()
            .is_some_and(|db| db.constraint() == Some("spot_memory_candidate_promoted_needs_ci")),
        "{error}"
    );
}

/// A procedure with three resolved confirmations: what the learning policy needs.
fn three_times_confirmed() -> Procedure {
    let sanitiser = sanitiser();
    let embedder = embedder();
    let confirmations = (1..=3)
        .map(|n| {
            Confirmation::new(
                EpisodeId::parse(&format!("episode.e{n}")).expect("the episode id"),
                Outcome::Resolved,
                1_000 + n,
            )
            .expect("the confirmation")
        })
        .collect();
    Procedure::record(
        &sanitiser,
        &embedder,
        ProcedureInput {
            id: ProcedureId::parse("procedure.stuck").expect("the procedure id"),
            title: "retry the stuck job",
            symptoms: vec![Symptom::parse("job.stuck_running").expect("the symptom")],
            steps: vec![ProcedureStep::new(RemedyId::new("job.retry_failed"))],
            confirmations,
            meta: spot_agent_memory::testing::meta(
                "tenant_a",
                Tier::T2,
                1_000,
                SourceKind::RemedyRun,
            ),
        },
    )
    .expect("the procedure records")
}
