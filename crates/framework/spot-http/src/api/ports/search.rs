//! The search seam of `POST /api/v1/resource/{doctype}/query` (contract 0.22.0, WP-STO-13.T5):
//! the `search` clause, rank ordering, `facets` and `snippets` on the resource query, and the
//! `searchable` and `facetable` lists of `meta_get`.
//!
//! Search is not a new operation: it is an extension of `resource_query`, so there is no route
//! group of its own to register. `spot-http` defines [`SearchPort`], the one call a search
//! needs, and a helper module (`api::router::routes::search`) that checks the shape of the
//! extension and renders the extra response members. The service
//! (`spot_api_services::search::SearchService`) implements the port over the document engine.
//!
//! Everything that decides is the port's: who the caller is (from `ctx.credential` on
//! `ctx.site`), the rows the caller may read, the fields the caller may search, facet or see a
//! snippet of (the field guard, channel `Search`), the projection's capability and its lag. The
//! routes pass **no user and no tenant**. A term is never logged.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, Page, QuerySpec, RequestCtx};

/// The port key of [`SearchPort`].
pub const KEY: PortKey = PortKey::of::<dyn SearchPort>();

/// What a port call answers.
pub type SearchResult<T> = Result<T, ApiError>;

/// The longest search text, in characters (the contract's `maxLength`).
pub const MAX_TEXT_CHARS: usize = 512;
/// The most fields in `search.fields` and `snippets.fields`.
pub const MAX_FIELDS: usize = 16;
/// The most facets in one request.
pub const MAX_FACETS: usize = 4;
/// The most values returned per facet.
pub const MAX_FACET_VALUES: usize = 50;
/// The default snippet length in characters.
pub const DEFAULT_SNIPPET_CHARS: u32 = 120;
/// The smallest snippet length.
pub const MIN_SNIPPET_CHARS: u32 = 20;
/// The largest snippet length.
pub const MAX_SNIPPET_CHARS: u32 = 200;

/// How the search text is read (the contract's `search.mode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchModeIn {
    /// A search box: forgiving, quoted phrases and `-` exclusions.
    Web,
    /// Every word, in order, adjacent.
    Phrase,
    /// Every word, in any order.
    Plain,
}

impl SearchModeIn {
    /// Parses the contract's text.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "web" => Some(SearchModeIn::Web),
            "phrase" => Some(SearchModeIn::Phrase),
            "plain" => Some(SearchModeIn::Plain),
            _ => None,
        }
    }
}

/// The `snippets` member of a query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnippetsIn {
    /// The fields to show a snippet of.
    pub fields: Vec<String>,
    /// The snippet length in characters, 20 to 200.
    pub max_chars: u32,
}

/// The search members of a query, checked for shape by the route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchExtensionIn {
    /// How the text is read.
    pub mode: SearchModeIn,
    /// What the caller typed, 1 to 512 characters.
    pub text: String,
    /// The searchable fields to match; empty means every searchable field the caller may use.
    pub fields: Vec<String>,
    /// `true` when the query's first order key is `{rank: true}` (the route removed it from
    /// `QuerySpec::order`; the remaining keys break ties).
    pub rank: bool,
    /// Facetable fields to count, at most four.
    pub facets: Vec<String>,
    /// Snippets, when asked for.
    pub snippets: Option<SnippetsIn>,
}

/// One facet value and its count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FacetValueOut {
    /// The value in the contract's text form; `None` counts the rows with no value.
    pub value: Option<String>,
    /// The rows that have it, among the rows the caller may read.
    pub count: u64,
}

/// A page of search results.
#[derive(Clone, Debug, PartialEq)]
pub struct SearchPageOut {
    /// The documents and the cursor.
    pub page: Page,
    /// Facet counts by field; `Some` exactly when facets were asked for.
    pub facets: Option<BTreeMap<String, Vec<FacetValueOut>>>,
    /// Snippets by document id, then field; `Some` exactly when snippets were asked for.
    pub snippets: Option<BTreeMap<String, BTreeMap<String, String>>>,
    /// The projection's position gap; `None` when the backend reports none (the database path).
    pub projection_lag: Option<u64>,
}

/// The fields a DocType declares for search, as the caller may use them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchableOut {
    /// Searchable fields the caller may read (a protected field is never listed).
    pub searchable: Vec<String>,
    /// Facetable fields the caller may read.
    pub facetable: Vec<String>,
}

/// Search for the caller the credential names.
pub trait SearchPort: Send + Sync {
    /// Runs a query with a search clause: the caller's filter and permission filter, the search
    /// match, the order, facets and snippets, as one answer.
    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
        search: &'a SearchExtensionIn,
    ) -> BoxFuture<'a, SearchResult<SearchPageOut>>;

    /// The searchable and facetable fields of a DocType the caller may use (for `meta_get`);
    /// empty lists for a DocType with no search declaration.
    fn describe<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
    ) -> BoxFuture<'a, SearchResult<SearchableOut>>;
}

/// A scripted [`SearchPort`] for route tests: it answers what a test scripts and keeps what it
/// was asked.
#[derive(Default)]
pub struct MemorySearchPort {
    script: Mutex<Script>,
}

#[derive(Default)]
struct Script {
    next: Option<SearchResult<SearchPageOut>>,
    describe: SearchableOut,
    calls: Vec<(String, SearchExtensionIn)>,
}

impl MemorySearchPort {
    /// A port that answers an empty page until scripted.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The answer of the next `query`.
    pub fn answer(&self, next: SearchResult<SearchPageOut>) {
        self.lock().next = Some(next);
    }

    /// The answer of `describe`.
    pub fn describes(&self, describe: SearchableOut) {
        self.lock().describe = describe;
    }

    /// The `(doctype, extension)` of each `query` call.
    pub fn calls(&self) -> Vec<(String, SearchExtensionIn)> {
        self.lock().calls.clone()
    }
}

impl SearchPort for MemorySearchPort {
    fn query<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
        search: &'a SearchExtensionIn,
    ) -> BoxFuture<'a, SearchResult<SearchPageOut>> {
        Box::pin(async move {
            let mut script = self.lock();
            script.calls.push((doctype.to_owned(), search.clone()));
            match script.next.take() {
                Some(answer) => answer,
                None => Ok(SearchPageOut {
                    page: Page {
                        documents: Vec::new(),
                        next_cursor: None,
                        total: spec.with_total.then_some(0),
                    },
                    facets: search.facets.first().map(|_| BTreeMap::new()),
                    snippets: search.snippets.as_ref().map(|_| BTreeMap::new()),
                    projection_lag: None,
                }),
            }
        })
    }

    fn describe<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        _doctype: &'a str,
    ) -> BoxFuture<'a, SearchResult<SearchableOut>> {
        Box::pin(async move { Ok(self.lock().describe.clone()) })
    }
}
