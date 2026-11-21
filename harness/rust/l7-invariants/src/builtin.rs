//! The framework's built-in invariants.
//!
//! Each one states, in its `describe()`, exactly what it proves. Those that read the catalog
//! (`child-integrity`, `link-integrity`, `unique-keys`) work for any catalog; the others compare
//! the store with the oracle of the platform workload ([`crate::model::Model`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use spot_storage_contract::{
    AggFunc, ChangeOp, CmpOp, EntityKind, Expr, FieldType, Join, JoinKind, LinkCheck, LinkTarget,
    Name, Query, QueryPath, RecordId, Selection, StorageError, StorageResult, Value,
};

use crate::invariant::{BoxFuture, CheckContext, Invariant, InvariantRegistry, Violation};
use crate::model::Model;
use crate::oracle::change_code;

/// Id of [`SeqGapless`].
pub const SEQ_GAPLESS: &str = "seq-gapless";
/// Id of [`VersionContinuity`].
pub const DOC_VERSION_CONTINUITY: &str = "doc-version_continuity";
/// Id of [`OutboxComplete`].
pub const OUTBOX_COMPLETE: &str = "outbox-complete";
/// Id of [`ChildIntegrity`].
pub const CHILD_INTEGRITY: &str = "child-integrity";
/// Id of [`LinkIntegrity`].
pub const LINK_INTEGRITY: &str = "link-integrity";
/// Id of [`UniqueKeys`].
pub const UNIQUE_KEYS: &str = "unique-keys";
/// Id of [`CounterNoLostUpdate`].
pub const COUNTER_NO_LOST_UPDATE: &str = "lost_update-free";
/// Id of [`CountMatchesOracle`].
pub const DOC_COUNT_MATCHES_ORACLE: &str = "doc-count_matches_oracle";
/// Id of [`SnapshotConsistent`].
pub const READ_SNAPSHOT_CONSISTENT: &str = "read-snapshot_consistent";

/// A registry holding every built-in framework invariant.
pub fn framework_registry(model: &Arc<Model>) -> InvariantRegistry {
    let mut registry = InvariantRegistry::new();
    let items: Vec<Box<dyn Invariant>> = vec![
        Box::new(SeqGapless::new(model.clone())),
        Box::new(VersionContinuity::new(model.clone())),
        Box::new(OutboxComplete),
        Box::new(ChildIntegrity),
        Box::new(LinkIntegrity),
        Box::new(UniqueKeys),
        Box::new(CounterNoLostUpdate::new(model.clone())),
        Box::new(CountMatchesOracle::new(model.clone())),
        Box::new(SnapshotConsistent),
    ];
    for item in items {
        // The ids above are distinct; a refusal would be a bug in this list, caught by tests.
        let _ = registry.register(item);
    }
    registry
}

macro_rules! invariant {
    ($ty:ty, $id:expr, $desc:expr) => {
        impl Invariant for $ty {
            fn id(&self) -> &str {
                $id
            }
            fn describe(&self) -> &str {
                $desc
            }
            fn check<'a>(&'a self, ctx: &'a CheckContext) -> BoxFuture<'a, Vec<Violation>> {
                Box::pin(async move {
                    let mut out = Vec::new();
                    if let Err(e) = self.run(ctx, &mut out).await {
                        out.push(Violation::read_failed($id, "store", &e));
                    }
                    out
                })
            }
        }
    };
}

// ---------------------------------------------------------------------------------------------
// Query and row helpers
// ---------------------------------------------------------------------------------------------

fn name(s: &str) -> StorageResult<Name> {
    Name::new(s)
}

fn field(alias: &Name, f: &str) -> StorageResult<Expr> {
    Ok(Expr::field(alias, &name(f)?))
}

fn select(expr: Expr, alias: &str) -> StorageResult<Selection> {
    Ok(Selection {
        expr,
        alias: name(alias)?,
    })
}

fn count_star() -> Expr {
    Expr::Agg {
        func: AggFunc::Count,
        arg: None,
        distinct: false,
    }
}

fn agg(func: AggFunc, arg: Expr, distinct: bool) -> Expr {
    Expr::Agg {
        func,
        arg: Some(Box::new(arg)),
        distinct,
    }
}

fn bad_row(what: &str) -> StorageError {
    StorageError::new(
        spot_storage_contract::ErrorKind::Internal,
        format!("unexpected row shape: {what}"),
    )
}

fn as_id(v: &Value) -> StorageResult<RecordId> {
    match v {
        Value::Id(id) => Ok(*id),
        _ => Err(bad_row("id")),
    }
}

fn as_int(v: &Value) -> StorageResult<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        _ => Err(bad_row("int")),
    }
}

fn as_text(v: &Value) -> StorageResult<String> {
    match v {
        Value::Text(t) => Ok(t.clone()),
        _ => Err(bad_row("text")),
    }
}

fn cell(row: &[Value], i: usize) -> StorageResult<&Value> {
    row.get(i).ok_or_else(|| bad_row("column count"))
}

fn is_null_expr(e: Expr) -> Expr {
    Expr::IsNull(Box::new(e))
}

fn not(e: Expr) -> Expr {
    Expr::Not(Box::new(e))
}

/// `(id, version)` of every document of `entity`.
async fn versions(ctx: &CheckContext, entity: &Name) -> StorageResult<BTreeMap<RecordId, u64>> {
    let a = name("d")?;
    let mut q = Query::from(entity, &a, QueryPath::Transactional);
    q.select = vec![
        select(field(&a, "id")?, "doc_id")?,
        select(field(&a, "version")?, "doc_version")?,
    ];
    let rows = ctx.query(&q).await?;
    let mut map = BTreeMap::new();
    for row in &rows.rows {
        let version = u64::try_from(as_int(cell(row, 1)?)?).unwrap_or(0);
        map.insert(as_id(cell(row, 0)?)?, version);
    }
    Ok(map)
}

// ---------------------------------------------------------------------------------------------
// seq-gapless
// ---------------------------------------------------------------------------------------------

/// `seq-gapless`.
pub struct SeqGapless {
    model: Arc<Model>,
}

impl SeqGapless {
    /// The invariant for the platform model.
    pub fn new(model: Arc<Model>) -> SeqGapless {
        SeqGapless { model }
    }

    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        let m = &self.model;
        let oracle = ctx.oracle();
        for (series, number) in oracle.duplicate_numbers() {
            out.push(Violation::new(
                SEQ_GAPLESS,
                "sequence",
                series,
                format!("duplicate_allocation number={number}"),
            ));
        }
        // What the store holds: one ticket document per number handed out inside a committed
        // transaction, written in the same transaction as the allocation.
        let a = name("t")?;
        let mut q = Query::from(&m.ticket, &a, QueryPath::Transactional);
        q.select = vec![
            select(field(&a, "series")?, "series")?,
            select(field(&a, "number")?, "number")?,
        ];
        let rows = ctx.query(&q).await?;
        let mut stored: BTreeMap<String, BTreeMap<u64, u64>> = BTreeMap::new();
        for row in &rows.rows {
            let number = u64::try_from(as_int(cell(row, 1)?)?).unwrap_or(0);
            *stored
                .entry(as_text(cell(row, 0)?)?)
                .or_default()
                .entry(number)
                .or_insert(0) += 1;
        }
        for series in stored.keys() {
            if !oracle.series().contains_key(series) {
                out.push(Violation::new(
                    SEQ_GAPLESS,
                    "sequence",
                    series,
                    "unknown_series",
                ));
            }
        }
        for (series, numbers) in oracle.series() {
            let top = numbers.iter().next_back().copied().unwrap_or(0);
            let mut gaps = 0;
            for k in 1..=top {
                if !numbers.contains(&k) {
                    gaps += 1;
                    if gaps <= 50 {
                        out.push(Violation::new(
                            SEQ_GAPLESS,
                            "sequence",
                            series,
                            format!("gap number={k}"),
                        ));
                    }
                }
            }
            let held = stored.get(series).cloned().unwrap_or_default();
            for k in numbers {
                if !held.contains_key(k) {
                    out.push(Violation::new(
                        SEQ_GAPLESS,
                        "sequence",
                        series,
                        format!("missing_ticket number={k}"),
                    ));
                }
            }
            for (k, copies) in &held {
                if !numbers.contains(k) {
                    out.push(Violation::new(
                        SEQ_GAPLESS,
                        "sequence",
                        series,
                        format!("unrecorded_ticket number={k}"),
                    ));
                }
                if *copies > 1 {
                    out.push(Violation::new(
                        SEQ_GAPLESS,
                        "sequence",
                        series,
                        format!("duplicate_ticket number={k} copies={copies}"),
                    ));
                }
            }
            // The series' own counter: the next number must follow the last committed one.
            let next = ctx.next_number(series).await?;
            if next != top + 1 {
                out.push(Violation::new(
                    SEQ_GAPLESS,
                    "sequence",
                    series,
                    format!("next_number expected={} actual={next}", top + 1),
                ));
            }
        }
        Ok(())
    }
}

invariant!(
    SeqGapless,
    SEQ_GAPLESS,
    "For every numbering series the workload used, the numbers handed out by committed \
     transactions (the driver's record of committed allocations) are exactly 1..=n with no \
     duplicate and no gap; the store holds one ticket per such number and no other; and the next \
     allocation of the series returns n+1, so a number taken by a transaction that rolled back \
     was given back. It does not prove anything about numbers the workload never allocated."
);

// ---------------------------------------------------------------------------------------------
// doc-version_continuity
// ---------------------------------------------------------------------------------------------

/// `doc-version_continuity`.
pub struct VersionContinuity {
    model: Arc<Model>,
}

impl VersionContinuity {
    /// The invariant for the platform model.
    pub fn new(model: Arc<Model>) -> VersionContinuity {
        VersionContinuity { model }
    }

    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        for entity in self.model.document_entities() {
            let stored = versions(ctx, entity).await?;
            for (id, state) in ctx.oracle().live_docs(entity) {
                if let Some(actual) = stored.get(&id) {
                    let expected = 1 + state.updates;
                    if *actual != expected {
                        out.push(Violation::new(
                            DOC_VERSION_CONTINUITY,
                            entity,
                            id,
                            format!("version expected={expected} actual={actual}"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

invariant!(
    VersionContinuity,
    DOC_VERSION_CONTINUITY,
    "Every document the oracle holds as live has a stored version equal to 1 plus the number of \
     committed updates the driver recorded for it: no version was skipped, repeated or rewound. \
     A document missing from the store is reported by doc-count_matches_oracle, not here."
);

// ---------------------------------------------------------------------------------------------
// lost_update-free
// ---------------------------------------------------------------------------------------------

/// `lost_update-free`.
pub struct CounterNoLostUpdate {
    model: Arc<Model>,
}

impl CounterNoLostUpdate {
    /// The invariant for the platform model.
    pub fn new(model: Arc<Model>) -> CounterNoLostUpdate {
        CounterNoLostUpdate { model }
    }

    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        let m = &self.model;
        let a = name("c")?;
        let mut q = Query::from(&m.counter, &a, QueryPath::Transactional);
        q.select = vec![
            select(field(&a, "id")?, "doc_id")?,
            select(field(&a, "value")?, "doc_value")?,
        ];
        let rows = ctx.query(&q).await?;
        let mut stored = BTreeMap::new();
        for row in &rows.rows {
            stored.insert(as_id(cell(row, 0)?)?, as_int(cell(row, 1)?)?);
        }
        for (id, successes) in ctx.oracle().counters() {
            match stored.get(id) {
                None => out.push(Violation::new(
                    COUNTER_NO_LOST_UPDATE,
                    &m.counter,
                    id,
                    "missing_counter",
                )),
                Some(actual) if i128::from(*actual) != i128::from(*successes) => {
                    out.push(Violation::new(
                        COUNTER_NO_LOST_UPDATE,
                        &m.counter,
                        id,
                        format!("value expected={successes} actual={actual}"),
                    ));
                }
                Some(_) => {}
            }
        }
        Ok(())
    }
}

invariant!(
    CounterNoLostUpdate,
    COUNTER_NO_LOST_UPDATE,
    "The final value of each counter document equals the number of increments the driver saw \
     succeed (a commit that returned success, or an indeterminate one resolved as committed). \
     Every increment is a read-modify-write that declares how it avoids a lost update (version \
     check or row lock); a lost update shows up as a value below that count, and a phantom \
     increment as a value above it."
);

// ---------------------------------------------------------------------------------------------
// doc-count_matches_oracle
// ---------------------------------------------------------------------------------------------

/// `doc-count_matches_oracle`.
pub struct CountMatchesOracle {
    model: Arc<Model>,
}

impl CountMatchesOracle {
    /// The invariant for the platform model.
    pub fn new(model: Arc<Model>) -> CountMatchesOracle {
        CountMatchesOracle { model }
    }

    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        for entity in self.model.document_entities() {
            let stored = versions(ctx, entity).await?;
            let expected = ctx.oracle().live_count(entity);
            let actual = stored.len() as u64;
            if expected == actual {
                // Equal counts can still hide a swap; compare the sets as well.
                let same = ctx
                    .oracle()
                    .live_docs(entity)
                    .all(|(id, _)| stored.contains_key(&id));
                if same {
                    continue;
                }
            }
            out.push(Violation::new(
                DOC_COUNT_MATCHES_ORACLE,
                entity,
                "-",
                format!("count expected={expected} actual={actual}"),
            ));
            let live: BTreeSet<RecordId> =
                ctx.oracle().live_docs(entity).map(|(id, _)| id).collect();
            for id in stored.keys().filter(|id| !live.contains(id)) {
                out.push(Violation::new(
                    DOC_COUNT_MATCHES_ORACLE,
                    entity,
                    id,
                    "unexpected_document",
                ));
            }
            for id in live.iter().filter(|id| !stored.contains_key(id)) {
                out.push(Violation::new(
                    DOC_COUNT_MATCHES_ORACLE,
                    entity,
                    id,
                    "missing_document",
                ));
            }
        }
        Ok(())
    }
}

invariant!(
    CountMatchesOracle,
    DOC_COUNT_MATCHES_ORACLE,
    "For every document entity the set of stored documents is exactly the set the oracle holds \
     as live (committed creates minus committed deletes): none lost, none that no committed \
     transaction created."
);

// ---------------------------------------------------------------------------------------------
// read-snapshot_consistent
// ---------------------------------------------------------------------------------------------

/// `read-snapshot_consistent`.
pub struct SnapshotConsistent;

impl SnapshotConsistent {
    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        for (id, detail) in ctx.oracle().read_anomalies() {
            out.push(Violation::new(
                READ_SNAPSHOT_CONSISTENT,
                "l7_order",
                id,
                detail,
            ));
        }
        Ok(())
    }
}

invariant!(
    SnapshotConsistent,
    READ_SNAPSHOT_CONSISTENT,
    "Every read-only snapshot the workload took saw an order whose stored total equals the sum \
     of its child quantities: a reader never sees half of a transaction. Orders are written only \
     by transactions that keep that equality."
);

// ---------------------------------------------------------------------------------------------
// child-integrity
// ---------------------------------------------------------------------------------------------

/// `child-integrity`.
pub struct ChildIntegrity;

impl ChildIntegrity {
    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        for def in ctx.catalog().entities() {
            let EntityKind::Child { parent } = &def.kind else {
                continue;
            };
            let c = name("c")?;
            let p = name("p")?;
            // Every child row belongs to an existing parent.
            let mut q = Query::from(&def.name, &c, QueryPath::Transactional);
            q.joins = vec![Join {
                kind: JoinKind::Left,
                source: spot_storage_contract::Source {
                    entity: parent.clone(),
                    alias: p.clone(),
                },
                on: field(&c, "parent")?.eq(field(&p, "id")?),
            }];
            q.select = vec![
                select(field(&c, "id")?, "child_id")?,
                select(field(&c, "parent")?, "parent_id")?,
            ];
            q.filter = Some(is_null_expr(field(&p, "id")?));
            let rows = ctx.query(&q).await?;
            for row in &rows.rows {
                out.push(Violation::new(
                    CHILD_INTEGRITY,
                    &def.name,
                    as_id(cell(row, 0)?)?,
                    format!("orphan_child parent={}", as_id(cell(row, 1)?)?),
                ));
            }
            // Child order is dense: idx runs 1..=count per parent and table.
            let mut q = Query::from(&def.name, &c, QueryPath::Transactional);
            q.select = vec![
                select(field(&c, "parent")?, "parent_id")?,
                select(field(&c, "parent_field")?, "parent_field")?,
                select(count_star(), "rows")?,
                select(agg(AggFunc::Min, field(&c, "idx")?, false), "lowest")?,
                select(agg(AggFunc::Max, field(&c, "idx")?, false), "highest")?,
                select(agg(AggFunc::Count, field(&c, "idx")?, true), "distinct_idx")?,
            ];
            q.group_by = vec![field(&c, "parent")?, field(&c, "parent_field")?];
            let rows = ctx.query(&q).await?;
            for row in &rows.rows {
                let (count, lo, hi, distinct) = (
                    as_int(cell(row, 2)?)?,
                    as_int(cell(row, 3)?)?,
                    as_int(cell(row, 4)?)?,
                    as_int(cell(row, 5)?)?,
                );
                if lo != 1 || hi != count || distinct != count {
                    out.push(Violation::new(
                        CHILD_INTEGRITY,
                        parent,
                        as_id(cell(row, 0)?)?,
                        format!(
                            "child_order_not_dense field={} rows={count} lowest={lo} highest={hi} distinct={distinct}",
                            as_text(cell(row, 1)?)?
                        ),
                    ));
                }
            }
        }
        Ok(())
    }
}

invariant!(
    ChildIntegrity,
    CHILD_INTEGRITY,
    "Every child row of every child entity in the catalog belongs to a parent document that \
     exists, and for each parent and child table the rows' positions are exactly 1..=count, \
     with no gap and no repeat. It reads the catalog, so it covers any app's child tables."
);

// ---------------------------------------------------------------------------------------------
// link-integrity
// ---------------------------------------------------------------------------------------------

/// `link-integrity`.
pub struct LinkIntegrity;

impl LinkIntegrity {
    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        for def in ctx.catalog().entities() {
            for f in &def.fields {
                let FieldType::Link {
                    target: LinkTarget::Entity(target),
                    check,
                } = &f.ty
                else {
                    continue;
                };
                let s = name("s")?;
                let t = name("t")?;
                let mut q = Query::from(&def.name, &s, QueryPath::Transactional);
                q.joins = vec![Join {
                    kind: JoinKind::Left,
                    source: spot_storage_contract::Source {
                        entity: target.clone(),
                        alias: t.clone(),
                    },
                    on: Expr::field(&s, &f.name).eq(field(&t, "id")?),
                }];
                q.select = vec![
                    select(field(&s, "id")?, "holder")?,
                    select(Expr::field(&s, &f.name), "pointee")?,
                ];
                q.filter = Some(Expr::And(vec![
                    not(is_null_expr(Expr::field(&s, &f.name))),
                    is_null_expr(field(&t, "id")?),
                ]));
                let rows = ctx.query(&q).await?;
                for row in &rows.rows {
                    let holder = as_id(cell(row, 0)?)?;
                    let pointee = as_id(cell(row, 1)?)?;
                    // A link the engine (not the database) checks may outlive a committed delete
                    // of its target; a database-checked link never may.
                    let excused = matches!(check, LinkCheck::EngineOnly { .. })
                        && ctx.oracle().was_deleted(target, pointee);
                    if !excused {
                        out.push(Violation::new(
                            LINK_INTEGRITY,
                            &def.name,
                            holder,
                            format!("dangling_link field={} target={pointee}", f.name),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

invariant!(
    LinkIntegrity,
    LINK_INTEGRITY,
    "Every link field in the catalog that is set points to an existing document of its target \
     entity. The one exception is a link declared engine-checked whose target the oracle \
     recorded as deleted by a committed transaction; a database-checked link has no exception. \
     Dynamic links are not checked here."
);

// ---------------------------------------------------------------------------------------------
// unique-keys
// ---------------------------------------------------------------------------------------------

/// `unique-keys`.
pub struct UniqueKeys;

impl UniqueKeys {
    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        for def in ctx.catalog().entities() {
            for index in def.indexes.iter().filter(|i| i.unique) {
                let a = name("u")?;
                let mut q = Query::from(&def.name, &a, QueryPath::Transactional);
                let mut selects = Vec::new();
                for (i, f) in index.fields.iter().enumerate() {
                    selects.push(select(Expr::field(&a, f), &format!("k{i}"))?);
                }
                selects.push(select(count_star(), "copies")?);
                q.select = selects;
                q.group_by = index.fields.iter().map(|f| Expr::field(&a, f)).collect();
                q.having = Some(count_star().cmp(CmpOp::Gt, Expr::value(1i64)));
                // A key with a null part never conflicts, as in SQL unique indexes.
                q.filter = Some(Expr::And(
                    index
                        .fields
                        .iter()
                        .map(|f| not(is_null_expr(Expr::field(&a, f))))
                        .collect(),
                ));
                let groups = ctx.query(&q).await?;
                for g in &groups.rows {
                    let copies = as_int(cell(g, index.fields.len())?)?;
                    let mut rows_q = Query::from(&def.name, &a, QueryPath::Transactional);
                    rows_q.select = vec![select(field(&a, "id")?, "doc_id")?];
                    rows_q.filter = Some(Expr::And(
                        index
                            .fields
                            .iter()
                            .enumerate()
                            .map(|(i, f)| {
                                Ok(Expr::field(&a, f).eq(Expr::Value(cell(g, i)?.clone())))
                            })
                            .collect::<StorageResult<Vec<_>>>()?,
                    ));
                    for row in &ctx.query(&rows_q).await?.rows {
                        out.push(Violation::new(
                            UNIQUE_KEYS,
                            &def.name,
                            as_id(cell(row, 0)?)?,
                            format!("duplicate_key index={} copies={copies}", index.name),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

invariant!(
    UniqueKeys,
    UNIQUE_KEYS,
    "No two rows of an entity share the same non-null key for any unique index the catalog \
     declares. The check groups the rows itself rather than trusting the index, so a dropped or \
     bypassed index is caught."
);

// ---------------------------------------------------------------------------------------------
// outbox-complete
// ---------------------------------------------------------------------------------------------

/// `outbox-complete`.
pub struct OutboxComplete;

impl OutboxComplete {
    async fn run(&self, ctx: &CheckContext, out: &mut Vec<Violation>) -> StorageResult<()> {
        let oracle = ctx.oracle();
        let records = ctx.outbox().await?;
        let v = |id: &dyn std::fmt::Display, detail: String| {
            Violation::new(OUTBOX_COMPLETE, "outbox", id, detail)
        };
        for pair in records.windows(2) {
            if pair[1].position <= pair[0].position {
                out.push(v(&pair[1].event.id, "position_not_increasing".into()));
            }
        }
        let mut seen: HashMap<RecordId, Vec<usize>> = HashMap::new();
        for (i, rec) in records.iter().enumerate() {
            seen.entry(rec.event.id).or_default().push(i);
        }
        let mut expected: Vec<_> = oracle.events().iter().collect();
        expected.sort_by_key(|(_, e)| (e.group, e.ordinal));
        // Every committed event is present once, with its enqueue ordinal.
        let mut group_tx: HashMap<u64, RecordId> = HashMap::new();
        let mut tx_group: HashMap<RecordId, u64> = HashMap::new();
        let mut group_pos: BTreeMap<u64, (usize, usize)> = BTreeMap::new();
        for (id, exp) in &expected {
            match seen.get(id).map(Vec::as_slice) {
                None | Some([]) => out.push(v(id, "missing_event".into())),
                Some([one]) => {
                    let rec = &records[*one];
                    if rec.ordinal != exp.ordinal {
                        out.push(v(
                            id,
                            format!(
                                "wrong_ordinal expected={} actual={}",
                                exp.ordinal, rec.ordinal
                            ),
                        ));
                    }
                    let known = *group_tx.entry(exp.group).or_insert(rec.transaction);
                    if known != rec.transaction {
                        out.push(v(id, "events_of_one_transaction_split".into()));
                    }
                    let owner = *tx_group.entry(rec.transaction).or_insert(exp.group);
                    if owner != exp.group {
                        out.push(v(id, "events_of_two_transactions_merged".into()));
                    }
                    let span = group_pos.entry(exp.group).or_insert((*one, *one));
                    span.0 = span.0.min(*one);
                    span.1 = span.1.max(*one);
                }
                Some(many) => out.push(v(id, format!("duplicate_event copies={}", many.len()))),
            }
        }
        // No orphans: nothing the committed transactions did not enqueue.
        for rec in &records {
            if !oracle.events().contains_key(&rec.event.id) {
                let kind = if oracle.is_aborted_event(&rec.event.id) {
                    "orphan_event"
                } else {
                    "unknown_event"
                };
                out.push(v(&rec.event.id, kind.into()));
            }
        }
        // Transaction order: within one transaction, publication order is enqueue order.
        let mut last_ordinal: HashMap<RecordId, u32> = HashMap::new();
        for rec in &records {
            if oracle.events().contains_key(&rec.event.id)
                && let Some(prev) = last_ordinal.insert(rec.transaction, rec.ordinal)
                && prev >= rec.ordinal
            {
                out.push(v(&rec.event.id, "ordinal_out_of_order".into()));
            }
        }
        // Causal order: a transaction that began after another committed is published after it.
        // `group_pos` holds record indexes, which grow with position.
        let mut spans: Vec<(u64, u64, usize, usize, u64)> = Vec::new();
        for (group, (lo, hi)) in &group_pos {
            if let Some(w) = oracle.windows().get(group) {
                spans.push((w.start_us, w.end_us, *lo, *hi, *group));
            }
        }
        let mut by_end: Vec<usize> = (0..spans.len()).collect();
        by_end.sort_by_key(|i| spans[*i].1);
        let mut by_start: Vec<usize> = (0..spans.len()).collect();
        by_start.sort_by_key(|i| spans[*i].0);
        let (mut next, mut highest) = (0usize, None::<usize>);
        for b in by_start {
            while next < by_end.len() && spans[by_end[next]].1 < spans[b].0 {
                let hi = spans[by_end[next]].3;
                highest = Some(highest.map_or(hi, |h| h.max(hi)));
                next += 1;
            }
            if let Some(h) = highest
                && spans[b].2 <= h
            {
                out.push(v(
                    &records[spans[b].2].event.id,
                    format!("causal_order group={}", spans[b].4),
                ));
            }
        }
        // The change feed contains exactly the committed document writes.
        let changes = ctx.changes().await?;
        for pair in changes.windows(2) {
            if pair[1].position <= pair[0].position {
                out.push(Violation::new(
                    OUTBOX_COMPLETE,
                    &pair[1].document.entity,
                    pair[1].document.id,
                    "change_position_not_increasing",
                ));
            }
        }
        let mut actual: BTreeMap<(Name, RecordId, u8, u64), u64> = BTreeMap::new();
        for c in &changes {
            *actual
                .entry((
                    c.document.entity.clone(),
                    c.document.id,
                    change_code(c.op),
                    c.version.0,
                ))
                .or_insert(0) += 1;
        }
        let describe = |op: u8, version: u64| {
            let op = match crate::oracle::op_from_code(op) {
                ChangeOp::Insert => "insert",
                ChangeOp::Update => "update",
                ChangeOp::Delete => "delete",
            };
            format!("op={op} version={version}")
        };
        for (key, want) in oracle.expected_changes() {
            let got = actual.get(key).copied().unwrap_or(0);
            if got != *want {
                let what = if got == 0 {
                    "missing_change"
                } else {
                    "wrong_change_count"
                };
                out.push(Violation::new(
                    OUTBOX_COMPLETE,
                    &key.0,
                    key.1,
                    format!(
                        "{what} {} expected={want} actual={got}",
                        describe(key.2, key.3)
                    ),
                ));
            }
        }
        for (key, got) in &actual {
            if !oracle.expected_changes().contains_key(key) {
                out.push(Violation::new(
                    OUTBOX_COMPLETE,
                    &key.0,
                    key.1,
                    format!("unexpected_change {} actual={got}", describe(key.2, key.3)),
                ));
            }
        }
        Ok(())
    }
}

invariant!(
    OutboxComplete,
    OUTBOX_COMPLETE,
    "Every outbox event enqueued by a transaction that committed is present exactly once, with \
     the ordinal it was enqueued at and in position order within its transaction; no event of a \
     transaction that did not commit is present and none the driver never enqueued; events of a \
     transaction that began after another committed are published after the earlier \
     transaction's events; and the change feed holds exactly the document writes of committed \
     transactions, one net entry per document and transaction, with their versions."
);
