//! The search members of `POST /api/v1/resource/{doctype}/query` (contract 0.22.0,
//! WP-STO-13.T5, ADR-087 section 7): the `search` clause, `{rank: true}` order, `facets` and
//! `snippets` on the request, and `facets`, `snippets` and `projection_lag` on the page.
//!
//! Search is an extension of the existing query operation, not an operation of its own, so this
//! file is a **helper module, not a registered group**: it declares none of `SERVES`, `routes`
//! and `required_ports`. The query route (`router/resource.rs`, not owned by this task) calls
//! it in three places; the smallest edit is written in the task report:
//!
//! 1. before `parse_query_request`: `let ext = search::take_extension(&mut json)` removes the
//!    search members from the body (and a leading `{rank: true}` order key), so the existing
//!    parser, which refuses unknown members, sees an ordinary query;
//! 2. when `ext` is `Some`: `port.query(&ctx, &doctype, &spec, &ext)` on the
//!    [`SearchPort`](crate::api::ports::search::SearchPort) replaces `resource.query`, and a site
//!    without the port answers `501 search.unsupported`;
//! 3. `page_response` merges [`page_members`] into the page object.
//!
//! `meta_get` merges [`meta_members`] (`searchable`, `facetable`).
//!
//! What this module decides is the **shape**: the bounds of the contract (text 1 to 512
//! characters, 1 to 16 fields, at most 4 facets, snippet length 20 to 200), that rank order,
//! facets and snippets need a `search`, and that rank is the first order key. It never decides
//! which fields exist, are searchable or may be seen by the caller (the port does), and never
//! puts the text, a field name or a cell in an error.

use std::collections::BTreeSet;

use serde_json::{Map, Value as Json, json};
use spot_storage_contract::Name;

use crate::api::ports::search::{
    DEFAULT_SNIPPET_CHARS, MAX_FACETS, MAX_FIELDS, MAX_SNIPPET_CHARS, MAX_TEXT_CHARS,
    MIN_SNIPPET_CHARS, SearchExtensionIn, SearchModeIn, SearchPageOut, SearchableOut, SnippetsIn,
};
use crate::api::registry::Code;
use crate::api::service::ApiError;

/// The members of a query body this module owns.
pub const SEARCH_MEMBERS: [&str; 3] = ["search", "facets", "snippets"];

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

fn names(value: &Json, member: &str, min: usize, max: usize) -> Result<Vec<String>, ApiError> {
    let items = value
        .as_array()
        .ok_or_else(|| invalid(format!("`{member}` must be an array of field names.")))?;
    if items.len() < min || items.len() > max {
        return Err(invalid(format!(
            "`{member}` must name {min} to {max} fields."
        )));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let name = item
            .as_str()
            .filter(|n| Name::new(n).is_ok())
            .ok_or_else(|| invalid(format!("`{member}` must be an array of field names.")))?;
        if !seen.insert(name) {
            return Err(invalid(format!("`{member}` names a field twice.")));
        }
        out.push(name.to_owned());
    }
    Ok(out)
}

fn clause(value: &Json) -> Result<(SearchModeIn, String, Vec<String>), ApiError> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid("`search` must be an object."))?;
    if map
        .keys()
        .any(|k| !["mode", "text", "fields"].contains(&k.as_str()))
    {
        return Err(invalid(
            "`search` has a member this operation does not accept.",
        ));
    }
    let text = map
        .get("text")
        .and_then(Json::as_str)
        .ok_or_else(|| invalid("`search.text` is required and must be text."))?;
    let chars = text.chars().count();
    if chars > MAX_TEXT_CHARS {
        return Err(ApiError::from_code(Code::SearchTermTooLong));
    }
    if text.trim().is_empty() {
        return Err(invalid("`search.text` must not be blank."));
    }
    let mode = match map.get("mode") {
        None => SearchModeIn::Web,
        Some(value) => value
            .as_str()
            .and_then(SearchModeIn::parse)
            .ok_or_else(|| invalid("`search.mode` must be `web`, `phrase` or `plain`."))?,
    };
    let fields = match map.get("fields") {
        None => Vec::new(),
        Some(value) => names(value, "search.fields", 1, MAX_FIELDS)?,
    };
    Ok((mode, text.to_owned(), fields))
}

fn snippets(value: &Json) -> Result<SnippetsIn, ApiError> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid("`snippets` must be an object."))?;
    if map
        .keys()
        .any(|k| !["fields", "max_chars"].contains(&k.as_str()))
    {
        return Err(invalid(
            "`snippets` has a member this operation does not accept.",
        ));
    }
    let fields = names(
        map.get("fields")
            .ok_or_else(|| invalid("`snippets.fields` is required."))?,
        "snippets.fields",
        1,
        MAX_FIELDS,
    )?;
    let max_chars = match map.get("max_chars") {
        None => DEFAULT_SNIPPET_CHARS,
        Some(value) => value
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| (MIN_SNIPPET_CHARS..=MAX_SNIPPET_CHARS).contains(n))
            .ok_or_else(|| {
                invalid(format!(
                    "`snippets.max_chars` must be a whole number from {MIN_SNIPPET_CHARS} to {MAX_SNIPPET_CHARS}."
                ))
            })?,
    };
    Ok(SnippetsIn { fields, max_chars })
}

/// Takes the search members out of a query body.
///
/// Returns `Ok(None)` for a body with no search member (the body is left as it was), and
/// `Ok(Some(..))` for a search, after removing `search`, `facets`, `snippets` and a leading
/// `{rank: true}` order key from `body`. `facets`, `snippets` and rank order without a `search`
/// are `400 api.invalid_request`; a `rank` key anywhere but first, or with another member, is
/// too. Text over 512 characters is `422 search.term_too_long`.
pub fn take_extension(body: &mut Json) -> Result<Option<SearchExtensionIn>, ApiError> {
    let Some(map) = body.as_object_mut() else {
        return Ok(None);
    };
    let rank_at = rank_position(map)?;
    let has_search = map.get("search").is_some_and(|v| !v.is_null());
    let others = ["facets", "snippets"]
        .iter()
        .any(|k| map.get(*k).is_some_and(|v| !v.is_null()));
    if !has_search {
        if others || rank_at.is_some() {
            return Err(invalid(
                "`facets`, `snippets` and rank order need a `search`.",
            ));
        }
        // An explicit null is no search; drop it so the ordinary parser sees an ordinary query.
        for key in SEARCH_MEMBERS {
            map.remove(key);
        }
        return Ok(None);
    }
    let (mode, text, fields) = match map.get("search") {
        Some(value) => clause(value)?,
        None => return Ok(None),
    };
    let facets = match map.get("facets") {
        None | Some(Json::Null) => Vec::new(),
        Some(value) => names(value, "facets", 0, MAX_FACETS)?,
    };
    let snippets = match map.get("snippets") {
        None | Some(Json::Null) => None,
        Some(value) => Some(self::snippets(value)?),
    };
    if rank_at.is_some()
        && let Some(Json::Array(order)) = map.get_mut("order")
    {
        order.remove(0);
        if order.is_empty() {
            map.remove("order");
        }
    }
    for key in SEARCH_MEMBERS {
        map.remove(key);
    }
    Ok(Some(SearchExtensionIn {
        mode,
        text,
        fields,
        rank: rank_at.is_some(),
        facets,
        snippets,
    }))
}

/// The position of the `{rank: true}` order key: `Some(0)` when it is first, `None` when there
/// is none. A rank key that is not first, not exactly `{rank: true}` or repeated is invalid.
fn rank_position(map: &Map<String, Json>) -> Result<Option<usize>, ApiError> {
    let Some(Json::Array(order)) = map.get("order") else {
        return Ok(None);
    };
    let mut found = None;
    for (i, key) in order.iter().enumerate() {
        let Some(key) = key.as_object() else {
            continue;
        };
        if !key.contains_key("rank") {
            continue;
        }
        if i != 0 || found.is_some() || key.len() != 1 || key.get("rank") != Some(&Json::Bool(true))
        {
            return Err(invalid(
                "`{rank: true}` must be the first order key and stand alone.",
            ));
        }
        found = Some(i);
    }
    Ok(found)
}

/// The members a search page adds to the page object: `facets`, `snippets`, `projection_lag`
/// (each only when present).
pub fn page_members(out: &SearchPageOut) -> Map<String, Json> {
    let mut map = Map::new();
    if let Some(facets) = &out.facets {
        let mut object = Map::new();
        for (field, values) in facets.iter().take(MAX_FACETS) {
            object.insert(
                field.clone(),
                Json::Array(
                    values
                        .iter()
                        .take(100)
                        .map(|v| {
                            json!({
                                "value": v.value.as_ref().map(|t| t.chars().take(500).collect::<String>()),
                                "count": v.count,
                            })
                        })
                        .collect(),
                ),
            );
        }
        map.insert("facets".to_owned(), Json::Object(object));
    }
    if let Some(snippets) = &out.snippets {
        let mut object = Map::new();
        for (id, fields) in snippets.iter().take(500) {
            let by_field: Map<String, Json> = fields
                .iter()
                .take(MAX_FIELDS)
                .map(|(f, text)| (f.clone(), Json::String(text.chars().take(1000).collect())))
                .collect();
            object.insert(id.clone(), Json::Object(by_field));
        }
        map.insert("snippets".to_owned(), Json::Object(object));
    }
    if let Some(lag) = out.projection_lag {
        map.insert("projection_lag".to_owned(), json!(lag));
    }
    map
}

/// The members `meta_get` adds: `searchable` and `facetable` (omitted when empty).
pub fn meta_members(out: &SearchableOut) -> Map<String, Json> {
    let mut map = Map::new();
    if !out.searchable.is_empty() {
        map.insert("searchable".to_owned(), json!(out.searchable));
    }
    if !out.facetable.is_empty() {
        map.insert("facetable".to_owned(), json!(out.facetable));
    }
    map
}
