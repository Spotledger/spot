//! Record rows to and from the domain type.
//!
//! The record is spelled out field by field (the domain types are not `Serialize`, and a wire
//! form must not change because a struct gained a field). A read decodes `body` and then checks
//! it against the columns the row also carries; a row that disagrees with itself, or whose body
//! is not exactly a record, is a corrupt row and an error: never skipped and never repaired.
//! Unknown keys in a body are refused for the same reason.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value as Json, json};
use spot_bpm_contract::OutcomeKind;
use spot_bpm_conversations::record::{Attention, Intent};
use spot_bpm_conversations::{
    ConversationError, ConversationRecord, Direction, Ending, Event, Wait, WaitFor, WaitKind,
};
use spot_bpm_model::{RejectAction, TimeoutAction};
use spot_link_exchange::{Conversation, MessageId, RejectCode};
use spot_tenant_link::LinkId;
use tokio_postgres::Row;

use crate::error::corrupt;
use crate::schema::MAX_BODY_BYTES;

type Res<T> = Result<T, ConversationError>;

/// What a record becomes in a statement: the body and the facts the columns repeat.
pub(crate) struct Encoded {
    pub body: Json,
    pub case: Option<String>,
    pub open: bool,
    pub waiting: bool,
}

/// The length in bytes of the text PostgreSQL prints for `value` stored as jsonb: what
/// `octet_length(body::text)` returns. Walks the value once and allocates nothing proportional to
/// it.
///
/// jsonb text is compact JSON plus one space after every `:` and every `,` outside a string.
/// Strings are escaped the way `serde_json` escapes them (`\"`, `\\`, `\b \f \n \r \t`, `\u00XX`
/// for other control characters, everything else literal). Integers print identically. A float
/// prints as PostgreSQL's `numeric` does, never in exponent form (see [`number_len`]).
pub(crate) fn pg_text_len(value: &Json) -> usize {
    match value {
        Json::Null | Json::Bool(true) => 4,
        Json::Bool(false) => 5,
        Json::Number(n) => number_len(&n.to_string()),
        Json::String(s) => string_len(s),
        Json::Array(items) => {
            let separators = 2 * items.len().saturating_sub(1);
            items
                .iter()
                .fold(2 + separators, |n, v| n.saturating_add(pg_text_len(v)))
        }
        Json::Object(map) => {
            let separators = 2 * map.len().saturating_sub(1);
            map.iter().fold(2 + separators, |n, (k, v)| {
                // The key, `": "` (colon and its space) and the value.
                n.saturating_add(string_len(k))
                    .saturating_add(2)
                    .saturating_add(pg_text_len(v))
            })
        }
    }
}

/// The escaped, quoted length of a string.
fn string_len(s: &str) -> usize {
    s.chars().fold(2, |n, c| {
        n + match c {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            c if c < '\u{20}' => 6,
            c => c.len_utf8(),
        }
    })
}

/// The length of a number's PostgreSQL text, given `serde_json`'s text for it. Integers and plain
/// decimals (`12`, `-3`, `1.5`) print the same; an exponent form (`1e21`, `1.5e-7`) is expanded
/// the way `numeric` prints it: digits, then zeros or a decimal point.
fn number_len(text: &str) -> usize {
    let Some(at) = text.find(['e', 'E']) else {
        return text.len();
    };
    let (mantissa, exp) = text.split_at(at);
    let Ok(exp) = exp[1..].trim_start_matches('+').parse::<i64>() else {
        // Not a number serde_json prints; count the larger of the two so the cap fails closed.
        return text.len().saturating_mul(2);
    };
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => (1, rest),
        None => (0, mantissa),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let significant = whole
        .chars()
        .chain(fraction.chars())
        .skip_while(|c| *c == '0')
        .count();
    // The value is `digits * 10^shift`; a negative shift is the scale after the point.
    let shift = exp.saturating_sub(i64::try_from(fraction.len()).unwrap_or(i64::MAX));
    let body = if shift >= 0 {
        if significant == 0 {
            1
        } else {
            significant.saturating_add(usize::try_from(shift).unwrap_or(usize::MAX))
        }
    } else {
        let scale = usize::try_from(shift.unsigned_abs()).unwrap_or(usize::MAX);
        if scale >= significant {
            // `0.` then zeros then the digits.
            scale.saturating_add(2)
        } else {
            significant + 1
        }
    };
    sign + body
}

/// Encodes a record whose version is `version`. A body above [`MAX_BODY_BYTES`] is refused with
/// [`ConversationError::TooLong`] before any statement runs; the body is measured as PostgreSQL
/// will measure it ([`pg_text_len`]).
pub(crate) fn encode(record: &ConversationRecord, version: u64) -> Res<Encoded> {
    let events: Vec<Json> = record
        .events
        .iter()
        .map(|e| {
            json!({
                "direction": match e.direction {
                    Direction::Sent => "sent",
                    Direction::Received => "received",
                },
                "message": e.message,
                "id": e.id.to_string(),
                "at_ms": e.at_ms,
                "payload": e.payload,
            })
        })
        .collect();
    let counts: Map<String, Json> = record
        .counts
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    let intents: Map<String, Json> = record
        .intents
        .iter()
        .map(|(k, i)| {
            (
                k.clone(),
                json!({ "id": i.id.to_string(), "message": i.message }),
            )
        })
        .collect();
    let consumed: Vec<String> = record.consumed.iter().map(ToString::to_string).collect();
    let consumed_by: Vec<Json> = record
        .consumed_by
        .iter()
        .map(|((node, attempt), id)| json!({ "node": node, "attempt": attempt, "id": id.to_string() }))
        .collect();
    let waits: Vec<Json> = record.waits.iter().map(encode_wait).collect();
    let ending = record.ending.as_ref().map(|e| {
        json!({
            "outcome": e.outcome,
            "kind": kind_name(e.kind),
            "at_ms": e.at_ms,
            "by_peer": e.by_peer,
            "timed_out": e.timed_out,
        })
    });
    let attention = record
        .attention
        .as_ref()
        .map(|a| json!({ "code": a.code, "at_ms": a.at_ms }));
    let arrivals: Vec<String> = record
        .arrivals_done
        .iter()
        .map(ToString::to_string)
        .collect();
    let body = json!({
        "version": version,
        "link": record.link.to_string(),
        "conversation": record.conversation.as_str(),
        "contract": record.contract,
        "contract_version": record.contract_version,
        "role": record.role,
        "peer_role": record.peer_role,
        "case": record.case,
        "events": events,
        "counts": counts,
        "intents": intents,
        "consumed": consumed,
        "consumed_by": consumed_by,
        "waits": waits,
        "ending": ending,
        "attention": attention,
        "arrivals_done": arrivals,
        "compensated": record.compensated,
    });
    // The table CHECK measures `octet_length(body::text)`, the text PostgreSQL prints for the
    // stored jsonb (a space after every `:` and `,`), which is longer than `serde_json`'s compact
    // text. `pg_text_len` computes that length without building the text, so the cap here and the
    // CHECK agree to the byte and an over-cap body is `TooLong`, never a store failure.
    if pg_text_len(&body) > MAX_BODY_BYTES {
        return Err(ConversationError::TooLong);
    }
    Ok(Encoded {
        body,
        case: record.case.clone(),
        open: record.ending.is_none(),
        waiting: !record.waits.is_empty(),
    })
}

fn encode_wait(w: &Wait) -> Json {
    let kind = match &w.kind {
        WaitKind::Message {
            message,
            on_timeout,
        } => json!({
            "type": "message",
            "message": message,
            "on_timeout": match on_timeout {
                TimeoutAction::ErrorBranch => "error_branch",
                TimeoutAction::TimeoutEdge => "timeout_edge",
            },
        }),
        WaitKind::Delivery {
            id,
            wait_for,
            on_reject,
        } => json!({
            "type": "delivery",
            "id": id.to_string(),
            "wait_for": match wait_for {
                WaitFor::Queued => "queued",
                WaitFor::Acknowledged => "acknowledged",
                WaitFor::Applied => "applied",
            },
            "on_reject": match on_reject {
                RejectAction::ErrorBranch => "error_branch",
                RejectAction::Continue => "continue",
            },
        }),
    };
    json!({ "node": w.node, "attempt": w.attempt, "kind": kind, "due_ms": w.due_ms })
}

fn kind_name(kind: OutcomeKind) -> &'static str {
    match kind {
        OutcomeKind::Completed => "completed",
        OutcomeKind::Cancelled => "cancelled",
        OutcomeKind::Failed => "failed",
        OutcomeKind::NeedsAttention => "needs_attention",
    }
}

// ---- decoding -------------------------------------------------------------------------------

fn obj<'a>(v: &'a Json, keys: &[&str]) -> Res<&'a Map<String, Json>> {
    let m = v.as_object().ok_or_else(corrupt)?;
    if m.len() != keys.len() || keys.iter().any(|k| !m.contains_key(*k)) {
        return Err(corrupt());
    }
    Ok(m)
}

fn text(m: &Map<String, Json>, key: &str) -> Res<String> {
    m.get(key)
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(corrupt)
}

fn num(m: &Map<String, Json>, key: &str) -> Res<u64> {
    m.get(key).and_then(Json::as_u64).ok_or_else(corrupt)
}

fn small(m: &Map<String, Json>, key: &str) -> Res<u32> {
    u32::try_from(num(m, key)?).map_err(|_| corrupt())
}

fn flag(m: &Map<String, Json>, key: &str) -> Res<bool> {
    m.get(key).and_then(Json::as_bool).ok_or_else(corrupt)
}

fn opt_text(m: &Map<String, Json>, key: &str) -> Res<Option<String>> {
    match m.get(key) {
        Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s.clone())),
        _ => Err(corrupt()),
    }
}

fn array<'a>(m: &'a Map<String, Json>, key: &str) -> Res<&'a Vec<Json>> {
    m.get(key).and_then(Json::as_array).ok_or_else(corrupt)
}

fn mid(text: &str) -> Res<MessageId> {
    text.parse().map_err(|_| corrupt())
}

fn mids(m: &Map<String, Json>, key: &str) -> Res<BTreeSet<MessageId>> {
    let items = array(m, key)?;
    let set: BTreeSet<MessageId> = items
        .iter()
        .map(|v| v.as_str().ok_or_else(corrupt).and_then(mid))
        .collect::<Res<_>>()?;
    if set.len() == items.len() {
        Ok(set)
    } else {
        Err(corrupt())
    }
}

fn decode_event(v: &Json) -> Res<Event> {
    let m = obj(v, &["direction", "message", "id", "at_ms", "payload"])?;
    let direction = match text(m, "direction")?.as_str() {
        "sent" => Direction::Sent,
        "received" => Direction::Received,
        _ => return Err(corrupt()),
    };
    let payload = match m.get("payload") {
        Some(Json::Null) | None => None,
        Some(other) => Some(other.clone()),
    };
    // A sent event has no payload and a received one always has: anything else is not a record.
    if (direction == Direction::Sent) != payload.is_none() {
        return Err(corrupt());
    }
    Ok(Event {
        direction,
        message: text(m, "message")?,
        id: mid(&text(m, "id")?)?,
        at_ms: num(m, "at_ms")?,
        payload,
    })
}

fn decode_wait(v: &Json) -> Res<Wait> {
    let m = obj(v, &["node", "attempt", "kind", "due_ms"])?;
    let kind = m.get("kind").ok_or_else(corrupt)?;
    let kind_map = kind.as_object().ok_or_else(corrupt)?;
    let kind = match text(kind_map, "type")?.as_str() {
        "message" => {
            let k = obj(kind, &["type", "message", "on_timeout"])?;
            WaitKind::Message {
                message: text(k, "message")?,
                on_timeout: match text(k, "on_timeout")?.as_str() {
                    "error_branch" => TimeoutAction::ErrorBranch,
                    "timeout_edge" => TimeoutAction::TimeoutEdge,
                    _ => return Err(corrupt()),
                },
            }
        }
        "delivery" => {
            let k = obj(kind, &["type", "id", "wait_for", "on_reject"])?;
            WaitKind::Delivery {
                id: mid(&text(k, "id")?)?,
                wait_for: match text(k, "wait_for")?.as_str() {
                    "queued" => WaitFor::Queued,
                    "acknowledged" => WaitFor::Acknowledged,
                    "applied" => WaitFor::Applied,
                    _ => return Err(corrupt()),
                },
                on_reject: match text(k, "on_reject")?.as_str() {
                    "error_branch" => RejectAction::ErrorBranch,
                    "continue" => RejectAction::Continue,
                    _ => return Err(corrupt()),
                },
            }
        }
        _ => return Err(corrupt()),
    };
    let due_ms = match m.get("due_ms") {
        Some(Json::Null) => None,
        Some(other) => Some(other.as_u64().ok_or_else(corrupt)?),
        None => return Err(corrupt()),
    };
    Ok(Wait {
        node: text(m, "node")?,
        attempt: small(m, "attempt")?,
        kind,
        due_ms,
    })
}

fn decode_ending(v: &Json) -> Res<Option<Ending>> {
    if v.is_null() {
        return Ok(None);
    }
    let m = obj(v, &["outcome", "kind", "at_ms", "by_peer", "timed_out"])?;
    let kind = match text(m, "kind")?.as_str() {
        "completed" => OutcomeKind::Completed,
        "cancelled" => OutcomeKind::Cancelled,
        "failed" => OutcomeKind::Failed,
        "needs_attention" => OutcomeKind::NeedsAttention,
        _ => return Err(corrupt()),
    };
    Ok(Some(Ending {
        outcome: text(m, "outcome")?,
        kind,
        at_ms: num(m, "at_ms")?,
        by_peer: flag(m, "by_peer")?,
        timed_out: flag(m, "timed_out")?,
    }))
}

fn decode_attention(v: &Json) -> Res<Option<Attention>> {
    if v.is_null() {
        return Ok(None);
    }
    let m = obj(v, &["code", "at_ms"])?;
    Ok(Some(Attention {
        code: text(m, "code")?,
        at_ms: num(m, "at_ms")?,
    }))
}

/// Decodes a body into a record (no column checks).
fn decode_body(body: &Json) -> Res<ConversationRecord> {
    let m = obj(
        body,
        &[
            "version",
            "link",
            "conversation",
            "contract",
            "contract_version",
            "role",
            "peer_role",
            "case",
            "events",
            "counts",
            "intents",
            "consumed",
            "consumed_by",
            "waits",
            "ending",
            "attention",
            "arrivals_done",
            "compensated",
        ],
    )?;
    let events = array(m, "events")?
        .iter()
        .map(decode_event)
        .collect::<Res<Vec<_>>>()?;
    let counts = m
        .get("counts")
        .and_then(Json::as_object)
        .ok_or_else(corrupt)?
        .iter()
        .map(|(k, v)| {
            let n = v
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(corrupt)?;
            Ok((k.clone(), n))
        })
        .collect::<Res<BTreeMap<_, _>>>()?;
    let intents = m
        .get("intents")
        .and_then(Json::as_object)
        .ok_or_else(corrupt)?
        .iter()
        .map(|(k, v)| {
            let i = obj(v, &["id", "message"])?;
            Ok((
                k.clone(),
                Intent {
                    id: mid(&text(i, "id")?)?,
                    message: text(i, "message")?,
                },
            ))
        })
        .collect::<Res<BTreeMap<_, _>>>()?;
    let consumed_by_items = array(m, "consumed_by")?;
    let consumed_by = consumed_by_items
        .iter()
        .map(|v| {
            let c = obj(v, &["node", "attempt", "id"])?;
            Ok((
                (text(c, "node")?, small(c, "attempt")?),
                mid(&text(c, "id")?)?,
            ))
        })
        .collect::<Res<BTreeMap<_, _>>>()?;
    if consumed_by.len() != consumed_by_items.len() {
        return Err(corrupt());
    }
    Ok(ConversationRecord {
        version: num(m, "version")?,
        link: text(m, "link")?.parse::<LinkId>().map_err(|_| corrupt())?,
        conversation: Conversation::new(text(m, "conversation")?).map_err(|_| corrupt())?,
        contract: text(m, "contract")?,
        contract_version: small(m, "contract_version")?,
        role: text(m, "role")?,
        peer_role: text(m, "peer_role")?,
        case: opt_text(m, "case")?,
        events,
        counts,
        intents,
        consumed: mids(m, "consumed")?,
        consumed_by,
        waits: array(m, "waits")?
            .iter()
            .map(decode_wait)
            .collect::<Res<Vec<_>>>()?,
        ending: decode_ending(m.get("ending").ok_or_else(corrupt)?)?,
        attention: decode_attention(m.get("attention").ok_or_else(corrupt)?)?,
        arrivals_done: mids(m, "arrivals_done")?,
        compensated: flag(m, "compensated")?,
    })
}

/// The record a row holds, after checking its columns against its body.
pub(crate) fn record_from_row(row: &Row) -> Res<ConversationRecord> {
    let body: Json = row.try_get("body").map_err(|_| corrupt())?;
    let record = decode_body(&body)?;
    let link: String = row.try_get("link_id").map_err(|_| corrupt())?;
    let conversation: String = row.try_get("conversation").map_err(|_| corrupt())?;
    let version: i64 = row.try_get("version").map_err(|_| corrupt())?;
    let case: Option<String> = row.try_get("case_id").map_err(|_| corrupt())?;
    let open: bool = row.try_get("open").map_err(|_| corrupt())?;
    let waiting: bool = row.try_get("waiting").map_err(|_| corrupt())?;
    let agrees = link == record.link.to_string()
        && conversation == record.conversation.as_str()
        && u64::try_from(version).is_ok_and(|v| v == record.version)
        && case == record.case
        && open == record.ending.is_none()
        && waiting == !record.waits.is_empty();
    if agrees { Ok(record) } else { Err(corrupt()) }
}

/// A reject code as stored.
pub(crate) fn code_name(code: RejectCode) -> Res<String> {
    match serde_json::to_value(code) {
        Ok(Json::String(s)) => Ok(s),
        _ => Err(corrupt()),
    }
}

/// A reject code read back.
pub(crate) fn code_from(name: &str) -> Res<RejectCode> {
    serde_json::from_value(Json::String(name.to_owned())).map_err(|_| corrupt())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    /// Values with their jsonb text length as PostgreSQL prints it; the integration test
    /// `tests/size_cap.rs` and `matches_postgres` below check the same table against a database.
    fn table() -> Vec<(Json, usize)> {
        vec![
            (json!(null), 4),
            (json!(true), 4),
            (json!(false), 5),
            (json!(0), 1),
            (json!(-12), 3),
            (json!(u64::MAX), 20),
            (json!(1.5), 3),
            (json!(1.0), 3),
            (json!(1e21), 22),
            (json!(1.5e-7), 10),
            (json!(-2.5e-9), 13),
            (json!("é\"\\\n\u{1}\u{7f}"), 2 + 2 + 2 + 2 + 2 + 6 + 1),
            (json!([]), 2),
            (json!({}), 2),
            (json!([1, 2, 3]), 9),
            (json!({"a": 1, "b": [true, null]}), 27),
            (json!({"a": {"b": {}}, "c": []}), 25),
        ]
    }

    #[test]
    fn lengths_match_the_table() {
        for (value, want) in table() {
            assert_eq!(pg_text_len(&value), want, "{value}");
        }
    }

    #[test]
    fn matches_postgres() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let Some(db) = rt.block_on(spot_postgres::testing::TestDb::create()) else {
            return;
        };
        rt.block_on(async {
            let client = db.raw_client().await;
            for (value, _) in table() {
                let row = client
                    .query_one("SELECT octet_length($1::jsonb::text)", &[&value])
                    .await
                    .unwrap();
                let got: i32 = row.get(0);
                assert_eq!(
                    pg_text_len(&value),
                    usize::try_from(got).unwrap(),
                    "{value}"
                );
            }
        });
    }
}
