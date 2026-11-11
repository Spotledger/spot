//! Bounded work: paged reads, the batched TTL purge, the per-kind cap, the recall ceiling, the
//! overflow-proof TTL arithmetic, the monotonic candidate queue and the database-name check.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{fixture, runtime, tenant};
use spot_agent_memory::testing::{embedder, episode_saying, procedure, sanitiser};
use spot_agent_memory::{
    Confirmation, EMBEDDING_DIM, Embedder, Entry, EpisodeId, Filters, LearningPolicy, MemoryKind,
    MemoryStore, Outcome, Procedure, ProcedureId, ProcedureInput, ProcedureStep, RemedyId,
    SourceKind, Symptom, Tier, propose_remedy,
};
use spot_agent_memory_pg::{
    MAX_RECALL_K, MAX_TTL_MS, PgMemoryStore, RecallQuery, RecencyBoost, pool_for,
};

fn query_vector(text: &str) -> Vec<f32> {
    embedder()
        .embed(&[text.to_owned()])
        .expect("the fake embedder answers")
        .remove(0)
}

fn recall_at(
    fixture: &common::Fixture,
    vector: &[f32],
    k: usize,
    now_ms: i64,
    recency: RecencyBoost,
) -> Vec<String> {
    let filters = Filters::any();
    let request = RecallQuery {
        tenant: &fixture.tenant,
        agent_tier: Tier::T2,
        vector,
        filters: &filters,
        k,
        now_ms,
        recency,
    };
    fixture
        .store
        .recall(&request)
        .expect("the recall answers")
        .into_iter()
        .map(|hit| hit.provenance.id)
        .collect()
}

#[test]
fn connect_refuses_a_database_that_is_not_the_tenants_memory_database() {
    let rt = runtime();
    let config = spot_postgres::PgConfig::new("localhost", 5432, "business_db", "u", "p");
    let refused = PgMemoryStore::connect(&config, tenant("tenant_a"), rt);
    assert!(refused.is_err());
    let _ = pool_for(&config).expect("a pool builds without connecting");
}

#[test]
fn a_listing_is_read_in_pages_by_id() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    for n in 1..=5 {
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                n,
                "stuck job",
                1_000,
            )))
            .expect("the episode");
    }
    let first = store.list_page(MemoryKind::Episode, None, 2).unwrap();
    assert_eq!(first.len(), 2);
    let second = store
        .list_page(MemoryKind::Episode, Some(first[1].id_str()), 2)
        .unwrap();
    assert_eq!(second.len(), 2);
    let third = store
        .list_page(MemoryKind::Episode, Some(second[1].id_str()), 2)
        .unwrap();
    assert_eq!(third.len(), 1);
    assert_eq!(store.list(&fixture.tenant, MemoryKind::Episode).len(), 5);
    assert_eq!(store.tenants_page(None, 10).unwrap().len(), 1);
}

#[test]
fn the_purge_removes_expired_entries_in_batches_and_keeps_the_rest() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    for n in 1..=5 {
        let mut short = episode_saying("tenant_a", n, "stuck job", 1_000);
        short.meta.ttl_ms = Some(500);
        store.put(Entry::Episode(short)).expect("a short one");
    }
    let mut long = episode_saying("tenant_a", 6, "stuck job", 1_000);
    long.meta.ttl_ms = Some(1_000_000);
    store.put(Entry::Episode(long)).expect("a long one");
    store
        .put(Entry::Episode(episode_saying(
            "tenant_a",
            7,
            "stuck job",
            1_000,
        )))
        .expect("one with no ttl");
    assert_eq!(store.purge_expired(2_000, 2).unwrap(), 5);
    assert_eq!(store.count(MemoryKind::Episode).unwrap(), 2);
    assert_eq!(store.purge_expired(2_000, 2).unwrap(), 0);
}

#[test]
fn a_new_entry_beyond_the_per_kind_cap_is_refused_but_a_replacement_is_not() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let rt = fixture.rt.clone();
    let pool = pool_for(&fixture.db.config()).unwrap();
    let store = PgMemoryStore::new(pool, fixture.tenant.clone(), rt)
        .unwrap()
        .with_max_entries(2)
        .unwrap();
    for n in 1..=2 {
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                n,
                "stuck job",
                1_000,
            )))
            .expect("room for two");
    }
    assert!(
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                3,
                "stuck job",
                1_000
            )))
            .is_err()
    );
    store
        .put(Entry::Episode(episode_saying(
            "tenant_a",
            2,
            "new words",
            2_000,
        )))
        .expect("a replacement needs no room");
    store
        .put(Entry::Procedure(procedure("tenant_a", 1)))
        .expect("another kind has its own room");
    assert_eq!(store.count(MemoryKind::Episode).unwrap(), 2);
}

#[test]
fn a_huge_k_is_clamped_to_the_recall_ceiling() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    for n in 1..=3 {
        fixture
            .store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                n,
                "stuck job",
                1_000,
            )))
            .expect("the episode");
    }
    let vector = query_vector("stuck job");
    let hits = recall_at(&fixture, &vector, usize::MAX, 2_000, RecencyBoost::NONE);
    assert_eq!(hits.len(), 3);
    assert!(hits.len() <= MAX_RECALL_K);
    assert_eq!(EMBEDDING_DIM, vector.len());
}

#[test]
fn a_ttl_of_i64_max_and_an_extreme_now_do_not_overflow_a_recall() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    let mut forever = episode_saying("tenant_a", 1, "stuck job", 10);
    forever.meta.ttl_ms = Some(i64::MAX);
    store.put(Entry::Episode(forever)).expect("never expires");
    store
        .put(Entry::Episode(episode_saying(
            "tenant_a",
            2,
            "stuck job",
            1_000,
        )))
        .expect("an ordinary one");
    let stored = store
        .get(&fixture.tenant, MemoryKind::Episode, "episode.e1")
        .unwrap();
    assert_eq!(stored.meta().ttl_ms, Some(MAX_TTL_MS));

    let vector = query_vector("stuck job");
    for now in [0, 2_000, i64::MIN] {
        let hits = recall_at(&fixture, &vector, 5, now, RecencyBoost::DEFAULT);
        assert!(hits.contains(&"episode.e1".to_owned()), "now = {now}");
    }
    // `now` is clamped to the end of year 9999, which is past the longest stored life (about 100
    // years), so the "forever" entry has legitimately expired there; the entry with no TTL has not.
    let hits = recall_at(&fixture, &vector, 5, i64::MAX, RecencyBoost::DEFAULT);
    assert_eq!(hits, vec!["episode.e2".to_owned()]);
    let extreme = RecencyBoost {
        cap_bp: i32::MAX,
        window_ms: i64::MAX,
    };
    assert_eq!(recall_at(&fixture, &vector, 5, i64::MAX, extreme).len(), 1);
}

#[test]
fn a_stale_proposed_write_does_not_overwrite_a_promoted_row() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    let source = common_procedure();
    let candidate = propose_remedy(&source, &LearningPolicy::default()).expect("proposed");
    store.record_candidate(&candidate, 1_000).unwrap();
    let client = fixture.rt.block_on(async { fixture.db.raw_client().await });
    fixture
        .rt
        .block_on(async {
            client
                .execute(
                    "UPDATE spot_memory_candidate SET status = 'promoted', ci_run = 'run-1'",
                    &[],
                )
                .await
        })
        .expect("CI promoted it");
    store.record_candidate(&candidate, 2_000).unwrap();
    let stored = store.candidate(candidate.id()).unwrap().unwrap();
    assert_eq!(stored.status, spot_agent_memory::CandidateStatus::Promoted);
    assert_eq!(stored.updated_ms, 1_000);
}

fn common_procedure() -> Procedure {
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
