//! Behaviour the contract cases do not reach: waiting locks, concurrent writers, repeatable
//! read, commit-time re-checks, links to child rows, schema preparation and traversal limits.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use spot_storage_contract::contract_tests;
use spot_storage_contract::*;

use crate::FakeBackend;

type R = Result<(), StorageError>;

fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{e}"))
}

fn kind<T: std::fmt::Debug>(r: StorageResult<T>) -> Option<ErrorKind> {
    r.err().map(|e| e.kind)
}

/// The contract catalog plus `note`, a document linking to an invoice line.
fn catalog() -> StorageResult<Arc<Catalog>> {
    let mut entities: Vec<EntityDef> = contract_tests::catalog()?.entities().cloned().collect();
    entities.push(EntityDef {
        name: n("note"),
        kind: EntityKind::Document,
        fields: vec![FieldDef {
            name: n("line"),
            ty: FieldType::Link {
                target: LinkTarget::Entity(n("invoice_line")),
                check: LinkCheck::Database,
            },
            nullable: true,
        }],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    });
    Ok(Arc::new(Catalog::new(1, Collation::new("en")?, entities)?))
}

async fn store() -> StorageResult<(FakeBackend, Arc<Catalog>)> {
    let b = FakeBackend::new();
    let cat = catalog()?;
    b.prepare(&cat).await?;
    Ok((b, cat))
}

fn opts(isolation: Isolation) -> TxOptions {
    let mut o = TxOptions::new(n("unit_test"), isolation);
    o.lock_timeout = Duration::from_millis(100);
    o
}

async fn begin(b: &FakeBackend, cat: &Arc<Catalog>) -> StorageResult<crate::FakeTx> {
    b.begin(cat.clone(), opts(Isolation::ReadCommitted)).await
}

fn party(name: &str) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("name"), name.into());
    fields.insert(n("credit_limit"), Value::Decimal(0.into()));
    for f in [
        "parent_party",
        "note",
        "since",
        "opens_at",
        "seen_at",
        "active",
        "visits",
        "profile",
        "logo",
    ] {
        fields.insert(n(f), Value::Null);
    }
    DocumentData {
        entity: n("party"),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    }
}

fn line(item: &str) -> ChildRow {
    let mut fields = Fields::new();
    fields.insert(n("item"), item.into());
    fields.insert(n("qty"), Value::Decimal(1.into()));
    fields.insert(n("amount"), Value::Decimal(1.into()));
    ChildRow {
        id: RecordId::new_v7(),
        fields,
    }
}

fn invoice(number: &str, party: RecordId, lines: Vec<ChildRow>) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("number"), number.into());
    fields.insert(n("party"), Value::Id(party));
    fields.insert(n("total"), Value::Decimal(0.into()));
    fields.insert(n("ref_entity"), Value::Null);
    fields.insert(n("ref_id"), Value::Null);
    DocumentData {
        entity: n("invoice"),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::from([(n("lines"), lines)]),
    }
}

fn note(line: RecordId) -> DocumentData {
    DocumentData {
        entity: n("note"),
        id: RecordId::new_v7(),
        fields: Fields::from([(n("line"), Value::Id(line))]),
        children: BTreeMap::new(),
    }
}

async fn seed(b: &FakeBackend, cat: &Arc<Catalog>, docs: &[&DocumentData]) -> R {
    let mut tx = begin(b, cat).await?;
    for d in docs {
        tx.insert(d).await?;
    }
    tx.commit().await
}

#[tokio::test]
async fn blocked_lock_wakes_when_holder_commits() -> R {
    let (b, cat) = store().await?;
    let p = party("Acme");
    seed(&b, &cat, &[&p]).await?;
    let mut t1 = begin(&b, &cat).await?;
    t1.load(&p.entity, p.id, RowLock::Update(Wait::Block))
        .await?;
    let mut t2 = b
        .begin(
            cat.clone(),
            TxOptions::new(n("waiter"), Isolation::ReadCommitted),
        )
        .await?;
    let waited = AtomicBool::new(false);
    let (got, committed) = tokio::join!(
        async {
            let r = t2.load(&p.entity, p.id, RowLock::Update(Wait::Block)).await;
            waited.store(true, Ordering::SeqCst);
            r
        },
        async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let still_waiting = !waited.load(Ordering::SeqCst);
            (still_waiting, t1.commit().await)
        }
    );
    let (still_waiting, commit) = committed;
    commit?;
    assert!(still_waiting, "the second FOR UPDATE did not wait");
    assert!(got?.is_some());
    Ok(())
}

#[tokio::test]
async fn blocked_lock_times_out() -> R {
    let (b, cat) = store().await?;
    let key = LockKey {
        namespace: n("stock_balance"),
        key: "item-1".into(),
    };
    let mut t1 = begin(&b, &cat).await?;
    t1.lock_key(&key, LockMode::Exclusive, Wait::Block).await?;
    let mut t2 = begin(&b, &cat).await?;
    let started = Instant::now();
    let r = t2.lock_key(&key, LockMode::Shared, Wait::Block).await;
    assert_eq!(kind(r), Some(ErrorKind::LockTimeout));
    assert!(started.elapsed() >= Duration::from_millis(100));
    // Dropping the holder releases its locks.
    drop(t1);
    t2.lock_key(&key, LockMode::Exclusive, Wait::NoWait).await?;
    Ok(())
}

#[tokio::test]
async fn second_writer_waits_then_sees_stale_version() -> R {
    let (b, cat) = store().await?;
    let mut p = party("Acme");
    seed(&b, &cat, &[&p]).await?;
    p.fields.insert(n("visits"), Value::Int(1));
    let mut t1 = begin(&b, &cat).await?;
    assert_eq!(t1.update(&p, Version::FIRST).await?, Version(2));
    let mut t2 = begin(&b, &cat).await?;
    let waited = AtomicBool::new(false);
    let (second, committed) = tokio::join!(
        async {
            let r = t2.update(&p, Version::FIRST).await;
            waited.store(true, Ordering::SeqCst);
            r
        },
        async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            let still_waiting = !waited.load(Ordering::SeqCst);
            (still_waiting, t1.commit().await)
        }
    );
    let (still_waiting, commit) = committed;
    commit?;
    assert!(
        still_waiting,
        "the second update did not wait for the row lock"
    );
    assert_eq!(kind(second), Some(ErrorKind::StaleVersion));
    Ok(())
}

#[tokio::test]
async fn removing_a_linked_child_row_is_a_reference_violation() -> R {
    let (b, cat) = store().await?;
    let p = party("Acme");
    let (keep, linked) = (line("keep"), line("linked"));
    let mut inv = invoice("INV-1", p.id, vec![keep.clone(), linked.clone()]);
    let nt = note(linked.id);
    seed(&b, &cat, &[&p, &inv, &nt]).await?;
    inv.children.insert(n("lines"), vec![keep]);
    let mut tx = begin(&b, &cat).await?;
    tx.update(&inv, Version::FIRST).await?;
    let err = tx.commit().await.err();
    assert_eq!(
        err.as_ref().map(|e| e.kind),
        Some(ErrorKind::ReferenceViolation)
    );
    assert_eq!(err.as_ref().and_then(|e| e.entity.clone()), Some(n("note")));
    assert_eq!(err.and_then(|e| e.field), Some(n("line")));
    // Nothing was committed: the row is still there.
    let mut tx = begin(&b, &cat).await?;
    let stored = tx.load(&inv.entity, inv.id, RowLock::None).await?;
    assert_eq!(stored.map(|d| d.data.children[&n("lines")].len()), Some(2));
    // A new link to a missing child row fails too.
    let dangling = note(RecordId::new_v7());
    tx.insert(&dangling).await?;
    assert_eq!(kind(tx.commit().await), Some(ErrorKind::ReferenceViolation));
    Ok(())
}

#[tokio::test]
async fn repeatable_read_sees_its_snapshot_and_fails_on_conflict() -> R {
    let (b, cat) = store().await?;
    let mut p = party("Acme");
    seed(&b, &cat, &[&p]).await?;
    let mut rr = b
        .begin(cat.clone(), opts(Isolation::RepeatableRead))
        .await?;
    assert!(rr.load(&p.entity, p.id, RowLock::None).await?.is_some());
    let late = party("Late");
    seed(&b, &cat, &[&late]).await?;
    p.fields.insert(n("visits"), Value::Int(5));
    let mut rc = begin(&b, &cat).await?;
    rc.update(&p, Version::FIRST).await?;
    rc.commit().await?;
    // The snapshot neither sees the new party nor the update.
    assert!(
        rr.load(&late.entity, late.id, RowLock::None)
            .await?
            .is_none()
    );
    let seen = rr.load(&p.entity, p.id, RowLock::None).await?;
    assert_eq!(seen.map(|d| d.version), Some(Version::FIRST));
    assert_eq!(
        kind(rr.update(&p, Version::FIRST).await),
        Some(ErrorKind::SerializationFailure)
    );
    assert_eq!(
        kind(rr.load(&p.entity, p.id, RowLock::Share(Wait::NoWait)).await),
        Some(ErrorKind::SerializationFailure)
    );
    Ok(())
}

#[tokio::test]
async fn commit_rechecks_unique_keys_and_child_row_owners() -> R {
    let (b, cat) = store().await?;
    let mut t1 = begin(&b, &cat).await?;
    let mut t2 = begin(&b, &cat).await?;
    t1.insert(&party("Same")).await?;
    // t1's write is invisible to t2, so t2's write-time check passes.
    t2.insert(&party("Same")).await?;
    t1.commit().await?;
    let err = t2.commit().await.err();
    assert_eq!(
        err.as_ref().map(|e| e.kind),
        Some(ErrorKind::UniqueViolation)
    );
    assert_eq!(err.and_then(|e| e.constraint), Some(n("party_name")));

    let p = party("Acme");
    seed(&b, &cat, &[&p]).await?;
    let shared_line = line("x");
    let mut t1 = begin(&b, &cat).await?;
    let mut t2 = begin(&b, &cat).await?;
    t1.insert(&invoice("A", p.id, vec![shared_line.clone()]))
        .await?;
    t2.insert(&invoice("B", p.id, vec![shared_line])).await?;
    t1.commit().await?;
    assert_eq!(kind(t2.commit().await), Some(ErrorKind::AlreadyExists));
    Ok(())
}

#[tokio::test]
async fn sequences_serialize_per_series() -> R {
    let (b, cat) = store().await?;
    let s = SeriesKey::new("SINV-")?;
    let mut t1 = begin(&b, &cat).await?;
    assert_eq!(t1.allocate(&s, 2).await?, 1);
    let mut t2 = begin(&b, &cat).await?;
    let (second, committed) = tokio::join!(t2.allocate(&s, 1), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        t1.commit().await
    });
    committed?;
    assert_eq!(
        second?, 3,
        "the waiter continues after the committed numbers"
    );
    // Another series is independent, and NoWait guard locks see the series lock.
    let mut t3 = begin(&b, &cat).await?;
    assert_eq!(t3.allocate(&SeriesKey::new("PINV-")?, 1).await?, 1);
    let key = LockKey {
        namespace: n("sequence"),
        key: "SINV-".into(),
    };
    assert_eq!(
        kind(t3.lock_key(&key, LockMode::Shared, Wait::NoWait).await),
        Some(ErrorKind::LockNotAvailable)
    );
    Ok(())
}

#[tokio::test]
async fn savepoints_nest_and_release() -> R {
    let (b, cat) = store().await?;
    let (a, c) = (party("A"), party("C"));
    let mut tx = begin(&b, &cat).await?;
    let s1 = tx.savepoint().await?;
    tx.insert(&a).await?;
    let s2 = tx.savepoint().await?;
    tx.insert(&c).await?;
    tx.release(s2).await?;
    assert_eq!(
        kind(tx.rollback_to(s2).await),
        Some(ErrorKind::InvalidValue)
    );
    tx.rollback_to(s1).await?;
    // s1 stays usable after rolling back to it.
    tx.insert(&c).await?;
    tx.rollback_to(s1).await?;
    tx.commit().await?;
    assert!(b.read_changes(FeedPosition::START, 10).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn changes_follow_write_order_within_a_transaction() -> R {
    let (b, cat) = store().await?;
    let p = party("Acme");
    let inv = invoice("INV-1", p.id, vec![line("a")]);
    // Written in reverse link order: the invoice first.
    seed(&b, &cat, &[&inv, &p]).await?;
    let changes = b.read_changes(FeedPosition::START, 10).await?;
    let ids: Vec<RecordId> = changes.iter().map(|c| c.document.id).collect();
    assert_eq!(ids, vec![inv.id, p.id]);
    assert!(changes.iter().all(|c| c.op == ChangeOp::Insert));
    Ok(())
}

#[tokio::test]
async fn misuse_is_rejected_with_typed_errors() -> R {
    let (b, cat) = store().await?;
    let p = party("Acme");
    seed(&b, &cat, &[&p]).await?;
    let mut too_big = opts(Isolation::ReadCommitted);
    too_big.max_writes = 2_000_000;
    assert_eq!(
        kind(b.begin(cat.clone(), too_big).await),
        Some(ErrorKind::InvalidValue)
    );
    let mut tx = begin(&b, &cat).await?;
    assert_eq!(
        kind(
            tx.load(&p.entity, p.id, RowLock::Update(Wait::SkipLocked))
                .await
        ),
        Some(ErrorKind::InvalidQuery)
    );
    assert_eq!(
        kind(tx.load(&n("invoice_line"), p.id, RowLock::None).await),
        Some(ErrorKind::UnknownEntity)
    );
    let mut bad = invoice("INV-1", p.id, vec![]);
    bad.children.insert(n("items"), vec![]);
    assert_eq!(kind(tx.insert(&bad).await), Some(ErrorKind::InvalidValue));
    let mut ro = b
        .begin(cat.clone(), opts(Isolation::ReadCommitted).read_only())
        .await?;
    assert_eq!(
        kind(ro.load(&p.entity, p.id, RowLock::Share(Wait::Block)).await),
        Some(ErrorKind::ReadOnlyTransaction)
    );
    let mut q = Query::from(&p.entity, &n("p"), QueryPath::Report);
    assert_eq!(kind(tx.query(&q).await), Some(ErrorKind::InvalidQuery));
    q.select = vec![Selection {
        expr: Expr::field(&n("p"), &n("name")),
        alias: n("name"),
    }];
    assert_eq!(kind(tx.query(&q).await), None, "a valid query runs");
    let t = |max_depth, field: &str| Traversal {
        start: vec![NodeRef {
            entity: p.entity.clone(),
            id: p.id,
        }],
        edges: vec![EdgeSpec {
            entity: n("party"),
            field: n(field),
        }],
        direction: Direction::Outgoing,
        max_depth,
        max_nodes: 10,
    };
    assert_eq!(
        kind(tx.traverse(&t(0, "parent_party")).await),
        Some(ErrorKind::InvalidQuery)
    );
    assert_eq!(
        kind(tx.traverse(&t(65, "parent_party")).await),
        Some(ErrorKind::InvalidQuery)
    );
    assert_eq!(
        kind(tx.traverse(&t(1, "parent")).await),
        Some(ErrorKind::InvalidQuery)
    );
    assert_eq!(tx.traverse(&t(1, "parent_party")).await?.nodes.len(), 1);
    Ok(())
}

#[tokio::test]
async fn traversal_follows_dynamic_links() -> R {
    let (b, cat) = store().await?;
    let p = party("Acme");
    let mut inv = invoice("INV-1", p.id, vec![]);
    inv.fields.insert(n("ref_entity"), "party".into());
    inv.fields.insert(n("ref_id"), Value::Id(p.id));
    let mut tx = begin(&b, &cat).await?;
    tx.insert(&p).await?;
    tx.insert(&inv).await?;
    let edge = EdgeSpec {
        entity: n("invoice"),
        field: n("ref_id"),
    };
    let node = |d: &DocumentData| NodeRef {
        entity: d.entity.clone(),
        id: d.id,
    };
    // Uncommitted writes are visible to the transaction's own traversal.
    let out = tx
        .traverse(&Traversal {
            start: vec![node(&p)],
            edges: vec![edge.clone()],
            direction: Direction::Incoming,
            max_depth: 3,
            max_nodes: 10,
        })
        .await?;
    let reached: Vec<(NodeRef, u16)> = out.nodes.into_iter().map(|r| (r.node, r.depth)).collect();
    assert_eq!(reached, vec![(node(&p), 0), (node(&inv), 1)]);
    Ok(())
}

#[tokio::test]
async fn prepare_is_additive_only() -> R {
    let (b, cat) = store().await?;
    let acme = party("Acme");
    seed(&b, &cat, &[&acme]).await?;
    b.prepare(&cat).await?;
    let with_field = |nullable: bool| -> StorageResult<Catalog> {
        let entities = cat
            .entities()
            .cloned()
            .map(|mut e| {
                if e.name == n("party") {
                    e.fields.push(FieldDef {
                        name: n("rating"),
                        ty: FieldType::Int,
                        nullable,
                    });
                }
                e
            })
            .collect();
        Catalog::new(2, Collation::new("en")?, entities)
    };
    assert_eq!(
        kind(b.prepare(&with_field(false)?).await),
        Some(ErrorKind::InvalidSchema)
    );
    let wider = Arc::new(with_field(true)?);
    b.prepare(&wider).await?;
    let entities = cat.entities().cloned().collect();
    let other = Catalog::new(3, Collation::new("ur")?, entities)?;
    assert_eq!(
        kind(b.prepare(&other).await),
        Some(ErrorKind::InvalidSchema)
    );
    // Existing records read the new field as null.
    let mut tx = b
        .begin(wider.clone(), opts(Isolation::ReadCommitted))
        .await?;
    let loaded = tx.load(&acme.entity, acme.id, RowLock::None).await?;
    let rating = loaded.and_then(|d| d.data.fields.get(&n("rating")).cloned());
    assert_eq!(rating, Some(Value::Null));
    // A catalog without the field still loads exactly its own fields.
    let mut tx = begin(&b, &cat).await?;
    let loaded = tx.load(&acme.entity, acme.id, RowLock::None).await?;
    assert_eq!(loaded.map(|d| d.data.fields), Some(acme.fields));
    Ok(())
}

struct SuiteFixture;

impl contract_tests::Fixture for SuiteFixture {
    type Backend = FakeBackend;

    async fn fresh(&self) -> FakeBackend {
        FakeBackend::new()
    }
}

/// ADR-087, option B: the fake advertises that it has no geo filters, range, exclusion or
/// search, and each construct answers `Unsupported` from those flags.
#[tokio::test]
async fn the_fake_advertises_and_refuses_the_adr_087_constructs() -> R {
    let caps = FakeBackend::new().capabilities();
    assert_eq!(
        (caps.geo_filters, caps.range_exclusion, caps.search),
        (false, false, false)
    );
    assert_eq!(caps.contract_version, "0.3.0");

    // Geo points are stored: a catalog with a geo field prepares.
    let geo = Arc::new(contract_tests::geo_catalog()?);
    FakeBackend::new().prepare(&geo).await?;
    // Range fields, exclusions and search declarations are refused, naming what and where.
    let range = contract_tests::range_catalog()?;
    let err = FakeBackend::new().prepare(&range).await.err();
    assert_eq!(err.as_ref().map(|e| e.kind), Some(ErrorKind::Unsupported));
    assert_eq!(err.and_then(|e| e.entity), Some(n("stay")));
    let search = contract_tests::search_catalog()?;
    assert_eq!(
        kind(FakeBackend::new().prepare(&search).await),
        Some(ErrorKind::Unsupported)
    );
    Ok(())
}

/// The suite holds 30 base cases and 13 construct cases; every one runs and passes.
#[tokio::test]
async fn the_suite_has_43_cases_and_all_pass() {
    let report = contract_tests::run_all(&SuiteFixture).await;
    assert_eq!(report.outcomes.len(), 43);
    let constructs = contract_tests::run_matching(&SuiteFixture, |c| {
        contract_tests::CONSTRUCT_CASES.contains(&c)
    })
    .await;
    assert_eq!(
        constructs.outcomes.len(),
        contract_tests::CONSTRUCT_CASES.len()
    );
    report.assert_all_passed();
}

// ---- ADR-087 option B: geo storage, and every construct answered by the capability flags ------

/// A WGS-84 point from two written decimals, kept exactly as written (no float, no rounding).
fn point(lon: &str, lat: &str) -> GeoPoint {
    let read = |s: &str| {
        s.parse::<rust_decimal::Decimal>()
            .unwrap_or_else(|e| panic!("{e}"))
    };
    GeoPoint::new(read(lon), read(lat))
}

fn place(name: &str, pin: Option<GeoPoint>) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("name"), name.into());
    fields.insert(n("pin"), pin.map_or(Value::Null, Value::GeoPoint));
    DocumentData {
        entity: n("place"),
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    }
}

fn select(expr: Expr, alias: &str) -> Selection {
    Selection {
        expr,
        alias: n(alias),
    }
}

fn column(field: &str) -> Expr {
    Expr::Field(FieldRef {
        alias: n("i"),
        field: n(field),
    })
}

/// Deliverable 1 of WP-STO-10.T4 for the half the fake *does* model: a geo point is stored and
/// read back exactly at scale 7 (including the poles and a sub-centimetre longitude), a null is
/// not a point at 0,0, and a point outside the valid box or with more than seven places is
/// refused on the write with `InvalidValue`, never rounded and never approximated.
#[tokio::test]
async fn geo_points_round_trip_exactly_and_are_validated() -> R {
    let cat = Arc::new(contract_tests::geo_catalog()?);
    let b = FakeBackend::new();
    b.prepare(&cat).await?;
    let berlin = place("berlin", Some(point("13.4050000", "52.5200000")));
    let equator = place("equator", Some(point("-0.0000001", "0.0000001")));
    let corner = place("corner", Some(point("-180", "90")));
    let unset = place("unset", None);
    seed(&b, &cat, &[&berlin, &equator, &corner, &unset]).await?;

    // Read back through `load`: the value is the one written, decimals and scale included, so
    // nothing was rounded on the way through the store.
    let mut tx = begin(&b, &cat).await?;
    let loaded = tx
        .load(&n("place"), equator.id, RowLock::None)
        .await?
        .map(|d| d.data.fields);
    match loaded.as_ref().and_then(|f| f.get(&n("pin"))) {
        Some(Value::GeoPoint(p)) => {
            assert_eq!(*p, point("-0.0000001", "0.0000001"));
            assert_eq!((p.lon.scale(), p.lat.scale()), (7, 7));
        }
        other => panic!("the pin came back as {other:?}"),
    }
    // A null point stays null: it is not read as a point at 0,0.
    let loaded = tx.load(&n("place"), unset.id, RowLock::None).await?;
    assert_eq!(
        loaded.and_then(|d| d.data.fields.get(&n("pin")).cloned()),
        Some(Value::Null)
    );
    tx.rollback().await?;

    // Read back through a query, ordered by name: every point, exactly.
    let mut q = Query::from(&n("place"), &n("i"), QueryPath::Report);
    q.select.push(select(column("name"), "c0"));
    q.select.push(select(column("pin"), "c1"));
    q.order_by.push(Order {
        expr: column("name"),
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    });
    let rows = b.read(cat.clone(), ReadSource::Primary, &q).await?.rows;
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Text("berlin".into()),
                Value::GeoPoint(point("13.4050000", "52.5200000"))
            ],
            vec![
                Value::Text("corner".into()),
                Value::GeoPoint(point("-180", "90"))
            ],
            vec![
                Value::Text("equator".into()),
                Value::GeoPoint(point("-0.0000001", "0.0000001"))
            ],
            vec![Value::Text("unset".into()), Value::Null],
        ]
    );

    // Validation is the shared one (`FieldType::check`), so every backend refuses alike.
    for (pin, why) in [
        (point("0", "91"), "a latitude of 91"),
        (point("181", "0"), "a longitude of 181"),
        (point("13.12345678", "52.5"), "eight decimal places"),
    ] {
        let err = seed(&b, &cat, &[&place("bad", Some(pin))]).await.err();
        assert_eq!(
            err.as_ref().map(|e| e.kind),
            Some(ErrorKind::InvalidValue),
            "{why} was not refused"
        );
        assert_eq!(err.and_then(|e| e.field), Some(n("pin")), "{why}");
    }
    Ok(())
}

/// Deliverable 1, the half the fake does not model: `WithinDistance`, `WithinBox`, a range test,
/// an exclusion and a search are answered `Unsupported` (ADR-087 option B), and the answer comes
/// from the `Capabilities` flags — the same query and catalog are accepted as soon as the flag
/// says the construct is served. Nothing is approximated: a geo filter over stored points returns
/// no rows at all, not a bounding box or an equality.
#[tokio::test]
async fn every_construct_answer_comes_from_the_capability_flags() -> R {
    let caps = FakeBackend::new().capabilities();
    assert_eq!(
        (caps.geo_filters, caps.range_exclusion, caps.search),
        (false, false, false),
        "the fake models none of the ADR-087 constructs"
    );
    let mut served = caps.clone();
    served.geo_filters = true;
    served.range_exclusion = true;
    served.search = true;

    let field = column("pin");
    let berlin = point("13.4050", "52.5200");
    let distance = Expr::Geo(
        Box::new(field.clone()),
        GeoTest::WithinDistance {
            lon: berlin.lon,
            lat: berlin.lat,
            metres: 5_000,
        },
    );
    let constructs = [
        ("WithinDistance", distance.clone()),
        (
            "WithinBox",
            Expr::Geo(
                Box::new(field),
                GeoTest::WithinBox {
                    min_lon: point("13.0", "52.0").lon,
                    min_lat: point("13.0", "52.0").lat,
                    max_lon: point("13.5", "52.6").lon,
                    max_lat: point("13.5", "52.6").lat,
                },
            ),
        ),
        (
            "a range test",
            Expr::Range(
                Box::new(column("during")),
                RangeTest::Contains(Value::Int(2)),
            ),
        ),
        (
            "Search",
            Expr::Search(SearchTerm {
                mode: SearchMode::Web,
                text: "berlin".into(),
                fields: vec![n("title")],
            }),
        ),
        (
            "SearchRank",
            Expr::SearchRank(SearchTerm {
                mode: SearchMode::Plain,
                text: "berlin".into(),
                fields: vec![n("title")],
            }),
        ),
    ];

    for (what, expr) in &constructs {
        let mut q = Query::from(&n("place"), &n("i"), QueryPath::Report);
        q.select.push(select(column("name"), "c0"));
        q.filter = Some(expr.clone());
        let refused = caps.check_query(&q).err();
        assert_eq!(
            refused.as_ref().map(|e| e.kind),
            Some(ErrorKind::Unsupported),
            "the fake answered {what} with something other than Unsupported"
        );
        assert!(
            refused
                .unwrap_or_else(|| panic!("{what}"))
                .to_string()
                .contains("capabilities"),
            "{what} must name the capabilities as the source of the refusal"
        );
        assert!(
            served.check_query(&q).is_ok(),
            "{what} is refused by an ad hoc check, not by the capability flag"
        );
    }

    // The catalogs: a range field with its exclusion, and a search declaration.
    assert_eq!(
        caps.check_catalog(&contract_tests::range_catalog()?)
            .err()
            .map(|e| e.kind),
        Some(ErrorKind::Unsupported)
    );
    assert!(
        served
            .check_catalog(&contract_tests::range_catalog()?)
            .is_ok()
    );
    assert_eq!(
        caps.check_catalog(&contract_tests::search_catalog()?)
            .err()
            .map(|e| e.kind),
        Some(ErrorKind::Unsupported)
    );
    assert!(
        served
            .check_catalog(&contract_tests::search_catalog()?)
            .is_ok()
    );
    // A geo catalog needs no flag: storing a point is not a filter (SEM-GEO-04).
    assert!(caps.check_catalog(&contract_tests::geo_catalog()?).is_ok());

    // Through the backend's own paths: `prepare`, `read` and a transaction all answer
    // `Unsupported`, and the stored points are never matched by an approximation.
    let cat = Arc::new(contract_tests::geo_catalog()?);
    let b = FakeBackend::new();
    b.prepare(&cat).await?;
    seed(&b, &cat, &[&place("berlin", Some(berlin))]).await?;
    let mut q = Query::from(&n("place"), &n("i"), QueryPath::Report);
    q.select.push(select(column("name"), "c0"));
    q.filter = Some(distance.clone());
    q.validate(&cat)?;
    assert_eq!(
        kind(b.read(cat.clone(), ReadSource::Primary, &q).await),
        Some(ErrorKind::Unsupported)
    );
    let mut tx = begin(&b, &cat).await?;
    assert_eq!(kind(tx.query(&q).await), Some(ErrorKind::Unsupported));
    // Shared validation runs first: a bad point in the filter is `InvalidQuery`, not
    // `Unsupported`, so a caller sees what is wrong with the query before what is missing.
    let mut bad = Query::from(&n("place"), &n("i"), QueryPath::Report);
    bad.select.push(select(column("name"), "c0"));
    bad.filter = Some(Expr::Geo(
        Box::new(column("pin")),
        GeoTest::WithinDistance {
            lon: point("0", "0").lon,
            lat: point("0", "91").lat,
            metres: 100,
        },
    ));
    assert_eq!(
        kind(b.read(cat.clone(), ReadSource::Primary, &bad).await),
        Some(ErrorKind::InvalidQuery)
    );
    Ok(())
}
