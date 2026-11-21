//! `POST /api/v1/graph/query` and `POST /api/v1/lineage` (`graph_query` and `lineage_trace`,
//! native API 0.14.0, WP-GRA-03.T6): the permission-aware walk over the business graph and the
//! named lineage questions. Both are **reads that need a body**: they change nothing, take no
//! `Idempotency-Key` and are not audited as writes.
//!
//! # What the routes decide, and what they do not
//!
//! The routes read the shape of a request and refuse a bad one before the port is asked:
//!
//! 1. the **hard limits** first (`depth` 1 to 8 and `limit` 1 to 1,000 for a graph query, at most
//!    16 edge types, a filter nested at most 4 deep with at most 32 terms; `depth` 1 to 12 and
//!    `limit` 1 to 5,000 for a lineage question): a request over one is `422 graph.too_large`
//!    whatever else is wrong with it, so the specific code is the answer;
//! 2. then the closed request schema (`additionalProperties: false` everywhere), as
//!    `422 graph.query_invalid` with the field path in `details.path` and never a submitted
//!    value, or `422 lineage.kind_unknown` for a `kind` outside the five names.
//!
//! They never decide who the caller is, whether the start record is readable, or which nodes,
//! edges and properties the caller may see: the port answers for the caller its credential names
//! on the site the request resolved to, and the routes pass it no user, role, actor or tenant.
//! A body field, header or query parameter naming an actor is not read: it is an unknown field
//! and the request is refused.
//!
//! The answer is built from the port's result only. A port error that is not one of the codes
//! the contract declares for the operation is `500 http.internal` without detail, never a pass
//! and never a pass-through of the port's text. A start record that does not exist and one the
//! caller may not read are the port's one `graph.not_found`, rebuilt here from the code alone, so
//! the two answers cannot differ in status, body or headers.
//!
//! A port that answers [`not_configured`](crate::api::ports::graph::not_configured) (the
//! `Unconfigured` stub a host mounts for the service it does not run) is answered the same way.
//! A site whose router holds no port for an operation answers it `501 service.not_configured`
//! (never `200` with an empty answer). Every response is `private, no-store`. A refused request
//! is logged with the operation id and the problem code only: never the body, a token or a
//! value.

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Map, Value as Json, json};

use super::{Registration, RouteState};
use crate::api::codec::parse_record_id;
use crate::api::ports::PortKey;
use crate::api::ports::graph::{
    CompareOpIn, DirectionIn, EdgeOut, EdgePropertyOut, FilterIn, GRAPH_MAX_DEPTH,
    GRAPH_MAX_EDGE_TYPES, GRAPH_MAX_FILTER_DEPTH, GRAPH_MAX_FILTER_TERMS, GRAPH_MAX_LIMIT,
    GraphAnswerOut, GraphPort, GraphQueryIn, GraphTruncationOut, LINEAGE_MAX_DEPTH,
    LINEAGE_MAX_LIMIT, LineageAnswerOut, LineageKindIn, LineagePort, LineageQueryIn,
    LineageTruncationOut, LiteralIn, NodeRef, is_not_configured,
};
use crate::api::registry::Code;
use crate::api::service::{ApiError, ErrorCode, RequestCtx};
use crate::site::SiteContext;

use super::super::{Failure, build_ctx, fail};

/// The operation ids this group serves once registered.
pub const SERVES: &[&str] = &["graph_query", "lineage_trace"];

/// The ports this group needs: the graph port and the lineage port, whose adapters decide
/// identity, readability and what the caller may see. A group without both is not mounted at
/// all.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 2] = [
        PortKey::of::<dyn GraphPort>(),
        PortKey::of::<dyn LineagePort>(),
    ];
    &KEYS
}

/// The graph and lineage routes.
pub fn routes() -> Router<RouteState> {
    Router::new()
        .route("/api/v1/graph/query", post(graph_query))
        .route("/api/v1/lineage", post(lineage_trace))
}

/// The group as the registry's mount function takes it, for a host that mounts it itself.
pub fn registration() -> Registration {
    Registration {
        name: "graph",
        serves: SERVES,
        required_ports,
        routes,
        public: None,
    }
}

/// The largest request body read: a graph query is a few hundred bytes, and a filter at its hard
/// limits stays well below this. A larger body is refused before it is buffered.
const MAX_BODY: usize = 256 * 1024;

/// The most characters of a string literal in a filter.
const MAX_LITERAL_CHARS: usize = 1_024;
/// The most terms of an `and` or `or`.
const MAX_BOOLEAN_TERMS: usize = 8;
/// The most candidates of an `in`.
const MAX_IN_VALUES: usize = 32;
/// The most edges of a graph answer.
const MAX_ANSWER_EDGES: usize = 10_000;
/// The most properties on an edge.
const MAX_EDGE_PROPERTIES: usize = 32;
/// The most characters of a text property.
const MAX_PROPERTY_CHARS: usize = 4_096;
/// The most levels of a lineage answer (depth 0 to 12).
const MAX_LINEAGE_LEVELS: usize = 13;

/// The codes a graph port may answer with; anything else is `500 http.internal`.
const GRAPH_CODES: &[Code] = &[
    Code::GraphNotFound,
    Code::GraphQueryInvalid,
    Code::GraphTooLarge,
    Code::GraphUnavailable,
];

/// The codes a lineage port may answer with.
const LINEAGE_CODES: &[Code] = &[
    Code::GraphNotFound,
    Code::GraphQueryInvalid,
    Code::GraphTooLarge,
    Code::GraphUnavailable,
    Code::LineageKindUnknown,
];

type Handled = Result<Response, Failure>;

/// The longest `details.path` a port's own `graph.query_invalid` may carry through.
const MAX_PATH_CHARS: usize = 200;

/// Whether `text` can be a path: field names, dots and brackets only, never a value.
fn path_shape(text: &str) -> bool {
    !text.is_empty()
        && text.chars().count() <= MAX_PATH_CHARS
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '[' | ']'))
}

/// A `graph.query_invalid` that names the field path (the route's own text, never a value).
fn bad(path: &str) -> ApiError {
    let mut error = ApiError::from_code(Code::GraphQueryInvalid);
    error
        .details
        .insert("path".to_owned(), Json::String(path.to_owned()));
    error
}

/// A `graph.too_large`.
fn too_large() -> ApiError {
    ApiError::from_code(Code::GraphTooLarge)
}

fn internal() -> ApiError {
    ApiError::from_code(Code::HttpInternal)
}

/// The refusals of the caller's identity (`401`, `403`): a port authenticates the caller itself and
/// these are the contract's answers for it, whatever the operation.
fn is_auth_refusal(code: Code) -> bool {
    matches!(code.status().as_u16(), 401 | 403)
}

/// A port's error as the route answers it. A declared code is rebuilt from the code alone (the
/// port's message, explanation and field issues are dropped), keeping only a `details.path`
/// that has the shape of a path. Any other error is `http.internal`.
fn port_error(error: ApiError, declared: &[Code]) -> ApiError {
    match error.code {
        ErrorCode::Registry(code) if declared.contains(&code) || is_auth_refusal(code) => {
            let mut out = ApiError::from_code(code);
            if matches!(code, Code::GraphQueryInvalid | Code::GraphTooLarge)
                && let Some(Json::String(path)) = error.details.get("path")
                && path_shape(path)
            {
                out.details
                    .insert("path".to_owned(), Json::String(path.clone()));
            }
            out
        }
        _ => {
            tracing::error!(
                code = error.code.as_str(),
                "graph port answered with an undeclared code"
            );
            internal()
        }
    }
}

// ---------------------------------------------------------------------------
// The request: credential, body, hard limits

fn authenticate(site: &SiteContext, headers: &HeaderMap) -> Result<RequestCtx, Failure> {
    let ctx = build_ctx(site, headers, None);
    if ctx.credential.is_none() {
        return Err(fail(
            ApiError::from_code(Code::AuthRequired),
            &site.request_id,
        ));
    }
    Ok(ctx)
}

/// The JSON body, read with this group's own bound: the declared length is checked before a byte
/// is buffered and the read stops at the bound, so a larger body is never held.
async fn read_body(request: Request, site: &SiteContext) -> Result<Json, Failure> {
    let request_id = &site.request_id;
    let (parts, body) = request.into_parts();
    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_ascii_lowercase());
    if let Some(ct) = &content_type
        && ct != "application/json"
    {
        return Err(fail(
            ApiError::from_code(Code::ApiUnsupportedMediaType),
            request_id,
        ));
    }
    let declared = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > MAX_BODY) {
        return Err(fail(
            ApiError::from_code(Code::HttpBodyTooLarge),
            request_id,
        ));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| fail(ApiError::from_code(Code::HttpBodyTooLarge), request_id))?;
    if bytes.is_empty() {
        return Err(fail(
            ApiError::new(
                Code::ApiInvalidJson,
                "The request body must be a JSON document.",
            ),
            request_id,
        ));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| fail(ApiError::from_code(Code::ApiInvalidJson), request_id))
}

/// Whether a filter is nested deeper than [`GRAPH_MAX_FILTER_DEPTH`] or holds more than
/// [`GRAPH_MAX_FILTER_TERMS`] terms. Reads only the combinators, stops as soon as a limit is
/// passed (so the walk is bounded by the limits, not by the input), and takes no position on
/// anything else about the filter.
fn filter_over_limits(value: &Json, level: usize, terms: &mut usize) -> bool {
    *terms += 1;
    if level > GRAPH_MAX_FILTER_DEPTH || *terms > GRAPH_MAX_FILTER_TERMS {
        return true;
    }
    let Some(map) = value.as_object() else {
        return false;
    };
    for key in ["and", "or"] {
        if let Some(Json::Array(items)) = map.get(key) {
            for item in items {
                if filter_over_limits(item, level + 1, terms) {
                    return true;
                }
            }
        }
    }
    map.get("not")
        .is_some_and(|inner| filter_over_limits(inner, level + 1, terms))
}

fn number_over(map: &Map<String, Json>, key: &str, most: u64) -> bool {
    map.get(key)
        .and_then(Json::as_u64)
        .is_some_and(|n| n > most)
}

/// Whether the body of a graph query is over a hard limit. Checked before the schema.
fn graph_over_limits(body: &Json) -> bool {
    let Some(map) = body.as_object() else {
        return false;
    };
    if number_over(map, "depth", GRAPH_MAX_DEPTH) || number_over(map, "limit", GRAPH_MAX_LIMIT) {
        return true;
    }
    if let Some(Json::Array(types)) = map.get("edge_types")
        && types.len() > GRAPH_MAX_EDGE_TYPES
    {
        return true;
    }
    ["node_filter", "edge_filter"]
        .iter()
        .filter_map(|key| map.get(*key))
        .any(|filter| filter_over_limits(filter, 1, &mut 0))
}

/// Whether the body of a lineage question is over a hard limit. Checked before the schema.
fn lineage_over_limits(body: &Json) -> bool {
    let Some(map) = body.as_object() else {
        return false;
    };
    number_over(map, "depth", LINEAGE_MAX_DEPTH) || number_over(map, "limit", LINEAGE_MAX_LIMIT)
}

// ---------------------------------------------------------------------------
// The request: the closed schemas

type Parsed<T> = Result<T, ApiError>;

/// Whether `text` is the contract's `Identifier`: `^[a-z][a-z0-9_]{0,62}$`.
fn is_identifier(text: &str) -> bool {
    let bytes = text.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// Whether `text` is the contract's `RecordId`: a canonical lower-case UUIDv7 with an RFC 4122
/// variant.
fn is_record_id(text: &str) -> bool {
    parse_record_id(text).is_some()
        && matches!(text.as_bytes().get(19), Some(b'8' | b'9' | b'a' | b'b'))
}

fn child(path: &str, key: &str) -> String {
    format!("{path}.{key}")
}

fn item(path: &str, index: usize) -> String {
    format!("{path}[{index}]")
}

/// An object whose keys are all in `allowed` and that has every key in `required`. An unknown
/// key is reported at the object's own path: the key's name is not echoed.
fn object<'a>(
    value: &'a Json,
    path: &str,
    allowed: &[&str],
    required: &[&str],
) -> Parsed<&'a Map<String, Json>> {
    let map = value.as_object().ok_or_else(|| bad(path))?;
    if map.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(bad(path));
    }
    if let Some(missing) = required.iter().find(|k| !map.contains_key(**k)) {
        return Err(bad(&child(path, missing)));
    }
    Ok(map)
}

fn text_at<'a>(map: &'a Map<String, Json>, key: &str, path: &str) -> Parsed<&'a str> {
    map.get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| bad(&child(path, key)))
}

fn identifier_at(map: &Map<String, Json>, key: &str, path: &str) -> Parsed<String> {
    let text = text_at(map, key, path)?;
    if is_identifier(text) {
        Ok(text.to_owned())
    } else {
        Err(bad(&child(path, key)))
    }
}

fn record_id_at(map: &Map<String, Json>, key: &str, path: &str) -> Parsed<String> {
    let text = text_at(map, key, path)?;
    if is_record_id(text) {
        Ok(text.to_owned())
    } else {
        Err(bad(&child(path, key)))
    }
}

fn node_ref(value: &Json, path: &str) -> Parsed<NodeRef> {
    let map = object(value, path, &["doctype", "id"], &["doctype", "id"])?;
    Ok(NodeRef {
        doctype: identifier_at(map, "doctype", path)?,
        id: record_id_at(map, "id", path)?,
    })
}

/// A whole number from `low` to `high`, in the type the port takes.
fn whole_in(value: &Json, path: &str, low: u64, high: u64) -> Parsed<u64> {
    value
        .as_u64()
        .filter(|n| (low..=high).contains(n))
        .ok_or_else(|| bad(path))
}

fn literal(value: &Json, path: &str) -> Parsed<LiteralIn> {
    match value {
        Json::String(text) if text.chars().count() <= MAX_LITERAL_CHARS => {
            Ok(LiteralIn::Text(text.clone()))
        }
        Json::Bool(flag) => Ok(LiteralIn::Bool(*flag)),
        Json::Number(number) => number.as_i64().map(LiteralIn::Int).ok_or_else(|| bad(path)),
        _ => Err(bad(path)),
    }
}

fn operator(text: &str) -> Option<CompareOpIn> {
    Some(match text {
        "eq" => CompareOpIn::Eq,
        "ne" => CompareOpIn::Ne,
        "lt" => CompareOpIn::Lt,
        "le" => CompareOpIn::Le,
        "gt" => CompareOpIn::Gt,
        "ge" => CompareOpIn::Ge,
        _ => return None,
    })
}

fn terms_of(value: &Json, path: &str, most: usize) -> Parsed<Vec<FilterIn>> {
    let items = value.as_array().ok_or_else(|| bad(path))?;
    if items.is_empty() || items.len() > most {
        return Err(bad(path));
    }
    items
        .iter()
        .enumerate()
        .map(|(i, term)| filter(term, &item(path, i)))
        .collect()
}

/// A filter of the contract's vocabulary. The depth and the term count are within the hard
/// limits already (checked before the schema), so the recursion is at most four deep.
fn filter(value: &Json, path: &str) -> Parsed<FilterIn> {
    let map = value.as_object().ok_or_else(|| bad(path))?;
    if map.contains_key("and") {
        let map = object(value, path, &["and"], &["and"])?;
        return Ok(FilterIn::And(terms_of(
            &map["and"],
            &child(path, "and"),
            MAX_BOOLEAN_TERMS,
        )?));
    }
    if map.contains_key("or") {
        let map = object(value, path, &["or"], &["or"])?;
        return Ok(FilterIn::Or(terms_of(
            &map["or"],
            &child(path, "or"),
            MAX_BOOLEAN_TERMS,
        )?));
    }
    if map.contains_key("not") {
        let map = object(value, path, &["not"], &["not"])?;
        return Ok(FilterIn::Not(Box::new(filter(
            &map["not"],
            &child(path, "not"),
        )?)));
    }
    if map.contains_key("in") {
        let map = object(value, path, &["field", "in"], &["field", "in"])?;
        let in_path = child(path, "in");
        let items = map["in"].as_array().ok_or_else(|| bad(&in_path))?;
        if items.is_empty() || items.len() > MAX_IN_VALUES {
            return Err(bad(&in_path));
        }
        let values = items
            .iter()
            .enumerate()
            .map(|(i, v)| literal(v, &item(&in_path, i)))
            .collect::<Parsed<Vec<_>>>()?;
        return Ok(FilterIn::In {
            field: identifier_at(map, "field", path)?,
            values,
        });
    }
    if map.contains_key("is_null") {
        let map = object(value, path, &["field", "is_null"], &["field", "is_null"])?;
        let is_null = map["is_null"]
            .as_bool()
            .ok_or_else(|| bad(&child(path, "is_null")))?;
        return Ok(FilterIn::IsNull {
            field: identifier_at(map, "field", path)?,
            is_null,
        });
    }
    if map.contains_key("op") {
        let map = object(
            value,
            path,
            &["field", "op", "value"],
            &["field", "op", "value"],
        )?;
        let op = text_at(map, "op", path)
            .ok()
            .and_then(operator)
            .ok_or_else(|| bad(&child(path, "op")))?;
        return Ok(FilterIn::Compare {
            field: identifier_at(map, "field", path)?,
            op,
            value: literal(&map["value"], &child(path, "value"))?,
        });
    }
    Err(bad(path))
}

fn optional_filter(map: &Map<String, Json>, key: &str) -> Parsed<Option<FilterIn>> {
    map.get(key)
        .map(|value| filter(value, &child("body", key)))
        .transpose()
}

fn graph_query_of(body: &Json) -> Parsed<GraphQueryIn> {
    let map = object(
        body,
        "body",
        &[
            "start",
            "direction",
            "edge_types",
            "depth",
            "node_filter",
            "edge_filter",
            "return_edge_properties",
            "limit",
        ],
        &["start", "direction", "depth", "limit"],
    )?;
    let start = node_ref(&map["start"], "body.start")?;
    let direction = match text_at(map, "direction", "body").ok() {
        Some("out") => DirectionIn::Out,
        Some("in") => DirectionIn::In,
        Some("both") => DirectionIn::Both,
        _ => return Err(bad("body.direction")),
    };
    let edge_types = match map.get("edge_types") {
        None => Vec::new(),
        Some(value) => {
            let path = "body.edge_types";
            let items = value.as_array().ok_or_else(|| bad(path))?;
            if items.len() > GRAPH_MAX_EDGE_TYPES {
                return Err(too_large());
            }
            items
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    v.as_str()
                        .filter(|t| is_identifier(t))
                        .map(str::to_owned)
                        .ok_or_else(|| bad(&item(path, i)))
                })
                .collect::<Parsed<Vec<_>>>()?
        }
    };
    let depth = whole_in(&map["depth"], "body.depth", 1, GRAPH_MAX_DEPTH)?;
    let limit = whole_in(&map["limit"], "body.limit", 1, GRAPH_MAX_LIMIT)?;
    let return_edge_properties = match map.get("return_edge_properties") {
        None => false,
        Some(Json::Bool(flag)) => *flag,
        Some(_) => return Err(bad("body.return_edge_properties")),
    };
    Ok(GraphQueryIn {
        start,
        direction,
        edge_types,
        depth: u8::try_from(depth).map_err(|_| bad("body.depth"))?,
        node_filter: optional_filter(map, "node_filter")?,
        edge_filter: optional_filter(map, "edge_filter")?,
        return_edge_properties,
        limit: u32::try_from(limit).map_err(|_| bad("body.limit"))?,
    })
}

fn lineage_query_of(body: &Json) -> Parsed<LineageQueryIn> {
    let map = object(
        body,
        "body",
        &["kind", "doctype", "id", "depth", "limit"],
        &["kind", "doctype", "id"],
    )?;
    let kind = match text_at(map, "kind", "body")? {
        "chain" => LineageKindIn::Chain,
        "trace_forward" => LineageKindIn::TraceForward,
        "trace_back" => LineageKindIn::TraceBack,
        "bom" => LineageKindIn::Bom,
        "where_used" => LineageKindIn::WhereUsed,
        _ => return Err(ApiError::from_code(Code::LineageKindUnknown)),
    };
    let start = NodeRef {
        doctype: identifier_at(map, "doctype", "body")?,
        id: record_id_at(map, "id", "body")?,
    };
    let depth = match map.get("depth") {
        None => None,
        Some(value) => {
            let n = whole_in(value, "body.depth", 1, LINEAGE_MAX_DEPTH)?;
            Some(u8::try_from(n).map_err(|_| bad("body.depth"))?)
        }
    };
    let limit = match map.get("limit") {
        None => None,
        Some(value) => {
            let n = whole_in(value, "body.limit", 1, LINEAGE_MAX_LIMIT)?;
            Some(u32::try_from(n).map_err(|_| bad("body.limit"))?)
        }
    };
    Ok(LineageQueryIn {
        kind,
        start,
        depth,
        limit,
    })
}

// ---------------------------------------------------------------------------
// The answers

fn node_json(node: &NodeRef) -> Json {
    json!({ "doctype": node.doctype, "id": node.id })
}

/// Whether `text` is the contract's `Decimal`: `^-?[0-9]+(\.[0-9]+)?$`.
fn is_decimal(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (digits, None),
    };
    let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    all_digits(whole) && fraction.is_none_or(all_digits)
}

fn property_json(value: &EdgePropertyOut) -> Result<Json, ApiError> {
    Ok(match value {
        EdgePropertyOut::Text(text) => {
            if text.chars().count() > MAX_PROPERTY_CHARS {
                return Err(internal());
            }
            Json::String(text.clone())
        }
        EdgePropertyOut::Bool(flag) => Json::Bool(*flag),
        EdgePropertyOut::Int(n) => Json::from(*n),
        EdgePropertyOut::Null => Json::Null,
    })
}

fn edge_json(edge: &EdgeOut) -> Result<Json, ApiError> {
    let mut map = Map::new();
    map.insert("from".to_owned(), node_json(&edge.from));
    map.insert("to".to_owned(), node_json(&edge.to));
    map.insert("type".to_owned(), Json::String(edge.kind.clone()));
    if let Some(properties) = &edge.properties {
        if properties.len() > MAX_EDGE_PROPERTIES {
            return Err(internal());
        }
        let mut out = Map::new();
        for (name, value) in properties {
            out.insert(name.clone(), property_json(value)?);
        }
        map.insert("properties".to_owned(), Json::Object(out));
    }
    Ok(Json::Object(map))
}

/// The contract's `GraphAnswer`, built from the port's result only. An answer that breaks the
/// contract's bounds is a port defect and answers `http.internal`.
fn graph_answer_json(answer: &GraphAnswerOut, limit: u32) -> Result<Json, ApiError> {
    let most_nodes = usize::try_from(limit).unwrap_or(usize::MAX);
    if answer.nodes.len() > most_nodes || answer.edges.len() > MAX_ANSWER_EDGES {
        return Err(internal());
    }
    let edges = answer
        .edges
        .iter()
        .map(edge_json)
        .collect::<Result<Vec<_>, _>>()?;
    let mut data = Map::new();
    data.insert(
        "nodes".to_owned(),
        Json::Array(answer.nodes.iter().map(node_json).collect()),
    );
    data.insert("edges".to_owned(), Json::Array(edges));
    data.insert("truncated".to_owned(), Json::Bool(answer.truncated));
    if answer.truncated
        && let Some(reason) = answer.truncation_reason
    {
        let text = match reason {
            GraphTruncationOut::Depth => "depth",
            GraphTruncationOut::Nodes => "nodes",
            GraphTruncationOut::Paths => "paths",
        };
        data.insert("truncation_reason".to_owned(), Json::from(text));
    }
    data.insert("withheld".to_owned(), Json::Bool(answer.withheld));
    Ok(json!({ "data": Json::Object(data) }))
}

/// The contract's `LineageAnswer`, built from the port's result only.
fn lineage_answer_json(answer: &LineageAnswerOut, limit: u32) -> Result<Json, ApiError> {
    if answer.levels.len() > MAX_LINEAGE_LEVELS {
        return Err(internal());
    }
    let most_nodes = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut total = 0usize;
    let mut levels = Vec::with_capacity(answer.levels.len());
    for level in &answer.levels {
        total = total.saturating_add(level.nodes.len());
        if total > most_nodes
            || usize::try_from(level.node_count).ok() != Some(level.nodes.len())
            || level.depth > 12
        {
            return Err(internal());
        }
        let mut nodes = Vec::with_capacity(level.nodes.len());
        for entry in &level.nodes {
            let mut node = Map::new();
            node.insert("doctype".to_owned(), Json::from(entry.node.doctype.clone()));
            node.insert("id".to_owned(), Json::from(entry.node.id.clone()));
            if let Some(quantity) = &entry.quantity {
                // An exact decimal string (ADR-12): a value that is not one is never sent.
                if !is_decimal(quantity) {
                    return Err(internal());
                }
                node.insert("quantity".to_owned(), Json::from(quantity.clone()));
            }
            nodes.push(Json::Object(node));
        }
        let mut out = Map::new();
        out.insert("depth".to_owned(), Json::from(level.depth));
        out.insert("nodes".to_owned(), Json::Array(nodes));
        out.insert("node_count".to_owned(), Json::from(level.node_count));
        if let Some(sum) = &level.quantity_total {
            if !is_decimal(sum) {
                return Err(internal());
            }
            out.insert("quantity_total".to_owned(), Json::from(sum.clone()));
        }
        levels.push(Json::Object(out));
    }
    let mut data = Map::new();
    data.insert("levels".to_owned(), Json::Array(levels));
    data.insert("truncated".to_owned(), Json::Bool(answer.truncated));
    if answer.truncated
        && let Some(reason) = answer.truncation_reason
    {
        let text = match reason {
            LineageTruncationOut::Depth => "depth",
            LineageTruncationOut::Nodes => "nodes",
        };
        data.insert("truncation_reason".to_owned(), Json::from(text));
    }
    data.insert("withheld".to_owned(), Json::Bool(answer.withheld));
    Ok(json!({ "data": Json::Object(data) }))
}

// ---------------------------------------------------------------------------
// The handlers

fn not_configured(site: &SiteContext) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has no graph service configured.",
        "retryable": false,
        "request_id": site.request_id.as_str(),
    }});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Finishes a call: `private, no-store` on every answer, and a refusal is logged with the
/// operation id and the problem code only.
fn done(operation: &'static str, result: Handled) -> Response {
    let mut response = match result {
        Ok(response) => response,
        Err(failure) => {
            tracing::info!(
                operation,
                code = failure.error.code.as_str(),
                "graph request refused"
            );
            failure.into_response()
        }
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
}

async fn graph_query(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let ctx = authenticate(&site, &headers)?;
        let Some(port) = state.port::<dyn GraphPort>() else {
            return Ok(not_configured(&site));
        };
        let body = read_body(request, &site).await?;
        if graph_over_limits(&body) {
            return Err(fail(too_large(), request_id));
        }
        let query = graph_query_of(&body).map_err(|e| fail(e, request_id))?;
        let answer = match port.query(&ctx, &query).await {
            Ok(answer) => answer,
            Err(e) if is_not_configured(&e) => return Ok(not_configured(&site)),
            Err(e) => return Err(fail(port_error(e, GRAPH_CODES), request_id)),
        };
        let json = graph_answer_json(&answer, query.limit).map_err(|e| fail(e, request_id))?;
        Ok((StatusCode::OK, axum::Json(json)).into_response())
    }
    .await;
    done("graph_query", result)
}

async fn lineage_trace(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> Response {
    let result: Handled = async {
        let request_id = &site.request_id;
        let ctx = authenticate(&site, &headers)?;
        let Some(port) = state.port::<dyn LineagePort>() else {
            return Ok(not_configured(&site));
        };
        let body = read_body(request, &site).await?;
        if lineage_over_limits(&body) {
            return Err(fail(too_large(), request_id));
        }
        let query = lineage_query_of(&body).map_err(|e| fail(e, request_id))?;
        let answer = match port.trace(&ctx, &query).await {
            Ok(answer) => answer,
            Err(e) if is_not_configured(&e) => return Ok(not_configured(&site)),
            Err(e) => return Err(fail(port_error(e, LINEAGE_CODES), request_id)),
        };
        // A lineage question without a `limit` runs under the port's default; the answer is
        // still bounded by the contract's ceiling.
        let ceiling = query
            .limit
            .unwrap_or_else(|| u32::try_from(LINEAGE_MAX_LIMIT).unwrap_or(u32::MAX));
        let json = lineage_answer_json(&answer, ceiling).map_err(|e| fail(e, request_id))?;
        Ok((StatusCode::OK, axum::Json(json)).into_response())
    }
    .await;
    done("lineage_trace", result)
}
