//! The construct cases (ADR-087): geo, range and exclusion, search. See the module docs of
//! [`super`] for how they follow the backend's [`Capabilities`].

use super::*;

/// A catalog with one geo field: `place` with a unique `name`, a nullable `pin` and a GiST
/// index over it.
pub fn geo_catalog() -> StorageResult<Catalog> {
    let mut gist = IndexDef::btree(Name::new("place_pin")?, vec![Name::new("pin")?], false);
    gist.method = IndexMethod::Gist;
    Catalog::new(
        1,
        Collation::new("en")?,
        vec![EntityDef {
            name: Name::new("place")?,
            kind: EntityKind::Document,
            fields: vec![
                FieldDef {
                    name: Name::new("name")?,
                    ty: FieldType::Text {
                        max_chars: Some(140),
                    },
                    nullable: false,
                },
                FieldDef {
                    name: Name::new("pin")?,
                    ty: FieldType::Geo { srid: GEO_SRID },
                    nullable: true,
                },
            ],
            children: vec![],
            indexes: vec![
                IndexDef::btree(Name::new("place_name")?, vec![Name::new("name")?], true),
                gist,
            ],
            exclusions: vec![],
            search: None,
        }],
    )
}

/// A catalog with a date-range field and an exclusion: `stay`, whose `during` ranges may not
/// overlap within one `unit`.
pub fn range_catalog() -> StorageResult<Catalog> {
    Catalog::new(
        1,
        Collation::new("en")?,
        vec![EntityDef {
            name: Name::new("stay")?,
            kind: EntityKind::Document,
            fields: vec![
                FieldDef {
                    name: Name::new("unit")?,
                    ty: FieldType::Text {
                        max_chars: Some(63),
                    },
                    nullable: true,
                },
                FieldDef {
                    name: Name::new("during")?,
                    ty: FieldType::Range { of: RangeOf::Date },
                    nullable: true,
                },
            ],
            children: vec![],
            indexes: vec![],
            exclusions: vec![ExclusionDef {
                name: Name::new("stay_unit_during_excl")?,
                field: Name::new("during")?,
                scope: vec![Name::new("unit")?],
                message: "Those dates are already taken for this unit".into(),
            }],
            search: None,
        }],
    )
}

/// A catalog with a search declaration: `listing`, searchable by `title` (weight A) and `body`
/// (weight D).
pub fn search_catalog() -> StorageResult<Catalog> {
    Catalog::new(
        1,
        Collation::new("en")?,
        vec![EntityDef {
            name: Name::new("listing")?,
            kind: EntityKind::Document,
            fields: vec![
                FieldDef {
                    name: Name::new("title")?,
                    ty: FieldType::Text {
                        max_chars: Some(140),
                    },
                    nullable: false,
                },
                FieldDef {
                    name: Name::new("body")?,
                    ty: FieldType::Text { max_chars: None },
                    nullable: true,
                },
            ],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: Some(SearchDef {
                language: None,
                fields: vec![
                    SearchField {
                        name: Name::new("title")?,
                        weight: SearchWeight::A,
                    },
                    SearchField {
                        name: Name::new("body")?,
                        weight: SearchWeight::D,
                    },
                ],
                facets: vec![],
                typo_tolerance: false,
            }),
        }],
    )
}

pub(super) fn dec(units: i64, scale: u32) -> Decimal {
    Decimal::new(units, scale)
}

pub(super) fn point(lon: Decimal, lat: Decimal) -> Value {
    Value::GeoPoint(GeoPoint::new(lon, lat))
}

pub(super) fn place(name: &str, pin: Value) -> Result<DocumentData, Failure> {
    let mut fields = Fields::new();
    fields.insert(n("name")?, name.into());
    fields.insert(n("pin")?, pin);
    Ok(DocumentData {
        entity: n("place")?,
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    })
}

pub(super) fn day(d: u8) -> Result<Date, Failure> {
    Date::from_calendar_date(2026, Month::November, d).map_err(|e| Failure(e.to_string()))
}

/// The half-open date range `[from, to)` in November 2026.
pub(super) fn nights(from: u8, to: u8) -> Result<Value, Failure> {
    Ok(Value::Range(RangeValue::half_open(
        RangePoint::Date(day(from)?),
        RangePoint::Date(day(to)?),
    )))
}

pub(super) fn stay(unit: Option<&str>, during: Value) -> Result<DocumentData, Failure> {
    let mut fields = Fields::new();
    fields.insert(n("unit")?, unit.map_or(Value::Null, Value::from));
    fields.insert(n("during")?, during);
    Ok(DocumentData {
        entity: n("stay")?,
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    })
}

pub(super) fn listing(title: &str, body: Option<&str>) -> Result<DocumentData, Failure> {
    let mut fields = Fields::new();
    fields.insert(n("title")?, title.into());
    fields.insert(n("body")?, body.map_or(Value::Null, Value::from));
    Ok(DocumentData {
        entity: n("listing")?,
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    })
}

/// A report query selecting `field` of `entity` (alias `x`).
pub(super) fn select_of(entity: &str, field: &str) -> Result<Query, Failure> {
    let mut q = Query::from(&n(entity)?, &n("x")?, QueryPath::Report);
    q.select = vec![sel(col("x", field)?, field)?];
    Ok(q)
}

/// Runs `q` with `filter` outside a transaction and expects `InvalidQuery`.
pub(super) async fn expect_invalid_filter<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
    mut q: Query,
    filter: Expr,
    what: &str,
) -> CaseResult {
    q.filter = Some(filter);
    expect_kind(
        b.read(cat.clone(), ReadSource::Primary, &q).await,
        ErrorKind::InvalidQuery,
        what,
    )
}

/// Inserts `doc` in a throwaway transaction and expects `kind`.
pub(super) async fn expect_insert_kind<B: StorageBackend>(
    b: &B,
    cat: &Arc<Catalog>,
    doc: &DocumentData,
    kind: ErrorKind,
    what: &str,
) -> CaseResult {
    let mut tx = begin(b, cat).await?;
    let r = tx.insert(doc).await;
    tx.rollback().await?;
    expect_kind(r, kind, what)
}

pub(super) async fn geo_values_roundtrip_exactly<B: StorageBackend>(b: &B) -> CaseResult {
    let cat = Arc::new(geo_catalog()?);
    b.prepare(&cat).await?;
    // Seven decimal places, both hemispheres; the origin; a null.
    let docs = [
        place("exact", point(dec(739_000_001, 7), dec(-338_688_197, 7)))?,
        place("origin", point(dec(0, 0), dec(0, 0)))?,
        place("edge", point(dec(180, 0), dec(-90, 0)))?,
        place("unknown", Value::Null)?,
    ];
    seed(b, &cat, &docs.iter().collect::<Vec<_>>()).await?;
    for d in &docs {
        let stored = load(b, &cat, "place", d.id).await?;
        ensure!(
            stored.as_ref().map(|s| &s.data) == Some(d),
            "a geo point must come back exactly: stored {stored:?}, wrote {d:?}"
        );
    }
    let pin = n("pin")?;
    let null_pin = load(b, &cat, "place", docs[3].id).await?;
    let origin_pin = load(b, &cat, "place", docs[1].id).await?;
    ensure!(
        null_pin.is_some_and(|d| d.data.fields.get(&pin) == Some(&Value::Null))
            && origin_pin
                .is_some_and(|d| matches!(d.data.fields.get(&pin), Some(Value::GeoPoint(_)))),
        "a null point is not the point at 0,0"
    );
    Ok(())
}

pub(super) async fn geo_values_are_validated<B: StorageBackend>(b: &B) -> CaseResult {
    let cat = Arc::new(geo_catalog()?);
    b.prepare(&cat).await?;
    let bad = [
        ("latitude 91", point(dec(0, 0), dec(91, 0))),
        ("latitude below -90", point(dec(0, 0), dec(-900_000_001, 7))),
        ("longitude 181", point(dec(181, 0), dec(0, 0))),
        (
            "longitude below -180",
            point(dec(-1_800_000_001, 7), dec(0, 0)),
        ),
        ("eight decimal places", point(dec(1, 8), dec(0, 0))),
        ("a text where a point belongs", "51.5,-0.1".into()),
    ];
    for (what, v) in bad {
        let doc = place(what, v)?;
        expect_insert_kind(b, &cat, &doc, ErrorKind::InvalidValue, what).await?;
    }
    Ok(())
}

pub(super) fn geo_within(lon: Decimal, lat: Decimal, metres: u32) -> Result<Expr, Failure> {
    Ok(Expr::Geo(
        Box::new(col("x", "pin")?),
        GeoTest::WithinDistance { lon, lat, metres },
    ))
}

pub(super) async fn geo_filters_are_validated<B: StorageBackend>(b: &B) -> CaseResult {
    // Validation runs before any capability check, so every backend refuses these the same way.
    let cat = Arc::new(geo_catalog()?);
    let q = || select_of("place", "name");
    let within = |lon, lat, m| geo_within(lon, lat, m);
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        within(dec(0, 0), dec(91, 0), 10)?,
        "latitude 91",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        within(dec(-181, 0), dec(0, 0), 10)?,
        "longitude -181",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        within(dec(1, 8), dec(0, 0), 10)?,
        "eight decimal places",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        within(dec(0, 0), dec(0, 0), MAX_GEO_RADIUS_M + 1)?,
        "radius above the limit",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        Expr::Geo(
            Box::new(col("x", "pin")?),
            GeoTest::WithinBox {
                min_lon: dec(1, 0),
                min_lat: dec(0, 0),
                max_lon: dec(0, 0),
                max_lat: dec(1, 0),
            },
        ),
        "inverted box",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        Expr::Geo(
            Box::new(col("x", "name")?),
            GeoTest::WithinDistance {
                lon: dec(0, 0),
                lat: dec(0, 0),
                metres: 10,
            },
        ),
        "geo test on a text field",
    )
    .await?;
    expect_invalid_filter(
        b,
        &cat,
        q()?,
        Expr::Geo(
            Box::new(Expr::value("x")),
            GeoTest::WithinDistance {
                lon: dec(0, 0),
                lat: dec(0, 0),
                metres: 10,
            },
        ),
        "geo test on a literal",
    )
    .await?;
    let mut ordered = q()?;
    ordered.order_by = vec![Order {
        expr: col("x", "pin")?,
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    }];
    expect_kind(
        b.read(cat.clone(), ReadSource::Primary, &ordered).await,
        ErrorKind::InvalidQuery,
        "order_by on a geo field",
    )
}

pub(super) async fn geo_filters_follow_capabilities<B: StorageBackend>(b: &B) -> CaseResult {
    let cat = Arc::new(geo_catalog()?);
    b.prepare(&cat).await?;
    // On the equator one degree of longitude is 111,319.49 m, so these are hand-computed:
    // 0.005 degrees is about 557 m, 0.01 about 1,113 m.
    let pts = [
        ("a_origin", point(dec(0, 0), dec(0, 0))),
        ("b_557m", point(dec(5, 3), dec(0, 0))),
        ("c_1113m", point(dec(1, 2), dec(0, 0))),
        ("d_111km", point(dec(1, 0), dec(0, 0))),
        ("e_unknown", Value::Null),
    ];
    let docs = pts
        .into_iter()
        .map(|(name, pin)| place(name, pin))
        .collect::<Result<Vec<_>, _>>()?;
    seed(b, &cat, &docs.iter().collect::<Vec<_>>()).await?;
    let names_where = |filter: Expr| -> Result<Query, Failure> {
        let mut q = select_of("place", "name")?;
        q.filter = Some(filter);
        q.order_by = vec![Order {
            expr: col("x", "name")?,
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        }];
        Ok(q)
    };
    let within_1000 = names_where(geo_within(dec(0, 0), dec(0, 0), 1_000)?)?;
    let in_box = names_where(Expr::Geo(
        Box::new(col("x", "pin")?),
        GeoTest::WithinBox {
            min_lon: dec(-1, 3),
            min_lat: dec(-1, 3),
            max_lon: dec(75, 4),
            max_lat: dec(1, 3),
        },
    ))?;
    if !b.capabilities().geo_filters {
        for (q, what) in [(&within_1000, "WithinDistance"), (&in_box, "WithinBox")] {
            expect_kind(
                b.read(cat.clone(), ReadSource::Primary, q).await,
                ErrorKind::Unsupported,
                what,
            )?;
            let mut tx = begin(b, &cat).await?;
            expect_kind(tx.query(q).await, ErrorKind::Unsupported, what)?;
        }
        return Ok(());
    }
    let names = |rows: Rows| -> Vec<Value> { rows.rows.into_iter().flatten().collect() };
    let got = names(
        b.read(cat.clone(), ReadSource::Primary, &within_1000)
            .await?,
    );
    ensure!(
        got == vec![Value::from("a_origin"), Value::from("b_557m")],
        "within 1,000 m: {got:?}"
    );
    let got = names(b.read(cat.clone(), ReadSource::Primary, &in_box).await?);
    ensure!(
        got == vec![Value::from("a_origin"), Value::from("b_557m")],
        "in the box: {got:?}"
    );
    let within_everything = names_where(geo_within(dec(0, 0), dec(0, 0), 112_000)?)?;
    let got = names(
        b.read(cat.clone(), ReadSource::Primary, &within_everything)
            .await?,
    );
    ensure!(
        got.len() == 4,
        "within 112 km finds every non-null point, and only those: {got:?}"
    );
    Ok(())
}

pub(super) async fn constructs_are_validated_by_the_catalog<B: StorageBackend>(
    _: &B,
) -> CaseResult {
    // The shared catalog validation: every backend refuses these the same way, at `Catalog::new`.
    for ok in [geo_catalog(), range_catalog(), search_catalog()] {
        ok?;
    }
    type Change = Box<dyn Fn(&mut EntityDef) -> Result<(), Failure>>;
    let with = |change: &Change| -> Result<StorageResult<Catalog>, Failure> {
        let mut e = range_catalog()?.entity(&n("stay")?)?.clone();
        e.fields.push(FieldDef {
            name: n("title")?,
            ty: FieldType::Text { max_chars: None },
            nullable: true,
        });
        change(&mut e)?;
        Ok(Catalog::new(1, Collation::new("en")?, vec![e]))
    };
    let text_field = |nm: &str| -> Result<FieldDef, Failure> {
        Ok(FieldDef {
            name: n(nm)?,
            ty: FieldType::Text { max_chars: None },
            nullable: true,
        })
    };
    let search_over = |fields: Vec<SearchField>| SearchDef {
        language: None,
        fields,
        facets: vec![],
        typo_tolerance: false,
    };
    let bad: Vec<(&str, Change)> = vec![
        (
            "SRID 3857",
            Box::new(|e| {
                e.fields.push(FieldDef {
                    name: n("g")?,
                    ty: FieldType::Geo { srid: 3857 },
                    nullable: true,
                });
                Ok(())
            }),
        ),
        (
            "exclusion on a text field",
            Box::new(|e| {
                e.exclusions[0].field = n("unit")?;
                Ok(())
            }),
        ),
        (
            "exclusion scoped by an unknown field",
            Box::new(|e| {
                e.exclusions[0].scope = vec![n("nowhere")?];
                Ok(())
            }),
        ),
        (
            "exclusion scoped by its own range",
            Box::new(|e| {
                e.exclusions[0].scope = vec![n("during")?];
                Ok(())
            }),
        ),
        (
            "search over a range field",
            Box::new(|e| {
                e.search = Some(SearchDef {
                    language: None,
                    fields: vec![SearchField {
                        name: n("during")?,
                        weight: SearchWeight::A,
                    }],
                    facets: vec![],
                    typo_tolerance: false,
                });
                Ok(())
            }),
        ),
        (
            "search with no fields",
            Box::new(|e| {
                e.search = Some(SearchDef {
                    language: None,
                    fields: vec![],
                    facets: vec![],
                    typo_tolerance: false,
                });
                Ok(())
            }),
        ),
        (
            "a unique GiST index",
            Box::new(|e| {
                let mut ix = IndexDef::btree(n("stay_u")?, vec![n("unit")?], true);
                ix.method = IndexMethod::Gist;
                e.indexes.push(ix);
                Ok(())
            }),
        ),
        (
            "a B-tree index over a range",
            Box::new(|e| {
                e.indexes
                    .push(IndexDef::btree(n("stay_d")?, vec![n("during")?], false));
                Ok(())
            }),
        ),
        (
            "a trigram class on a B-tree index",
            Box::new(|e| {
                let mut ix = IndexDef::btree(n("stay_t")?, vec![n("title")?], false);
                ix.opclass = Some(OpClass::GinTrgm);
                e.indexes.push(ix);
                Ok(())
            }),
        ),
        (
            "an include of an unknown field",
            Box::new(|e| {
                let mut ix = IndexDef::btree(n("stay_i")?, vec![n("unit")?], false);
                ix.include = vec![n("nowhere")?];
                e.indexes.push(ix);
                Ok(())
            }),
        ),
    ];
    for (what, change) in &bad {
        expect_kind(with(change)?, ErrorKind::InvalidSchema, what)?;
    }
    // A valid trigram index and a valid include are accepted.
    let good: Change = Box::new(|e| {
        let mut ix = IndexDef::btree(n("stay_ok")?, vec![n("title")?], false);
        ix.method = IndexMethod::Gin;
        ix.opclass = Some(OpClass::GinTrgm);
        e.indexes.push(ix);
        let mut cover = IndexDef::btree(n("stay_cover")?, vec![n("unit")?], false);
        cover.include = vec![n("title")?];
        e.indexes.push(cover);
        Ok(())
    });
    with(&good)??;
    // Too many searchable fields.
    let many = (0..=MAX_SEARCH_FIELDS)
        .map(|i| text_field(&format!("t{i}")))
        .collect::<Result<Vec<_>, _>>()?;
    let search_all = search_over(
        many.iter()
            .map(|f| SearchField {
                name: f.name.clone(),
                weight: SearchWeight::C,
            })
            .collect(),
    );
    let e = EntityDef {
        name: n("wide")?,
        kind: EntityKind::Document,
        fields: many,
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: Some(search_all),
    };
    expect_kind(
        Catalog::new(1, Collation::new("en")?, vec![e]),
        ErrorKind::InvalidSchema,
        "more searchable fields than the limit",
    )
}
