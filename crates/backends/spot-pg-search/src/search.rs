//! The read path: one search term, filtered by what the caller may read, with facet counts and
//! the projection's lag (ADR-087 sections 7.5 to 7.7).
//!
//! # The permission filter
//!
//! Storage cannot know permission, so the caller states what the caller may read, in two parts:
//!
//! - [`ReadScope::readable`] — the fields this caller may read **unmasked**. A search term or a
//!   facet that names a field outside this set is refused with
//!   [`permission.field_denied`](crate::FIELD_DENIED), never silently narrowed: an empty result
//!   for data that exists is a lie (ADR-070 section 12 rule 7, ADR-087 section 7.7 point 2).
//! - [`RowScope`] — the rows the caller may read, as the query IR predicate the permission layer
//!   already compiles (`spot-perm-query`). It is an explicit choice: [`RowScope::All`] is named,
//!   not the default of an omitted field, so "every row" is always something a caller wrote down.
//!
//! Everything else follows from the projection itself: `terms` holds the text of fields with no
//! protection group, so **a field the caller may not read cannot affect a match or a rank** — its
//! text is not in the vector at all. That is a structural guarantee, not a filter applied to
//! results, which is why the tests can prove it by putting a token in a protected column and
//! finding nothing.
//!
//! Facet counts obey the same filter twice over: they are computed over the *matching, permitted
//! rows* (the same predicate and the same term as the page, as a CTE), and only for facets the
//! caller may read. A count taken from the index's global totals would reveal that values exist
//! (ADR-087 section 7.7 point 4).
//!
//! # No rank, no float
//!
//! `ts_rank`/`ts_rank_cd` return `real`, and `Value` has no float (ADR-053), so a rank is used
//! **only as an `ORDER BY` key inside the compiled SQL** and never selected, never returned, and
//! never converted. [`SearchHit`] carries an id and nothing else; a facet bucket is text and a
//! count; [`ProjectionLag`] is whole milliseconds. The page's SQL is returned as
//! [`SearchPage::sql`] so the ADR-045 plan check can `EXPLAIN` exactly what ran.
//!
//! # The two statements
//!
//! The page and its facets are compiled from the same [`Query`] (same term, same row predicate)
//! by `spot-pg-query`, so there is one definition of "matching" and one of "permitted"; the facet
//! statement wraps the page's SQL as a CTE with the projection joined back in for the facet
//! values. Every identifier comes from the catalog or from this module's constants and every
//! value is a parameter — the wrapper is the only SQL text this module adds, which is where it
//! belongs (ADR-045: SQL lives in the storage crate that owns the tables).

use std::collections::{BTreeMap, BTreeSet};

use spot_pg_query::{CompiledQuery, DefaultSchemaMap, Param, compile};
use spot_storage_contract::{
    Catalog, ErrorKind, Expr, Name, Nulls, Order, OrderDir, Query, QueryPath, RecordId, SearchTerm,
    Selection, StorageError, schema::system,
};
use tokio_postgres::types::{FromSql, IsNull, ToSql, Type, accepts, to_sql_checked};
use uuid::Uuid;

use crate::error::SearchError;
use crate::lag::{self, DEFAULT_BOUND_MS, ProjectionLag};
use crate::plan::ProjectionPlan;
use crate::schema::{DOC_ID, DOCTYPE, FACETS, TABLE, quote};

/// The alias of the DocType in the compiled query.
const ENTITY_ALIAS: &str = "s";
/// The alias of the selected id column.
const DOC_ALIAS: &str = "doc";
/// The CTE the facet statement wraps the page query in.
const HITS_ALIAS: &str = "hits";
/// The projection's alias in the facet statement.
const PROJECTION_ALIAS: &str = "sd";

/// What the caller may read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadScope {
    readable: BTreeSet<Name>,
    row: RowScope,
}

impl ReadScope {
    /// A scope over the fields in `readable` and the rows `row` selects.
    #[must_use]
    pub fn new(readable: impl IntoIterator<Item = Name>, row: RowScope) -> Self {
        Self {
            readable: readable.into_iter().collect(),
            row,
        }
    }

    /// A scope over every row, for the fields in `readable`. Naming it is a deliberate act: a
    /// caller that has a permission filter passes [`RowScope::Filter`] instead.
    #[must_use]
    pub fn all_rows(readable: impl IntoIterator<Item = Name>) -> Self {
        Self::new(readable, RowScope::All)
    }

    /// The fields the caller may read unmasked.
    #[must_use]
    pub fn readable(&self) -> &BTreeSet<Name> {
        &self.readable
    }

    /// The rows the caller may read.
    #[must_use]
    pub fn row(&self) -> &RowScope {
        &self.row
    }

    /// Whether `field` may be read unmasked.
    #[must_use]
    pub fn allows(&self, field: &Name) -> bool {
        self.readable.contains(field)
    }
}

/// Which rows of the DocType the caller may read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowScope {
    /// Every row (an operator read, or a DocType with no permission rules).
    All,
    /// The predicate the permission layer compiled for this caller.
    Filter(Expr),
}

impl RowScope {
    /// The predicate, or `None` for every row.
    #[must_use]
    pub fn predicate(&self) -> Option<&Expr> {
        match self {
            RowScope::All => None,
            RowScope::Filter(expr) => Some(expr),
        }
    }
}

/// One search: the term, the facets to count, the page, and the lag bound.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchRequest {
    /// What the caller typed, and the fields it names.
    pub term: SearchTerm,
    /// The facets to count (each must be declared and readable); empty for none.
    pub facets: Vec<Name>,
    /// The page size; `None` for every match (a plan check or an export), which a caller must ask
    /// for deliberately.
    pub limit: Option<u32>,
    /// Rows skipped before the page.
    pub offset: u64,
    /// How stale the projection may be before the read refuses with `Unavailable` (ADR-087
    /// section 7.8).
    pub lag_bound_ms: u64,
}

impl SearchRequest {
    /// A first page of twenty-five matches, the default staleness bound, and no facets.
    #[must_use]
    pub fn new(term: SearchTerm) -> Self {
        Self {
            term,
            facets: Vec::new(),
            limit: Some(25),
            offset: 0,
            lag_bound_ms: DEFAULT_BOUND_MS,
        }
    }

    /// Counts these facets too.
    #[must_use]
    pub fn with_facets(mut self, facets: impl IntoIterator<Item = Name>) -> Self {
        self.facets = facets.into_iter().collect();
        self
    }

    /// Page this many rows (`None` for all of them).
    #[must_use]
    pub fn with_limit(mut self, limit: Option<u32>) -> Self {
        self.limit = limit;
        self
    }

    /// Skip this many rows.
    #[must_use]
    pub fn with_offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    /// Refuse a read whose projection is older than `bound_ms`.
    #[must_use]
    pub fn with_lag_bound(mut self, bound_ms: u64) -> Self {
        self.lag_bound_ms = bound_ms;
        self
    }
}

/// One matching document. Deliberately just the id: a rank is not a value (ADR-053, ADR-087
/// section 7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SearchHit {
    /// The document's id.
    pub id: RecordId,
}

/// One facet bucket: the value as text, and how many matching permitted rows have it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetBucket {
    /// The value, as `jsonb_each_text` renders it (a null bucket is never returned).
    pub value: String,
    /// The number of matching permitted rows with that value.
    pub count: u64,
}

/// One page of an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchPage {
    /// The matching documents, in rank order with the id as the tie-break.
    pub hits: Vec<SearchHit>,
    /// Facet counts, per facet field, ordered by value.
    pub facets: BTreeMap<Name, Vec<FacetBucket>>,
    /// How far the projection was behind when the answer was produced.
    pub lag: ProjectionLag,
    /// The SQL the page ran, for the ADR-045 plan check and for support logs.
    pub sql: String,
}

/// The facet statement: the page's match set as a CTE, joined to the projection's facets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetQuery {
    /// SQL with `$n` placeholders: the page query's, then the DocType, then the facet names.
    pub sql: String,
    /// Every parameter, in order.
    pub params: Vec<Param>,
}

/// Validates the term and the facets against the projection and the caller's scope.
///
/// The order is deliberate: existence, then permission, then capability. Naming a protected
/// field is a refusal (`permission.field_denied`), not a bad query, and a field that exists and
/// is readable but is not declared searchable is `InvalidQuery`.
fn check_scope(
    catalog: &Catalog,
    plan: &ProjectionPlan,
    scope: &ReadScope,
    request: &SearchRequest,
) -> Result<(), SearchError> {
    let entity = catalog.entity(plan.doctype())?;
    let declared = |field: &Name| entity.fields.iter().any(|f| &f.name == field);
    let unknown = |field: &Name| {
        SearchError::Storage(
            StorageError::new(
                ErrorKind::UnknownField,
                format!("`{}.{field}` is not a field", plan.doctype()),
            )
            .with_entity(plan.doctype())
            .with_field(field),
        )
    };
    let not_searchable = |field: &Name| {
        SearchError::Storage(
            StorageError::new(
                ErrorKind::InvalidQuery,
                format!(
                    "`{}.{field}` is not a declared searchable field",
                    plan.doctype()
                ),
            )
            .with_entity(plan.doctype())
            .with_field(field),
        )
    };
    for field in &request.term.fields {
        if !declared(field) {
            return Err(unknown(field));
        }
        if !scope.allows(field) {
            return Err(SearchError::field_denied(field));
        }
        if !plan.is_searchable(field) {
            return Err(not_searchable(field));
        }
    }
    for facet in &request.facets {
        if !declared(facet) {
            return Err(unknown(facet));
        }
        if !scope.allows(facet) {
            return Err(SearchError::field_denied(facet));
        }
        if !plan.is_facet(facet) {
            return Err(SearchError::Storage(
                StorageError::new(
                    ErrorKind::InvalidQuery,
                    format!("`{}.{facet}` is not a declared facet", plan.doctype()),
                )
                .with_entity(plan.doctype())
                .with_field(facet),
            ));
        }
    }
    Ok(())
}

/// The query the page (and the facet CTE) compile from: the permitted rows that match the term.
fn select_query(plan: &ProjectionPlan, scope: &ReadScope, request: &SearchRequest) -> Query {
    let alias = crate::schema::column(ENTITY_ALIAS);
    let mut query = Query::from(plan.doctype(), &alias, QueryPath::Transactional);
    query.select = vec![Selection {
        expr: Expr::field(&alias, &crate::schema::column(system::ID)),
        alias: crate::schema::column(DOC_ALIAS),
    }];
    let search = Expr::Search(request.term.clone());
    query.filter = Some(match scope.row() {
        RowScope::All => search,
        RowScope::Filter(predicate) => Expr::And(vec![predicate.clone(), search]),
    });
    query.order_by = vec![Order {
        expr: Expr::SearchRank(request.term.clone()),
        dir: OrderDir::Desc,
        nulls: Nulls::Last,
    }];
    query.limit = request.limit;
    query.offset = request.offset;
    query
}

/// Compiles the page query: the term and the permission predicate, with the rank as the ordering
/// key only. Public so the ADR-045 plan check can `EXPLAIN` exactly what a search runs.
///
/// # Errors
///
/// `permission.field_denied` for a field outside the scope; `InvalidQuery`/`UnknownField` for a
/// field that is not searchable; a storage failure from the compiler.
pub fn compile_page(
    catalog: &Catalog,
    plan: &ProjectionPlan,
    scope: &ReadScope,
    request: &SearchRequest,
) -> Result<CompiledQuery, SearchError> {
    check_scope(catalog, plan, scope, request)?;
    let query = select_query(plan, scope, request);
    compile(&query, catalog, &DefaultSchemaMap).map_err(SearchError::Storage)
}

/// Compiles the facet statement for a request with facets. `None` when none are asked for.
///
/// # Errors
///
/// As [`compile_page`].
pub fn compile_facets(
    catalog: &Catalog,
    plan: &ProjectionPlan,
    scope: &ReadScope,
    request: &SearchRequest,
) -> Result<Option<FacetQuery>, SearchError> {
    if request.facets.is_empty() {
        return Ok(None);
    }
    check_scope(catalog, plan, scope, request)?;
    // The same predicate and term, with no paging: the counts are over the whole match set.
    let mut query = select_query(plan, scope, request);
    query.limit = None;
    query.offset = 0;
    // The page's own rank order is worthless for a count and would sort the whole set for nothing.
    query.order_by.clear();
    query.path = QueryPath::Report;
    let inner = compile(&query, catalog, &DefaultSchemaMap).map_err(SearchError::Storage)?;

    let doctype_param = inner.params.len() + 1;
    let facets_param = inner.params.len() + 2;
    // One `text` parameter holding the facet names, split by PostgreSQL: a `text[]` parameter
    // would need an array binding the compiler has no `Param` for, and a name can contain no
    // comma, so this is the same set with one parameter (never a value in the SQL text).
    let names: Vec<&str> = request.facets.iter().map(Name::as_str).collect();
    let mut params = inner.params.clone();
    params.push(Param::Text(plan.doctype().as_str().to_owned()));
    params.push(Param::Text(names.join(",")));

    let sql = format!(
        "WITH {hits} AS ({inner_sql}) SELECT f.k, f.v, count(*) AS n FROM {hits} h \
         JOIN {table} {projection} ON {projection}.{doc_id} = h.{doc} \
         CROSS JOIN LATERAL jsonb_each_text({projection}.{facets}) AS f(k, v) \
         WHERE {projection}.{doctype} = ${doctype_param}::text AND f.v IS NOT NULL \
           AND f.k = ANY(string_to_array(${facets_param}::text, ',')) \
         GROUP BY f.k, f.v ORDER BY f.k, f.v",
        hits = quote(HITS_ALIAS),
        inner_sql = inner.sql,
        table = quote(TABLE),
        projection = quote(PROJECTION_ALIAS),
        doc_id = quote(DOC_ID),
        doc = quote(DOC_ALIAS),
        facets = quote(FACETS),
        doctype = quote(DOCTYPE),
    );
    Ok(Some(FacetQuery { sql, params }))
}

/// Runs a search: the page, its facet counts, and the projection's lag.
///
/// The lag is measured first, and the read refuses with `Unavailable` when the projection is
/// behind past the request's bound **and** rows are missing: an answer built on a stale
/// projection must not be presented as current (ADR-087 section 7.8).
///
/// # Errors
///
/// `permission.field_denied`; `storage.unavailable` for a projection past its bound; a storage
/// failure from the transaction.
pub async fn search(
    tx: &mut spot_postgres::PgTransaction,
    catalog: &Catalog,
    plan: &ProjectionPlan,
    scope: &ReadScope,
    request: &SearchRequest,
) -> Result<SearchPage, SearchError> {
    let page = compile_page(catalog, plan, scope, request)?;
    let facets = compile_facets(catalog, plan, scope, request)?;

    let lag = lag::measure(tx, plan).await?;
    lag.record(request.lag_bound_ms);
    if !lag.within_bound(request.lag_bound_ms) {
        return Err(SearchError::Storage(
            StorageError::new(
                ErrorKind::Unavailable,
                format!(
                    "the search projection of `{}` is behind: {} document(s) unprojected, newest \
                     row {} ms old, bound {} ms",
                    plan.doctype(),
                    lag.missing,
                    lag.age_ms.unwrap_or(0),
                    request.lag_bound_ms
                ),
            )
            .with_entity(plan.doctype()),
        ));
    }

    let hits = run_page(tx, &page).await?;
    let facets = match &facets {
        Some(facet) => run_facets(tx, facet, &request.facets).await?,
        None => BTreeMap::new(),
    };
    Ok(SearchPage {
        hits,
        facets,
        lag,
        sql: page.sql.clone(),
    })
}

async fn run_page(
    tx: &mut spot_postgres::PgTransaction,
    page: &CompiledQuery,
) -> Result<Vec<SearchHit>, SearchError> {
    let bound = bind(&page.params);
    let refs: Vec<&(dyn ToSql + Sync)> =
        bound.iter().map(|b| &**b as &(dyn ToSql + Sync)).collect();
    let rows = tx.raw_query(&page.sql, &refs, None).await?;
    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        let id: Uuid = row
            .try_get(0)
            .map_err(|_| SearchError::internal("the id column of a search page is not a uuid"))?;
        let id = RecordId::from_bytes(*id.as_bytes())?;
        hits.push(SearchHit { id });
    }
    Ok(hits)
}

async fn run_facets(
    tx: &mut spot_postgres::PgTransaction,
    facet: &FacetQuery,
    requested: &[Name],
) -> Result<BTreeMap<Name, Vec<FacetBucket>>, SearchError> {
    let bound = bind(&facet.params);
    let refs: Vec<&(dyn ToSql + Sync)> =
        bound.iter().map(|b| &**b as &(dyn ToSql + Sync)).collect();
    let rows = tx.raw_query(&facet.sql, &refs, None).await?;
    let wanted: BTreeSet<&str> = requested.iter().map(Name::as_str).collect();
    let mut out: BTreeMap<Name, Vec<FacetBucket>> = BTreeMap::new();
    for row in rows {
        let key: String = row
            .try_get(0)
            .map_err(|_| SearchError::internal("a facet key is not text"))?;
        let value: String = row
            .try_get(1)
            .map_err(|_| SearchError::internal("a facet value is not text"))?;
        let count = u64::try_from(row.try_get::<_, i64>(2).unwrap_or(0)).unwrap_or(0);
        if !wanted.contains(key.as_str()) {
            continue;
        }
        if let Ok(name) = Name::new(&key) {
            out.entry(name)
                .or_default()
                .push(FacetBucket { value, count });
        }
    }
    Ok(out)
}

/// JSON text in a `jsonb` column: binary `jsonb` is a version byte (1) and the JSON text.
#[derive(Debug)]
struct JsonText(String);

impl ToSql for JsonText {
    fn to_sql(
        &self,
        _ty: &Type,
        out: &mut bytes::BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        use bytes::BufMut;
        out.put_u8(1);
        out.put_slice(self.0.as_bytes());
        Ok(IsNull::No)
    }

    accepts!(JSONB);
    to_sql_checked!();
}

impl<'a> FromSql<'a> for JsonText {
    fn from_sql(
        _ty: &Type,
        raw: &'a [u8],
    ) -> Result<Self, Box<dyn std::error::Error + Sync + Send>> {
        match raw.split_first() {
            Some((1, text)) => Ok(JsonText(std::str::from_utf8(text)?.to_owned())),
            _ => Err("unsupported jsonb version".into()),
        }
    }

    accepts!(JSONB);
}

type Bound = Box<dyn ToSql + Sync + Send>;

fn null_of(ty: spot_pg_query::PgType) -> Bound {
    use spot_pg_query::PgType;
    match ty {
        PgType::Bool => Box::new(Option::<bool>::None),
        PgType::Int8 => Box::new(Option::<i64>::None),
        PgType::Numeric => Box::new(Option::<rust_decimal::Decimal>::None),
        PgType::Text => Box::new(Option::<String>::None),
        PgType::Date => Box::new(Option::<time::Date>::None),
        PgType::Time => Box::new(Option::<time::Time>::None),
        PgType::TimestampTz => Box::new(Option::<time::OffsetDateTime>::None),
        PgType::Uuid => Box::new(Option::<Uuid>::None),
        PgType::Jsonb => Box::new(Option::<JsonText>::None),
        PgType::Bytea => Box::new(Option::<Vec<u8>>::None),
    }
}

/// The compiler's parameters as driver values. The statement is sent untyped, so the server
/// infers each type from the SQL the compiler wrote; the values here are the Rust types that
/// match those inferences (an id is `uuid`, a term is `text`, paging is `int8`).
fn bind(params: &[Param]) -> Vec<Bound> {
    params
        .iter()
        .map(|p| match p {
            Param::Bool(v) => Box::new(*v) as Bound,
            Param::Int8(v) => Box::new(*v) as Bound,
            Param::Numeric(v) => Box::new(*v) as Bound,
            Param::Text(v) => Box::new(v.clone()) as Bound,
            Param::Date(v) => Box::new(*v) as Bound,
            Param::Time(v) => Box::new(*v) as Bound,
            Param::TimestampTz(v) => Box::new(*v) as Bound,
            Param::Uuid(v) => Box::new(Uuid::from_bytes(*v.as_bytes())) as Bound,
            Param::Jsonb(v) => Box::new(JsonText(v.clone())) as Bound,
            Param::Bytea(v) => Box::new(v.clone()) as Bound,
            Param::Null(t) => null_of(*t),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    fn term(text: &str, fields: &[&str]) -> SearchTerm {
        SearchTerm {
            mode: spot_storage_contract::SearchMode::Web,
            text: text.to_owned(),
            fields: fields.iter().map(|f| fixture::name(f)).collect(),
        }
    }

    fn scope(fields: &[&str]) -> ReadScope {
        ReadScope::all_rows(fields.iter().map(|f| fixture::name(f)))
    }

    #[test]
    fn the_page_query_ranks_but_never_selects_a_rank() {
        let request = SearchRequest::new(term("harbour", &["title"]));
        let page = compile_page(
            &fixture::catalog(),
            &fixture::plan(),
            &scope(&["title"]),
            &request,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let select = page
            .sql
            .split(" FROM ")
            .next()
            .unwrap_or_default()
            .to_owned();
        assert!(
            !select.contains("ts_rank"),
            "the select list is the id only: {select}"
        );
        assert!(page.sql.contains("AS \"doc\""));
        let order = page
            .sql
            .split(" ORDER BY ")
            .nth(1)
            .unwrap_or_default()
            .to_owned();
        assert!(
            order.contains("ts_rank"),
            "the rank orders the page: {order}"
        );
        assert!(order.contains("DESC"));
        assert!(
            order.contains("\"s\".\"id\" ASC"),
            "the id tie-break follows the rank: {order}"
        );
        assert!(!page.sql.contains("float"), "no float anywhere in a search");
    }

    #[test]
    fn the_permission_predicate_is_part_of_the_compiled_filter() {
        let predicate = Expr::Text(
            Box::new(Expr::field(&fixture::name("s"), &fixture::name("title"))),
            spot_storage_contract::TextMatch {
                mode: spot_storage_contract::TextMode::Prefix,
                pattern: "pub".to_owned(),
                case_insensitive: false,
            },
        );
        let scope = ReadScope::new(vec![fixture::name("title")], RowScope::Filter(predicate));
        let request = SearchRequest::new(term("harbour", &["title"]));
        let page = compile_page(&fixture::catalog(), &fixture::plan(), &scope, &request)
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(page.sql.contains("starts_with"), "{}", page.sql);
        assert!(
            page.sql.contains("@@"),
            "the term is in the same filter: {}",
            page.sql
        );
        // The permission pattern, the parsed term and the rank's own copy of it, then paging.
        assert!(
            page.params
                .iter()
                .any(|p| matches!(p, Param::Text(t) if t == "pub"))
        );
        assert!(
            page.params
                .iter()
                .any(|p| matches!(p, Param::Text(t) if t == "harbour"))
        );
        assert_eq!(page.params.len(), 7, "{:?}", page.params);
    }

    #[test]
    fn a_search_naming_an_unreadable_field_is_refused_with_field_denied() {
        let request = SearchRequest::new(term("harbour", &["owner_note"]));
        let error = crate::fixture::refusal(compile_page(
            &fixture::catalog(),
            &fixture::plan(),
            &scope(&["title"]),
            &request,
        ));
        assert_eq!(error.code(), crate::FIELD_DENIED);
        assert_eq!(error.field().map(Name::as_str), Some("owner_note"));

        let request = SearchRequest::new(term("harbour", &["body"]));
        let error = crate::fixture::refusal(compile_page(
            &fixture::catalog(),
            &fixture::plan(),
            &scope(&["title"]),
            &request,
        ));
        assert_eq!(error.code(), crate::FIELD_DENIED);
    }

    #[test]
    fn an_unknown_field_is_unknown_before_it_is_a_refusal() {
        let request = SearchRequest::new(term("harbour", &["nope"]));
        let error = crate::fixture::refusal(compile_page(
            &fixture::catalog(),
            &fixture::plan(),
            &scope(&["title"]),
            &request,
        ));
        assert_eq!(error.code(), "storage.unknown_field");
    }

    #[test]
    fn a_readable_field_that_is_not_searchable_is_invalid_query() {
        let request = SearchRequest::new(term("harbour", &["price_band"]));
        let error = crate::fixture::refusal(compile_page(
            &fixture::catalog(),
            &fixture::plan(),
            &scope(&["price_band"]),
            &request,
        ));
        assert_eq!(error.code(), "storage.invalid_query");
    }

    #[test]
    fn a_facet_the_caller_may_not_read_is_refused() {
        let request = SearchRequest::new(term("harbour", &["title"]))
            .with_facets([fixture::name("price_band")]);
        let error = crate::fixture::refusal(compile_facets(
            &fixture::catalog(),
            &fixture::plan(),
            &scope(&["title"]),
            &request,
        ));
        assert_eq!(error.code(), crate::FIELD_DENIED);
        assert_eq!(error.field().map(Name::as_str), Some("price_band"));
    }

    #[test]
    fn the_facet_statement_wraps_the_page_query_and_appends_two_parameters() {
        let request = SearchRequest::new(term("harbour", &["title"]))
            .with_facets([fixture::name("price_band")]);
        let facets = compile_facets(
            &fixture::catalog(),
            &fixture::plan(),
            &scope(&["title", "price_band"]),
            &request,
        )
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("facets were asked for"));
        assert!(facets.sql.starts_with("WITH \"hits\" AS (SELECT"));
        assert!(facets.sql.contains("jsonb_each_text(\"sd\".\"facets\")"));
        assert!(facets.sql.contains("GROUP BY f.k, f.v"));
        assert!(
            !facets.sql.contains("LIMIT"),
            "counts cover the whole match set"
        );
        assert!(!facets.sql.contains("ts_rank"), "a count needs no order");
        // Three for the term (the parsed term and the rank's copy are dropped with the order),
        // then the DocType and the facet names.
        assert_eq!(facets.params.len(), 5, "{:?}", facets.params);
        assert!(facets.sql.contains("$4::text"), "{}", facets.sql);
        assert!(facets.sql.contains("$5::text"), "{}", facets.sql);
        assert!(matches!(facets.params.get(4), Some(Param::Text(names)) if names == "price_band"));
        assert_eq!(
            facets
                .params
                .iter()
                .filter(|p| matches!(p, Param::Text(_)))
                .count(),
            4
        );
        let page = compile_page(
            &fixture::catalog(),
            &fixture::plan(),
            &scope(&["title", "price_band"]),
            &request,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            page.params.len() > facets.params.len(),
            "the page also pages"
        );
    }

    #[test]
    fn no_facets_means_no_second_statement() {
        let request = SearchRequest::new(term("harbour", &["title"]));
        let facets = compile_facets(
            &fixture::catalog(),
            &fixture::plan(),
            &scope(&["title"]),
            &request,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(facets.is_none());
    }

    #[test]
    fn paging_reaches_the_compiled_page() {
        let request = SearchRequest::new(term("harbour", &["title"]))
            .with_limit(Some(3))
            .with_offset(9);
        let page = compile_page(
            &fixture::catalog(),
            &fixture::plan(),
            &scope(&["title"]),
            &request,
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(page.sql.contains("LIMIT $"));
        assert!(page.sql.contains("OFFSET $"));
        let values: Vec<&Param> = page
            .params
            .iter()
            .filter(|p| matches!(p, Param::Int8(_)))
            .collect();
        assert_eq!(values.len(), 2, "one limit and one offset");
        assert!(matches!(values[0], Param::Int8(3)));
        assert!(matches!(values[1], Param::Int8(9)));
    }

    #[test]
    fn a_search_answer_holds_ids_text_and_counts_and_nothing_else() {
        // A `Value` can carry no float (ADR-053); a rank is never a value (ADR-087 section 7.3),
        // so the page's type must not mention `Value` at all.
        let hit = SearchHit {
            id: RecordId::new_v7(),
        };
        let bucket = FacetBucket {
            value: "cheap".to_owned(),
            count: 3,
        };
        let _: [u8; 16] = *hit.id.as_bytes();
        let _: u64 = bucket.count;
        assert!(!std::any::type_name::<SearchPage>().contains("Value"));
        assert!(!std::any::type_name::<FacetBucket>().contains("Value"));
        let _: Option<spot_storage_contract::Value> = None;
    }

    #[test]
    fn a_scope_that_allows_nothing_allows_nothing() {
        let scope = ReadScope::all_rows(std::iter::empty());
        assert!(!scope.allows(&fixture::name("title")));
        assert!(matches!(scope.row(), RowScope::All));
        assert_eq!(RowScope::All.predicate(), None);
    }
}
