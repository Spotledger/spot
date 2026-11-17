//! Search through the resource query (contract 0.22.0, WP-STO-13.T5, ADR-087 section 7): the
//! [`SearchService`] that implements `spot-http`'s [`SearchPort`] over the same permission-filtered
//! read path as every other list.
//!
//! # How a search runs
//!
//! 1. The caller's filter compiles as for any query ([`crate::query::compile`]); the search is
//!    `Expr::Search` ANDed onto it, and rank order is `Expr::SearchRank` as the first order key
//!    (a rank is an ordering key and never a value, ADR-087 7.6, so it is never selected).
//! 2. The query goes through [`SearchStore::read`], which for a host is the guarded engine: the
//!    principal's read filter is ANDed in and the no-inference check runs there, **the same code
//!    path** as `resource_query`. This module never builds a permission filter of its own.
//! 3. Fields: every field named by `search.fields`, `facets` or `snippets` is checked by the
//!    [`SearchAccess`] seam (the host implements it over `spot-field-guard`, channel `Search`)
//!    **before** storage is touched. A field the caller may not use (protected, hidden, or not a
//!    field at all) is `permission.field_denied`; only then is a field that the caller may use but
//!    the DocType did not declare searchable (or facetable) `search.field_not_searchable`. So an
//!    unknown and a protected field give one answer.
//! 4. Facets are a bounded second query (at most 4 facets, 50 values each) over the same
//!    permission-filtered set, grouped in storage; never an index-global count.
//! 5. Snippets are computed on the page's rows only, from the documents the engine loaded for the
//!    caller (so masks apply), for fields the caller may use, escaped, with `<mark>` as the only
//!    markup and at most 200 source characters.
//! 6. A DocType without a search declaration, or a backend whose projection reports no capability
//!    ([`SearchProjection`]), answers `501 search.unsupported`; a lag past the bound is
//!    `503 search.projection_behind`.
//!
//! # Cursors
//!
//! A rank is not a value, so a keyset over (rank, id) cannot be written in the query IR. The
//! cursor of a search page therefore encodes the position in the total order (rank, then the
//! caller's keys, then id) together with the scope of the query (site, caller, DocType, metadata
//! version, the request): a cursor of another query, caller or metadata version is
//! `api.invalid_request`. The order is total and deterministic, so a walk without concurrent
//! writes is stable; this is the same position paging `spot-pg-search` uses.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::Clock;
use spot_http::api::codec::{ApiDocument, FieldValue, encode_scalar};
use spot_http::api::meta::DocTypeMeta;
use spot_http::api::ports::search::{
    FacetValueOut, MAX_FACET_VALUES, SearchExtensionIn, SearchModeIn, SearchPageOut, SearchPort,
    SearchResult, SearchableOut,
};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, Page, QuerySpec, RequestCtx};
use spot_meta::EffectiveMeta;
use spot_storage_contract::{
    AggFunc, Catalog, Collation, Expr, Name, Nulls, Order, OrderDir, Query, QueryPath, RecordId,
    Rows, SearchMode, SearchTerm, Selection, Value,
};

use crate::conversion::{api_error, to_api_document};
use crate::engine_resources::ActorResolver;
use crate::meta_resources::def_meta;
use crate::query::{QueryService, compile};
use crate::util::build_invocation;

/// The most rows a page of snippets is computed for (the contract's `maxProperties`).
const MAX_SNIPPET_ROWS: usize = 500;
/// The longest snippet text the contract allows, in characters.
const MAX_SNIPPET_TEXT: usize = 1000;
/// The most `<mark>` pairs in one snippet.
const MAX_MARKS: usize = 20;
/// The longest cursor the service reads.
const MAX_CURSOR: usize = 4096;

/// What the service needs of storage and the engine, as one seam so tests can script it.
pub trait SearchStore: Send + Sync {
    /// The storage catalogue of the site's current metadata.
    fn catalog(&self) -> Result<Arc<Catalog>, ApiError>;

    /// A DocType's API metadata.
    fn doctype_meta<'a>(&'a self, doctype: &'a str)
    -> BoxFuture<'a, Result<DocTypeMeta, ApiError>>;

    /// Runs `query` in the invocation's scope **with the caller's read filter applied**.
    fn read<'a>(
        &'a self,
        inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a Query,
    ) -> BoxFuture<'a, Result<Rows, ApiError>>;

    /// Loads one document as the caller may see it (masks applied).
    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<ApiDocument, ApiError>>;
}

/// [`SearchStore`] over a document engine that is also a [`QueryService`] (the host's guarded
/// engine) and the effective metadata.
pub struct EngineSearchStore<S> {
    engine: Arc<S>,
    meta: Arc<EffectiveMeta>,
}

impl<S> EngineSearchStore<S> {
    /// A store over `engine` and `meta`.
    pub fn new(engine: Arc<S>, meta: Arc<EffectiveMeta>) -> Self {
        EngineSearchStore { engine, meta }
    }
}

impl<S: DocumentService + QueryService + 'static> SearchStore for EngineSearchStore<S> {
    fn catalog(&self) -> Result<Arc<Catalog>, ApiError> {
        let collation =
            Collation::new("und").map_err(|_| ApiError::from_code(Code::HttpInternal))?;
        self.meta
            .catalog(collation)
            .map(Arc::new)
            .map_err(|_| ApiError::from_code(Code::HttpInternal))
    }

    fn doctype_meta<'a>(
        &'a self,
        doctype: &'a str,
    ) -> BoxFuture<'a, Result<DocTypeMeta, ApiError>> {
        Box::pin(async move {
            let def = self.engine.meta(doctype).await.map_err(api_error)?;
            def_meta(&def)
        })
    }

    fn read<'a>(
        &'a self,
        inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a Query,
    ) -> BoxFuture<'a, Result<Rows, ApiError>> {
        self.engine.read(inv, catalog, query)
    }

    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<ApiDocument, ApiError>> {
        Box::pin(async move {
            let outcome = self
                .engine
                .load(inv, doctype, id)
                .await
                .map_err(api_error)?;
            Ok(to_api_document(outcome.value))
        })
    }
}

/// Which fields the caller may search, count and see snippets of: the host implements it over
/// the field guard (`spot-field-guard`, channel `Search`); this module re-implements nothing.
pub trait SearchAccess: Send + Sync {
    /// `Ok` when the caller may use every one of `fields` on the search channel; otherwise
    /// `permission.field_denied`. A name that is not a field of the DocType is refused in the
    /// same way (the guard cannot tell it from a protected one), so the answer is no oracle.
    fn check<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        fields: &'a [String],
    ) -> BoxFuture<'a, Result<(), ApiError>>;

    /// The members of `fields` the caller may use, in the order given.
    fn usable<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        fields: &'a [String],
    ) -> BoxFuture<'a, Vec<String>>;
}

/// What the backend reports about a DocType's search projection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectionStatus {
    /// Whether the backend serves search for the DocType at all.
    pub available: bool,
    /// The projection's gap behind the data, when the backend reports one.
    pub lag: Option<u64>,
    /// The gap past which a search is refused as `search.projection_behind`.
    pub bound: Option<u64>,
}

/// The backend's search capability and projection lag.
pub trait SearchProjection: Send + Sync {
    /// The status for `doctype` on the caller's site.
    fn status<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
    ) -> BoxFuture<'a, ProjectionStatus>;
}

/// The service: implements [`SearchPort`].
pub struct SearchService {
    store: Arc<dyn SearchStore>,
    access: Arc<dyn SearchAccess>,
    projection: Option<Arc<dyn SearchProjection>>,
    clock: Arc<dyn Clock>,
    actor: Arc<ActorResolver>,
}

impl SearchService {
    /// A service. Without a [`SearchProjection`] every search answers `search.unsupported`.
    pub fn new(
        store: Arc<dyn SearchStore>,
        access: Arc<dyn SearchAccess>,
        clock: Arc<dyn Clock>,
        actor: Arc<ActorResolver>,
    ) -> Self {
        SearchService {
            store,
            access,
            projection: None,
            clock,
            actor,
        }
    }

    /// Installs the backend's capability and lag reporting.
    #[must_use]
    pub fn with_projection(mut self, projection: Arc<dyn SearchProjection>) -> Self {
        self.projection = Some(projection);
        self
    }

    fn invocation(&self, ctx: &RequestCtx) -> Result<Invocation, ApiError> {
        build_invocation(ctx, self.clock.as_ref(), (self.actor)(ctx)?)
    }

    /// The search declaration of `doctype`, or `search.unsupported`; and the projection status.
    async fn declared(
        &self,
        inv: &Invocation,
        catalog: &Catalog,
        doctype: &str,
    ) -> Result<(spot_storage_contract::SearchDef, ProjectionStatus), ApiError> {
        let name = Name::new(doctype).map_err(|_| ApiError::from_code(Code::ApiInvalidRequest))?;
        let entity = catalog
            .entity(&name)
            .map_err(|_| ApiError::from_code(Code::MetaUnknownDoctype))?;
        let Some(def) = entity.search.clone() else {
            return Err(ApiError::from_code(Code::SearchUnsupported));
        };
        let Some(projection) = &self.projection else {
            return Err(ApiError::from_code(Code::SearchUnsupported));
        };
        let status = projection.status(inv, doctype).await;
        if !status.available {
            return Err(ApiError::from_code(Code::SearchUnsupported));
        }
        Ok((def, status))
    }
}

fn invalid() -> ApiError {
    ApiError::from_code(Code::ApiInvalidRequest)
}

fn name(text: &str) -> Result<Name, ApiError> {
    Name::new(text).map_err(|_| invalid())
}

fn mode_of(mode: SearchModeIn) -> SearchMode {
    match mode {
        SearchModeIn::Web => SearchMode::Web,
        SearchModeIn::Phrase => SearchMode::Phrase,
        SearchModeIn::Plain => SearchMode::Plain,
    }
}

fn count_expr() -> Expr {
    Expr::Agg {
        func: AggFunc::Count,
        arg: None,
        distinct: false,
    }
}

/// The scope a cursor is bound to.
fn scope(
    inv: &Invocation,
    meta: &DocTypeMeta,
    spec: &QuerySpec,
    search: &SearchExtensionIn,
) -> String {
    let mut original = spec.clone();
    original.cursor = None;
    format!(
        "{}:{:?}:{}:{}:{original:?}:{search:?}",
        inv.site.as_str(),
        inv.actor,
        meta.doctype,
        meta.version
    )
}

fn offset_of(cursor: &str, binding: &str) -> Result<u64, ApiError> {
    if cursor.len() > MAX_CURSOR {
        return Err(invalid());
    }
    let bytes = URL_SAFE_NO_PAD.decode(cursor).map_err(|_| invalid())?;
    let payload: Json = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if payload["scope"].as_str() != Some(binding) {
        return Err(invalid());
    }
    payload["offset"].as_u64().ok_or_else(invalid)
}

fn cursor_for(offset: u64, binding: &str) -> Result<String, ApiError> {
    let bytes =
        serde_json::to_vec(&json!({"scope": binding, "offset": offset})).map_err(|_| invalid())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The words of `text` a snippet highlights: lower-cased alphanumeric runs, without the words a
/// web search excludes with `-` and without duplicates.
pub fn highlight_terms(text: &str, mode: SearchModeIn) -> Vec<Vec<char>> {
    let mut out: Vec<Vec<char>> = Vec::new();
    let mut excluded_next = false;
    let mut word: Vec<char> = Vec::new();
    let push = |word: &mut Vec<char>, excluded: bool, out: &mut Vec<Vec<char>>| {
        if !word.is_empty() {
            if !excluded && out.len() < 16 && !out.contains(word) {
                out.push(word.clone());
            }
            word.clear();
        }
    };
    let mut previous: Option<char> = None;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if word.is_empty() {
                excluded_next = mode == SearchModeIn::Web && previous == Some('-');
            }
            word.extend(c.to_lowercase());
        } else {
            push(&mut word, excluded_next, &mut out);
        }
        previous = Some(c);
    }
    push(&mut word, excluded_next, &mut out);
    out
}

fn escape_into(out: &mut String, c: char) {
    match c {
        '&' => out.push_str("&amp;"),
        '<' => out.push_str("&lt;"),
        '>' => out.push_str("&gt;"),
        '"' => out.push_str("&quot;"),
        '\'' => out.push_str("&#39;"),
        other => out.push(other),
    }
}

/// A snippet of `text`: at most `max_chars` characters of the source around the first match,
/// every character HTML-escaped, matches wrapped in `<mark>` (the only markup), and never more
/// than 1,000 characters in all. Text with no match gives its beginning.
#[allow(clippy::needless_range_loop)]
pub fn make_snippet(text: &str, terms: &[Vec<char>], max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars
        .iter()
        .map(|c| c.to_lowercase().next().unwrap_or(*c))
        .collect();
    // Match ranges, merged.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for term in terms.iter().filter(|t| !t.is_empty()) {
        let mut i = 0;
        while i + term.len() <= lower.len() {
            if lower[i..i + term.len()] == term[..] {
                ranges.push((i, i + term.len()));
                i += term.len();
            } else {
                i += 1;
            }
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    let first = merged.first().map_or(0, |r| r.0);
    let start = first.saturating_sub(max_chars / 4);
    let end = (start + max_chars).min(chars.len());
    let mut out = String::new();
    if start > 0 {
        out.push('\u{2026}');
    }
    let mut marks = 0usize;
    let mut open = false;
    for index in start..end {
        if out.chars().count() >= MAX_SNIPPET_TEXT - 20 {
            break;
        }
        let starts = merged.iter().any(|r| r.0 == index);
        let ends = merged.iter().any(|r| r.1 == index);
        if open && ends {
            out.push_str("</mark>");
            open = false;
        }
        if !open && starts && marks < MAX_MARKS {
            out.push_str("<mark>");
            open = true;
            marks += 1;
        }
        escape_into(&mut out, chars[index]);
    }
    if open {
        out.push_str("</mark>");
    }
    if end < chars.len() {
        out.push('\u{2026}');
    }
    out
}

fn text_of(value: &FieldValue) -> Option<&str> {
    match value {
        FieldValue::Scalar(Value::Text(text)) => Some(text.as_str()),
        _ => None,
    }
}

fn facet_text(value: &Value, meta: &DocTypeMeta, field: &str) -> Result<Option<String>, ApiError> {
    if value.is_null() {
        return Ok(None);
    }
    let Some(meta_field) = meta.field(field) else {
        return Err(ApiError::from_code(Code::HttpInternal));
    };
    Ok(Some(match encode_scalar(value, meta_field)? {
        Json::String(text) => text,
        Json::Null => return Ok(None),
        other => other.to_string(),
    }))
}

impl SearchService {
    async fn facets(
        &self,
        inv: &Invocation,
        catalog: &Arc<Catalog>,
        meta: &DocTypeMeta,
        alias: &Name,
        filter: Option<&Expr>,
        facets: &[String],
    ) -> Result<BTreeMap<String, Vec<FacetValueOut>>, ApiError> {
        let mut out = BTreeMap::new();
        for facet in facets {
            let field = name(facet)?;
            let expr = Expr::field(alias, &field);
            let mut q = Query::from(&meta.doctype, alias, QueryPath::Transactional);
            q.select = vec![
                Selection {
                    expr: expr.clone(),
                    alias: name("v")?,
                },
                Selection {
                    expr: count_expr(),
                    alias: name("n")?,
                },
            ];
            q.filter = filter.cloned();
            q.group_by = vec![expr.clone()];
            q.order_by = vec![
                Order {
                    expr: count_expr(),
                    dir: OrderDir::Desc,
                    nulls: Nulls::Last,
                },
                Order {
                    expr,
                    dir: OrderDir::Asc,
                    nulls: Nulls::Last,
                },
            ];
            q.limit = Some(u32::try_from(MAX_FACET_VALUES).map_err(|_| invalid())?);
            let rows = self.store.read(inv, catalog.clone(), &q).await?;
            let mut values = Vec::with_capacity(rows.rows.len());
            for row in &rows.rows {
                let (Some(value), Some(Value::Int(count))) = (row.first(), row.get(1)) else {
                    return Err(ApiError::from_code(Code::HttpInternal));
                };
                values.push(FacetValueOut {
                    value: facet_text(value, meta, facet)?,
                    count: u64::try_from(*count).map_err(|_| invalid())?,
                });
            }
            out.insert(facet.clone(), values);
        }
        Ok(out)
    }

    async fn total(
        &self,
        inv: &Invocation,
        catalog: &Arc<Catalog>,
        meta: &DocTypeMeta,
        alias: &Name,
        filter: Option<&Expr>,
    ) -> Result<u64, ApiError> {
        let mut q = Query::from(&meta.doctype, alias, QueryPath::Transactional);
        q.select = vec![Selection {
            expr: count_expr(),
            alias: name("count")?,
        }];
        q.filter = filter.cloned();
        let rows = self.store.read(inv, catalog.clone(), &q).await?;
        match rows.rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => u64::try_from(*n).map_err(|_| invalid()),
            _ => Err(ApiError::from_code(Code::HttpInternal)),
        }
    }

    async fn run(
        &self,
        ctx: &RequestCtx,
        doctype: &str,
        spec: &QuerySpec,
        search: &SearchExtensionIn,
    ) -> Result<SearchPageOut, ApiError> {
        let inv = self.invocation(ctx)?;
        let catalog = self.store.catalog()?;
        let (def, status) = self.declared(&inv, &catalog, doctype).await?;
        if let (Some(lag), Some(bound)) = (status.lag, status.bound)
            && lag > bound
        {
            return Err(ApiError::from_code(Code::SearchProjectionBehind));
        }
        let meta = self.store.doctype_meta(doctype).await?;

        // The fields: asked-for, or every declared searchable field the caller may use.
        let declared: Vec<String> = def
            .fields
            .iter()
            .map(|f| f.name.as_str().to_owned())
            .collect();
        let searched: Vec<String> = if search.fields.is_empty() {
            self.access.usable(&inv, doctype, &declared).await
        } else {
            search.fields.clone()
        };
        let snippet_fields: Vec<String> = search
            .snippets
            .as_ref()
            .map(|s| s.fields.clone())
            .unwrap_or_default();
        // One check over every channel the data leaves by (match, rank, facet, snippet).
        let mut named: Vec<String> = searched.clone();
        named.extend(search.facets.iter().cloned());
        named.extend(snippet_fields.iter().cloned());
        let mut seen = BTreeSet::new();
        named.retain(|f| seen.insert(f.clone()));
        if searched.is_empty() {
            return Err(ApiError::from_code(Code::PermissionFieldDenied));
        }
        self.access.check(&inv, doctype, &named).await?;
        // Only now the declaration: usable by the caller, but not declared.
        let declared_facets: BTreeSet<&str> = def.facets.iter().map(Name::as_str).collect();
        if searched.iter().any(|f| !declared.contains(f))
            || search
                .facets
                .iter()
                .any(|f| !declared_facets.contains(f.as_str()))
        {
            return Err(ApiError::from_code(Code::SearchFieldNotSearchable));
        }

        let term = SearchTerm {
            mode: mode_of(search.mode),
            text: search.text.clone(),
            fields: searched.iter().map(|f| name(f)).collect::<Result<_, _>>()?,
        };
        let alias = name("r")?;
        let mut q = compile(&meta, spec)?;
        // The page needs the id only; keys are not selected (a rank cannot be) and the cursor is
        // a position.
        q.select.truncate(1);
        let matched = Expr::Search(term.clone());
        let base_filter = Some(match q.filter.take() {
            Some(existing) => Expr::And(vec![existing, matched]),
            None => matched,
        });
        q.filter = base_filter.clone();
        if search.rank {
            q.order_by.insert(
                0,
                Order {
                    expr: Expr::SearchRank(term),
                    dir: OrderDir::Desc,
                    nulls: Nulls::Last,
                },
            );
        }
        let binding = scope(&inv, &meta, spec, search);
        let offset = match &spec.cursor {
            Some(cursor) => offset_of(cursor, &binding)?,
            None => 0,
        };
        q.offset = offset;
        let rows = self.store.read(&inv, catalog.clone(), &q).await?;
        let limit = spec.limit as usize;
        let has_more = rows.rows.len() > limit;
        let kept = &rows.rows[..rows.rows.len().min(limit)];
        let next_cursor = if has_more {
            Some(cursor_for(offset + u64::from(spec.limit), &binding)?)
        } else {
            None
        };

        let allowed_snippets: BTreeSet<String> = if snippet_fields.is_empty() {
            BTreeSet::new()
        } else {
            self.access
                .usable(&inv, doctype, &snippet_fields)
                .await
                .into_iter()
                .collect()
        };
        let terms = highlight_terms(&search.text, search.mode);
        let max_chars = search.snippets.as_ref().map_or(0, |s| s.max_chars as usize);
        let mut documents = Vec::with_capacity(kept.len());
        let mut snippets: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
        for row in kept {
            let Some(Value::Id(id)) = row.first() else {
                return Err(ApiError::from_code(Code::HttpInternal));
            };
            let mut document = self.store.load(&inv, doctype, *id).await?;
            if search.snippets.is_some() && snippets.len() < MAX_SNIPPET_ROWS {
                let mut by_field = BTreeMap::new();
                for field in &snippet_fields {
                    if !allowed_snippets.contains(field) {
                        continue;
                    }
                    if let Some(text) = name(field)
                        .ok()
                        .and_then(|n| document.fields.get(&n))
                        .and_then(text_of)
                    {
                        by_field.insert(field.clone(), make_snippet(text, &terms, max_chars));
                    }
                }
                snippets.insert(id.to_string(), by_field);
            }
            if !spec.fields.is_empty() {
                document.fields.retain(|key, _| spec.fields.contains(key));
            }
            documents.push(document);
        }
        let total = if spec.with_total {
            Some(
                self.total(&inv, &catalog, &meta, &alias, base_filter.as_ref())
                    .await?,
            )
        } else {
            None
        };
        let facets = if search.facets.is_empty() {
            None
        } else {
            Some(
                self.facets(
                    &inv,
                    &catalog,
                    &meta,
                    &alias,
                    base_filter.as_ref(),
                    &search.facets,
                )
                .await?,
            )
        };
        Ok(SearchPageOut {
            page: Page {
                documents,
                next_cursor,
                total,
            },
            facets,
            snippets: search.snippets.as_ref().map(|_| snippets),
            projection_lag: status.lag,
        })
    }
}

impl SearchPort for SearchService {
    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
        search: &'a SearchExtensionIn,
    ) -> BoxFuture<'a, SearchResult<SearchPageOut>> {
        Box::pin(async move { self.run(ctx, doctype, spec, search).await })
    }

    fn describe<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
    ) -> BoxFuture<'a, SearchResult<SearchableOut>> {
        Box::pin(async move {
            let inv = self.invocation(ctx)?;
            let catalog = self.store.catalog()?;
            // A DocType with no declaration, or no projection, lists nothing; it is not an error.
            let Ok((def, _)) = self.declared(&inv, &catalog, doctype).await else {
                return Ok(SearchableOut::default());
            };
            let searchable: Vec<String> = def
                .fields
                .iter()
                .map(|f| f.name.as_str().to_owned())
                .collect();
            let facetable: Vec<String> = def.facets.iter().map(|f| f.as_str().to_owned()).collect();
            Ok(SearchableOut {
                searchable: self.access.usable(&inv, doctype, &searchable).await,
                facetable: self.access.usable(&inv, doctype, &facetable).await,
            })
        })
    }
}

/// The actor type a host's resolver returns, re-exported so a host names one path.
pub type SearchActor = Actor;
