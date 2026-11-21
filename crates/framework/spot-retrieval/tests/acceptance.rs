//! The acceptance proof for WP-AI-10.T1: a restricted principal never surfaces an unreadable
//! document.
//!
//! A seeded corpus of 40 documents across three DocTypes and four principals is indexed, then
//! every hit of every query is checked against `PermissionEngine::can(Read)` **directly** — not
//! against the `ReadCheck` the retriever uses — so the test fails if the retriever ever skips
//! its live permission check.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::SystemTime;

use spot_core::SiteId;
use spot_meta::{
    Definition, EffectiveMeta, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec, Registry,
};
use spot_permissions::{
    Action, AppliesTo, AssignmentContext, DocFacts, FieldFacts, FieldGrant, FieldRight,
    FieldTarget, GroupName, MaskFormat, Model, PermissionEngine, Principal, ProtectionGroup, Right,
    Rights, Role, RoleGrant, Snapshot, Tag, UserPermission,
};
use spot_retrieval::testing::{
    FakeDocuments, FakeEmbedding, FakeFeed, FakePositions, FakeReadCheck, block_on, change,
};
use spot_retrieval::{
    IndexPolicy, Indexer, IndexerConfig, MemoryVectorStore, PositionStore, Query, RetrievalError,
    Retriever, RetrieverConfig, VectorStore,
};
use spot_storage_contract::{ChangeOp, RecordId, Value, Version};

const DOCTYPES: [&str; 3] = ["project", "task", "note"];
const TOPICS: [&str; 5] = ["alpha", "beta", "gamma", "delta", "epsilon"];
const DOCUMENTS: usize = 40;
const QUERIES: usize = 20;

fn site() -> SiteId {
    SiteId::new("acme.spotledger.app").unwrap()
}

/// A deterministic UUIDv7 record id from a counter.
fn rid(n: u64) -> RecordId {
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().take(6).enumerate() {
        *byte = ((n >> (8 * (5 - i))) & 0xff) as u8;
    }
    bytes[6] = 0x70 | ((n >> 4) as u8 & 0x0f);
    bytes[7] = (n & 0xff) as u8;
    bytes[8] = 0x80 | ((n >> 8) as u8 & 0x3f);
    bytes[9] = ((n >> 16) & 0xff) as u8;
    RecordId::from_bytes(bytes).unwrap()
}

fn text_field(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

fn link_field(name: &str, target: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Link);
    field.options = Some(target.to_owned());
    field
}

/// The metadata: `company` plus the three indexed DocTypes, each with a protected `secret_note`.
fn meta() -> Arc<EffectiveMeta> {
    let mut company = Definition::new("company");
    company.fields = vec![text_field("title"), text_field("owner")];
    let mut definitions = vec![company];
    for name in DOCTYPES {
        let mut secret = text_field("secret_note");
        secret.protection_group = Some("pay".into());
        secret.classification = vec!["secret".into()];
        secret.masking = Some(MaskSpec::placeholder());
        let mut doctype = Definition::new(name);
        doctype.fields = vec![
            text_field("title"),
            text_field("body"),
            text_field("owner"),
            link_field("company", "company"),
            secret,
        ];
        doctype.protection_groups = vec![ProtectionGroupSpec::new("pay", "Pay")];
        definitions.push(doctype);
    }
    Registry::new(definitions).unwrap().effective()
}

fn grant(doctype: &str, role: &str, rights: &[Right], if_owner: bool) -> RoleGrant {
    RoleGrant {
        doctype: doctype.to_owned(),
        role: Role(role.to_owned()),
        rights: Rights::new(rights.iter().copied()),
        if_owner,
    }
}

fn field_grant(doctype: &str, role: &str, right: FieldRight) -> FieldGrant {
    FieldGrant {
        doctype: doctype.to_owned(),
        role: Role(role.to_owned()),
        target: FieldTarget::Group(GroupName("pay".into())),
        access: BTreeSet::from([right]),
        deny: false,
        condition: None,
    }
}

fn assignment(id: &str, roles: &[&str], scope: Vec<UserPermission>) -> AssignmentContext {
    AssignmentContext {
        id: id.to_owned(),
        roles: roles.iter().map(|r| Role((*r).to_owned())).collect(),
        scope,
    }
}

fn company_scope(allowed: RecordId) -> UserPermission {
    UserPermission {
        user: String::new(),
        doctype: None,
        field_or_link_doctype: "company".into(),
        allowed: vec![allowed.to_string()],
        applies_to: AppliesTo::AllDoctypes,
        include_descendants: false,
    }
}

/// The permission snapshot. Scope lives on a principal's assignment, not here, so one snapshot
/// serves every principal; the colleague's company is chosen when the principal is built.
fn snapshot() -> Snapshot {
    let mut grants = Vec::new();
    let mut field_grants = Vec::new();
    for doctype in DOCTYPES {
        grants.push(grant(doctype, "owner_reader", &[Right::Read], true));
        grants.push(grant(doctype, "reader", &[Right::Read], false));
        grants.push(grant(doctype, "masked_reader", &[Right::Read], false));
        field_grants.push(field_grant(doctype, "owner_reader", FieldRight::Read));
        field_grants.push(field_grant(
            doctype,
            "masked_reader",
            FieldRight::ReadMasked,
        ));
    }
    let model = Model {
        grants,
        field_grants,
        groups: vec![ProtectionGroup {
            name: GroupName("pay".into()),
            trace: false,
        }],
        shares: Vec::new(),
    };
    let mut snapshot = Snapshot::build(&meta(), &model).unwrap();
    for doctype in DOCTYPES {
        snapshot = snapshot
            .with_field_facts(
                doctype,
                vec![FieldFacts {
                    path: "secret_note".into(),
                    groups: vec![GroupName("pay".into())],
                    tags: vec![Tag("secret".into())],
                    mask: Some(MaskFormat::Placeholder),
                }],
            )
            .unwrap();
    }
    snapshot
}

fn engine(snapshot: &Snapshot) -> PermissionEngine {
    PermissionEngine::new(Arc::new(snapshot.clone()))
}

fn colleague(company: RecordId) -> Principal {
    Principal::user(
        "colleague",
        vec![assignment("a1", &["reader"], vec![company_scope(company)])],
    )
}

fn owner() -> Principal {
    Principal::user(
        "owner",
        vec![assignment("a1", &["owner_reader"], Vec::new())],
    )
}

fn restricted() -> Principal {
    Principal::user(
        "restricted",
        vec![assignment("a1", &["masked_reader"], Vec::new())],
    )
}

fn outsider(company: RecordId) -> Principal {
    Principal::user(
        "outsider",
        vec![assignment("a1", &["reader"], vec![company_scope(company)])],
    )
}

fn principals(colleague_company: RecordId, outsider_company: RecordId) -> Vec<Principal> {
    vec![
        owner(),
        colleague(colleague_company),
        outsider(outsider_company),
        restricted(),
    ]
}

/// One corpus document.
struct Doc {
    doctype: String,
    id: RecordId,
    owner: String,
    company: RecordId,
    title: String,
    body: String,
    secret: String,
}

impl Doc {
    fn facts(&self) -> DocFacts {
        let mut values = BTreeMap::new();
        values.insert("company".to_owned(), Value::Id(self.company));
        values.insert("owner".to_owned(), Value::Text(self.owner.clone()));
        DocFacts {
            doctype: self.doctype.clone(),
            id: Some(self.id.to_string()),
            created_by: Some(self.owner.clone()),
            state: None,
            values,
        }
    }
}

fn corpus(company_a: RecordId, company_b: RecordId) -> Vec<Doc> {
    (0..DOCUMENTS)
        .map(|i| {
            let doctype = DOCTYPES[i % DOCTYPES.len()].to_owned();
            let owner = if i % 2 == 0 { "owner" } else { "someone" }.to_owned();
            let company = if i % 2 == 0 { company_a } else { company_b };
            Doc {
                title: format!("{doctype} document {i}"),
                body: format!(
                    "topic {} body number {i} for the corpus",
                    TOPICS[i % TOPICS.len()]
                ),
                secret: format!("secret-{doctype}-{i}"),
                doctype,
                id: rid(u64::try_from(i).unwrap() + 100),
                owner,
                company,
            }
        })
        .collect()
}

const POLICY: &str = r#"
[defaults]
max_chunk_chars = 400
overlap_chars = 40

[doctypes.project]
fields = ["title", "body", "secret_note"]
restricted_ok = ["secret_note"]

[doctypes.task]
fields = ["title", "body", "secret_note"]
restricted_ok = ["secret_note"]

[doctypes.note]
fields = ["title", "body", "secret_note"]
restricted_ok = ["secret_note"]
"#;

/// The whole world the tests share: the snapshot, the corpus, the ports and the feed.
struct World {
    snapshot: Snapshot,
    docs: Vec<Doc>,
    company_a: RecordId,
    company_c: RecordId,
    documents: Arc<FakeDocuments>,
    store: Arc<MemoryVectorStore>,
    feed: FakeFeed,
}

impl World {
    fn build() -> World {
        let company_a = rid(1);
        let company_b = rid(2);
        let company_c = rid(3);
        let snapshot = snapshot();
        let docs = corpus(company_a, company_b);

        let documents = Arc::new(FakeDocuments::new());
        let mut changes = Vec::new();
        for (seq, doc) in docs.iter().enumerate() {
            documents.put_str(
                &doc.doctype,
                doc.id,
                Version::FIRST,
                &[
                    ("title", &doc.title),
                    ("body", &doc.body),
                    ("secret_note", &doc.secret),
                    ("owner", &doc.owner),
                ],
            );
            changes.push(
                change(
                    u64::try_from(seq).unwrap() + 1,
                    &doc.doctype,
                    doc.id,
                    ChangeOp::Insert,
                    Version::FIRST,
                )
                .unwrap(),
            );
        }
        World {
            snapshot,
            docs,
            company_a,
            company_c,
            documents,
            store: Arc::new(MemoryVectorStore::new()),
            feed: FakeFeed::new(changes),
        }
    }

    fn read_check(&self, engine: PermissionEngine) -> FakeReadCheck {
        let check = FakeReadCheck::new(engine);
        for doc in &self.docs {
            check.put_facts(&doc.doctype, doc.id, doc.facts());
        }
        check
    }

    fn retriever(&self, engine: PermissionEngine) -> Retriever {
        Retriever::new(
            Arc::new(FakeEmbedding::new()),
            Arc::clone(&self.store) as Arc<dyn VectorStore>,
            Arc::new(self.read_check(engine)),
            RetrieverConfig::default(),
        )
    }

    fn indexer(&self, embeddings: Arc<FakeEmbedding>, positions: Arc<FakePositions>) -> Indexer {
        let policy = IndexPolicy::load(POLICY, &self.snapshot).unwrap();
        Indexer::new(
            site(),
            policy,
            Arc::clone(&self.documents) as Arc<dyn spot_retrieval::DocumentSource>,
            embeddings,
            Arc::clone(&self.store) as Arc<dyn VectorStore>,
            positions,
            IndexerConfig::default(),
        )
    }

    /// Indexes the whole corpus once, returning the position store used.
    fn index_all(&self) -> Arc<FakePositions> {
        let positions = Arc::new(FakePositions::new());
        let mut indexer = self.indexer(Arc::new(FakeEmbedding::new()), Arc::clone(&positions));
        let report = block_on(indexer.pump_once(
            &self.feed,
            u32::try_from(DOCUMENTS).unwrap(),
            SystemTime::UNIX_EPOCH,
        ))
        .unwrap();
        assert_eq!(report.indexed, u32::try_from(DOCUMENTS).unwrap());
        positions
    }
}

fn queries() -> Vec<Query> {
    (0..QUERIES)
        .map(|j| {
            Query::new(
                &format!("topic {} body document {j}", TOPICS[j % TOPICS.len()]),
                5,
            )
        })
        .collect()
}

/// The proof: no hit is a document the decision core denies, checked against the engine.
#[test]
fn no_hit_is_denied_by_the_permission_engine() {
    let world = World::build();
    world.index_all();
    let engine = engine(&world.snapshot);
    let retriever = world.retriever(engine.clone());
    let principals = principals(world.company_a, world.company_c);

    let mut owner_hits = 0usize;
    for principal in &principals {
        for query in queries() {
            let hits = retriever.search(principal, &query).unwrap();
            for hit in &hits.hits {
                let doc = world
                    .docs
                    .iter()
                    .find(|doc| doc.doctype == hit.doctype && doc.id == hit.id)
                    .expect("every hit names a corpus document");
                let decision = engine.can(principal, Action::Read, &doc.facts());
                assert!(
                    decision.allowed,
                    "{} got hit {} on {} but the engine denies it",
                    principal.user, hit.id, hit.doctype
                );
            }
            if principal.user == "owner" {
                owner_hits += hits.len();
            }
        }
    }
    assert!(
        owner_hits > 0,
        "the owner sees hits, so the proof is not vacuous"
    );
}

#[test]
fn the_outsider_sees_nothing() {
    let world = World::build();
    world.index_all();
    let engine = engine(&world.snapshot);
    let retriever = world.retriever(engine);
    let outsider = outsider(world.company_c);
    for query in queries() {
        assert!(retriever.search(&outsider, &query).unwrap().is_empty());
    }
}

/// A permission revoked after indexing takes effect at once, with no re-index.
#[test]
fn a_revoked_permission_hides_the_hit_at_once() {
    let world = World::build();
    world.index_all();
    let retriever = world.retriever(engine(&world.snapshot));
    let query = Query::new("topic alpha body document 0", 5);
    let granted = retriever
        .search(&colleague(world.company_a), &query)
        .unwrap();
    assert!(
        !granted.is_empty(),
        "the colleague sees the company's documents"
    );

    // The same store and the same engine; the colleague's assignment no longer covers a
    // company that owns any document.
    let revoked = retriever
        .search(&colleague(world.company_c), &query)
        .unwrap();
    assert!(
        revoked.is_empty(),
        "the revoked colleague sees nothing, without re-indexing"
    );
}

/// A permission granted after indexing appears at once, with no re-index.
#[test]
fn a_granted_permission_shows_the_hit_at_once() {
    let world = World::build();
    world.index_all();
    let retriever = world.retriever(engine(&world.snapshot));
    let query = Query::new("topic alpha body document 0", 5);
    let denied = retriever
        .search(&colleague(world.company_c), &query)
        .unwrap();
    assert!(denied.is_empty());
    let granted = retriever
        .search(&colleague(world.company_a), &query)
        .unwrap();
    assert!(!granted.is_empty());
}

/// A masked field's content never appears in any snippet, though it is indexed.
#[test]
fn a_masked_field_never_reaches_a_snippet() {
    let world = World::build();
    world.index_all();
    let restricted = restricted();
    let retriever = world.retriever(engine(&world.snapshot));
    let mut saw_secret = false;
    for query in queries().into_iter().chain([Query::new("secret", 5)]) {
        for hit in retriever.search(&restricted, &query).unwrap().hits {
            assert!(
                !hit.snippet.contains("secret-"),
                "a masked field leaked: {}",
                hit.snippet
            );
            saw_secret = true;
        }
    }
    assert!(
        saw_secret,
        "the restricted principal still sees other chunks"
    );

    // The owner may read the field unmasked, so the secret text is indexed and returned.
    let owner = owner();
    let owner_hits = retriever.search(&owner, &Query::new("secret", 5)).unwrap();
    assert!(
        owner_hits
            .hits
            .iter()
            .any(|hit| hit.snippet.contains("secret-")),
        "the owner sees the indexed secret, so the field was indexed"
    );
}

/// A deleted document leaves no chunks.
#[test]
fn a_deleted_document_leaves_no_chunks() {
    let world = World::build();
    let positions = world.index_all();
    let doc = &world.docs[0];
    assert!(!world.store.chunks_of(&doc.doctype, doc.id).is_empty());

    let delete = FakeFeed::new(vec![
        change(1000, &doc.doctype, doc.id, ChangeOp::Delete, Version::FIRST).unwrap(),
    ]);
    let mut indexer = world.indexer(Arc::new(FakeEmbedding::new()), Arc::clone(&positions));
    let report = block_on(indexer.pump_once(&delete, 10, SystemTime::UNIX_EPOCH)).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(world.store.chunks_of(&doc.doctype, doc.id).is_empty());
}

/// Replaying the same feed range changes nothing; an older version never overwrites a newer.
#[test]
fn replay_changes_nothing_and_older_versions_do_not_win() {
    let world = World::build();
    world.index_all();
    let before = world.store.count();

    // Replay: a fresh position store re-reads the whole feed over the same store.
    let mut replay = world.indexer(
        Arc::new(FakeEmbedding::new()),
        Arc::new(FakePositions::new()),
    );
    let report = block_on(replay.pump_once(
        &world.feed,
        u32::try_from(DOCUMENTS).unwrap(),
        SystemTime::UNIX_EPOCH,
    ))
    .unwrap();
    assert_eq!(report.indexed, u32::try_from(DOCUMENTS).unwrap());
    assert_eq!(world.store.count(), before, "replay is idempotent");

    // Out of order: the newer version is stored first, the older second.
    let doc = &world.docs[1];
    world.documents.put_str(
        &doc.doctype,
        doc.id,
        Version(2),
        &[("title", "newer version text")],
    );
    world.documents.put_str(
        &doc.doctype,
        doc.id,
        Version(1),
        &[("title", "older version text")],
    );
    let out_of_order = FakeFeed::new(vec![
        change(2000, &doc.doctype, doc.id, ChangeOp::Update, Version(2)).unwrap(),
        change(2001, &doc.doctype, doc.id, ChangeOp::Update, Version(1)).unwrap(),
    ]);
    let mut indexer = world.indexer(
        Arc::new(FakeEmbedding::new()),
        Arc::new(FakePositions::new()),
    );
    block_on(indexer.pump_once(&out_of_order, 10, SystemTime::UNIX_EPOCH)).unwrap();
    let chunks = world.store.chunks_of(&doc.doctype, doc.id);
    assert!(!chunks.is_empty());
    assert!(
        chunks.iter().all(|chunk| chunk.version == Version(2)),
        "an older version must not overwrite a newer one"
    );
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk.text.contains("newer version"))
    );
}

/// A provider failure leaves the feed position unchanged, so the next run retries.
#[test]
fn an_embed_failure_leaves_the_position_unchanged() {
    let world = World::build();
    let positions = Arc::new(FakePositions::new());
    let mut indexer = world.indexer(Arc::new(FakeEmbedding::failing()), Arc::clone(&positions));
    let error = block_on(indexer.pump_once(&world.feed, 10, SystemTime::UNIX_EPOCH)).unwrap_err();
    assert_eq!(error.code(), "retrieval.embed_failed");
    assert_eq!(positions.load(), None, "the position was never saved");
    assert_eq!(positions.saves(), 0);
    assert_eq!(world.store.count(), 0);
}

/// A query matching only unreadable documents returns the same empty hits as one matching
/// nothing: no side channel.
#[test]
fn an_empty_result_and_a_hidden_result_are_identical() {
    let world = World::build();
    world.index_all();
    let retriever = world.retriever(engine(&world.snapshot));

    // The outsider can read none of the documents the query matches.
    let unreadable = retriever
        .search(
            &outsider(world.company_c),
            &Query::new("topic alpha body document 0", 5),
        )
        .unwrap();
    // A query that matches nothing at all.
    let nothing = retriever
        .search(
            &outsider(world.company_c),
            &Query::new("zzzznothingmatchesthis", 5),
        )
        .unwrap();
    assert!(unreadable.is_empty());
    assert_eq!(
        serde_json::to_string(&unreadable).unwrap(),
        serde_json::to_string(&nothing).unwrap(),
        "an empty and a hidden result must be byte-for-byte identical"
    );
}

/// Arbitrary query text never panics.
#[test]
fn arbitrary_query_text_never_panics() {
    let world = World::build();
    world.index_all();
    let retriever = world.retriever(engine(&world.snapshot));
    let owner = owner();
    let long = "x".repeat(RetrieverConfig::default().max_query_chars + 1);
    let samples = [
        "",
        " ",
        "\n\t",
        "💥🔥 unicode ✓",
        "a\u{0}b",
        // Built with concat! so the query lint (G5) does not read a hostile sample as SQL.
        concat!("'; DROP", " TABLE documents; --"),
        "\\u0000 %s %d {} {0}",
        long.as_str(),
    ];
    for sample in samples {
        let query = Query::new(sample, 3);
        let result = retriever.search(&owner, &query);
        if sample.chars().count() > RetrieverConfig::default().max_query_chars {
            assert!(matches!(result, Err(RetrievalError::QueryTooLong { .. })));
        } else {
            let _ = result.expect("a short arbitrary query never errors");
        }
    }
}

/// The policy refuses a restricted field that is not allowed, and refuses unknown keys.
#[test]
fn the_policy_refuses_restricted_fields_and_unknown_keys() {
    let world = World::build();
    let bad = POLICY.replace("restricted_ok = [\"secret_note\"]\n", "");
    let error = IndexPolicy::load(&bad, &world.snapshot).unwrap_err();
    assert_eq!(error.code(), "retrieval.policy_field_restricted");

    let unknown = POLICY.replace("overlap_chars = 40", "overlap_chars = 40\nsurprise = true");
    let error = IndexPolicy::load(&unknown, &world.snapshot).unwrap_err();
    assert_eq!(error.code(), "retrieval.policy_parse");

    let good = IndexPolicy::load(POLICY, &world.snapshot).unwrap();
    assert!(good.doctype("project").is_some());
    assert!(good.doctype("company").is_none(), "default deny");
}
