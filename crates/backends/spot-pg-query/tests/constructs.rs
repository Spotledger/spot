//! ADR-087 constructs (WP-STO-02.T9): golden compiled SQL for geo, range, search and search
//! rank, the parameter typing, and the refusals the compiler owns. Pure: no database. The
//! index and plan cases run on real PostgreSQL in the `spot-postgres` harness.

mod common;

use common::{f, n, sel};
use rust_decimal::Decimal;
use spot_pg_query::{CompiledQuery, DefaultSchemaMap, Param, PgType, compile};
use spot_storage_contract::contract_tests::{geo_catalog, range_catalog, search_catalog};
use spot_storage_contract::{
    Catalog, Collation, EntityDef, EntityKind, ErrorKind, Expr, FieldDef, FieldType, GeoTest,
    MAX_GEO_RADIUS_M, MAX_SEARCH_TERM_CHARS, Nulls, Order, OrderDir, Query, QueryPath, RangeOf,
    RangeTest, SearchMode, SearchTerm, StorageError, Value,
};
use time::{Date, Month, OffsetDateTime};

fn render(c: &CompiledQuery) -> String {
    let mut out = format!("{}\n", c.sql);
    for (i, p) in c.params.iter().enumerate() {
        out.push_str(&format!("${} = {p:?}\n", i + 1));
    }
    out.push_str(&format!("columns = {:?}\n", c.columns));
    out.push_str(&format!("decode = {:?}\n", c.decode));
    out
}

fn run(q: &Query, cat: &Catalog) -> Result<CompiledQuery, StorageError> {
    compile(q, cat, &DefaultSchemaMap)
}

fn ok(q: &Query, cat: &Catalog) -> CompiledQuery {
    run(q, cat).unwrap_or_else(|e| panic!("{e}"))
}

fn refused(q: &Query, cat: &Catalog) -> StorageError {
    match run(q, cat) {
        Err(e) => e,
        Ok(c) => panic!("expected a refusal, compiled: {}", c.sql),
    }
}

fn dec(units: i64, scale: u32) -> Decimal {
    Decimal::new(units, scale)
}

fn day(d: u8) -> Date {
    Date::from_calendar_date(2026, Month::November, d).unwrap_or_else(|e| panic!("{e}"))
}

fn on(entity: &str, field: &str) -> Query {
    let mut q = Query::from(&n(entity), &n("x"), QueryPath::Transactional);
    q.select = vec![sel(f("x", field), field)];
    q
}

fn place_with(test: GeoTest) -> Query {
    let mut q = Query::from(&n("place"), &n("x"), QueryPath::Transactional);
    q.select = vec![sel(f("x", "name"), "name")];
    q.filter = Some(Expr::Geo(Box::new(f("x", "pin")), test));
    q
}

fn karachi(metres: u32) -> GeoTest {
    GeoTest::WithinDistance {
        lon: dec(670_011_234, 7),
        lat: dec(248_607_345, 7),
        metres,
    }
}

fn stay_with(test: RangeTest) -> Query {
    let mut q = Query::from(&n("stay"), &n("x"), QueryPath::Transactional);
    q.select = vec![sel(f("x", "unit"), "unit")];
    q.filter = Some(Expr::Range(Box::new(f("x", "during")), test));
    q
}

fn term(mode: SearchMode, text: &str) -> SearchTerm {
    SearchTerm {
        mode,
        text: text.to_owned(),
        fields: vec![n("title"), n("body")],
    }
}

fn listing(filter: Option<Expr>) -> Query {
    let mut q = Query::from(&n("listing"), &n("x"), QueryPath::Transactional);
    q.select = vec![sel(f("x", "title"), "title")];
    q.filter = filter;
    q
}

/// A catalog with one range field of each kind, and a search declaration with typo tolerance.
fn wide_catalog() -> Catalog {
    let range = |name: &str, of| FieldDef {
        name: n(name),
        ty: FieldType::Range { of },
        nullable: true,
    };
    let base = search_catalog().unwrap_or_else(|e| panic!("{e}"));
    let mut listing = base
        .entity(&n("listing"))
        .unwrap_or_else(|e| panic!("{e}"))
        .clone();
    if let Some(s) = listing.search.as_mut() {
        s.typo_tolerance = true;
    }
    let slots = EntityDef {
        name: n("slot"),
        kind: EntityKind::Document,
        fields: vec![
            range("qty", RangeOf::Int),
            range("price", RangeOf::Decimal),
            range("days", RangeOf::Date),
            range("at", RangeOf::DateTime),
        ],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    };
    Catalog::new(
        1,
        Collation::new("en").unwrap_or_else(|e| panic!("{e}")),
        vec![listing, slots],
    )
    .unwrap_or_else(|e| panic!("{e}"))
}

fn slot_with(field: &str, test: RangeTest) -> Query {
    let mut q = Query::from(&n("slot"), &n("x"), QueryPath::Transactional);
    q.select = vec![sel(Expr::Value(Value::Int(1)), "one")];
    q.filter = Some(Expr::Range(Box::new(f("x", field)), test));
    q
}

macro_rules! snap {
    ($name:literal, $q:expr, $cat:expr) => {{
        insta::assert_snapshot!($name, render(&ok(&$q, &$cat)));
    }};
}

fn geo_cat() -> Catalog {
    geo_catalog().unwrap_or_else(|e| panic!("{e}"))
}
fn range_cat() -> Catalog {
    range_catalog().unwrap_or_else(|e| panic!("{e}"))
}
fn search_cat() -> Catalog {
    search_catalog().unwrap_or_else(|e| panic!("{e}"))
}

// ---- geo -----------------------------------------------------------------------------------

#[test]
fn geo_within_distance() {
    snap!("geo_within_distance", place_with(karachi(5_000)), geo_cat());
}

#[test]
fn geo_within_distance_zero_and_maximum_radius() {
    snap!(
        "geo_distance_zero_metres",
        place_with(karachi(0)),
        geo_cat()
    );
    snap!(
        "geo_distance_maximum_radius",
        place_with(karachi(MAX_GEO_RADIUS_M)),
        geo_cat()
    );
}

#[test]
fn geo_within_box() {
    let t = GeoTest::WithinBox {
        min_lon: dec(669_000_000, 7),
        min_lat: dec(247_000_000, 7),
        max_lon: dec(672_500_000, 7),
        max_lat: dec(250_000_000, 7),
    };
    snap!("geo_within_box", place_with(t), geo_cat());
}

#[test]
fn geo_distance_filters_the_bare_column_before_the_cast() {
    let c = ok(&place_with(karachi(5_000)), &geo_cat());
    // The index-usable box test names the bare column; the geography cast comes only after.
    let bare = c.sql.find("\"x\".\"pin\" && ST_Expand(");
    let cast = c.sql.find("\"x\".\"pin\"::geography");
    assert!(bare.is_some() && cast.is_some() && bare < cast, "{}", c.sql);
    // No function wraps the column in the box test.
    assert!(!c.sql.contains("(\"x\".\"pin\") &&"), "{}", c.sql);
    assert!(!c.sql.contains("ST_Transform"), "{}", c.sql);
}

#[test]
fn geo_coordinates_bind_as_exact_decimals_and_metres_as_whole_integers() {
    let c = ok(&place_with(karachi(5_000)), &geo_cat());
    assert_eq!(
        c.params[0],
        Param::Numeric(dec(670_011_234, 7)),
        "{:?}",
        c.params
    );
    assert_eq!(c.params[1], Param::Numeric(dec(248_607_345, 7)));
    assert_eq!(c.params[2], Param::Int8(5_000));
    assert!(
        c.params
            .iter()
            .all(|p| p.pg_type() != PgType::Text || matches!(p, Param::Text(_)))
    );
    // Never a float parameter: the cast to float8 happens inside PostgreSQL.
    assert!(c.sql.contains("$1::float8"), "{}", c.sql);
}

#[test]
fn geo_refusals() {
    let cat = geo_cat();
    let too_wide = refused(&place_with(karachi(MAX_GEO_RADIUS_M + 1)), &cat);
    assert_eq!(too_wide.kind, ErrorKind::InvalidQuery);
    let lat = GeoTest::WithinDistance {
        lon: dec(0, 0),
        lat: dec(91, 0),
        metres: 10,
    };
    assert_eq!(
        refused(&place_with(lat), &cat).kind,
        ErrorKind::InvalidQuery
    );
    let scale = GeoTest::WithinDistance {
        lon: dec(123_456_789, 8),
        lat: dec(0, 0),
        metres: 10,
    };
    assert_eq!(
        refused(&place_with(scale), &cat).kind,
        ErrorKind::InvalidQuery
    );
    // An operand of the wrong type: a text field.
    let mut q = place_with(karachi(10));
    q.filter = Some(Expr::Geo(Box::new(f("x", "name")), karachi(10)));
    assert_eq!(refused(&q, &cat).kind, ErrorKind::InvalidQuery);
    // Ordering by a geo field.
    let mut q = place_with(karachi(10));
    q.order_by = vec![Order {
        expr: f("x", "pin"),
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    }];
    let e = refused(&q, &cat);
    assert_eq!(e.kind, ErrorKind::InvalidQuery);
    let msg = e.to_string();
    assert!(msg.contains("cannot be ordered by"), "{msg}");
}

// ---- range ---------------------------------------------------------------------------------

#[test]
fn range_overlap_and_containment() {
    let (a, b) = (Value::Date(day(2)), Value::Date(day(5)));
    snap!(
        "range_overlaps",
        stay_with(RangeTest::Overlaps {
            start: a.clone(),
            end: b.clone()
        }),
        range_cat()
    );
    snap!(
        "range_contains_element",
        stay_with(RangeTest::Contains(a.clone())),
        range_cat()
    );
    snap!(
        "range_contains_range",
        stay_with(RangeTest::ContainsRange { start: a, end: b }),
        range_cat()
    );
}

#[test]
fn range_every_kind_uses_its_own_constructor_and_typed_bounds() {
    let cat = wide_catalog();
    let i = |a, b| RangeTest::Overlaps {
        start: Value::Int(a),
        end: Value::Int(b),
    };
    snap!("range_int", slot_with("qty", i(1, 9)), cat);
    snap!(
        "range_decimal",
        slot_with(
            "price",
            RangeTest::Overlaps {
                start: Value::Decimal(dec(1050, 2)),
                end: Value::Decimal(dec(2000, 2)),
            }
        ),
        cat
    );
    snap!(
        "range_date",
        slot_with(
            "days",
            RangeTest::ContainsRange {
                start: Value::Date(day(1)),
                end: Value::Date(day(3)),
            }
        ),
        cat
    );
    let t0 = OffsetDateTime::UNIX_EPOCH;
    snap!(
        "range_instant",
        slot_with(
            "at",
            RangeTest::Overlaps {
                start: Value::DateTime(t0),
                end: Value::DateTime(t0 + time::Duration::hours(2)),
            }
        ),
        cat
    );
    snap!(
        "range_contains_instant",
        slot_with("at", RangeTest::Contains(Value::DateTime(t0))),
        cat
    );
}

#[test]
fn range_refusals() {
    let cat = range_cat();
    let empty = RangeTest::Overlaps {
        start: Value::Date(day(5)),
        end: Value::Date(day(5)),
    };
    assert_eq!(
        refused(&stay_with(empty), &cat).kind,
        ErrorKind::InvalidQuery
    );
    // A date range is never tested with an instant.
    let instant = RangeTest::Contains(Value::DateTime(OffsetDateTime::UNIX_EPOCH));
    assert_eq!(
        refused(&stay_with(instant), &cat).kind,
        ErrorKind::InvalidQuery
    );
    let huge = RangeTest::Overlaps {
        start: Value::Date(day(1)),
        end: Value::Date(
            Date::from_calendar_date(2200, Month::January, 1).unwrap_or_else(|e| panic!("{e}")),
        ),
    };
    assert_eq!(
        refused(&stay_with(huge), &cat).kind,
        ErrorKind::InvalidQuery
    );
    // A range test on a text field.
    let mut q = stay_with(RangeTest::Contains(Value::Date(day(1))));
    q.filter = Some(Expr::Range(
        Box::new(f("x", "unit")),
        RangeTest::Contains(Value::Date(day(1))),
    ));
    assert_eq!(refused(&q, &cat).kind, ErrorKind::InvalidQuery);
}

// ---- search --------------------------------------------------------------------------------

#[test]
fn search_modes() {
    for (name, mode) in [
        ("search_web", SearchMode::Web),
        ("search_phrase", SearchMode::Phrase),
        ("search_plain", SearchMode::Plain),
    ] {
        let q = listing(Some(Expr::Search(term(mode, "harbour view"))));
        let c = ok(&q, &search_cat());
        insta::assert_snapshot!(name, render(&c));
    }
}

#[test]
fn search_with_declared_typo_tolerance_adds_trigram_similarity() {
    let q = listing(Some(Expr::Search(term(SearchMode::Web, "harbur"))));
    snap!("search_typo_tolerance", q, wide_catalog());
    // Not declared: no trigram operator.
    let c = ok(&q, &search_cat());
    assert!(!c.sql.contains(" % "), "{}", c.sql);
}

#[test]
fn search_rank_orders_and_is_never_selected() {
    let mut q = listing(Some(Expr::Search(term(SearchMode::Web, "harbour"))));
    q.order_by = vec![Order {
        expr: Expr::SearchRank(term(SearchMode::Web, "harbour")),
        dir: OrderDir::Desc,
        nulls: Nulls::Last,
    }];
    let c = ok(&q, &search_cat());
    insta::assert_snapshot!("search_rank_order_by", render(&c));
    let order = c.sql.split(" ORDER BY ").nth(1).unwrap_or_default();
    let select = c.sql.split(" FROM ").next().unwrap_or_default();
    assert!(order.contains("ts_rank"), "{}", c.sql);
    assert!(!select.contains("ts_rank"), "{}", c.sql);
    // The contract's id tie-break follows the rank.
    assert!(order.ends_with("\"x\".\"id\" ASC OFFSET $5"), "{}", c.sql);
}

#[test]
fn search_refusals() {
    let cat = search_cat();
    // SearchRank outside order_by.
    let rank = || Expr::SearchRank(term(SearchMode::Web, "harbour"));
    let mut q = listing(Some(rank()));
    assert_eq!(refused(&q, &cat).kind, ErrorKind::InvalidQuery);
    q = listing(None);
    q.select = vec![sel(rank(), "r")];
    assert_eq!(refused(&q, &cat).kind, ErrorKind::InvalidQuery);
    // A term above the limit, a blank term, an unknown field, an entity without search.
    let long = "x".repeat(MAX_SEARCH_TERM_CHARS + 1);
    for bad in [
        term(SearchMode::Web, &long),
        term(SearchMode::Web, "   "),
        SearchTerm {
            mode: SearchMode::Web,
            text: "a".into(),
            fields: vec![n("nope")],
        },
    ] {
        let q = listing(Some(Expr::Search(bad)));
        assert_eq!(refused(&q, &cat).kind, ErrorKind::InvalidQuery);
    }
    let mut q = on("stay", "unit");
    q.filter = Some(Expr::Search(term(SearchMode::Web, "x")));
    assert_eq!(refused(&q, &range_cat()).kind, ErrorKind::InvalidQuery);
}

// ---- no value in the text --------------------------------------------------------------

#[test]
fn no_literal_value_term_or_coordinate_reaches_the_sql_text() {
    let secret = "needle'; DROP TABLE listing; --";
    let q = listing(Some(Expr::Or(vec![
        Expr::Search(term(SearchMode::Web, secret)),
        Expr::Search(term(SearchMode::Phrase, "ocean 12345")),
    ])));
    let mut q = q;
    q.order_by = vec![Order {
        expr: Expr::SearchRank(term(SearchMode::Plain, "rankneedle")),
        dir: OrderDir::Desc,
        nulls: Nulls::Last,
    }];
    let c = ok(&q, &wide_catalog());
    for leaked in ["needle", "DROP", "ocean", "12345", "rankneedle", "listing;"] {
        // The entity name `listing` is an identifier; the others are values.
        if leaked == "listing;" {
            continue;
        }
        assert!(!c.sql.contains(leaked), "{leaked} in {}", c.sql);
    }

    let c = ok(&place_with(karachi(5_000)), &geo_cat());
    for leaked in ["67.0011234", "24.8607345", "670011234", "5000", "5_000"] {
        assert!(!c.sql.contains(leaked), "{leaked} in {}", c.sql);
    }
    let b = GeoTest::WithinBox {
        min_lon: dec(669_123_456, 7),
        min_lat: dec(247_123_456, 7),
        max_lon: dec(672_123_456, 7),
        max_lat: dec(250_123_456, 7),
    };
    let c = ok(&place_with(b), &geo_cat());
    for leaked in ["66.9123456", "24.7123456", "123456"] {
        assert!(!c.sql.contains(leaked), "{leaked} in {}", c.sql);
    }

    let c = ok(
        &stay_with(RangeTest::Overlaps {
            start: Value::Date(day(2)),
            end: Value::Date(day(19)),
        }),
        &range_cat(),
    );
    for leaked in ["2026", "-11-", "19"] {
        assert!(!c.sql.contains(leaked), "{leaked} in {}", c.sql);
    }
}

#[test]
fn the_sql_text_does_not_depend_on_the_values() {
    let a = ok(&place_with(karachi(10)), &geo_cat());
    let b = ok(
        &place_with(GeoTest::WithinDistance {
            lon: dec(-1, 0),
            lat: dec(1, 0),
            metres: 999,
        }),
        &geo_cat(),
    );
    assert_eq!(a.sql, b.sql);
    let t = |s: &str| listing(Some(Expr::Search(term(SearchMode::Web, s))));
    assert_eq!(
        ok(&t("one"), &search_cat()).sql,
        ok(&t("two words"), &search_cat()).sql
    );
}
