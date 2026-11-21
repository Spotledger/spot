//! The query engine through the public API: joins, grouping, paging and row locks.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_storage_contract::contract_tests;
use spot_storage_contract::query::{
    AggFunc, Join, JoinKind, Nulls, Order, OrderDir, Selection, TextMatch, TextMode,
};
use spot_storage_contract::schema::system;
use spot_storage_contract::*;
use spot_storage_fake::FakeBackend;

type R = Result<(), StorageError>;

fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{e}"))
}

fn col(alias: &str, field: &str) -> Expr {
    Expr::field(&n(alias), &n(field))
}

fn sel(expr: Expr, alias: &str) -> Selection {
    Selection {
        expr,
        alias: n(alias),
    }
}

fn party(name: &str, parent: Option<RecordId>) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(n("name"), name.into());
    fields.insert(n("credit_limit"), Value::Decimal(0.into()));
    fields.insert(n("parent_party"), parent.map_or(Value::Null, Value::Id));
    for f in [
        "note", "since", "opens_at", "seen_at", "active", "visits", "profile", "logo",
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

async fn store(docs: &[&DocumentData]) -> Result<(FakeBackend, Arc<Catalog>), StorageError> {
    let b = FakeBackend::new();
    let cat = Arc::new(contract_tests::catalog()?);
    b.prepare(&cat).await?;
    let mut tx = begin(&b, &cat).await?;
    for d in docs {
        tx.insert(d).await?;
    }
    tx.commit().await?;
    Ok((b, cat))
}

async fn begin(b: &FakeBackend, cat: &Arc<Catalog>) -> StorageResult<spot_storage_fake::FakeTx> {
    let mut o = TxOptions::new(n("query_test"), Isolation::ReadCommitted);
    o.lock_timeout = std::time::Duration::from_millis(100);
    b.begin(cat.clone(), o).await
}

fn names_query() -> Query {
    let mut q = Query::from(&n("party"), &n("p"), QueryPath::Report);
    q.select = vec![sel(col("p", "name"), "name")];
    q.order_by = vec![Order {
        expr: col("p", "name"),
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    }];
    q
}

fn texts(rows: &Rows) -> Vec<String> {
    rows.rows
        .iter()
        .map(|r| match r.as_slice() {
            [Value::Text(s)] => s.clone(),
            other => panic!("expected one text column, got {other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn offset_and_limit_page_after_ordering() -> R {
    let ps: Vec<_> = ["d", "a", "c", "b", "e"]
        .iter()
        .map(|s| party(s, None))
        .collect();
    let (b, cat) = store(&ps.iter().collect::<Vec<_>>()).await?;
    let mut q = names_query();
    q.offset = 1;
    q.limit = Some(3);
    let mut tx = begin(&b, &cat).await?;
    assert_eq!(texts(&tx.query(&q).await?), ["b", "c", "d"]);
    q.offset = 10;
    assert!(tx.query(&q).await?.rows.is_empty());
    Ok(())
}

#[tokio::test]
async fn equal_sort_keys_fall_back_to_the_record_id() -> R {
    let ps: Vec<_> = (0..4).map(|i| party(&format!("p{i}"), None)).collect();
    let (b, cat) = store(&ps.iter().collect::<Vec<_>>()).await?;
    // Every credit limit is 0, so the order is decided by the id alone.
    let mut q = names_query();
    q.order_by[0].expr = col("p", "credit_limit");
    q.select.push(sel(col("p", system::ID), "id"));
    let mut tx = begin(&b, &cat).await?;
    let ids: Vec<RecordId> = tx
        .query(&q)
        .await?
        .rows
        .iter()
        .filter_map(|r| match r.get(1) {
            Some(Value::Id(id)) => Some(*id),
            _ => None,
        })
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids.len(), 4);
    assert_eq!(ids, sorted);
    Ok(())
}

#[tokio::test]
async fn left_join_reads_a_missing_row_as_null_and_inner_drops_it() -> R {
    let parent = party("Parent", None);
    let child = party("Child", Some(parent.id));
    let (b, cat) = store(&[&parent, &child]).await?;
    let mut q = names_query();
    q.select.push(sel(col("up", "name"), "parent"));
    let join = |kind| Join {
        kind,
        source: Source {
            entity: n("party"),
            alias: n("up"),
        },
        on: col("p", "parent_party").eq(col("up", system::ID)),
    };
    let mut tx = begin(&b, &cat).await?;
    q.joins = vec![join(JoinKind::Left)];
    let rows = tx.query(&q).await?.rows;
    assert_eq!(
        rows,
        vec![
            vec!["Child".into(), "Parent".into()],
            vec!["Parent".into(), Value::Null],
        ]
    );
    q.joins = vec![join(JoinKind::Inner)];
    assert_eq!(
        tx.query(&q).await?.rows,
        vec![vec!["Child".into(), "Parent".into()]]
    );
    Ok(())
}

#[tokio::test]
async fn grouping_treats_null_keys_as_equal_and_having_filters_groups() -> R {
    let parent = party("P", None);
    let ps = [
        party("a", Some(parent.id)),
        party("b", Some(parent.id)),
        party("c", None),
        party("d", None),
        party("e", None),
    ];
    let mut all = vec![&parent];
    all.extend(ps.iter());
    let (b, cat) = store(&all).await?;
    let mut q = Query::from(&n("party"), &n("p"), QueryPath::Report);
    q.group_by = vec![col("p", "parent_party")];
    let count = Expr::Agg {
        func: AggFunc::Count,
        arg: None,
        distinct: false,
    };
    q.select = vec![
        sel(col("p", "parent_party"), "parent"),
        sel(count.clone(), "n"),
    ];
    q.order_by = vec![Order {
        expr: count.clone(),
        dir: OrderDir::Desc,
        nulls: Nulls::Last,
    }];
    let mut tx = begin(&b, &cat).await?;
    let rows = tx.query(&q).await?.rows;
    assert_eq!(
        rows,
        vec![
            vec![Value::Null, Value::Int(4)],
            vec![Value::Id(parent.id), Value::Int(2)],
        ]
    );
    q.having = Some(count.cmp(CmpOp::Lt, Expr::value(3)));
    assert_eq!(tx.query(&q).await?.rows.len(), 1);
    Ok(())
}

#[tokio::test]
async fn aggregating_without_keys_gives_one_row_over_nothing() -> R {
    let (b, cat) = store(&[]).await?;
    let mut q = Query::from(&n("party"), &n("p"), QueryPath::Report);
    q.select = vec![
        sel(
            Expr::Agg {
                func: AggFunc::Count,
                arg: None,
                distinct: false,
            },
            "n",
        ),
        sel(
            Expr::Agg {
                func: AggFunc::Max,
                arg: Some(Box::new(col("p", "name"))),
                distinct: false,
            },
            "last",
        ),
    ];
    let mut tx = begin(&b, &cat).await?;
    assert_eq!(
        tx.query(&q).await?.rows,
        vec![vec![Value::Int(0), Value::Null]]
    );
    Ok(())
}

#[tokio::test]
async fn text_matching_is_literal_and_case_insensitive_on_request() -> R {
    let ps = [
        party("100%_sure", None),
        party("100 sure", None),
        party("Ab\\c", None),
    ];
    let (b, cat) = store(&ps.iter().collect::<Vec<_>>()).await?;
    let m = |mode, pattern: &str, ci| {
        let mut q = names_query();
        q.filter = Some(Expr::Text(
            Box::new(col("p", "name")),
            TextMatch {
                mode,
                pattern: pattern.into(),
                case_insensitive: ci,
            },
        ));
        q
    };
    let mut tx = begin(&b, &cat).await?;
    assert_eq!(
        texts(&tx.query(&m(TextMode::Contains, "%_", false)).await?),
        ["100%_sure"]
    );
    assert_eq!(
        texts(&tx.query(&m(TextMode::Prefix, "100", false)).await?).len(),
        2
    );
    assert_eq!(
        texts(&tx.query(&m(TextMode::Contains, "\\", false)).await?),
        ["Ab\\c"]
    );
    assert!(
        tx.query(&m(TextMode::Equals, "ab\\C", false))
            .await?
            .rows
            .is_empty()
    );
    assert_eq!(
        texts(&tx.query(&m(TextMode::Equals, "ab\\C", true)).await?),
        ["Ab\\c"]
    );
    Ok(())
}

#[tokio::test]
async fn skip_locked_leaves_rows_locked_by_others_out() -> R {
    let ps = [party("a", None), party("b", None), party("c", None)];
    let (b, cat) = store(&ps.iter().collect::<Vec<_>>()).await?;
    let mut holder = begin(&b, &cat).await?;
    let mut by_name = names_query();
    by_name.filter = Some(col("p", "name").eq(Expr::value("b")));
    by_name.lock = RowLock::Update(Wait::Block);
    assert_eq!(texts(&holder.query(&by_name).await?), ["b"]);

    let mut other = begin(&b, &cat).await?;
    let mut q = names_query();
    q.lock = RowLock::Update(Wait::SkipLocked);
    assert_eq!(texts(&other.query(&q).await?), ["a", "c"]);
    q.lock = RowLock::Update(Wait::NoWait);
    let err = other.query(&q).await.err().map(|e| e.kind);
    assert_eq!(err, Some(ErrorKind::LockNotAvailable));
    holder.rollback().await?;
    other.rollback().await?;
    let mut again = begin(&b, &cat).await?;
    q.lock = RowLock::Update(Wait::SkipLocked);
    assert_eq!(texts(&again.query(&q).await?), ["a", "b", "c"]);
    Ok(())
}
