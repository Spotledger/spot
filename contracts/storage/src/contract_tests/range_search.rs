//! The range, exclusion and search cases (ADR-087), split from [`super::constructs`] to keep
//! files small.

use super::constructs::{
    day, expect_insert_kind, expect_invalid_filter, listing, nights, select_of, stay,
};
use super::*;

pub(super) async fn range_construct_follows_capabilities<B: StorageBackend>(b: &B) -> CaseResult {
    let cat = Arc::new(range_catalog()?);
    let mut q = select_of("stay", "unit")?;
    q.filter = Some(Expr::Range(
        Box::new(col("x", "during")?),
        RangeTest::Overlaps {
            start: Value::Date(day(1)?),
            end: Value::Date(day(3)?),
        },
    ));
    if b.capabilities().range_exclusion {
        b.prepare(&cat).await?;
        let rows = b.read(cat.clone(), ReadSource::Primary, &q).await?;
        ensure!(rows.rows.is_empty(), "an empty table overlaps nothing");
        return Ok(());
    }
    // Option B: the range construct is refused by capability, not approximated.
    let err = b.prepare(&cat).await;
    match &err {
        Err(e) if e.kind == ErrorKind::Unsupported => {}
        other => {
            return Err(Failure(format!(
                "a catalog with a range field and an exclusion must be Unsupported: {other:?}"
            )));
        }
    }
    expect_kind(
        b.read(cat.clone(), ReadSource::Primary, &q).await,
        ErrorKind::Unsupported,
        "Overlaps",
    )?;
    let mut tx = begin(b, &cat).await?;
    expect_kind(
        tx.query(&q).await,
        ErrorKind::Unsupported,
        "Overlaps in a transaction",
    )
}

pub(super) async fn range_values_are_validated<B: StorageBackend>(b: &B) -> CaseResult {
    if !b.capabilities().range_exclusion {
        return Ok(());
    }
    let cat = Arc::new(range_catalog()?);
    b.prepare(&cat).await?;
    let date = |d| -> Result<RangePoint, Failure> { Ok(RangePoint::Date(day(d)?)) };
    let range = |lower, upper| Value::Range(RangeValue { lower, upper });
    let bad = [
        ("empty [4,4)", nights(4, 4)?),
        ("reversed [5,3)", nights(5, 3)?),
        (
            "equal but inclusive [3,3]",
            range(Bound::Included(date(3)?), Bound::Included(date(3)?)),
        ),
        (
            "closed [3,5]",
            range(Bound::Included(date(3)?), Bound::Included(date(5)?)),
        ),
        (
            "lower bound excluded (3,5)",
            range(Bound::Excluded(date(3)?), Bound::Excluded(date(5)?)),
        ),
        (
            "unbounded above [3,)",
            range(Bound::Included(date(3)?), Bound::Unbounded),
        ),
        (
            "unbounded below (,5)",
            range(Bound::Unbounded, Bound::Excluded(date(5)?)),
        ),
        (
            "an integer range in a date range field",
            Value::Range(RangeValue::half_open(
                RangePoint::Int(1),
                RangePoint::Int(2),
            )),
        ),
        (
            "bounds of two kinds",
            Value::Range(RangeValue::half_open(date(1)?, RangePoint::Int(2))),
        ),
        ("a date where a range belongs", Value::Date(day(3)?)),
    ];
    for (what, v) in bad {
        let doc = stay(Some("u1"), v)?;
        expect_insert_kind(b, &cat, &doc, ErrorKind::InvalidValue, what).await?;
    }
    Ok(())
}

pub(super) async fn range_values_roundtrip_exactly<B: StorageBackend>(b: &B) -> CaseResult {
    if !b.capabilities().range_exclusion {
        return Ok(());
    }
    let cat = Arc::new(range_catalog()?);
    b.prepare(&cat).await?;
    let docs = [
        stay(Some("u1"), nights(1, 3)?)?,
        stay(Some("u2"), nights(1, 2)?)?,
        stay(Some("u3"), Value::Null)?,
    ];
    seed(b, &cat, &docs.iter().collect::<Vec<_>>()).await?;
    for d in &docs {
        let stored = load(b, &cat, "stay", d.id).await?;
        ensure!(
            stored.as_ref().map(|s| &s.data) == Some(d),
            "a range must come back as written, [start, end): stored {stored:?}, wrote {d:?}"
        );
    }
    Ok(())
}

fn range_overlaps(from: u8, to: u8) -> Result<Expr, Failure> {
    Ok(Expr::Range(
        Box::new(col("x", "during")?),
        RangeTest::Overlaps {
            start: Value::Date(day(from)?),
            end: Value::Date(day(to)?),
        },
    ))
}

pub(super) async fn range_queries_are_validated<B: StorageBackend>(b: &B) -> CaseResult {
    let cat = Arc::new(range_catalog()?);
    let q = || select_of("stay", "unit");
    let at = |test: RangeTest| -> Result<Expr, Failure> {
        Ok(Expr::Range(Box::new(col("x", "during")?), test))
    };
    let far = Value::Date(
        Date::from_calendar_date(2130, Month::November, 1).map_err(|e| Failure(e.to_string()))?,
    );
    let moment = Value::DateTime(OffsetDateTime::UNIX_EPOCH);
    expect_invalid_filter(b, &cat, q()?, range_overlaps(3, 3)?, "empty tested range").await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        range_overlaps(5, 3)?,
        "reversed tested range",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        at(RangeTest::Overlaps {
            start: Value::Date(day(1)?),
            end: far.clone(),
        })?,
        "span above the limit",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        at(RangeTest::Contains(moment.clone()))?,
        "an instant tested against a date range",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        at(RangeTest::Contains(Value::Int(3)))?,
        "an integer tested against a date range",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        at(RangeTest::Contains(Value::Null))?,
        "null tested against a range",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        at(RangeTest::ContainsRange {
            start: Value::Date(day(1)?),
            end: moment,
        })?,
        "mixed bounds",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        Expr::Range(
            Box::new(col("x", "unit")?),
            RangeTest::Contains(Value::Date(day(1)?)),
        ),
        "a range test on a text field",
    )
    .await
}

pub(super) async fn exclusion_rejects_overlap_but_not_touching<B: StorageBackend>(
    b: &B,
) -> CaseResult {
    if !b.capabilities().range_exclusion {
        return Ok(());
    }
    let cat = Arc::new(range_catalog()?);
    b.prepare(&cat).await?;
    let u1 = stay(Some("u1"), nights(1, 3)?)?;
    let touching = stay(Some("u1"), nights(3, 5)?)?;
    let other_unit = stay(Some("u2"), nights(2, 4)?)?;
    let null_a = stay(None, nights(1, 3)?)?;
    let null_b = stay(None, nights(1, 3)?)?;
    let no_range_a = stay(Some("u1"), Value::Null)?;
    let no_range_b = stay(Some("u1"), Value::Null)?;
    // [1,3) and [3,5) do not overlap; another unit is another scope; a null scope or a null
    // range never conflicts (like a null unique key).
    seed(
        b,
        &cat,
        &[
            &u1,
            &touching,
            &other_unit,
            &null_a,
            &null_b,
            &no_range_a,
            &no_range_b,
        ],
    )
    .await?;
    // [2,4) overlaps both of u1's stays.
    let clash = stay(Some("u1"), nights(2, 4)?)?;
    let mut tx = begin(b, &cat).await?;
    let outcome = match tx.insert(&clash).await {
        Ok(_) => then_commit(tx, Ok(())).await,
        Err(e) => Err(e),
    };
    match outcome {
        Err(e) if e.kind == ErrorKind::ExclusionViolation => {
            ensure!(
                e.entity == Some(n("stay")?)
                    && e.constraint == Some(n("stay_unit_during_excl")?)
                    && e.field == Some(n("during")?),
                "the violation must name stay, stay_unit_during_excl and during: {e}"
            );
            Ok(())
        }
        other => Err(Failure(format!(
            "overlapping stays: expected an exclusion violation, got {other:?}"
        ))),
    }
}

pub(super) async fn exclusion_error_names_no_other_record<B: StorageBackend>(b: &B) -> CaseResult {
    if !b.capabilities().range_exclusion {
        return Ok(());
    }
    let cat = Arc::new(range_catalog()?);
    b.prepare(&cat).await?;
    let held = stay(Some("u1"), nights(1, 3)?)?;
    seed(b, &cat, &[&held]).await?;
    let clash = stay(Some("u1"), nights(2, 4)?)?;
    let mut tx = begin(b, &cat).await?;
    let outcome = match tx.insert(&clash).await {
        Ok(_) => then_commit(tx, Ok(())).await,
        Err(e) => Err(e),
    };
    match outcome {
        Err(e) if e.kind == ErrorKind::ExclusionViolation => {
            let text = e.to_string();
            ensure!(
                !text.contains(&held.id.to_string()),
                "the error must not name the conflicting record, which the caller may not read: {text}"
            );
            Ok(())
        }
        other => Err(Failure(format!(
            "expected an exclusion violation, got {other:?}"
        ))),
    }
}

fn search_for(text: &str, fields: &[&str]) -> Result<SearchTerm, Failure> {
    Ok(SearchTerm {
        mode: SearchMode::Web,
        text: text.into(),
        fields: fields.iter().map(|f| n(f)).collect::<Result<_, _>>()?,
    })
}

pub(super) async fn search_queries_are_validated<B: StorageBackend>(b: &B) -> CaseResult {
    let cat = Arc::new(search_catalog()?);
    let q = || select_of("listing", "title");
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        Expr::Search(search_for("   ", &["title"])?),
        "a blank term",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        Expr::Search(search_for(
            &"a".repeat(MAX_SEARCH_TERM_CHARS + 1),
            &["title"],
        )?),
        "a term over the limit",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        Expr::Search(search_for("harbour", &["nowhere"])?),
        "an undeclared field",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        Expr::Search(search_for("harbour", &[])?),
        "no fields",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        Expr::Search(search_for("harbour", &["title", "title"])?),
        "a field twice",
    )
    .await?;
    let many: Vec<String> = (0..=MAX_SEARCH_FIELDS).map(|i| format!("f{i}")).collect();
    let many: Vec<&str> = many.iter().map(String::as_str).collect();
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        Expr::Search(search_for("harbour", &many)?),
        "more fields than the limit",
    )
    .await?;
    // SearchRank is an ordering key: never selected, never a filter, never nested.
    let rank =
        || -> Result<Expr, Failure> { Ok(Expr::SearchRank(search_for("harbour", &["title"])?)) };
    let mut selected = q()?;
    selected.select.push(sel(rank()?, "rank")?);
    expect_kind(
        b.read(cat.clone(), ReadSource::Primary, &selected).await,
        ErrorKind::InvalidQuery,
        "SearchRank in select",
    )?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        rank()?.cmp(CmpOp::Gt, Expr::value(0)),
        "SearchRank in a comparison",
    )
    .await?;
    let mut nested = q()?;
    nested.order_by = vec![Order {
        expr: Expr::Coalesce(vec![rank()?, Expr::value(0)]),
        dir: OrderDir::Desc,
        nulls: Nulls::Last,
    }];
    expect_kind(
        b.read(cat.clone(), ReadSource::Primary, &nested).await,
        ErrorKind::InvalidQuery,
        "SearchRank inside another expression",
    )?;
    // An entity with no search declaration cannot be searched.
    let base = Arc::new(catalog()?);
    let mut plain = select_of("party", "name")?;
    plain.filter = Some(Expr::Search(search_for("acme", &["name"])?));
    expect_kind(
        b.read(base, ReadSource::Primary, &plain).await,
        ErrorKind::InvalidQuery,
        "search on an entity that declares none",
    )
}

pub(super) async fn search_follows_capabilities<B: StorageBackend>(b: &B) -> CaseResult {
    let cat = Arc::new(search_catalog()?);
    let mut matching = select_of("listing", "title")?;
    matching.filter = Some(Expr::Search(search_for("harbour", &["title", "body"])?));
    matching.order_by = vec![
        Order {
            expr: Expr::SearchRank(search_for("harbour", &["title", "body"])?),
            dir: OrderDir::Desc,
            nulls: Nulls::Last,
        },
        Order {
            expr: col("x", "title")?,
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        },
    ];
    if !b.capabilities().search {
        // Option B: no inverted index, so no search and no approximation.
        expect_kind(
            b.prepare(&cat).await,
            ErrorKind::Unsupported,
            "a search declaration",
        )?;
        expect_kind(
            b.read(cat.clone(), ReadSource::Primary, &matching).await,
            ErrorKind::Unsupported,
            "Search and SearchRank",
        )?;
        let mut rank_only = select_of("listing", "title")?;
        rank_only.order_by = vec![matching.order_by[0].clone()];
        return expect_kind(
            b.read(cat.clone(), ReadSource::Primary, &rank_only).await,
            ErrorKind::Unsupported,
            "SearchRank alone",
        );
    }
    b.prepare(&cat).await?;
    let docs = [
        listing("Harbour flat", Some("Two rooms"))?,
        listing("Mountain cabin", Some("A walk from the harbour"))?,
        listing("City loft", None)?,
    ];
    seed(b, &cat, &docs.iter().collect::<Vec<_>>()).await?;
    let rows = b.read(cat.clone(), ReadSource::Primary, &matching).await?;
    let mut titles: Vec<Value> = rows.rows.into_iter().flatten().collect();
    titles.sort_by_key(|v| format!("{v:?}"));
    ensure!(
        titles == vec![Value::from("Harbour flat"), Value::from("Mountain cabin")],
        "search over title and body: {titles:?}"
    );
    // Only declared fields are searched: `title` alone misses the cabin.
    let mut title_only = select_of("listing", "title")?;
    title_only.filter = Some(Expr::Search(search_for("harbour", &["title"])?));
    let rows = b
        .read(cat.clone(), ReadSource::Primary, &title_only)
        .await?;
    ensure!(
        rows.rows == vec![vec![Value::from("Harbour flat")]],
        "search over title only: {:?}",
        rows.rows
    );
    // A term that is only a stop word matches nothing, not everything.
    let mut stop = select_of("listing", "title")?;
    stop.filter = Some(Expr::Search(search_for("the", &["title", "body"])?));
    let rows = b.read(cat.clone(), ReadSource::Primary, &stop).await?;
    ensure!(rows.rows.is_empty(), "a stop word matched {:?}", rows.rows);
    Ok(())
}
