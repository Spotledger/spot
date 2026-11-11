//! The ADR-087 constructs: geo tests, range tests, search and search rank.
//!
//! - **Values are parameters.** Coordinates are bound as `numeric` (their exact decimal text)
//!   and cast to `float8` inside PostgreSQL, so the application never converts a decimal to a
//!   float. A radius is bound as a whole `int8` number of metres. Range bounds are bound as
//!   `int8`, `numeric`, `date` or `timestamptz`. A search term is bound as `text`. Only
//!   constants of the compiler itself (the SRID, `'[)'`, the text search configuration, the
//!   bounding-box margin) appear in the SQL text.
//! - **Geo distance** is `ST_DWithin` on `geography` (whole metres on the spheroid), preceded by
//!   a conservative bounding-box test `col && ST_Expand(point, dx, dy)` on the bare geometry
//!   column. The box is what lets the GiST index on the geometry column serve the query: a cast
//!   such as `col::geography` would hide the column from that index, so it appears only in the
//!   exact test, after the box has narrowed the rows. The box is a superset by construction: it
//!   uses 100 km per degree (the true figure is at least 110.5 km for latitude and 111.3 km
//!   times cosine of latitude for longitude), takes the cosine at the pole-most latitude of the
//!   circle, and becomes full width when it would cross the antimeridian or reach a pole.
//! - **Geo box** is `col && ST_MakeEnvelope(...)`; `&&` includes the edges.
//! - **Ranges** are built with the half-open flag `'[)'`, and tested with `&&` (overlap) and
//!   `@>` (contains a point, or a range). Two ranges that only touch do not overlap.
//! - **Search** is evaluated against the projection table (`SEARCH_TABLE`): a correlated
//!   `EXISTS` for a filter, a correlated scalar subquery for the rank. The term is parsed by
//!   `websearch_to_tsquery` (`Web`), `phraseto_tsquery` (`Phrase`) or `plainto_tsquery`
//!   (`Plain`). `ts_rank` appears only as an `ORDER BY` key; the contract's `id` tie-break
//!   follows it like any other key.

use rust_decimal::Decimal;
use spot_storage_contract::{
    EntityDef, Expr, FieldType, GEO_SCALE, GeoTest, MAX_GEO_RADIUS_M, MAX_RANGE_SPAN_DAYS,
    MAX_SEARCH_FIELDS, MAX_SEARCH_TERM_CHARS, Name, RangeOf, RangePoint, RangeTest, SearchMode,
    SearchTerm, StorageResult, Value,
};

use crate::compile::{Builder, invalid, quote_raw};
use crate::expr::Compiled;
use crate::schema_map::{SEARCH_DOC_ID, SEARCH_DOCTYPE, SEARCH_TABLE, SEARCH_TERMS};
use crate::types::{ColumnType, Param};

/// Metres per degree used for the bounding box: below every real figure, so the box only grows.
const BOX_METRES_PER_DEGREE: &str = "100000.0";
/// The text search configuration terms are parsed with.
const TS_CONFIG: &str = "'simple'::regconfig";
/// Alias of the projection row; `$` cannot occur in a catalog alias.
const SEARCH_ALIAS: &str = "sd$";

impl<'q> Builder<'q> {
    fn declared(&self, e: &Expr, what: &str) -> StorageResult<(&'q FieldType, String)> {
        let Expr::Field(r) = e else {
            return Err(invalid(format!(
                "{what} needs a field of the matching type"
            )));
        };
        let entity: &'q EntityDef = self
            .aliases
            .get(&r.alias)
            .copied()
            .ok_or_else(|| invalid(format!("unknown alias `{}`", r.alias)))?;
        let ty = &entity.field(&r.field)?.ty;
        Ok((ty, self.column(&r.alias, &r.field)?))
    }

    fn wrong_field(what: &str, e: &Expr) -> spot_storage_contract::StorageError {
        match e {
            Expr::Field(r) => invalid(format!(
                "{what} needs a field of the matching type; `{}.{}` is not one",
                r.alias, r.field
            )),
            _ => invalid(format!("{what} needs a field of the matching type")),
        }
    }

    /// `order_by` on a geo field is refused in v1: distance ordering is the client's.
    pub(crate) fn refuse_geo_order(&self, key: &Expr) -> StorageResult<()> {
        if let Expr::Field(r) = key {
            let entity = self.aliases.get(&r.alias).copied();
            if let Some(Ok(f)) = entity.map(|e| e.field(&r.field))
                && matches!(f.ty, FieldType::Geo { .. })
            {
                return Err(invalid(format!(
                    "`{}.{}` is a geo field and cannot be ordered by; sort by distance on the \
                     client",
                    r.alias, r.field
                )));
            }
        }
        Ok(())
    }

    fn numeric(&mut self, d: Decimal) -> String {
        self.param(Param::Numeric(d))
    }

    // ---- geo ---------------------------------------------------------------------------

    pub(crate) fn geo(&mut self, x: &'q Expr, test: &GeoTest) -> StorageResult<Compiled> {
        let (ty, col) = self.declared(x, "a geo test")?;
        if !matches!(ty, FieldType::Geo { .. }) {
            return Err(Self::wrong_field("a geo test", x));
        }
        let sql = match test {
            GeoTest::WithinDistance { lon, lat, metres } => {
                coordinate("longitude", *lon, 180)?;
                coordinate("latitude", *lat, 90)?;
                if *metres > MAX_GEO_RADIUS_M {
                    return Err(invalid(format!(
                        "a radius of {metres} m is above the limit of {MAX_GEO_RADIUS_M} m"
                    )));
                }
                let (lon, lat) = (self.numeric(*lon), self.numeric(*lat));
                let m = self.param(Param::Int8(i64::from(*metres)));
                let per = BOX_METRES_PER_DEGREE;
                let point = format!("ST_SetSRID(ST_MakePoint({lon}::float8, {lat}::float8), 4326)");
                let dlat = format!("{m}::float8 / {per}");
                let dlon = format!(
                    "LEAST(180.0, {m}::float8 / ({per} * cos(radians(LEAST(abs({lat}::float8) + \
                     {dlat}, 89.999)))))"
                );
                let dx = format!(
                    "CASE WHEN abs({lon}::float8) + {dlon} > 180.0 THEN 360.0 ELSE {dlon} END"
                );
                format!(
                    "({col} && ST_Expand({point}, {dx}, {dlat}) AND ST_DWithin({col}::geography, \
                     {point}::geography, {m}::float8))"
                )
            }
            GeoTest::WithinBox {
                min_lon,
                min_lat,
                max_lon,
                max_lat,
            } => {
                coordinate("min_lon", *min_lon, 180)?;
                coordinate("max_lon", *max_lon, 180)?;
                coordinate("min_lat", *min_lat, 90)?;
                coordinate("max_lat", *max_lat, 90)?;
                if min_lon > max_lon || min_lat > max_lat {
                    return Err(invalid(
                        "a box needs min_lon <= max_lon and min_lat <= max_lat; split a box \
                         across the antimeridian into two tests",
                    ));
                }
                let a = self.numeric(*min_lon);
                let b = self.numeric(*min_lat);
                let c = self.numeric(*max_lon);
                let d = self.numeric(*max_lat);
                format!(
                    "({col} && ST_MakeEnvelope({a}::float8, {b}::float8, {c}::float8, \
                     {d}::float8, 4326))"
                )
            }
        };
        Ok(Compiled::boolean(sql))
    }

    // ---- range -------------------------------------------------------------------------

    pub(crate) fn range(&mut self, x: &'q Expr, test: &RangeTest) -> StorageResult<Compiled> {
        let (ty, col) = self.declared(x, "a range test")?;
        let FieldType::Range { of } = ty else {
            return Err(Self::wrong_field("a range test", x));
        };
        let of = *of;
        let sql = match test {
            RangeTest::Contains(v) => {
                point_of(of, v)?;
                let p = self.value_param(v)?;
                format!("({col} @> {p})")
            }
            RangeTest::Overlaps { start, end } => {
                let r = self.tested_range(of, start, end)?;
                format!("({col} && {r})")
            }
            RangeTest::ContainsRange { start, end } => {
                let r = self.tested_range(of, start, end)?;
                format!("({col} @> {r})")
            }
        };
        Ok(Compiled::boolean(sql))
    }

    fn value_param(&mut self, v: &Value) -> StorageResult<String> {
        let p = match v {
            Value::Int(i) => Param::Int8(*i),
            Value::Decimal(d) => Param::Numeric(*d),
            Value::Date(d) => Param::Date(*d),
            Value::DateTime(t) => Param::TimestampTz(*t),
            other => return Err(invalid(format!("{other:?} is not a range bound"))),
        };
        Ok(self.param(p))
    }

    /// `int8range($a, $b, '[)')` and its siblings; both bounds checked like the shared
    /// validation does.
    fn tested_range(&mut self, of: RangeOf, start: &Value, end: &Value) -> StorageResult<String> {
        let (a, b) = (point_of(of, start)?, point_of(of, end)?);
        if a.compare(&b) != Some(std::cmp::Ordering::Less) {
            return Err(invalid(
                "the tested range is empty (start is not before end); ranges are half-open, \
                 [start, end)",
            ));
        }
        if a.days_until(&b)
            .is_some_and(|d| d > i64::from(MAX_RANGE_SPAN_DAYS))
        {
            return Err(invalid(format!(
                "the tested range spans more than {MAX_RANGE_SPAN_DAYS} days"
            )));
        }
        let ctor = match of {
            RangeOf::Int => "int8range",
            RangeOf::Decimal => "numrange",
            RangeOf::Date => "daterange",
            RangeOf::DateTime => "tstzrange",
        };
        let (pa, pb) = (self.value_param(start)?, self.value_param(end)?);
        Ok(format!("{ctor}({pa}, {pb}, '[)')"))
    }

    // ---- search ------------------------------------------------------------------------

    /// Checks `t` against the `from` entity's search declaration; true when typo tolerance is
    /// declared.
    fn check_term(&self, t: &SearchTerm) -> StorageResult<bool> {
        let entity = self
            .aliases
            .get(&self.from_alias)
            .copied()
            .ok_or_else(|| invalid("unknown from alias"))?;
        let Some(def) = &entity.search else {
            return Err(invalid(format!(
                "`{}` declares no search fields; search is opt-in",
                entity.name
            )));
        };
        if t.text.trim().is_empty() {
            return Err(invalid("a search term must not be blank"));
        }
        if t.text.contains('\0') {
            return Err(spot_storage_contract::StorageError::new(
                spot_storage_contract::ErrorKind::InvalidValue,
                "PostgreSQL text cannot contain NUL",
            ));
        }
        if t.text.chars().count() > MAX_SEARCH_TERM_CHARS {
            return Err(invalid(format!(
                "a search term is at most {MAX_SEARCH_TERM_CHARS} characters"
            )));
        }
        if t.fields.is_empty() || t.fields.len() > MAX_SEARCH_FIELDS {
            return Err(invalid(format!(
                "a search term names between 1 and {MAX_SEARCH_FIELDS} fields"
            )));
        }
        let mut seen = std::collections::BTreeSet::new();
        for f in &t.fields {
            if !def.fields.iter().any(|d| &d.name == f) || !seen.insert(f) {
                return Err(invalid(format!(
                    "`{}.{f}` is not a declared searchable field (or is named twice)",
                    entity.name
                )));
            }
        }
        Ok(def.typo_tolerance)
    }

    /// The parsed `tsquery` of a term, and the bound doctype.
    fn tsquery(&mut self, t: &SearchTerm) -> (String, String) {
        let func = match t.mode {
            SearchMode::Web => "websearch_to_tsquery",
            SearchMode::Phrase => "phraseto_tsquery",
            SearchMode::Plain => "plainto_tsquery",
        };
        let doctype = self.param(Param::Text(self.from_entity.as_str().to_owned()));
        let text = self.param(Param::Text(t.text.clone()));
        (format!("{func}({TS_CONFIG}, {text})"), doctype)
    }

    /// The projection row of the current `from` row.
    fn projection_where(&self, doctype: &str) -> StorageResult<String> {
        let id = Name::new(spot_storage_contract::schema::system::ID)?;
        let a = quote_raw(SEARCH_ALIAS);
        Ok(format!(
            "{a}.{} = {} AND {a}.{} = {doctype}",
            quote_raw(SEARCH_DOC_ID),
            self.column(&self.from_alias, &id)?,
            quote_raw(SEARCH_DOCTYPE),
        ))
    }

    pub(crate) fn search(&mut self, t: &SearchTerm) -> StorageResult<Compiled> {
        let typo = self.check_term(t)?;
        let (query, doctype) = self.tsquery(t);
        let a = quote_raw(SEARCH_ALIAS);
        let terms = format!("{a}.{}", quote_raw(SEARCH_TERMS));
        let mut matched = format!("{terms} @@ {query}");
        if typo {
            // Trigram similarity (`pg_trgm`), only where the entity declared typo tolerance.
            let text = self.param(Param::Text(t.text.clone()));
            matched = format!("({matched} OR strip({terms})::text % {text})");
        }
        let sql = format!(
            "EXISTS (SELECT 1 FROM {} AS {a} WHERE {} AND {matched})",
            quote_raw(SEARCH_TABLE),
            self.projection_where(&doctype)?
        );
        Ok(Compiled::boolean(sql))
    }

    /// The rank of a term as an ordering key (a correlated scalar subquery; null when the row
    /// has no projection row).
    pub(crate) fn search_rank(&mut self, t: &SearchTerm) -> StorageResult<String> {
        self.check_term(t)?;
        let (query, doctype) = self.tsquery(t);
        let a = quote_raw(SEARCH_ALIAS);
        Ok(format!(
            "(SELECT ts_rank({a}.{}, {query}) FROM {} AS {a} WHERE {})",
            quote_raw(SEARCH_TERMS),
            quote_raw(SEARCH_TABLE),
            self.projection_where(&doctype)?
        ))
    }
}

impl Compiled {
    pub(crate) fn boolean(sql: String) -> Self {
        Compiled::new(sql, ColumnType::Bool)
    }
}

fn point_of(of: RangeOf, v: &Value) -> StorageResult<RangePoint> {
    match RangePoint::from_value(v) {
        Some(p) if p.of() == of => Ok(p),
        _ => Err(invalid(format!(
            "a {of:?} range is tested with {of:?} values; got {v:?} (a date is never compared \
             with an instant)"
        ))),
    }
}

fn coordinate(what: &str, v: Decimal, limit: i64) -> StorageResult<()> {
    if v.abs() > Decimal::from(limit) {
        return Err(invalid(format!(
            "{what} {v} is outside -{limit}..={limit} degrees"
        )));
    }
    if v.normalize().scale() > GEO_SCALE {
        return Err(invalid(format!(
            "{what} {v} has more than {GEO_SCALE} decimal places; round it before storing"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The refusals the compiler owns, reached without the shared validation in front.

    use spot_storage_contract::contract_tests::{geo_catalog, search_catalog};
    use spot_storage_contract::{
        ErrorKind, Nulls, Order, OrderDir, Query, QueryPath, Selection, StorageResult,
    };

    use super::*;
    use crate::DefaultSchemaMap;
    use crate::compile::build;

    fn n(s: &str) -> Name {
        Name::new(s).unwrap_or_else(|e| panic!("{e}"))
    }

    fn query(entity: &str) -> Query {
        let mut q = Query::from(&n(entity), &n("x"), QueryPath::Report);
        q.select = vec![Selection {
            expr: Expr::field(&n("x"), &n("id")),
            alias: n("id"),
        }];
        q
    }

    fn assert_invalid<T>(r: StorageResult<T>, part: &str) {
        match r {
            Err(e) => {
                assert_eq!(e.kind, ErrorKind::InvalidQuery, "{e}");
                assert!(e.to_string().contains(part), "{e}");
            }
            Ok(_) => panic!("expected a refusal containing `{part}`"),
        }
    }

    fn term(text: &str) -> SearchTerm {
        SearchTerm {
            mode: SearchMode::Web,
            text: text.into(),
            fields: vec![n("title")],
        }
    }

    #[test]
    fn the_compiler_refuses_what_validation_would() {
        let geo = geo_catalog().unwrap_or_else(|e| panic!("{e}"));
        let mut q = query("place");
        q.filter = Some(Expr::Geo(
            Box::new(Expr::field(&n("x"), &n("pin"))),
            GeoTest::WithinDistance {
                lon: Decimal::ZERO,
                lat: Decimal::ZERO,
                metres: MAX_GEO_RADIUS_M + 1,
            },
        ));
        assert_invalid(build(&q, &geo, &DefaultSchemaMap), "above the limit");

        q.filter = Some(Expr::Geo(
            Box::new(Expr::field(&n("x"), &n("name"))),
            GeoTest::WithinDistance {
                lon: Decimal::ZERO,
                lat: Decimal::ZERO,
                metres: 1,
            },
        ));
        assert_invalid(build(&q, &geo, &DefaultSchemaMap), "matching type");

        q.filter = None;
        q.order_by = vec![Order {
            expr: Expr::field(&n("x"), &n("pin")),
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        }];
        assert_invalid(build(&q, &geo, &DefaultSchemaMap), "cannot be ordered by");

        let search = search_catalog().unwrap_or_else(|e| panic!("{e}"));
        let mut q = query("listing");
        q.select[0].expr = Expr::SearchRank(term("harbour"));
        assert_invalid(build(&q, &search, &DefaultSchemaMap), "order_by key");

        let mut q = query("listing");
        q.filter = Some(Expr::Search(term(&"x".repeat(MAX_SEARCH_TERM_CHARS + 1))));
        assert_invalid(build(&q, &search, &DefaultSchemaMap), "at most");
        q.filter = Some(Expr::Search(term("  ")));
        assert_invalid(build(&q, &search, &DefaultSchemaMap), "blank");
        let _ = QueryPath::Report;
    }
}
