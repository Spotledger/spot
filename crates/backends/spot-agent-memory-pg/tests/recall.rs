//! Hybrid recall: ranking, the structured filters, the tier and TTL gates, the recency boost,
//! and the plan assertion at 100 000 rows.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::fixture;
use spot_agent_memory::testing::{embedder, episode_saying, fact, procedure};
use spot_agent_memory::{
    EMBEDDING_DIM, Embedder, Entry, Filters, MemoryKind, MemoryStore, RecallHit, RemedyId, Symptom,
    Tier,
};
use spot_agent_memory_pg::{RecallQuery, RecencyBoost};

/// The embedded query for `text`, from the deterministic fake embedder.
fn query_vector(text: &str) -> Vec<f32> {
    embedder()
        .embed(&[text.to_owned()])
        .expect("the fake embedder answers")
        .remove(0)
}

fn request<'a>(
    tenant: &'a spot_agent_memory::TenantId,
    vector: &'a [f32],
    filters: &'a Filters,
) -> RecallQuery<'a> {
    RecallQuery {
        tenant,
        agent_tier: Tier::T2,
        vector,
        filters,
        k: 5,
        now_ms: 10_000,
        recency: RecencyBoost::NONE,
    }
}

fn ids(hits: &[RecallHit]) -> Vec<String> {
    hits.iter().map(|hit| hit.provenance.id.clone()).collect()
}

#[test]
fn a_recall_ranks_by_similarity_and_reports_provenance() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    store
        .put(Entry::Episode(episode_saying(
            "tenant_a",
            1,
            "the job stayed stuck in running",
            1_000,
        )))
        .expect("the first episode");
    store
        .put(Entry::Episode(episode_saying(
            "tenant_a",
            2,
            "the cache flush cleared the stale lease",
            1_000,
        )))
        .expect("the second episode");

    let vector = query_vector("stuck in running job");
    let filters = Filters::any();
    let hits = store
        .recall(&request(&fixture.tenant, &vector, &filters))
        .expect("the recall");
    assert_eq!(ids(&hits), ["episode.e1", "episode.e2"]);
    assert!(hits[0].score_bp > hits[1].score_bp);
    assert_eq!(hits[0].provenance.kind, MemoryKind::Episode);
    assert_eq!(
        hits[0].provenance.source,
        spot_agent_memory::SourceKind::Incident
    );
    assert_eq!(hits[0].provenance.audits.len(), 1);
    assert_eq!(hits[0].provenance.epoch, None);
    assert_eq!(hits[0].text, "the job stayed stuck in running");
    assert_eq!(hits[0].provenance.tenant, fixture.tenant);
}

#[test]
fn the_same_request_gives_the_same_order_and_the_same_scores() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    for n in 1..=3 {
        store
            .put(Entry::Episode(episode_saying(
                "tenant_a",
                n,
                "the job stayed stuck in running",
                1_000 * i64::from(n),
            )))
            .expect("the episode");
    }
    let vector = query_vector("stuck job");
    let filters = Filters::any();
    let first = store
        .recall(&request(&fixture.tenant, &vector, &filters))
        .expect("the recall");
    let second = store
        .recall(&request(&fixture.tenant, &vector, &filters))
        .expect("the recall again");
    assert_eq!(first, second);
}

#[test]
fn structured_filters_narrow_the_answer() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    store
        .put(Entry::Episode(episode_saying(
            "tenant_a",
            1,
            "stuck job",
            1_000,
        )))
        .expect("the episode");
    store
        .put(Entry::Fact(fact("tenant_a", 1)))
        .expect("the fact");
    let vector = query_vector("stuck job backend");
    let tenant = &fixture.tenant;

    let hits = store
        .recall(&request(tenant, &vector, &Filters::any()))
        .expect("the recall");
    assert_eq!(hits.len(), 2);
    assert!(
        hits.iter()
            .any(|hit| hit.provenance.kind == MemoryKind::Fact)
    );

    let only_facts = Filters::kinds(&[MemoryKind::Fact]);
    let hits = store
        .recall(&request(tenant, &vector, &only_facts))
        .expect("the recall");
    assert_eq!(ids(&hits), ["fact.f1"]);

    let by_symptom = Filters {
        symptom: Some(Symptom::parse("job.stuck_running").expect("the symptom")),
        ..Filters::any()
    };
    let hits = store
        .recall(&request(tenant, &vector, &by_symptom))
        .expect("the recall");
    assert_eq!(ids(&hits), ["episode.e1"]);

    let too_late = Filters {
        since_ms: Some(2_000),
        ..Filters::any()
    };
    assert!(
        store
            .recall(&request(tenant, &vector, &too_late))
            .expect("the recall")
            .is_empty()
    );

    let too_confident = Filters {
        min_confidence: Some(spot_agent_memory::Confidence::FULL),
        ..Filters::any()
    };
    assert_eq!(
        store
            .recall(&request(tenant, &vector, &too_confident))
            .expect("the recall")
            .len(),
        2,
        "both test entries carry full confidence"
    );
}

#[test]
fn a_filter_on_a_remedy_uses_the_stored_remedy_set() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    store
        .put(Entry::Procedure(procedure("tenant_a", 1)))
        .expect("the procedure");
    store
        .put(Entry::Episode(episode_saying(
            "tenant_a",
            2,
            "stuck job",
            1_000,
        )))
        .expect("the episode");
    let vector = query_vector("stuck job retry");
    let by_remedy = Filters {
        remedy: Some(RemedyId::new("job.retry_failed")),
        ..Filters::any()
    };
    let hits = store
        .recall(&request(&fixture.tenant, &vector, &by_remedy))
        .expect("the recall");
    assert_eq!(ids(&hits), ["procedure.p1"]);
    assert_eq!(hits[0].provenance.remedies.len(), 1);
}

#[test]
fn a_procedure_above_the_readers_tier_is_never_returned() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    store
        .put(Entry::Procedure(procedure("tenant_a", 1)))
        .expect("the procedure");
    let vector = query_vector("stuck job retry");
    let filters = Filters::any();
    let mut low = request(&fixture.tenant, &vector, &filters);
    low.agent_tier = Tier::T0;
    assert!(
        store.recall(&low).expect("the recall").is_empty(),
        "a T0 reader is not shown a T2 procedure"
    );
    let mut high = request(&fixture.tenant, &vector, &filters);
    high.agent_tier = Tier::T2;
    assert_eq!(store.recall(&high).expect("the recall").len(), 1);
}

#[test]
fn an_expired_entry_is_not_recalled() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    let mut short = episode_saying("tenant_a", 1, "stuck job", 1_000);
    short.meta.ttl_ms = Some(500);
    store.put(Entry::Episode(short)).expect("the short one");
    let mut long = episode_saying("tenant_a", 2, "stuck job", 1_000);
    long.meta.ttl_ms = Some(100_000);
    store.put(Entry::Episode(long)).expect("the long one");

    let vector = query_vector("stuck job");
    let filters = Filters::any();
    let mut request = request(&fixture.tenant, &vector, &filters);
    request.now_ms = 2_000;
    assert_eq!(
        ids(&store.recall(&request).expect("the recall")),
        ["episode.e2"]
    );
}

#[test]
fn the_recency_boost_lifts_a_fresher_entry_above_an_older_one() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    // The same words, so the similarity is identical and only recency can separate them.
    store
        .put(Entry::Episode(episode_saying(
            "tenant_a",
            1,
            "stuck job",
            1_000,
        )))
        .expect("the old one");
    store
        .put(Entry::Episode(episode_saying(
            "tenant_a",
            2,
            "stuck job",
            9_000,
        )))
        .expect("the new one");

    let vector = query_vector("stuck job");
    let filters = Filters::any();
    let mut request = request(&fixture.tenant, &vector, &filters);
    // Twelve seconds after the oldest entry, over a ten-second window: the older entry is past
    // the window and gains nothing, the newer one is three seconds old and keeps most of the cap.
    request.now_ms = 12_000;
    let plain = store.recall(&request).expect("the recall");
    assert_eq!(
        plain[0].score_bp, plain[1].score_bp,
        "similarity alone ties"
    );
    assert_eq!(
        ids(&plain),
        ["episode.e1", "episode.e2"],
        "ties break by id"
    );

    request.recency = RecencyBoost::new(500, 10_000).expect("the boost");
    let boosted = store.recall(&request).expect("the recall");
    assert_eq!(ids(&boosted), ["episode.e2", "episode.e1"]);
    assert_eq!(
        boosted[0].score_bp,
        boosted[1].score_bp + 350,
        "three seconds of a ten-second window keeps 350 of the 500 basis points"
    );
    // An entry older than the window gains nothing at all.
    assert_eq!(boosted[1].score_bp, plain[1].score_bp);
}

#[test]
fn a_query_for_another_tenant_or_another_width_is_refused() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let store = &fixture.store;
    let vector = query_vector("stuck job");
    let filters = Filters::any();
    let stranger = common::tenant("tenant_b");
    let error = store
        .recall(&request(&stranger, &vector, &filters))
        .expect_err("another tenant is refused");
    assert_eq!(error.code(), "agent_memory.store_failed");

    let short = vec![0.0f32; EMBEDDING_DIM - 1];
    let error = store
        .recall(&request(&fixture.tenant, &short, &filters))
        .expect_err("a short vector is refused");
    assert_eq!(error.code(), "agent_memory.entry_invalid");
}

/// The plan assertion at 100 000 rows: the recall must reach the vector index, not the table.
///
/// The rows are written straight into the table (the store's own write path would take a minute
/// for 100 000 entries and prove nothing extra about the *plan*), and the index was built by
/// `prepare` before they arrived — which is what a tenant's database looks like once it has grown:
/// the table is filled and the index maintains itself.
#[test]
fn the_recall_plan_uses_the_vector_index_at_a_hundred_thousand_rows() {
    let Some(fixture) = fixture("tenant_a") else {
        return;
    };
    let client = fixture.rt.block_on(async { fixture.db.raw_client().await });
    fixture.rt.block_on(async {
        client
            .batch_execute(
                "INSERT INTO spot_memory_episode (tenant, id, incident_doctype, incident_id, \
                 symptoms, actions, outcome, summary, audits, remedies, tier, created_ms, ttl_ms, \
                 confidence_bp, source, embedding) \
                 SELECT 'tenant_a', 'episode.bulk.' || g, 'job', 'j-' || g, \
                        ARRAY['job.stuck_running'], '[]'::jsonb, 'resolved', 'bulk ' || g, \
                        ARRAY['audit-1'], ARRAY[]::text[], 0, 1000 + g, NULL, 10000, 'incident', \
                        (SELECT ('[' || string_agg(random()::text, ',') || ']')::vector \
                         FROM generate_series(1, 64)) \
                 FROM generate_series(1, 100000) AS g",
            )
            .await
            .expect("the bulk rows are inserted");
    });
    // Make the statistics good enough that the planner's choice reflects the data, not a guess.
    fixture.rt.block_on(async {
        client
            .batch_execute("ANALYZE spot_memory_episode")
            .await
            .expect("the table is analysed");
    });

    let vector = query_vector("stuck job");
    let filters = Filters::any();
    let mut request = request(&fixture.tenant, &vector, &filters);
    request.recency = RecencyBoost::DEFAULT;
    let plan = store_plan(&fixture.store, &request);

    assert!(
        plan.contains("Index Scan"),
        "the recall must use an index, not a scan:\n{plan}"
    );
    assert!(
        plan.contains("spot_memory_episode_embedding_hnsw"),
        "the recall must use the vector index:\n{plan}"
    );
    assert!(
        !plan.contains("Seq Scan on spot_memory_episode"),
        "the episode table must not be scanned:\n{plan}"
    );

    // The index method follows the size: at 100 000 rows the next `prepare` switches to IVFFlat
    // and drops the HNSW index, so a table that keeps growing does not pay for an HNSW build.
    fixture
        .store
        .prepare()
        .expect("the schema is prepared again");
    let index = fixture.rt.block_on(async {
        spot_agent_memory_pg::schema::recall_index_of(&client, "spot_memory_episode")
            .await
            .expect("the index is read")
    });
    assert_eq!(index, Some(spot_agent_memory_pg::RecallIndex::Ivfflat));
}

fn store_plan(store: &spot_agent_memory_pg::PgMemoryStore, request: &RecallQuery<'_>) -> String {
    store.recall_plan(request).expect("the plan is available")
}
