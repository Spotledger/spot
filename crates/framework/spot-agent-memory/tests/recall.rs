//! Recall from outside the crate: ranking, the structured filters, the tier gate on procedures and
//! the tenant gate on episodes, with provenance on every hit.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use spot_agent_memory::testing::{embedder, episode_saying, fact, procedure};
use spot_agent_memory::{
    Entry, Filters, InMemoryStore, MemoryKind, MemoryStore, RecallRequest, RemedyId, Symptom,
    TenantId, Tier, read_procedure, recall,
};

fn tenant(name: &str) -> TenantId {
    TenantId::parse(name).expect("a well-formed tenant")
}

fn store() -> InMemoryStore {
    let store = InMemoryStore::new();
    for entry in [
        Entry::Episode(episode_saying(
            "tenant_a",
            1,
            "the running job was stuck and a retry cleared it",
            1_000,
        )),
        Entry::Episode(episode_saying(
            "tenant_a",
            2,
            "the cache namespace held a stale lease",
            2_000,
        )),
        Entry::Episode(episode_saying(
            "tenant_a",
            3,
            "the running job was stuck and a retry cleared it",
            3_000,
        )),
    ] {
        store.put(entry).unwrap();
    }
    // Another tenant's episode says exactly the same thing: it must never surface.
    store
        .put(Entry::Episode(episode_saying(
            "tenant_b",
            4,
            "the running job was stuck and a retry cleared it",
            4_000,
        )))
        .unwrap();
    store
}

fn request<'a>(tenant: &'a TenantId, query: &'a str, tier: Tier) -> RecallRequest<'a> {
    RecallRequest {
        tenant,
        agent_tier: tier,
        query,
        filters: Filters::any(),
        k: 5,
        now_ms: 10_000,
    }
}

#[test]
fn recall_ranks_by_similarity_and_returns_provenance() {
    let store = store();
    let tenant_a = tenant("tenant_a");
    let asked = request(&tenant_a, "a stuck running job", Tier::T2);
    let hits = recall(&store, &embedder(), &asked).unwrap();

    assert_eq!(hits.len(), 3, "only the asking tenant's episodes");
    assert_eq!(hits[0].provenance.id, "episode.e1");
    assert_eq!(
        hits[2].provenance.id, "episode.e2",
        "the unrelated episode ranks last"
    );
    assert!(hits[0].score_bp >= hits[2].score_bp);
    assert_eq!(hits[0].provenance.tenant, tenant("tenant_a"));
    assert_eq!(hits[0].provenance.kind, MemoryKind::Episode);
    assert_eq!(hits[0].provenance.source.as_str(), "incident");
    assert_eq!(
        hits[0].provenance.audits.len(),
        1,
        "the audit link is provenance"
    );
    assert_eq!(hits[0].provenance.tier, Tier::T0);
    assert!(hits[0].text.contains("stuck"));
}

#[test]
fn the_same_query_gives_the_same_ranking_twice() {
    let store = store();
    let tenant_a = tenant("tenant_a");
    let asked = request(&tenant_a, "stuck job retry lease cache", Tier::T2);
    let first = recall(&store, &embedder(), &asked).unwrap();
    let second = recall(&store, &embedder(), &asked).unwrap();
    assert_eq!(first, second);
}

#[test]
fn k_bounds_the_answer_and_zero_k_returns_nothing() {
    let store = store();
    let tenant_a = tenant("tenant_a");
    let asked = RecallRequest {
        k: 2,
        ..request(&tenant_a, "stuck job", Tier::T2)
    };
    assert_eq!(recall(&store, &embedder(), &asked).unwrap().len(), 2);

    let none = RecallRequest {
        k: 0,
        ..request(&tenant_a, "stuck job", Tier::T2)
    };
    assert!(recall(&store, &embedder(), &none).unwrap().is_empty());
}

#[test]
fn structured_filters_narrow_and_kinds_select() {
    let store = store();
    let tenant_a = tenant("tenant_a");
    let symptom = Symptom::parse("job.stuck_running").expect("a well-formed symptom");
    let by_symptom = RecallRequest {
        filters: Filters {
            symptom: Some(symptom),
            ..Filters::any()
        },
        ..request(&tenant_a, "cache lease", Tier::T2)
    };
    assert_eq!(recall(&store, &embedder(), &by_symptom).unwrap().len(), 3);

    let by_time = RecallRequest {
        filters: Filters {
            since_ms: Some(2_000),
            ..Filters::any()
        },
        ..request(&tenant_a, "stuck job", Tier::T2)
    };
    let hits = recall(&store, &embedder(), &by_time).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|hit| hit.provenance.created_ms >= 2_000));

    let only_facts = RecallRequest {
        filters: Filters {
            kinds: BTreeSet::from([MemoryKind::Fact]),
            ..Filters::any()
        },
        ..request(&tenant_a, "stuck job", Tier::T2)
    };
    assert!(recall(&store, &embedder(), &only_facts).unwrap().is_empty());
}

#[test]
fn a_procedure_above_the_readers_tier_is_omitted_and_a_direct_read_refuses() {
    let store = InMemoryStore::new();
    store
        .put(Entry::Procedure(procedure("tenant_a", 1)))
        .unwrap();
    let tenant_a = tenant("tenant_a");

    let as_t0 = recall(
        &store,
        &embedder(),
        &request(&tenant_a, "stuck job retry", Tier::T0),
    )
    .unwrap();
    assert!(
        as_t0.is_empty(),
        "a T2 procedure is invisible to a T0 agent"
    );
    let as_t1 = recall(
        &store,
        &embedder(),
        &request(&tenant_a, "stuck job retry", Tier::T1),
    )
    .unwrap();
    assert!(as_t1.is_empty());
    let as_t2 = recall(
        &store,
        &embedder(),
        &request(&tenant_a, "stuck job retry", Tier::T2),
    )
    .unwrap();
    assert_eq!(as_t2.len(), 1, "a T2 agent reads a T2 procedure");

    let refused = read_procedure(&store, &tenant_a, Tier::T1, "procedure.p1", 1_000).unwrap_err();
    assert_eq!(refused.code(), "agent_memory.tier_refused");
    assert!(read_procedure(&store, &tenant_a, Tier::T2, "procedure.p1", 1_000).is_ok());
}

#[test]
fn a_remedy_filter_finds_the_procedure_and_the_episode_that_ran_it() {
    let store = store();
    store
        .put(Entry::Procedure(procedure("tenant_a", 1)))
        .unwrap();
    let tenant_a = tenant("tenant_a");
    let remedy = RemedyId::new(spot_agent_memory::testing::RETRY_FAILED);

    let asked = RecallRequest {
        filters: Filters {
            remedy: Some(remedy),
            ..Filters::any()
        },
        ..request(&tenant_a, "stuck job", Tier::T2)
    };
    let hits = recall(&store, &embedder(), &asked).unwrap();
    assert_eq!(hits.len(), 1, "the test procedure names the remedy");
    assert_eq!(hits[0].provenance.kind, MemoryKind::Procedure);
    assert_eq!(hits[0].provenance.remedies.len(), 1);
}

#[test]
fn an_expired_entry_is_not_recalled() {
    let store = InMemoryStore::new();
    let mut expired = episode_saying("tenant_a", 1, "the job stayed stuck", 1_000);
    expired.meta.ttl_ms = Some(500);
    store.put(Entry::Episode(expired)).unwrap();
    let mut alive = episode_saying("tenant_a", 2, "the job stayed stuck", 1_000);
    alive.meta.ttl_ms = Some(100_000);
    store.put(Entry::Episode(alive)).unwrap();

    let tenant_a = tenant("tenant_a");
    let hits = recall(
        &store,
        &embedder(),
        &request(&tenant_a, "stuck job", Tier::T0),
    )
    .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].provenance.id, "episode.e2");
}

#[test]
fn a_fact_reports_its_deployment_epoch() {
    let store = InMemoryStore::new();
    store.put(Entry::Fact(fact("tenant_a", 1))).unwrap();
    let tenant_a = tenant("tenant_a");
    let hits = recall(
        &store,
        &embedder(),
        &request(&tenant_a, "backend fact", Tier::T0),
    )
    .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].provenance.epoch.map(|epoch| epoch.get()), Some(1));
}

#[test]
fn a_query_larger_than_the_limit_is_refused() {
    let store = store();
    let tenant_a = tenant("tenant_a");
    let long = "stuck ".repeat(200);
    let error = recall(&store, &embedder(), &request(&tenant_a, &long, Tier::T2)).unwrap_err();
    assert_eq!(error.code(), "agent_memory.query_too_long");
}

#[test]
fn a_vector_of_the_wrong_width_is_refused_on_store_and_on_query() {
    use spot_agent_memory::{EMBEDDING_DIM, FakeEmbedder};

    // On query: the provider answers with the wrong width.
    let narrow = FakeEmbedder::with_dim(EMBEDDING_DIM / 2);
    let request = RecallRequest {
        tenant: &tenant("tenant_a"),
        agent_tier: Tier::T3,
        query: "the running job was stuck",
        filters: Filters::any(),
        k: 3,
        now_ms: 5_000,
    };
    assert_eq!(
        recall(&store(), &narrow, &request).unwrap_err().code(),
        "agent_memory.embed_failed"
    );

    // On store: an entry carrying a wrong-width vector does not validate.
    let store = InMemoryStore::new();
    let mut episode = episode_saying("tenant_a", 9, "the running job was stuck", 1_000);
    episode.vector.truncate(EMBEDDING_DIM / 2);
    assert_eq!(
        store.put(Entry::Episode(episode)).unwrap_err().code(),
        "agent_memory.entry_invalid"
    );
}
