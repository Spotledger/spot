//! Engine-side helpers of the bulk data routes (`/api/v1/export*`, WP-INT-03.T2).
//!
//! The bulk adapter itself lives in `spot-cli` (`services/export.rs`), because it needs the bulk
//! crate, the permission engine and the field guard, none of which this crate depends on. What
//! lives here is the part that is the engine's own business and needs only the metadata and the
//! codec: turning a validated contract `Filter` into the storage expression an export runs, and
//! writing one exported row in the contract's value encoding. Nothing here decides who may see
//! what; the adapter does that before and after.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Deserialize;
use serde_json::{Map, Value as Json, json};
use spot_app_api::meta::{DocTypeDef, FieldKind};
use spot_core::Clock;
use spot_http::api::codec::{decode_scalar, encode_datetime, encode_scalar, parse_record_id};
use spot_http::api::filter::{FilterAst, Op};
use spot_http::api::meta::DocTypeMeta;
use spot_http::api::ports::export::{ExportFormatIn, ExportStartIn, OrderKeyIn};
use spot_http::api::registry::Code;
use spot_http::api::service::ApiError;
use spot_storage_contract::{
    ChangeFeed, ChangeOp, ErrorKind, Expr, FeedPosition, Name, RecordId, StorageError, TextMatch,
    Value,
};

fn invalid(message: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

fn unknown_field() -> ApiError {
    ApiError::from_code(Code::DocumentUnknownField)
}

/// A filter value decoded by the field's type; `id` is a record id.
fn scalar(value: &Json, field: &Name, meta: &DocTypeMeta) -> Result<Value, ApiError> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    if field.as_str() == "id" {
        return value
            .as_str()
            .and_then(parse_record_id)
            .map(Value::Id)
            .ok_or_else(|| invalid("`id` filter values must be canonical record ids."));
    }
    let meta_field = meta.field(field.as_str()).ok_or_else(unknown_field)?;
    decode_scalar(value, meta_field, field.as_str())
}

/// The storage expression of a validated filter, naming fields through `alias`.
///
/// # Errors
///
/// `document.unknown_field` for a field the DocType lacks, `api.invalid_request` for a value
/// the field's type does not take.
pub fn filter_expr(ast: &FilterAst, meta: &DocTypeMeta, alias: &Name) -> Result<Expr, ApiError> {
    Ok(match ast {
        FilterAst::And(nodes) => Expr::And(
            nodes
                .iter()
                .map(|n| filter_expr(n, meta, alias))
                .collect::<Result<_, _>>()?,
        ),
        FilterAst::Or(nodes) => Expr::Or(
            nodes
                .iter()
                .map(|n| filter_expr(n, meta, alias))
                .collect::<Result<_, _>>()?,
        ),
        FilterAst::Not(node) => Expr::Not(Box::new(filter_expr(node, meta, alias)?)),
        FilterAst::Cmp(c) => {
            let field = Expr::field(alias, &c.field);
            match c.op {
                Op::IsNull => Expr::IsNull(Box::new(field)),
                Op::IsNotNull => Expr::Not(Box::new(Expr::IsNull(Box::new(field)))),
                Op::In => Expr::In(
                    Box::new(field),
                    c.values
                        .iter()
                        .map(|v| scalar(&v.to_json(), &c.field, meta))
                        .collect::<Result<_, _>>()?,
                ),
                op => {
                    let first = c
                        .values
                        .first()
                        .ok_or_else(|| invalid("A filter comparison needs a value."))?;
                    if let Some(mode) = op.text_mode() {
                        let Json::String(pattern) = first.to_json() else {
                            return Err(invalid("A text match needs a string."));
                        };
                        Expr::Text(
                            Box::new(field),
                            TextMatch {
                                mode,
                                pattern,
                                case_insensitive: c.case_insensitive,
                            },
                        )
                    } else {
                        let cmp = op
                            .cmp()
                            .ok_or_else(|| invalid("The operator does not fit."))?;
                        field.cmp(cmp, Expr::value(scalar(&first.to_json(), &c.field, meta)?))
                    }
                }
            }
        }
    })
}

/// Every field a filter names, in order of appearance (with repeats).
pub fn filter_fields(ast: &FilterAst) -> Vec<String> {
    match ast {
        FilterAst::And(nodes) | FilterAst::Or(nodes) => {
            nodes.iter().flat_map(filter_fields).collect()
        }
        FilterAst::Not(node) => filter_fields(node),
        FilterAst::Cmp(c) => vec![c.field.as_str().to_owned()],
    }
}

/// One value in the contract's encoding. A column the metadata knows is encoded by its field
/// type; one it does not (`id`, an engine-managed column) by the value alone.
pub fn encode_value(value: &Value, column: &str, meta: &DocTypeMeta) -> Json {
    if let Some(field) = meta.field(column)
        && let Ok(encoded) = encode_scalar(value, field)
    {
        return encoded;
    }
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Int(i) => Json::from(*i),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Text(t) => Json::String(t.clone()),
        Value::Id(id) => Json::String(id.to_string()),
        Value::DateTime(dt) => Json::String(encode_datetime(dt)),
        Value::Json(text) => serde_json::from_str(text).unwrap_or(Json::Null),
        // A value with no known encoding is withheld, never guessed.
        _ => Json::Null,
    }
}

/// One exported row as the contract's `ExtractRow`: `id`, `version` and the columns. A column
/// called `id` or `version` is the row's own, never a second copy.
pub fn extract_row(
    id: &str,
    version: u64,
    columns: &[String],
    values: &[Value],
    meta: &DocTypeMeta,
) -> Json {
    let mut row = Map::new();
    row.insert("id".to_owned(), Json::String(id.to_owned()));
    row.insert("version".to_owned(), Json::from(version));
    for (column, value) in columns.iter().zip(values) {
        if column != "id" && column != "version" {
            row.insert(column.clone(), encode_value(value, column, meta));
        }
    }
    Json::Object(row)
}

/// The HTTP error of a bulk failure, by its stable code. `retry_after` is the wait, in seconds,
/// a `bulk.limit` carries. A code this function does not know is an internal error: nothing of
/// the message crosses.
///
/// `permission.field_denied` keeps the message, which names the refused paths and never a
/// value; so does `bulk.spec_invalid`, which names paths and counts only.
pub fn bulk_error(code: &str, message: &str, retry_after: u64) -> ApiError {
    match code {
        "bulk.cursor_invalid" => ApiError::new(
            Code::BulkCursorInvalid,
            "The cursor or watermark is not valid for this caller and request.",
        ),
        "bulk.watermark_expired" => ApiError::from_code(Code::BulkWatermarkExpired),
        "bulk.limit" => ApiError::from_code(Code::BulkLimit).detail(
            spot_http::api::storage_error::RETRY_AFTER_DETAIL,
            Json::from(retry_after.max(1)),
        ),
        "bulk.export_denied" => ApiError::from_code(Code::PermissionDenied),
        "permission.field_denied" => ApiError::new(Code::PermissionFieldDenied, message),
        "bulk.spec_invalid" => ApiError::new(Code::ApiInvalidRequest, message),
        "fieldguard.unknown_doctype" => ApiError::from_code(Code::MetaUnknownDoctype),
        _ => ApiError::from_code(Code::HttpInternal),
    }
}

// ---------------------------------------------------------------------------------------------
// The operator's limits: `services.export`
// ---------------------------------------------------------------------------------------------

/// `[services.export]`. Unknown keys are refused; a value is never echoed in an error.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site serves bulk export at all.
    pub enabled: bool,
    /// The most rows in one chunk.
    pub rows_per_chunk: u32,
    /// The most bytes in one chunk.
    pub bytes_per_chunk: usize,
    /// The most exports in flight on this site.
    pub max_concurrent_per_site: u32,
    /// The most exports in flight for one caller.
    pub max_concurrent_per_user: u32,
    /// The site's row budget per second.
    pub rows_per_second: u64,
    /// The policy cap on a request's `max_rows`.
    pub max_rows_cap: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            rows_per_chunk: 1_000,
            bytes_per_chunk: 4 * 1024 * 1024,
            max_concurrent_per_site: 4,
            max_concurrent_per_user: 2,
            rows_per_second: 20_000,
            max_rows_cap: 10_000_000,
        }
    }
}

impl Settings {
    /// The key of the first setting out of range.
    ///
    /// # Errors
    ///
    /// The name of the key, never its value.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(1..=10_000).contains(&self.rows_per_chunk) {
            return Err("rows_per_chunk");
        }
        if !(1024..=64 * 1024 * 1024).contains(&self.bytes_per_chunk) {
            return Err("bytes_per_chunk");
        }
        if !(1..=1_000).contains(&self.max_concurrent_per_site) {
            return Err("max_concurrent_per_site");
        }
        if !(1..=self.max_concurrent_per_site).contains(&self.max_concurrent_per_user) {
            return Err("max_concurrent_per_user");
        }
        if self.rows_per_second < u64::from(self.rows_per_chunk) {
            return Err("rows_per_second");
        }
        if self.max_rows_cap == 0 {
            return Err("max_rows_cap");
        }
        Ok(())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Default)]
struct Counts {
    total: u32,
    per_user: HashMap<String, u32>,
    second: i64,
    rows: u64,
}

/// Exports in flight per site and per caller, and the site's row budget per second, in this
/// process. A slot is held for one request (an export holds nothing between chunks).
pub struct Limiter {
    clock: Arc<dyn Clock>,
    counts: Mutex<Counts>,
}

impl Limiter {
    /// A limiter on `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            counts: Mutex::default(),
        })
    }

    /// Takes one slot for `user`, or `false` when the site or the caller is at its bound.
    pub fn acquire(&self, user: &str, per_site: u32, per_user: u32) -> bool {
        let mut counts = lock(&self.counts);
        let mine = counts.per_user.get(user).copied().unwrap_or(0);
        if counts.total >= per_site || mine >= per_user {
            return false;
        }
        counts.total += 1;
        counts.per_user.insert(user.to_owned(), mine + 1);
        true
    }

    /// Gives a slot back.
    pub fn release(&self, user: &str) {
        let mut counts = lock(&self.counts);
        counts.total = counts.total.saturating_sub(1);
        if let Some(n) = counts.per_user.get_mut(user) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.per_user.remove(user);
            }
        }
    }

    /// Takes `rows` from this second's budget, or `false` when it is used up.
    pub fn take_rows(&self, rows: u64, budget: u64) -> bool {
        let now = self.clock.now_utc().unix_timestamp();
        let mut counts = lock(&self.counts);
        if counts.second != now {
            counts.second = now;
            counts.rows = 0;
        }
        if counts.rows.saturating_add(rows) > budget {
            return false;
        }
        counts.rows += rows;
        true
    }
}

// ---------------------------------------------------------------------------------------------
// The change feed: where a watermark starts, and what changed since one
// ---------------------------------------------------------------------------------------------

/// The most pages one call reads of the change feed: a call does bounded work, whatever the
/// length of the feed.
pub const HEAD_PAGES: u32 = 8;
/// The most pages one extract page scans.
pub const SCAN_PAGES: u32 = 20;
/// Changes read per page.
pub const FEED_PAGE: u32 = 1_000;

/// A position at or before the head of the change feed: where an export's watermark starts. It
/// moves forward a bounded distance per call, so a long feed costs nothing at an export's start,
/// and an older position only means `export_extract` re-delivers more (never a gap).
#[derive(Default)]
pub struct Head {
    at: Mutex<Option<FeedPosition>>,
}

impl Head {
    /// Moves the cached position forward and returns it.
    pub async fn advance<B: ChangeFeed>(&self, backend: &B) -> FeedPosition {
        let mut after = (*lock(&self.at)).unwrap_or(FeedPosition::START);
        for _ in 0..HEAD_PAGES {
            match backend.read_changes(after, FEED_PAGE).await {
                Ok(changes) => {
                    if let Some(last) = changes.last() {
                        after = last.position;
                    }
                    if changes.len() < FEED_PAGE as usize {
                        break;
                    }
                }
                Err(e) if e.kind == ErrorKind::ResyncRequired && after != FeedPosition::START => {
                    after = FeedPosition::START;
                }
                Err(_) => break,
            }
        }
        *lock(&self.at) = Some(after);
        after
    }
}

/// What one scan of the change feed found.
pub struct Scan {
    /// The last operation and version of each document of the DocType that changed.
    pub seen: BTreeMap<RecordId, (ChangeOp, u64)>,
    /// Where the next scan starts.
    pub after: FeedPosition,
    /// Whether the feed has nothing more after `after`.
    pub at_end: bool,
}

/// Reads the feed from `since` for `limit` documents of `doctype`, at most [`SCAN_PAGES`] pages.
///
/// # Errors
///
/// The feed's own error (`ResyncRequired` for a position it no longer holds).
pub async fn scan_changes<B: ChangeFeed>(
    backend: &B,
    since: FeedPosition,
    doctype: &str,
    limit: u32,
) -> Result<Scan, StorageError> {
    let page = limit.min(FEED_PAGE);
    let mut scan = Scan {
        seen: BTreeMap::new(),
        after: since,
        at_end: false,
    };
    for _ in 0..SCAN_PAGES {
        if scan.seen.len() + page as usize > limit as usize {
            break;
        }
        let changes = backend.read_changes(scan.after, page).await?;
        for change in &changes {
            if change.document.entity.as_str() == doctype {
                scan.seen
                    .insert(change.document.id, (change.op, change.version.0));
            }
        }
        if let Some(last) = changes.last() {
            scan.after = last.position;
        }
        if changes.len() < page as usize {
            scan.at_end = true;
            break;
        }
    }
    Ok(scan)
}

// ---------------------------------------------------------------------------------------------
// What a cursor carries
// ---------------------------------------------------------------------------------------------

/// The request a cursor continues, in the shortest stable text.
pub fn spec_text(spec: &ExportStartIn) -> String {
    json!({
        "d": spec.doctype,
        "c": spec.columns,
        "f": spec.filter,
        "o": spec.order.iter().map(|o| json!([o.field, o.descending, o.nulls_first]))
            .collect::<Vec<_>>(),
        "t": spec.format.as_str(),
        "i": spec.include_children,
        "m": spec.max_rows,
    })
    .to_string()
}

/// The request `spec_text` wrote; `None` for anything else.
pub fn spec_from_text(text: &str) -> Option<ExportStartIn> {
    let json: Json = serde_json::from_str(text).ok()?;
    let columns = json["c"]
        .as_array()?
        .iter()
        .map(|s| s.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    let order = json["o"]
        .as_array()?
        .iter()
        .map(|o| {
            Some(OrderKeyIn {
                field: o[0].as_str()?.to_owned(),
                descending: o[1].as_bool()?,
                nulls_first: o[2].as_bool(),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(ExportStartIn {
        doctype: json["d"].as_str()?.to_owned(),
        columns,
        filter: match &json["f"] {
            Json::Null => None,
            other => Some(other.clone()),
        },
        order,
        format: match json["t"].as_str()? {
            "csv" => ExportFormatIn::Csv,
            "ndjson" => ExportFormatIn::Ndjson,
            _ => return None,
        },
        include_children: json["i"].as_bool()?,
        max_rows: json["m"].as_u64(),
    })
}

/// A feed position as two values a cursor can carry.
pub fn position_values(position: FeedPosition) -> Option<[Value; 2]> {
    Some([
        Value::Int(i64::try_from(position.generation()).ok()?),
        Value::Int(i64::try_from(position.seq()).ok()?),
    ])
}

/// The feed position two values carry.
pub fn position_of(generation: &Value, seq: &Value) -> Option<FeedPosition> {
    match (generation, seq) {
        (Value::Int(g), Value::Int(s)) => Some(FeedPosition::new(
            u64::try_from(*g).ok()?,
            u64::try_from(*s).ok()?,
        )),
        _ => None,
    }
}

/// The fields a DocType exports: its scalar fields, never a table (a child is its own part) or
/// a secret; and the names of its secrets.
pub fn exportable(def: &DocTypeDef) -> (Vec<String>, BTreeSet<String>) {
    let mut fields = Vec::new();
    let mut secret = BTreeSet::new();
    for field in &def.fields {
        match field.kind {
            FieldKind::Table { .. } | FieldKind::TableMultiselect { .. } => {}
            FieldKind::Secret => {
                secret.insert(field.name.as_str().to_owned());
            }
            _ => fields.push(field.name.as_str().to_owned()),
        }
    }
    (fields, secret)
}

/// `permission.field_denied` naming `paths` (never a value).
pub fn refused(paths: &[String]) -> ApiError {
    ApiError::new(
        Code::PermissionFieldDenied,
        format!(
            "permission.field_denied: The caller may not use these fields here. ({})",
            paths.join(", ")
        ),
    )
}

/// Refuses a requested column that is a secret (`permission.field_denied`) or that the DocType
/// does not export (`document.unknown_field`).
///
/// # Errors
///
/// The refusal, naming the path and never a value.
pub fn check_columns(
    requested: &[String],
    fields: &[String],
    secret: &BTreeSet<String>,
) -> Result<(), ApiError> {
    check_secrets(requested, secret)?;
    match requested.iter().find(|c| !fields.contains(c)) {
        Some(unknown) => Err(ApiError::new(
            Code::DocumentUnknownField,
            format!("The DocType has no exportable field `{unknown}`."),
        )),
        None => Ok(()),
    }
}

/// The order keys as `(field, descending, nulls first)`, each a field of the DocType. Nulls
/// default to last for ascending and first for descending, as in a list query.
///
/// # Errors
///
/// `document.unknown_field` for a field the DocType lacks.
pub fn order_parts(
    order: &[OrderKeyIn],
    meta: &DocTypeMeta,
) -> Result<Vec<(String, bool, bool)>, ApiError> {
    order
        .iter()
        .map(|key| {
            if meta.field(&key.field).is_none() {
                return Err(unknown_field());
            }
            Ok((
                key.field.clone(),
                key.descending,
                key.nulls_first.unwrap_or(key.descending),
            ))
        })
        .collect()
}

/// The request an extract plans its columns and rights with: NDJSON, no filter, no children.
pub fn extract_request(doctype: &str, columns: &[String]) -> ExportStartIn {
    ExportStartIn {
        doctype: doctype.to_owned(),
        columns: columns.to_vec(),
        filter: None,
        order: Vec::new(),
        format: ExportFormatIn::Ndjson,
        include_children: false,
        max_rows: None,
    }
}

/// The child tables of a DocType as `(table field, child DocType, exported columns)`.
pub fn child_tables(
    meta: &spot_meta::EffectiveMeta,
    def: &DocTypeDef,
) -> Vec<(String, String, Vec<String>)> {
    def.fields
        .iter()
        .filter_map(|field| {
            let (FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. }) =
                &field.kind
            else {
                return None;
            };
            let child = meta.doctype(doctype.as_str())?;
            Some((
                field.name.as_str().to_owned(),
                doctype.as_str().to_owned(),
                exportable(&child).0,
            ))
        })
        .collect()
}

/// Refuses a field that is a secret: it never leaves, and is never filtered or sorted on.
///
/// # Errors
///
/// `permission.field_denied` naming the fields.
pub fn check_secrets(used: &[String], secret: &BTreeSet<String>) -> Result<(), ApiError> {
    let secrets: Vec<String> = used
        .iter()
        .filter(|f| secret.contains(*f))
        .cloned()
        .collect();
    if secrets.is_empty() {
        Ok(())
    } else {
        Err(refused(&secrets))
    }
}

/// The row bounds of a request: `max_rows` is at most the site's policy cap, and does not go
/// with child tables (a cap on parents would not cap their children).
///
/// # Errors
///
/// `api.invalid_request`, with `details.max_rows` naming the cap when it is the cap.
pub fn check_rows(request: &ExportStartIn, cap: u64) -> Result<(), ApiError> {
    if request.include_children && request.max_rows.is_some() {
        return Err(invalid(
            "`max_rows` cannot be combined with `include_children`.",
        ));
    }
    match request.max_rows {
        Some(rows) if rows > cap => Err(invalid("`max_rows` is above the site's policy cap.")
            .detail("max_rows", Json::from(cap))),
        _ => Ok(()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use spot_core::ManualClock;
    use time::OffsetDateTime;

    use super::*;

    fn limiter() -> (Arc<ManualClock>, Arc<Limiter>) {
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        (clock.clone(), Limiter::new(clock))
    }

    #[test]
    fn slots_are_bounded_per_site_and_per_caller_and_given_back() {
        let (_, limiter) = limiter();
        assert!(limiter.acquire("a", 3, 2));
        assert!(limiter.acquire("a", 3, 2));
        assert!(!limiter.acquire("a", 3, 2), "the caller is at its bound");
        assert!(limiter.acquire("b", 3, 2));
        assert!(!limiter.acquire("c", 3, 2), "the site is at its bound");
        limiter.release("a");
        assert!(limiter.acquire("c", 3, 2), "a released slot is free again");
        // Releasing more than was taken never underflows.
        for _ in 0..10 {
            limiter.release("a");
            limiter.release("z");
        }
        assert!(limiter.acquire("a", 1, 1));
    }

    #[test]
    fn the_row_budget_resets_each_second_and_refuses_the_excess() {
        let (clock, limiter) = limiter();
        assert!(limiter.take_rows(60, 100));
        assert!(!limiter.take_rows(41, 100));
        assert!(limiter.take_rows(40, 100));
        assert!(!limiter.take_rows(1, 100));
        clock.advance(Duration::from_secs(1));
        assert!(limiter.take_rows(100, 100));
        assert!(
            !limiter.take_rows(u64::MAX, 100),
            "no overflow wraps the budget"
        );
    }

    #[test]
    fn settings_refuse_what_cannot_work_and_name_only_the_key() {
        assert!(Settings::default().validate().is_ok());
        for (bad, key) in [
            (
                Settings {
                    rows_per_chunk: 0,
                    ..Settings::default()
                },
                "rows_per_chunk",
            ),
            (
                Settings {
                    bytes_per_chunk: 10,
                    ..Settings::default()
                },
                "bytes_per_chunk",
            ),
            (
                Settings {
                    max_concurrent_per_user: 9,
                    ..Settings::default()
                },
                "max_concurrent_per_user",
            ),
            (
                Settings {
                    rows_per_second: 1,
                    ..Settings::default()
                },
                "rows_per_second",
            ),
            (
                Settings {
                    max_rows_cap: 0,
                    ..Settings::default()
                },
                "max_rows_cap",
            ),
        ] {
            assert_eq!(bad.validate(), Err(key));
        }
        let unknown: Result<Settings, _> = serde_json::from_str("{\"surprise\": 1}");
        assert!(unknown.is_err(), "an unknown key is refused");
    }

    #[test]
    fn a_cursor_carries_its_request_and_nothing_else_parses() {
        let request = ExportStartIn {
            doctype: "project".to_owned(),
            columns: vec!["code".to_owned(), "status".to_owned()],
            filter: Some(json!({"field": "status", "op": "eq", "value": "Active"})),
            order: vec![
                OrderKeyIn {
                    field: "code".to_owned(),
                    descending: true,
                    nulls_first: Some(true),
                },
                OrderKeyIn {
                    field: "title".to_owned(),
                    descending: false,
                    nulls_first: None,
                },
            ],
            format: ExportFormatIn::Csv,
            include_children: true,
            max_rows: Some(5),
        };
        assert_eq!(spec_from_text(&spec_text(&request)), Some(request));
        for text in ["", "{}", "[]", "{\"d\":1}", "not json"] {
            assert_eq!(spec_from_text(text), None, "{text}");
        }
    }

    #[test]
    fn positions_round_trip_and_refuse_a_value_that_is_not_one() {
        let at = FeedPosition::new(3, 41);
        let [generation, seq] = position_values(at).unwrap();
        assert_eq!(position_of(&generation, &seq), Some(at));
        assert_eq!(position_of(&Value::Int(-1), &seq), None);
        assert_eq!(position_of(&Value::Text("3".to_owned()), &seq), None);
    }

    #[test]
    fn errors_map_by_code_and_an_unknown_code_leaks_nothing() {
        assert_eq!(bulk_error("bulk.limit", "", 4).status().as_u16(), 429);
        assert_eq!(
            bulk_error("bulk.limit", "", 4).details.get("retry_after"),
            Some(&Json::from(4))
        );
        assert_eq!(
            bulk_error("bulk.limit", "", 0).details.get("retry_after"),
            Some(&Json::from(1))
        );
        assert_eq!(
            bulk_error("bulk.export_denied", "", 1).status().as_u16(),
            403
        );
        assert_eq!(
            bulk_error("bulk.cursor_invalid", "secret detail", 1)
                .status()
                .as_u16(),
            422
        );
        assert_eq!(
            bulk_error("bulk.watermark_expired", "", 1)
                .status()
                .as_u16(),
            410
        );
        let unknown = bulk_error("bulk.storage", concat!("SEL", "ECT secret FR", "OM t"), 1);
        assert_eq!(unknown.status().as_u16(), 500);
        assert!(!format!("{unknown:?}").contains("SELECT"));
        let denied = bulk_error("permission.field_denied", "x (details)", 1);
        assert_eq!(denied.status().as_u16(), 403);
    }

    #[test]
    fn row_bounds_and_secret_fields_are_checked() {
        let mut request = extract_request("project", &[]);
        request.max_rows = Some(11);
        let over = check_rows(&request, 10).unwrap_err();
        assert_eq!(over.details.get("max_rows"), Some(&Json::from(10)));
        request.max_rows = Some(10);
        assert!(check_rows(&request, 10).is_ok());
        request.include_children = true;
        assert!(
            check_rows(&request, 10).is_err(),
            "a cap does not cap child rows"
        );
        let secret: BTreeSet<String> = ["password_hash".to_owned()].into();
        let fields = vec!["code".to_owned()];
        assert!(check_columns(&["code".to_owned()], &fields, &secret).is_ok());
        let refused = check_columns(&["password_hash".to_owned()], &fields, &secret).unwrap_err();
        assert_eq!(refused.status().as_u16(), 403);
        let unknown = check_columns(&["nope".to_owned()], &fields, &secret).unwrap_err();
        assert_eq!(unknown.status().as_u16(), 422);
        assert!(check_secrets(&["code".to_owned(), "password_hash".to_owned()], &secret).is_err());
    }
}
