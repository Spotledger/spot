//! The contract test suite: behaviour every backend must show (blueprint §3, layer L2).
//!
//! A backend runs the suite from its own tests with a [`Fixture`] that hands out fresh, empty
//! stores, then asserts the [`Report`]:
//!
//! ```ignore
//! #[tokio::test]
//! async fn storage_contract() {
//!     spot_storage_contract::contract_tests::run_all(&MyFixture).await.assert_all_passed();
//! }
//! ```
//!
//! Each case runs on its own fresh store prepared with [`catalog`]. Cases open at most two
//! transactions at a time and never wait on a lock, so the suite runs on any executor.
//!
//! # The cases
//!
//! The suite has **43** cases: the **30** of v0.1 (documents, constraints, queries, locks,
//! sequences, feeds, traversal) and **13** construct cases added in v0.2.0 (ADR-087), listed in
//! [`CONSTRUCT_CASES`]. A construct case prepares its own catalog ([`geo_catalog`],
//! [`range_catalog`], [`search_catalog`]) on a fresh store.
//!
//! The construct cases are capability-driven (option B, ADR-087 sections 4.5, 5.5, 7.9): where
//! the backend's [`Capabilities`] advertise a construct, the case runs the real behaviour on it
//! (these are the **PostgreSQL-only** cases, [`REAL_CONSTRUCT_CASES`]); where they do not (the
//! in-memory fake), the case asserts that the construct answers `Unsupported` and that nothing
//! is approximated. Shared validation (`InvalidValue`, `InvalidQuery`, `InvalidSchema`) is
//! asserted on every backend, advertised or not, because it runs before any capability check.
//! Concurrency under load (200+ sessions) is the L7 invariant suite's job (WP-TST-07).

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::ops::Bound;
use std::sync::Arc;

use rust_decimal::Decimal;
use time::{Date, Month, OffsetDateTime, Time, UtcOffset};

use crate::*;

/// Why a case failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure(pub String);

impl From<StorageError> for Failure {
    fn from(e: StorageError) -> Self {
        Failure(format!("unexpected storage error: {e}"))
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Result of one case.
pub type CaseResult = Result<(), Failure>;

/// Supplies fresh, empty stores to the suite.
pub trait Fixture: Sync {
    /// The backend under test.
    type Backend: StorageBackend;

    /// A new, empty store.
    fn fresh(&self) -> impl Future<Output = Self::Backend> + Send;
}

/// The outcome of one case.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// Case name.
    pub case: &'static str,
    /// Its result.
    pub result: CaseResult,
}

/// Outcomes of a full run.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// One per case, in run order.
    pub outcomes: Vec<Outcome>,
}

impl Report {
    /// The failed cases.
    pub fn failures(&self) -> Vec<&Outcome> {
        self.outcomes.iter().filter(|o| o.result.is_err()).collect()
    }

    /// Panics with every failure listed, unless all cases passed.
    pub fn assert_all_passed(&self) {
        let failed = self.failures();
        if !failed.is_empty() {
            let lines: Vec<String> = failed
                .iter()
                .map(|o| {
                    format!(
                        "  {}: {}",
                        o.case,
                        o.result.as_ref().err().map_or("", |f| &f.0)
                    )
                })
                .collect();
            panic!(
                "{} of {} storage contract cases failed:\n{}",
                failed.len(),
                self.outcomes.len(),
                lines.join("\n")
            );
        }
    }
}

/// Runs every case, each on a fresh store.
pub async fn run_all<F: Fixture>(fixture: &F) -> Report {
    run_matching(fixture, |_| true).await
}

/// Cases that need the query IR (directly or to check their results). A backend under
/// construction may run the other cases first with
/// `run_matching(f, |c| !QUERY_CASES.contains(&c))`; a finished backend runs them all.
pub const QUERY_CASES: &[&str] = &[
    "update_replaces_children_in_order",
    "delete_removes_children",
    "null_and_empty_text_are_distinct",
    "query_filter_order_page",
    "query_join_and_aggregate",
    "query_is_validated",
    "text_match_is_literal",
    "text_sorts_by_collation",
    "row_lock_nowait",
    "read_outside_transaction",
    "geo_filters_are_validated",
    "geo_filters_follow_capabilities",
    "range_construct_follows_capabilities",
    "range_queries_are_validated",
    "exclusion_rejects_overlap_but_not_touching",
    "exclusion_error_names_no_other_record",
    "search_queries_are_validated",
    "search_follows_capabilities",
];

/// The construct cases added in v0.2.0 (ADR-087), in run order.
pub const CONSTRUCT_CASES: &[&str] = &[
    "geo_values_roundtrip_exactly",
    "geo_values_are_validated",
    "geo_filters_are_validated",
    "geo_filters_follow_capabilities",
    "constructs_are_validated_by_the_catalog",
    "range_construct_follows_capabilities",
    "range_values_are_validated",
    "range_values_roundtrip_exactly",
    "range_queries_are_validated",
    "exclusion_rejects_overlap_but_not_touching",
    "exclusion_error_names_no_other_record",
    "search_queries_are_validated",
    "search_follows_capabilities",
];

/// The construct cases that exercise the real construct, and therefore run their real branch
/// on PostgreSQL only: where the capability is absent the case checks `Unsupported` instead (or,
/// for the range value cases, has nothing to do and passes).
pub const REAL_CONSTRUCT_CASES: &[&str] = &[
    "geo_filters_follow_capabilities",
    "range_values_are_validated",
    "range_values_roundtrip_exactly",
    "exclusion_rejects_overlap_but_not_touching",
    "exclusion_error_names_no_other_record",
    "search_follows_capabilities",
];

/// Runs the cases whose names pass `include`, each on a fresh store.
pub async fn run_matching<F: Fixture>(fixture: &F, include: impl Fn(&str) -> bool) -> Report {
    let mut outcomes = Vec::new();
    macro_rules! case {
        ($($name:ident),* $(,)?) => {$(
            if include(stringify!($name)) {
            let result: CaseResult = async {
                let b = fixture.fresh().await;
                let cat = Arc::new(catalog()?);
                b.prepare(&cat).await?;
                $name(&b, &cat).await
            }
            .await;
            outcomes.push(Outcome { case: stringify!($name), result });
            }
        )*};
    }
    case!(
        capabilities_and_health,
        document_roundtrip,
        insert_existing_id_fails,
        update_replaces_children_in_order,
        stale_version_is_rejected,
        delete_removes_children,
        rollback_discards_documents_outbox_and_numbers,
        savepoint_rollback,
        unique_violation_is_typed,
        writes_are_checked_not_coerced,
        values_roundtrip_exactly,
        null_and_empty_text_are_distinct,
        query_filter_order_page,
        query_join_and_aggregate,
        query_is_validated,
        text_match_is_literal,
        text_sorts_by_collation,
        links_are_checked_by_commit,
        linked_document_cannot_be_deleted,
        dynamic_links_are_not_checked_by_storage,
        row_lock_nowait,
        guard_lock_nowait,
        sequences_are_gapless,
        outbox_never_skips_and_keeps_order,
        change_feed_reports_committed_writes,
        stale_bookmarks_require_resync,
        traversal_follows_links_and_stops_on_cycles,
        read_only_transaction_rejects_writes,
        write_limit_is_enforced,
        read_outside_transaction,
    );
    macro_rules! construct_case {
        ($($name:ident),* $(,)?) => {$(
            if include(stringify!($name)) {
                let result: CaseResult = async {
                    let b = fixture.fresh().await;
                    $name(&b).await
                }
                .await;
                outcomes.push(Outcome { case: stringify!($name), result });
            }
        )*};
    }
    construct_case!(
        geo_values_roundtrip_exactly,
        geo_values_are_validated,
        geo_filters_are_validated,
        geo_filters_follow_capabilities,
        constructs_are_validated_by_the_catalog,
        range_construct_follows_capabilities,
        range_values_are_validated,
        range_values_roundtrip_exactly,
        range_queries_are_validated,
        exclusion_rejects_overlap_but_not_touching,
        exclusion_error_names_no_other_record,
        search_queries_are_validated,
        search_follows_capabilities,
    );
    Report { outcomes }
}

/// The catalog every case runs against (collation `en`): parties with a self-link (a
/// cycle-capable hierarchy), invoices linking to parties plus a dynamic link, and invoice lines
/// as a child table.
pub fn catalog() -> StorageResult<Catalog> {
    let f = |name: &str, ty: FieldType, nullable: bool| -> StorageResult<FieldDef> {
        Ok(FieldDef {
            name: Name::new(name)?,
            ty,
            nullable,
        })
    };
    let text = FieldType::Text {
        max_chars: Some(140),
    };
    let money = FieldType::Decimal {
        precision: 18,
        scale: 2,
    };
    let link = |t: &str| -> StorageResult<FieldType> {
        Ok(FieldType::Link {
            target: LinkTarget::Entity(Name::new(t)?),
            check: LinkCheck::Database,
        })
    };
    let index = |name: &str, fields: &[&str], unique: bool| -> StorageResult<IndexDef> {
        Ok(IndexDef::btree(
            Name::new(name)?,
            fields
                .iter()
                .map(|s| Name::new(s))
                .collect::<StorageResult<_>>()?,
            unique,
        ))
    };
    Catalog::new(
        1,
        Collation::new("en")?,
        vec![
            EntityDef {
                name: Name::new("party")?,
                kind: EntityKind::Document,
                fields: vec![
                    f("name", text.clone(), false)?,
                    f("credit_limit", money.clone(), false)?,
                    f("parent_party", link("party")?, true)?,
                    f("note", FieldType::Text { max_chars: None }, true)?,
                    f("since", FieldType::Date, true)?,
                    f("opens_at", FieldType::Time, true)?,
                    f("seen_at", FieldType::DateTime, true)?,
                    f("active", FieldType::Bool, true)?,
                    f("visits", FieldType::Int, true)?,
                    f("profile", FieldType::Json, true)?,
                    f("logo", FieldType::Bytes, true)?,
                ],
                children: vec![],
                indexes: vec![index("party_name", &["name"], true)?],
                exclusions: vec![],
                search: None,
            },
            EntityDef {
                name: Name::new("invoice")?,
                kind: EntityKind::Document,
                fields: vec![
                    f("number", text, false)?,
                    f("party", link("party")?, false)?,
                    f("total", money.clone(), false)?,
                    f(
                        "ref_entity",
                        FieldType::Text {
                            max_chars: Some(63),
                        },
                        true,
                    )?,
                    f(
                        "ref_id",
                        FieldType::Link {
                            target: LinkTarget::Dynamic {
                                entity_field: Name::new("ref_entity")?,
                            },
                            check: LinkCheck::EngineOnly {
                                reason: "dynamic links are checked by the document engine".into(),
                            },
                        },
                        true,
                    )?,
                ],
                children: vec![ChildTableDef {
                    field: Name::new("lines")?,
                    entity: Name::new("invoice_line")?,
                }],
                indexes: vec![
                    index("invoice_number", &["number"], true)?,
                    index("invoice_party", &["party"], false)?,
                ],
                exclusions: vec![],
                search: None,
            },
            EntityDef {
                name: Name::new("invoice_line")?,
                kind: EntityKind::Child {
                    parent: Name::new("invoice")?,
                },
                fields: vec![
                    f(
                        "item",
                        FieldType::Text {
                            max_chars: Some(140),
                        },
                        false,
                    )?,
                    f(
                        "qty",
                        FieldType::Decimal {
                            precision: 18,
                            scale: 3,
                        },
                        false,
                    )?,
                    f("amount", money, false)?,
                ],
                children: vec![],
                indexes: vec![index("invoice_line_parent", &["parent", "idx"], false)?],
                exclusions: vec![],
                search: None,
            },
        ],
    )
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

macro_rules! ensure {
    ($cond:expr, $($msg:tt)+) => {
        if !$cond {
            return Err(Failure(format!($($msg)+)));
        }
    };
}

fn n(s: &str) -> Result<Name, Failure> {
    Ok(Name::new(s)?)
}

fn money(cents: i64) -> Value {
    Value::Decimal(Decimal::new(cents, 2))
}

fn col(alias: &str, field: &str) -> Result<Expr, Failure> {
    Ok(Expr::field(&n(alias)?, &n(field)?))
}

fn sel(expr: Expr, alias: &str) -> Result<Selection, Failure> {
    Ok(Selection {
        expr,
        alias: n(alias)?,
    })
}

fn expect_kind<T: fmt::Debug>(r: StorageResult<T>, kind: ErrorKind, what: &str) -> CaseResult {
    match r {
        Err(e) if e.kind == kind => Ok(()),
        Err(e) => Err(Failure(format!(
            "{what}: expected {}, got {e}",
            kind.code()
        ))),
        Ok(v) => Err(Failure(format!(
            "{what}: expected {}, got Ok({v:?})",
            kind.code()
        ))),
    }
}

async fn begin<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> Result<B::Tx, Failure> {
    Ok(b.begin(
        cat.clone(),
        TxOptions::new(n("contract_test")?, Isolation::ReadCommitted),
    )
    .await?)
}

/// A party with every optional field null.
fn party(name: &str, limit_cents: i64, parent: Option<RecordId>) -> Result<DocumentData, Failure> {
    let mut fields = Fields::new();
    fields.insert(n("name")?, name.into());
    fields.insert(n("credit_limit")?, money(limit_cents));
    fields.insert(n("parent_party")?, parent.map_or(Value::Null, Value::Id));
    for f in [
        "note", "since", "opens_at", "seen_at", "active", "visits", "profile", "logo",
    ] {
        fields.insert(n(f)?, Value::Null);
    }
    Ok(DocumentData {
        entity: n("party")?,
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    })
}

fn line(item: &str, qty_milli: i64, cents: i64) -> Result<ChildRow, Failure> {
    let mut fields = Fields::new();
    fields.insert(n("item")?, item.into());
    fields.insert(n("qty")?, Value::Decimal(Decimal::new(qty_milli, 3)));
    fields.insert(n("amount")?, money(cents));
    Ok(ChildRow {
        id: RecordId::new_v7(),
        fields,
    })
}

fn invoice(number: &str, party: RecordId, lines: Vec<ChildRow>) -> Result<DocumentData, Failure> {
    let mut total = Decimal::ZERO;
    for l in &lines {
        if let Some(Value::Decimal(d)) = l.fields.get(&n("amount")?) {
            total += *d;
        }
    }
    let mut fields = Fields::new();
    fields.insert(n("number")?, number.into());
    fields.insert(n("party")?, Value::Id(party));
    fields.insert(n("total")?, Value::Decimal(total));
    fields.insert(n("ref_entity")?, Value::Null);
    fields.insert(n("ref_id")?, Value::Null);
    let mut children = BTreeMap::new();
    children.insert(n("lines")?, lines);
    Ok(DocumentData {
        entity: n("invoice")?,
        id: RecordId::new_v7(),
        fields,
        children,
    })
}

/// Commits `docs` in one transaction.
async fn seed<B: StorageBackend>(b: &B, cat: &Arc<Catalog>, docs: &[&DocumentData]) -> CaseResult {
    let mut tx = begin(b, cat).await?;
    for d in docs {
        tx.insert(d).await?;
    }
    Ok(tx.commit().await?)
}

async fn load<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
    entity: &str,
    id: RecordId,
) -> Result<Option<Document>, Failure> {
    let mut tx = begin(b, cat).await?;
    let doc = tx.load(&n(entity)?, id, RowLock::None).await?;
    tx.rollback().await?;
    Ok(doc)
}

/// Party names matching `filter`, ordered by name.
async fn party_names<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
    filter: Option<Expr>,
) -> Result<Vec<String>, Failure> {
    let p = n("p")?;
    let mut q = Query::from(&n("party")?, &p, QueryPath::Report);
    q.select = vec![sel(col("p", "name")?, "name")?];
    q.filter = filter;
    q.order_by = vec![Order {
        expr: col("p", "name")?,
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    }];
    let mut tx = begin(b, cat).await?;
    let rows = tx.query(&q).await?;
    tx.rollback().await?;
    rows.rows
        .into_iter()
        .map(|r| match r.as_slice() {
            [Value::Text(s)] => Ok(s.clone()),
            other => Err(Failure(format!("expected one text column, got {other:?}"))),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------------------------

async fn capabilities_and_health<B: StorageBackend>(b: &B, _: &Arc<Catalog>) -> CaseResult {
    let caps = b.capabilities();
    ensure!(
        caps.contract_version == CONTRACT_VERSION,
        "contract version {}",
        caps.contract_version
    );
    ensure!(
        caps.read_sources.contains(&ReadSource::Primary),
        "Primary must be a read source"
    );
    ensure!(
        caps.max_writes_per_tx >= 10_000,
        "max_writes_per_tx below the standard 10,000"
    );
    let health = b.health().await;
    ensure!(
        health.status != HealthStatus::Down,
        "fresh backend reports Down: {health:?}"
    );
    Ok(())
}

async fn document_roundtrip<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let p = party("Acme", 100_000, None)?;
    let inv = invoice(
        "INV-1",
        p.id,
        vec![line("bolt", 2_000, 500)?, line("nut", 1_500, 250)?],
    )?;
    let mut tx = begin(b, cat).await?;
    ensure!(
        tx.insert(&p).await? == Version::FIRST,
        "insert must return version 1"
    );
    tx.insert(&inv).await?;
    let own = tx.load(&n("invoice")?, inv.id, RowLock::None).await?;
    ensure!(
        own.as_ref().map(|d| &d.data) == Some(&inv),
        "a transaction must see its own writes"
    );
    tx.commit().await?;
    let stored = load(b, cat, "invoice", inv.id).await?;
    ensure!(
        stored
            == Some(Document {
                data: inv,
                version: Version::FIRST
            }),
        "stored document differs: {stored:?}"
    );
    ensure!(
        load(b, cat, "party", RecordId::new_v7()).await?.is_none(),
        "missing id must load as None"
    );
    Ok(())
}

async fn insert_existing_id_fails<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let p = party("Acme", 0, None)?;
    seed(b, cat, &[&p]).await?;
    let mut again = party("Other", 0, None)?;
    again.id = p.id;
    let mut tx = begin(b, cat).await?;
    expect_kind(
        tx.insert(&again).await,
        ErrorKind::AlreadyExists,
        "insert of an existing id",
    )?;
    tx.rollback().await?;
    // A child row id may belong to one document only.
    let inv1 = invoice("INV-1", p.id, vec![line("a", 1_000, 100)?])?;
    let mut inv2 = invoice("INV-2", p.id, vec![])?;
    inv2.children
        .insert(n("lines")?, inv1.children[&n("lines")?].clone());
    seed(b, cat, &[&inv1]).await?;
    let mut tx = begin(b, cat).await?;
    expect_kind(
        tx.insert(&inv2).await,
        ErrorKind::AlreadyExists,
        "reuse of a child row id",
    )
}

async fn update_replaces_children_in_order<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
) -> CaseResult {
    let p = party("Acme", 0, None)?;
    let (a, bl, c) = (
        line("a", 1_000, 100)?,
        line("b", 1_000, 200)?,
        line("c", 1_000, 300)?,
    );
    let mut inv = invoice("INV-1", p.id, vec![a.clone(), bl, c.clone()])?;
    seed(b, cat, &[&p, &inv]).await?;
    // Reorder, change c, drop b, add d.
    let mut c2 = c.clone();
    c2.fields.insert(n("amount")?, money(333));
    let d = line("d", 1_000, 400)?;
    inv.children
        .insert(n("lines")?, vec![c2.clone(), a.clone(), d.clone()]);
    let mut tx = begin(b, cat).await?;
    let v = tx.update(&inv, Version::FIRST).await?;
    ensure!(v == Version(2), "update must return version 2, got {v:?}");
    tx.commit().await?;
    let stored = load(b, cat, "invoice", inv.id)
        .await?
        .ok_or(Failure("invoice vanished".into()))?;
    ensure!(
        stored.data.children[&n("lines")?] == vec![c2, a, d],
        "child rows not replaced in order"
    );
    ensure!(stored.version == Version(2), "stored version must be 2");
    // idx follows the new order; the kept row keeps its id.
    let l = n("l")?;
    let mut q = Query::from(&n("invoice_line")?, &l, QueryPath::Transactional);
    q.select = vec![sel(col("l", "idx")?, "idx")?];
    q.filter = Some(col("l", "id")?.eq(Expr::value(c.id)));
    let mut tx = begin(b, cat).await?;
    let rows = tx.query(&q).await?;
    ensure!(
        rows.rows == vec![vec![Value::Int(1)]],
        "moved row must have idx 1, got {:?}",
        rows.rows
    );
    Ok(())
}

async fn stale_version_is_rejected<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let mut p = party("Acme", 0, None)?;
    seed(b, cat, &[&p]).await?;
    p.fields.insert(n("credit_limit")?, money(5_000));
    let mut tx = begin(b, cat).await?;
    expect_kind(
        tx.update(&p, Version(7)).await,
        ErrorKind::StaleVersion,
        "update at wrong version",
    )?;
    tx.rollback().await?;
    let mut tx = begin(b, cat).await?;
    tx.update(&p, Version::FIRST).await?;
    tx.commit().await?;
    let mut tx = begin(b, cat).await?;
    expect_kind(
        tx.update(&p, Version::FIRST).await,
        ErrorKind::StaleVersion,
        "second update at v1",
    )?;
    tx.rollback().await?;
    let mut tx = begin(b, cat).await?;
    expect_kind(
        tx.delete(&n("party")?, p.id, Version::FIRST).await,
        ErrorKind::StaleVersion,
        "delete at stale version",
    )?;
    tx.rollback().await?;
    let mut tx = begin(b, cat).await?;
    let ghost = party("Ghost", 0, None)?;
    expect_kind(
        tx.update(&ghost, Version::FIRST).await,
        ErrorKind::NotFound,
        "update of missing doc",
    )
}

async fn delete_removes_children<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let p = party("Acme", 0, None)?;
    let inv = invoice(
        "INV-1",
        p.id,
        vec![line("a", 1_000, 100)?, line("b", 1_000, 100)?],
    )?;
    seed(b, cat, &[&p, &inv]).await?;
    let mut tx = begin(b, cat).await?;
    tx.delete(&n("invoice")?, inv.id, Version::FIRST).await?;
    tx.commit().await?;
    ensure!(
        load(b, cat, "invoice", inv.id).await?.is_none(),
        "deleted document still loads"
    );
    let l = n("l")?;
    let mut q = Query::from(&n("invoice_line")?, &l, QueryPath::Report);
    q.select = vec![sel(
        Expr::Agg {
            func: AggFunc::Count,
            arg: None,
            distinct: false,
        },
        "n",
    )?];
    let mut tx = begin(b, cat).await?;
    let rows = tx.query(&q).await?;
    ensure!(
        rows.rows == vec![vec![Value::Int(0)]],
        "child rows survived delete: {:?}",
        rows.rows
    );
    expect_kind(
        tx.delete(&n("invoice")?, inv.id, Version::FIRST).await,
        ErrorKind::NotFound,
        "delete of a deleted document",
    )
}

async fn rollback_discards_documents_outbox_and_numbers<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
) -> CaseResult {
    let p = party("Acme", 0, None)?;
    let series = SeriesKey::new("TEST-")?;
    let mut tx = begin(b, cat).await?;
    tx.insert(&p).await?;
    tx.enqueue(&event(&p, "party_created")?).await?;
    ensure!(
        tx.allocate(&series, 1).await? == 1,
        "first number must be 1"
    );
    tx.rollback().await?;
    ensure!(
        load(b, cat, "party", p.id).await?.is_none(),
        "rolled-back insert is visible"
    );
    ensure!(
        b.read_outbox(FeedPosition::START, 10).await?.is_empty(),
        "rolled-back event visible"
    );
    ensure!(
        b.read_changes(FeedPosition::START, 10).await?.is_empty(),
        "rolled-back change visible"
    );
    let mut tx = begin(b, cat).await?;
    ensure!(
        tx.allocate(&series, 1).await? == 1,
        "rolled-back number must be handed out again"
    );
    // Dropping without commit is a rollback too.
    tx.insert(&p).await?;
    drop(tx);
    ensure!(
        load(b, cat, "party", p.id).await?.is_none(),
        "dropped transaction was committed"
    );
    Ok(())
}

async fn savepoint_rollback<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let (keep, undo) = (party("Keep", 0, None)?, party("Undo", 0, None)?);
    let mut tx = begin(b, cat).await?;
    tx.insert(&keep).await?;
    let sp = tx.savepoint().await?;
    tx.insert(&undo).await?;
    tx.enqueue(&event(&undo, "party_created")?).await?;
    tx.rollback_to(sp).await?;
    // A failed insert inside a savepoint leaves the transaction usable after rollback_to.
    let mut dup = party("Dup", 0, None)?;
    dup.id = keep.id;
    expect_kind(
        tx.insert(&dup).await,
        ErrorKind::AlreadyExists,
        "duplicate inside savepoint",
    )?;
    tx.rollback_to(sp).await?;
    tx.release(sp).await?;
    tx.commit().await?;
    ensure!(
        load(b, cat, "party", keep.id).await?.is_some(),
        "work before the savepoint was lost"
    );
    ensure!(
        load(b, cat, "party", undo.id).await?.is_none(),
        "work after the savepoint survived"
    );
    ensure!(
        b.read_outbox(FeedPosition::START, 10).await?.is_empty(),
        "undone event was published"
    );
    Ok(())
}

async fn unique_violation_is_typed<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    seed(b, cat, &[&party("Acme", 0, None)?]).await?;
    let mut tx = begin(b, cat).await?;
    match tx.insert(&party("Acme", 0, None)?).await {
        Err(e) if e.kind == ErrorKind::UniqueViolation => {
            ensure!(
                e.constraint == Some(n("party_name")?),
                "violation must name the index: {e}"
            );
            Ok(())
        }
        other => Err(Failure(format!(
            "duplicate name: expected unique violation, got {other:?}"
        ))),
    }
}

async fn writes_are_checked_not_coerced<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
) -> CaseResult {
    let mut tx = begin(b, cat).await?;
    let mut sp = tx.savepoint().await?;
    type Mutation = Box<dyn Fn(&mut DocumentData) -> Result<(), Failure>>;
    let cases: Vec<(&str, Mutation)> = vec![
        (
            "three decimals in a 2-scale field",
            Box::new(|d: &mut DocumentData| {
                d.fields
                    .insert(n("credit_limit")?, Value::Decimal(Decimal::new(1_005, 3)));
                Ok(())
            }),
        ),
        (
            "null in a required field",
            Box::new(|d: &mut DocumentData| {
                d.fields.insert(n("name")?, Value::Null);
                Ok(())
            }),
        ),
        (
            "wrong type",
            Box::new(|d: &mut DocumentData| {
                d.fields.insert(n("visits")?, "many".into());
                Ok(())
            }),
        ),
        (
            "text over its limit",
            Box::new(|d: &mut DocumentData| {
                d.fields.insert(n("name")?, "x".repeat(141).into());
                Ok(())
            }),
        ),
        (
            "missing field",
            Box::new(|d: &mut DocumentData| {
                d.fields.remove(&n("note")?);
                Ok(())
            }),
        ),
        (
            "undeclared field",
            Box::new(|d: &mut DocumentData| {
                d.fields.insert(n("colour")?, "red".into());
                Ok(())
            }),
        ),
        (
            "invalid JSON",
            Box::new(|d: &mut DocumentData| {
                d.fields
                    .insert(n("profile")?, Value::Json("{not json".into()));
                Ok(())
            }),
        ),
    ];
    for (what, mutate) in cases {
        let mut p = party("Acme", 0, None)?;
        mutate(&mut p)?;
        expect_kind(tx.insert(&p).await, ErrorKind::InvalidValue, what)?;
        tx.rollback_to(sp).await?;
        sp = tx.savepoint().await?;
    }
    let mut line_doc = party("Acme", 0, None)?;
    line_doc.entity = n("invoice_line")?;
    expect_kind(
        tx.insert(&line_doc).await,
        ErrorKind::UnknownEntity,
        "direct write of a child entity",
    )
}

async fn values_roundtrip_exactly<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let mut p = party("Ünïcødé ✓", 10, None)?;
    let karachi = UtcOffset::from_hms(5, 0, 0).map_err(|e| Failure(e.to_string()))?;
    let seen = OffsetDateTime::from_unix_timestamp(1_790_000_000)
        .map_err(|e| Failure(e.to_string()))?
        .replace_millisecond(123)
        .map_err(|e| Failure(e.to_string()))?
        .to_offset(karachi);
    let date =
        Date::from_calendar_date(2026, Month::September, 26).map_err(|e| Failure(e.to_string()))?;
    let opens = Time::from_hms(9, 30, 0).map_err(|e| Failure(e.to_string()))?;
    p.fields.insert(
        n("credit_limit")?,
        Value::Decimal(Decimal::new(9_999_999_999_999_999, 2)),
    );
    p.fields.insert(n("since")?, Value::Date(date));
    p.fields.insert(n("opens_at")?, Value::Time(opens));
    p.fields.insert(n("seen_at")?, Value::DateTime(seen));
    p.fields.insert(n("active")?, Value::Bool(false));
    p.fields.insert(n("visits")?, Value::Int(i64::MIN));
    p.fields
        .insert(n("profile")?, Value::Json(r#"{"tier": "gold"}"#.into()));
    p.fields.insert(n("logo")?, Value::Bytes(vec![0, 255, 7]));
    p.fields.insert(n("parent_party")?, Value::Id(p.id));
    seed(b, cat, &[&p]).await?;
    let got = load(b, cat, "party", p.id)
        .await?
        .ok_or(Failure("party vanished".into()))?
        .data;
    for (k, want) in &p.fields {
        let have = got.fields.get(k);
        let same = match (want, have) {
            // JSON is compared after whitespace normalisation only in this suite.
            (Value::Json(a), Some(Value::Json(b))) => {
                a.split_whitespace().collect::<String>() == b.split_whitespace().collect::<String>()
            }
            (Value::DateTime(a), Some(Value::DateTime(b))) => a == b && b.offset().is_utc(),
            _ => Some(want) == have,
        };
        ensure!(same, "field {k}: stored {want:?}, loaded {have:?}");
    }
    Ok(())
}

async fn null_and_empty_text_are_distinct<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
) -> CaseResult {
    let null_note = party("A", 0, None)?;
    let mut empty_note = party("B", 0, None)?;
    empty_note
        .fields
        .insert(n("note")?, Value::Text(String::new()));
    let mut x_note = party("C", 0, None)?;
    x_note.fields.insert(n("note")?, "x".into());
    seed(b, cat, &[&null_note, &empty_note, &x_note]).await?;
    let note = col("p", "note")?;
    let empty = party_names(b, cat, Some(note.clone().eq(Expr::value("")))).await?;
    ensure!(empty == ["B"], "note = '' matched {empty:?}");
    let nulls = party_names(b, cat, Some(Expr::IsNull(Box::new(note.clone())))).await?;
    ensure!(nulls == ["A"], "note IS NULL matched {nulls:?}");
    let not_x = party_names(b, cat, Some(Expr::Not(Box::new(note.eq(Expr::value("x")))))).await?;
    ensure!(
        not_x == ["B"],
        "NOT (note = 'x') must exclude nulls, matched {not_x:?}"
    );
    let note_field = n("note")?;
    let loaded = load(b, cat, "party", empty_note.id)
        .await?
        .map(|d| d.data.fields[&note_field].clone());
    ensure!(
        loaded == Some(Value::Text(String::new())),
        "empty text loaded as {loaded:?}"
    );
    Ok(())
}

async fn query_filter_order_page<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let ps = [("A", 100), ("B", 500), ("C", 300), ("D", 500), ("E", 200)]
        .iter()
        .map(|(name, c)| party(name, c * 100, None))
        .collect::<Result<Vec<_>, _>>()?;
    seed(b, cat, &ps.iter().collect::<Vec<_>>()).await?;
    let p = n("p")?;
    let mut q = Query::from(&n("party")?, &p, QueryPath::Report);
    q.select = vec![
        sel(col("p", "name")?, "name")?,
        sel(col("p", "credit_limit")?, "limit")?,
    ];
    q.filter = Some(col("p", "credit_limit")?.cmp(CmpOp::Ge, Expr::Value(money(20_000))));
    q.order_by = vec![
        Order {
            expr: col("p", "credit_limit")?,
            dir: OrderDir::Desc,
            nulls: Nulls::Last,
        },
        Order {
            expr: col("p", "name")?,
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        },
    ];
    q.limit = Some(2);
    q.offset = 1;
    let mut tx = begin(b, cat).await?;
    let rows = tx.query(&q).await?;
    ensure!(
        rows.columns == vec![n("name")?, n("limit")?],
        "columns {:?}",
        rows.columns
    );
    let want = vec![
        vec!["D".into(), money(50_000)],
        vec!["C".into(), money(30_000)],
    ];
    ensure!(rows.rows == want, "page: got {:?}", rows.rows);
    Ok(())
}

async fn query_join_and_aggregate<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let p = party("Acme", 0, None)?;
    let i1 = invoice(
        "INV-1",
        p.id,
        vec![line("a", 1_000, 100)?, line("b", 2_000, 250)?],
    )?;
    let i2 = invoice("INV-2", p.id, vec![line("c", 1_000, 1_000)?])?;
    let i3 = invoice("INV-3", p.id, vec![])?;
    seed(b, cat, &[&p, &i1, &i2, &i3]).await?;
    // Per invoice: number, line count and amount sum, including an invoice without lines.
    let (i, l) = (n("i")?, n("l")?);
    let mut q = Query::from(&n("invoice")?, &i, QueryPath::Report);
    q.joins = vec![Join {
        kind: JoinKind::Left,
        source: Source {
            entity: n("invoice_line")?,
            alias: l,
        },
        on: col("l", "parent")?.eq(col("i", "id")?),
    }];
    let amount = Some(Box::new(col("l", "amount")?));
    q.select = vec![
        sel(col("i", "number")?, "number")?,
        sel(
            Expr::Agg {
                func: AggFunc::Count,
                arg: Some(Box::new(col("l", "id")?)),
                distinct: false,
            },
            "lines",
        )?,
        sel(
            Expr::Agg {
                func: AggFunc::Sum,
                arg: amount,
                distinct: false,
            },
            "amount",
        )?,
    ];
    q.group_by = vec![col("i", "number")?];
    q.order_by = vec![Order {
        expr: col("i", "number")?,
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    }];
    let mut tx = begin(b, cat).await?;
    let rows = tx.query(&q).await?;
    let want = vec![
        vec!["INV-1".into(), Value::Int(2), money(350)],
        vec!["INV-2".into(), Value::Int(1), money(1_000)],
        vec!["INV-3".into(), Value::Int(0), Value::Null],
    ];
    ensure!(rows.rows == want, "aggregate: got {:?}", rows.rows);
    Ok(())
}

async fn query_is_validated<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let p = n("p")?;
    let base = || -> Result<Query, Failure> {
        let mut q = Query::from(&n("party")?, &p, QueryPath::Report);
        q.select = vec![sel(col("p", "name")?, "name")?];
        Ok(q)
    };
    let mut tx = begin(b, cat).await?;
    let mut q = base()?;
    q.filter = Some(col("p", "colour")?.eq(Expr::value("red")));
    expect_kind(tx.query(&q).await, ErrorKind::UnknownField, "unknown field")?;
    let mut q = base()?;
    q.filter = Some(col("p", "note")?.eq(Expr::Value(Value::Null)));
    expect_kind(
        tx.query(&q).await,
        ErrorKind::InvalidQuery,
        "comparison with null",
    )?;
    let mut q = base()?;
    q.filter = Some(
        Expr::Agg {
            func: AggFunc::Count,
            arg: None,
            distinct: false,
        }
        .eq(Expr::value(1)),
    );
    expect_kind(
        tx.query(&q).await,
        ErrorKind::InvalidQuery,
        "aggregate in filter",
    )?;
    let mut q = base()?;
    q.select.push(sel(
        Expr::Agg {
            func: AggFunc::Count,
            arg: None,
            distinct: false,
        },
        "n",
    )?);
    expect_kind(
        tx.query(&q).await,
        ErrorKind::InvalidQuery,
        "ungrouped field",
    )?;
    let mut q = base()?;
    q.filter = Some(col("p", "credit_limit")?.eq(Expr::value("lots")));
    expect_kind(tx.query(&q).await, ErrorKind::InvalidQuery, "type mismatch")?;
    let mut q = Query::from(&n("nowhere")?, &p, QueryPath::Report);
    q.select = vec![sel(col("p", "name")?, "name")?];
    expect_kind(
        tx.query(&q).await,
        ErrorKind::UnknownEntity,
        "unknown entity",
    )
}

async fn text_match_is_literal<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let names = ["50% off", "500 units", "a_b", "axb", "back\\slash", "Axe"];
    let ps = names
        .iter()
        .map(|s| party(s, 0, None))
        .collect::<Result<Vec<_>, _>>()?;
    seed(b, cat, &ps.iter().collect::<Vec<_>>()).await?;
    let m = |mode, pattern: &str, ci| -> Result<Option<Expr>, Failure> {
        Ok(Some(Expr::Text(
            Box::new(col("p", "name")?),
            TextMatch {
                mode,
                pattern: pattern.into(),
                case_insensitive: ci,
            },
        )))
    };
    let got = party_names(b, cat, m(TextMode::Contains, "50%", false)?).await?;
    ensure!(got == ["50% off"], "contains '50%' matched {got:?}");
    let got = party_names(b, cat, m(TextMode::Equals, "a_b", false)?).await?;
    ensure!(got == ["a_b"], "equals 'a_b' matched {got:?}");
    let got = party_names(b, cat, m(TextMode::Suffix, "\\slash", false)?).await?;
    ensure!(
        got == ["back\\slash"],
        "suffix with backslash matched {got:?}"
    );
    let got = party_names(b, cat, m(TextMode::Prefix, "AX", true)?).await?;
    ensure!(
        got == ["axb", "Axe"],
        "case-insensitive prefix 'AX' matched {got:?}"
    );
    let got = party_names(b, cat, m(TextMode::Prefix, "ax", false)?).await?;
    ensure!(got == ["axb"], "case-sensitive prefix 'ax' matched {got:?}");
    Ok(())
}

async fn text_sorts_by_collation<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let names = [
        "Zebra", "apple", "Apple", "éclair", "eclair", "banana", "Ali", "ali",
    ];
    let ps = names
        .iter()
        .map(|s| party(s, 0, None))
        .collect::<Result<Vec<_>, _>>()?;
    // "Ali" and "ali" are different keys: the unique index is exact, not collation-equal.
    seed(b, cat, &ps.iter().collect::<Vec<_>>()).await?;
    let got = party_names(b, cat, None).await?;
    let want = [
        "ali", "Ali", "apple", "Apple", "banana", "eclair", "éclair", "Zebra",
    ];
    ensure!(got == want, "`en` collation order: got {got:?}");
    // Range comparisons follow the collation too: "Zebra" is not below "b".
    let below_b = col("p", "name")?.cmp(CmpOp::Lt, Expr::value("b"));
    let got = party_names(b, cat, Some(below_b)).await?;
    ensure!(
        got == ["ali", "Ali", "apple", "Apple"],
        "name < 'b' matched {got:?}"
    );
    let exact = party_names(b, cat, Some(col("p", "name")?.eq(Expr::value("ali")))).await?;
    ensure!(
        exact == ["ali"],
        "equality must stay exact, matched {exact:?}"
    );
    Ok(())
}

/// Commits after `outcome` succeeded; returns the first error.
async fn then_commit<T: Transaction>(tx: T, outcome: StorageResult<()>) -> StorageResult<()> {
    match outcome {
        Ok(()) => tx.commit().await,
        Err(e) => Err(e),
    }
}

fn expect_reference(r: StorageResult<()>, entity: &str, field: &str, what: &str) -> CaseResult {
    match r {
        Err(e) if e.kind == ErrorKind::ReferenceViolation => {
            ensure!(
                e.entity == Some(n(entity)?) && e.field == Some(n(field)?),
                "{what}: the violation must name {entity}.{field}: {e}"
            );
            Ok(())
        }
        other => Err(Failure(format!(
            "{what}: expected a reference violation by commit, got {other:?}"
        ))),
    }
}

async fn links_are_checked_by_commit<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let orphan = invoice("INV-1", RecordId::new_v7(), vec![])?;
    let mut tx = begin(b, cat).await?;
    let outcome = tx.insert(&orphan).await.map(|_| ());
    expect_reference(
        then_commit(tx, outcome).await,
        "invoice",
        "party",
        "link to a missing party",
    )?;
    ensure!(
        load(b, cat, "invoice", orphan.id).await?.is_none(),
        "orphan invoice was committed"
    );
    // Within one transaction, order does not matter and links may form a cycle.
    let (ida, idb) = (RecordId::new_v7(), RecordId::new_v7());
    let mut pa = party("A", 0, Some(idb))?;
    pa.id = ida;
    let mut pb = party("B", 0, Some(ida))?;
    pb.id = idb;
    let inv = invoice("INV-2", ida, vec![])?;
    seed(b, cat, &[&inv, &pa, &pb]).await
}

async fn linked_document_cannot_be_deleted<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
) -> CaseResult {
    let p = party("Acme", 0, None)?;
    let inv = invoice("INV-1", p.id, vec![])?;
    seed(b, cat, &[&p, &inv]).await?;
    let (party_e, invoice_e) = (n("party")?, n("invoice")?);
    let mut tx = begin(b, cat).await?;
    let outcome = tx.delete(&party_e, p.id, Version::FIRST).await;
    expect_reference(
        then_commit(tx, outcome).await,
        "invoice",
        "party",
        "delete of a linked party",
    )?;
    ensure!(
        load(b, cat, "party", p.id).await?.is_some(),
        "linked party was deleted"
    );
    // Deleting the linking document in the same transaction makes it allowed; nothing cascades.
    let mut tx = begin(b, cat).await?;
    tx.delete(&party_e, p.id, Version::FIRST).await?;
    tx.delete(&invoice_e, inv.id, Version::FIRST).await?;
    Ok(tx.commit().await?)
}

async fn dynamic_links_are_not_checked_by_storage<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
) -> CaseResult {
    let p = party("Acme", 0, None)?;
    let mut inv = invoice("INV-1", p.id, vec![])?;
    inv.fields.insert(n("ref_entity")?, "party".into());
    inv.fields
        .insert(n("ref_id")?, Value::Id(RecordId::new_v7()));
    seed(b, cat, &[&p, &inv]).await?;
    ensure!(
        load(b, cat, "invoice", inv.id).await?.is_some(),
        "dynamic link was checked by storage"
    );
    Ok(())
}

async fn row_lock_nowait<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let p = party("Acme", 0, None)?;
    seed(b, cat, &[&p]).await?;
    let entity = n("party")?;
    let mut t1 = begin(b, cat).await?;
    t1.load(&entity, p.id, RowLock::Update(Wait::Block)).await?;
    let mut t2 = begin(b, cat).await?;
    expect_kind(
        t2.load(&entity, p.id, RowLock::Update(Wait::NoWait)).await,
        ErrorKind::LockNotAvailable,
        "second FOR UPDATE NOWAIT",
    )?;
    t2.rollback().await?;
    // A plain read is never blocked by a row lock.
    let mut t3 = begin(b, cat).await?;
    ensure!(
        t3.load(&entity, p.id, RowLock::None).await?.is_some(),
        "plain read blocked"
    );
    t3.rollback().await?;
    // SKIP LOCKED leaves the locked row out.
    let mut t4 = begin(b, cat).await?;
    let mut q = Query::from(&entity, &n("p")?, QueryPath::Transactional);
    q.select = vec![sel(col("p", "id")?, "id")?];
    q.lock = RowLock::Update(Wait::SkipLocked);
    ensure!(
        t4.query(&q).await?.rows.is_empty(),
        "SKIP LOCKED returned a locked row"
    );
    t4.rollback().await?;
    t1.commit().await?;
    let mut t5 = begin(b, cat).await?;
    ensure!(
        t5.load(&entity, p.id, RowLock::Update(Wait::NoWait))
            .await?
            .is_some(),
        "lock not released at commit"
    );
    Ok(())
}

async fn guard_lock_nowait<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let key = LockKey {
        namespace: n("stock_balance")?,
        key: "item-1/wh-1".into(),
    };
    let mut t1 = begin(b, cat).await?;
    t1.lock_key(&key, LockMode::Shared, Wait::Block).await?;
    let mut t2 = begin(b, cat).await?;
    t2.lock_key(&key, LockMode::Shared, Wait::NoWait).await?;
    t2.rollback().await?;
    let mut t2 = begin(b, cat).await?;
    expect_kind(
        t2.lock_key(&key, LockMode::Exclusive, Wait::NoWait).await,
        ErrorKind::LockNotAvailable,
        "exclusive guard while shared is held",
    )?;
    t2.rollback().await?;
    t1.lock_key(&key, LockMode::Exclusive, Wait::Block).await?; // upgrade by the holder
    t1.rollback().await?;
    let mut t3 = begin(b, cat).await?;
    t3.lock_key(&key, LockMode::Exclusive, Wait::NoWait).await?;
    Ok(t3.commit().await?)
}

async fn sequences_are_gapless<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let (s, other) = (SeriesKey::new("SINV-2026-")?, SeriesKey::new("PINV-2026-")?);
    let mut tx = begin(b, cat).await?;
    ensure!(tx.allocate(&s, 1).await? == 1, "new series starts at 1");
    ensure!(tx.allocate(&s, 3).await? == 2, "block of 3 starts at 2");
    ensure!(tx.allocate(&other, 1).await? == 1, "series are independent");
    tx.commit().await?;
    let mut tx = begin(b, cat).await?;
    ensure!(tx.allocate(&s, 1).await? == 5, "next after 2..4 is 5");
    tx.rollback().await?;
    let mut tx = begin(b, cat).await?;
    ensure!(
        tx.allocate(&s, 1).await? == 5,
        "rolled-back 5 must be reused"
    );
    expect_kind(
        tx.set_next(&s, 3).await,
        ErrorKind::InvalidValue,
        "set_next reusing numbers",
    )?;
    tx.rollback().await?;
    let mut tx = begin(b, cat).await?;
    tx.set_next(&s, 100).await?;
    ensure!(
        tx.allocate(&s, 1).await? == 100,
        "set_next(100) then allocate"
    );
    expect_kind(
        tx.allocate(&s, 0).await,
        ErrorKind::InvalidValue,
        "allocate zero numbers",
    )
}

fn event(doc: &DocumentData, topic: &str) -> Result<OutboxEvent, Failure> {
    Ok(OutboxEvent {
        id: RecordId::new_v7(),
        topic: n(topic)?,
        subject: NodeRef {
            entity: doc.entity.clone(),
            id: doc.id,
        },
        subject_version: None,
        schema_version: 1,
        payload: r#"{"ok":true}"#.into(),
    })
}

fn event_ids(records: &[OutboxRecord]) -> Vec<RecordId> {
    records.iter().map(|r| r.event.id).collect()
}

async fn outbox_never_skips_and_keeps_order<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
) -> CaseResult {
    let p = party("Acme", 0, None)?;
    let (e1, e2, e3) = (event(&p, "a")?, event(&p, "b")?, event(&p, "c")?);
    // t1 starts first and stays open while t2 commits: the classic lost-event race.
    let mut t1 = begin(b, cat).await?;
    t1.enqueue(&e1).await?;
    t1.enqueue(&e2).await?;
    let mut t2 = begin(b, cat).await?;
    t2.enqueue(&e3).await?;
    t2.commit().await?;
    let first = b.read_outbox(FeedPosition::START, 10).await?;
    ensure!(
        event_ids(&first) == vec![e3.id],
        "only committed events may be published, got {:?}",
        event_ids(&first)
    );
    let bookmark = first[0].position;
    t1.commit().await?;
    let second = b.read_outbox(bookmark, 10).await?;
    ensure!(
        event_ids(&second) == vec![e1.id, e2.id],
        "a later commit must appear after the bookmark, in enqueue order; got {:?}",
        event_ids(&second)
    );
    ensure!(
        second[0].transaction == second[1].transaction
            && second[0].transaction != first[0].transaction,
        "events must carry their transaction id"
    );
    ensure!(
        second.iter().map(|r| r.ordinal).collect::<Vec<_>>() == vec![0, 1],
        "ordinals must follow enqueue order"
    );
    // Causal order: ea commits, then eb is written and committed; one read publishes both.
    let ea = event(&p, "d")?;
    let mut eb = event(&p, "e")?;
    eb.subject_version = Some(Version(2));
    let mut ta = begin(b, cat).await?;
    ta.enqueue(&ea).await?;
    ta.commit().await?;
    let mut tb = begin(b, cat).await?;
    tb.enqueue(&eb).await?;
    tb.commit().await?;
    let third = b.read_outbox(second[1].position, 10).await?;
    ensure!(
        event_ids(&third) == vec![ea.id, eb.id],
        "causal order: got {:?}",
        event_ids(&third)
    );
    ensure!(third[1].event == eb, "event content must round-trip");
    let all = b.read_outbox(FeedPosition::START, 10).await?;
    ensure!(
        event_ids(&all) == vec![e3.id, e1.id, e2.id, ea.id, eb.id],
        "whole log: got {:?}",
        event_ids(&all)
    );
    ensure!(
        all.windows(2).all(|w| w[0].position < w[1].position),
        "positions must increase"
    );
    let page = b.read_outbox(all[0].position, 1).await?;
    ensure!(event_ids(&page) == vec![e1.id], "limit must be respected");
    // An open transaction's events stay invisible.
    let mut open = begin(b, cat).await?;
    open.enqueue(&event(&p, "f")?).await?;
    ensure!(
        b.read_outbox(all[4].position, 10).await?.is_empty(),
        "an open transaction's event was published"
    );
    open.rollback().await?;
    let mut tx = begin(b, cat).await?;
    expect_kind(
        tx.enqueue(&e1).await,
        ErrorKind::AlreadyExists,
        "duplicate event id",
    )
}

async fn change_feed_reports_committed_writes<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
) -> CaseResult {
    let mut p = party("Acme", 0, None)?;
    seed(b, cat, &[&p]).await?;
    let mut tx = begin(b, cat).await?;
    p.fields.insert(n("credit_limit")?, money(100));
    tx.update(&p, Version::FIRST).await?;
    tx.commit().await?;
    let mut tx = begin(b, cat).await?;
    tx.delete(&p.entity, p.id, Version(2)).await?;
    tx.commit().await?;
    let changes = b.read_changes(FeedPosition::START, 10).await?;
    let seen: Vec<(ChangeOp, Version)> = changes.iter().map(|c| (c.op, c.version)).collect();
    let want = vec![
        (ChangeOp::Insert, Version(1)),
        (ChangeOp::Update, Version(2)),
        (ChangeOp::Delete, Version(2)),
    ];
    ensure!(seen == want, "change feed: got {seen:?}");
    let node = |d: &DocumentData| NodeRef {
        entity: d.entity.clone(),
        id: d.id,
    };
    ensure!(changes.iter().all(|c| c.document == node(&p)), "subject");
    ensure!(
        changes
            .windows(2)
            .all(|w| w[0].position < w[1].position && w[0].transaction != w[1].transaction),
        "positions must increase; each entry here comes from its own transaction"
    );
    let tail = b.read_changes(changes[1].position, 10).await?;
    ensure!(
        tail.len() == 1 && tail[0].op == ChangeOp::Delete,
        "read after position"
    );
    // One net entry per document per transaction: insert + update is one Insert at the final
    // version; insert + delete leaves nothing.
    let mut q = party("Quick", 0, None)?;
    let gone = party("Gone", 0, None)?;
    let mut tx = begin(b, cat).await?;
    tx.insert(&q).await?;
    q.fields.insert(n("credit_limit")?, money(1));
    tx.update(&q, Version::FIRST).await?;
    tx.insert(&gone).await?;
    tx.delete(&gone.entity, gone.id, Version::FIRST).await?;
    tx.commit().await?;
    let net: Vec<(NodeRef, ChangeOp, Version)> = b
        .read_changes(changes[2].position, 10)
        .await?
        .into_iter()
        .map(|c| (c.document, c.op, c.version))
        .collect();
    ensure!(
        net == vec![(node(&q), ChangeOp::Insert, Version(2))],
        "net change per transaction: got {net:?}"
    );
    Ok(())
}

async fn stale_bookmarks_require_resync<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
) -> CaseResult {
    let p = party("Acme", 0, None)?;
    let mut tx = begin(b, cat).await?;
    tx.insert(&p).await?;
    tx.enqueue(&event(&p, "party_created")?).await?;
    tx.commit().await?;
    let out = b.read_outbox(FeedPosition::START, 10).await?;
    let chg = b.read_changes(FeedPosition::START, 10).await?;
    let (o, c) = match (out.first(), chg.first()) {
        (Some(o), Some(c)) => (o.position, c.position),
        _ => return Err(Failure("committed entries were not published".into())),
    };
    for (what, pos) in [
        (
            "another generation",
            FeedPosition::new(o.generation().wrapping_add(1), o.seq()),
        ),
        (
            "beyond the end",
            FeedPosition::new(o.generation(), o.seq() + 1_000),
        ),
    ] {
        expect_kind(
            b.read_outbox(pos, 10).await,
            ErrorKind::ResyncRequired,
            &format!("outbox position from {what}"),
        )?;
    }
    for (what, pos) in [
        (
            "another generation",
            FeedPosition::new(c.generation().wrapping_add(1), c.seq()),
        ),
        (
            "beyond the end",
            FeedPosition::new(c.generation(), c.seq() + 1_000),
        ),
    ] {
        expect_kind(
            b.read_changes(pos, 10).await,
            ErrorKind::ResyncRequired,
            &format!("change-feed position from {what}"),
        )?;
    }
    ensure!(
        b.read_outbox(o, 10).await?.is_empty(),
        "reading at the head must give an empty page"
    );
    Ok(())
}

async fn traversal_follows_links_and_stops_on_cycles<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
) -> CaseResult {
    // a -> b -> c -> a (parent_party), and invoice x -> a (party) with one line.
    let (ida, idb, idc) = (RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7());
    let mk = |name: &str, id, parent| -> Result<DocumentData, Failure> {
        let mut p = party(name, 0, Some(parent))?;
        p.id = id;
        Ok(p)
    };
    let (a, pb, c) = (mk("a", ida, idb)?, mk("b", idb, idc)?, mk("c", idc, ida)?);
    let x = invoice("X", ida, vec![line("l", 1_000, 100)?])?;
    seed(b, cat, &[&a, &pb, &c, &x]).await?;
    let node = |e: &str, id| -> Result<NodeRef, Failure> { Ok(NodeRef { entity: n(e)?, id }) };
    let parent_edge = EdgeSpec {
        entity: n("party")?,
        field: n("parent_party")?,
    };
    let party_edge = EdgeSpec {
        entity: n("invoice")?,
        field: n("party")?,
    };
    let mut tx = begin(b, cat).await?;

    let out = tx
        .traverse(&Traversal {
            start: vec![node("party", ida)?],
            edges: vec![parent_edge.clone()],
            direction: Direction::Outgoing,
            max_depth: 10,
            max_nodes: 10,
        })
        .await?;
    let want = vec![
        Reached {
            node: node("party", ida)?,
            depth: 0,
            via: None,
        },
        Reached {
            node: node("party", idb)?,
            depth: 1,
            via: Some((node("party", ida)?, parent_edge.clone())),
        },
        Reached {
            node: node("party", idc)?,
            depth: 2,
            via: Some((node("party", idb)?, parent_edge.clone())),
        },
    ];
    ensure!(
        out.nodes == want,
        "outgoing walk with a cycle: got {:?}",
        out.nodes
    );

    let inc = tx
        .traverse(&Traversal {
            start: vec![node("party", ida)?],
            edges: vec![parent_edge.clone(), party_edge.clone()],
            direction: Direction::Incoming,
            max_depth: 1,
            max_nodes: 10,
        })
        .await?;
    let want = vec![
        Reached {
            node: node("party", ida)?,
            depth: 0,
            via: None,
        },
        Reached {
            node: node("invoice", x.id)?,
            depth: 1,
            via: Some((node("party", ida)?, party_edge)),
        },
        Reached {
            node: node("party", idc)?,
            depth: 1,
            via: Some((node("party", ida)?, parent_edge.clone())),
        },
    ];
    ensure!(inc.nodes == want, "incoming walk: got {:?}", inc.nodes);

    let line_id = x.children[&n("lines")?][0].id;
    let up = tx
        .traverse(&Traversal {
            start: vec![node("invoice_line", line_id)?],
            edges: vec![EdgeSpec {
                entity: n("invoice_line")?,
                field: n("parent")?,
            }],
            direction: Direction::Outgoing,
            max_depth: 1,
            max_nodes: 10,
        })
        .await?;
    let reached: Vec<&NodeRef> = up.nodes.iter().map(|r| &r.node).collect();
    ensure!(
        reached == vec![&node("invoice_line", line_id)?, &node("invoice", x.id)?],
        "child to parent: {reached:?}"
    );

    expect_kind(
        tx.traverse(&Traversal {
            start: vec![node("party", ida)?],
            edges: vec![parent_edge],
            direction: Direction::Both,
            max_depth: 10,
            max_nodes: 2,
        })
        .await,
        ErrorKind::TraversalTooLarge,
        "node limit",
    )?;
    expect_kind(
        tx.traverse(&Traversal {
            start: vec![node("party", ida)?],
            edges: vec![EdgeSpec {
                entity: n("party")?,
                field: n("name")?,
            }],
            direction: Direction::Outgoing,
            max_depth: 1,
            max_nodes: 10,
        })
        .await,
        ErrorKind::InvalidQuery,
        "edge over a non-link field",
    )
}

async fn read_only_transaction_rejects_writes<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
) -> CaseResult {
    let opts = TxOptions::new(n("report")?, Isolation::RepeatableRead).read_only();
    let mut tx = b.begin(cat.clone(), opts).await?;
    expect_kind(
        tx.insert(&party("A", 0, None)?).await,
        ErrorKind::ReadOnlyTransaction,
        "insert",
    )?;
    let mut tx = b
        .begin(
            cat.clone(),
            TxOptions::new(n("report")?, Isolation::ReadCommitted).read_only(),
        )
        .await?;
    expect_kind(
        tx.allocate(&SeriesKey::new("S")?, 1).await,
        ErrorKind::ReadOnlyTransaction,
        "allocate",
    )
}

async fn write_limit_is_enforced<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    let mut opts = TxOptions::new(n("small")?, Isolation::ReadCommitted);
    opts.max_writes = 3;
    let p = party("A", 0, None)?;
    // 1 invoice + 3 lines = 4 records.
    let inv = invoice(
        "I",
        p.id,
        vec![
            line("a", 1_000, 1)?,
            line("b", 1_000, 1)?,
            line("c", 1_000, 1)?,
        ],
    )?;
    let mut tx = b.begin(cat.clone(), opts.clone()).await?;
    expect_kind(
        tx.insert(&inv).await,
        ErrorKind::TransactionTooLarge,
        "4 writes with a limit of 3",
    )?;
    let mut tx = b.begin(cat.clone(), opts).await?;
    tx.insert(&p).await?;
    tx.enqueue(&event(&p, "a")?).await?;
    tx.enqueue(&event(&p, "b")?).await?;
    expect_kind(
        tx.enqueue(&event(&p, "c")?).await,
        ErrorKind::TransactionTooLarge,
        "outbox events count",
    )
}

async fn read_outside_transaction<B: StorageBackend>(b: &B, cat: &Arc<Catalog>) -> CaseResult {
    seed(b, cat, &[&party("Acme", 0, None)?]).await?;
    let mut q = Query::from(&n("party")?, &n("p")?, QueryPath::Report);
    q.select = vec![sel(col("p", "name")?, "name")?];
    let rows = b.read(cat.clone(), ReadSource::Primary, &q).await?;
    ensure!(
        rows.rows == vec![vec![Value::from("Acme")]],
        "primary read: {:?}",
        rows.rows
    );
    if !b
        .capabilities()
        .read_sources
        .contains(&ReadSource::AnalyticsMirror)
    {
        expect_kind(
            b.read(cat.clone(), ReadSource::AnalyticsMirror, &q).await,
            ErrorKind::Unsupported,
            "read from an absent mirror",
        )?;
    }
    q.lock = RowLock::Update(Wait::Block);
    expect_kind(
        b.read(cat.clone(), ReadSource::Primary, &q).await,
        ErrorKind::InvalidQuery,
        "lock outside tx",
    )
}

mod constructs;
mod range_search;
use constructs::{
    constructs_are_validated_by_the_catalog, geo_filters_are_validated,
    geo_filters_follow_capabilities, geo_values_are_validated, geo_values_roundtrip_exactly,
};
pub use constructs::{geo_catalog, range_catalog, search_catalog};
use range_search::{
    exclusion_error_names_no_other_record, exclusion_rejects_overlap_but_not_touching,
    range_construct_follows_capabilities, range_queries_are_validated, range_values_are_validated,
    range_values_roundtrip_exactly, search_follows_capabilities, search_queries_are_validated,
};
