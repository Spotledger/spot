//! `connector_inbound` (`POST /api/v1/connectors/{name}/inbound`), native API 0.25.0,
//! WP-INT-05.T4: the ingress for an external system's request. It is the only unauthenticated
//! route of the connector amendment, so it gets the most care.
//!
//! # What the route does, in order
//!
//! 1. no [`ConnectorsPort`] attached: `501 service.not_configured`;
//! 2. the rate limit (per source address and per connector), through the port, **before** the
//!    body is read: `429 api.rate_limited`. The registry's own public layer adds the `auth`
//!    class limit per client address when the group is mounted through [`registration`];
//! 3. `Content-Type` must be `application/json`, else `415 api.unsupported_media_type`;
//! 4. the body is read with a hard cap of 1 MiB **while it streams**: a declared length over the
//!    cap, or a body that passes it (an endless one included), is `413 http.body_too_large`
//!    and the read stops at the cap;
//! 5. the signature is checked in **constant time before any parse**: the timestamp window (5
//!    minutes either side), the HMAC-SHA256 over `<t>.<body>` (the scheme of the outbound
//!    webhooks) or the connector's bearer token. Everything is computed for every request: an
//!    unknown connector, a missing or malformed header, a stale timestamp and a wrong secret all
//!    run the same hash work against a dummy or the real key, and all answer **one** `401
//!    connector.signature_invalid`: the same status, body (but the request id) and headers, so
//!    the endpoint is no oracle for connector names and the answer is no timing channel;
//! 6. only a request that verified is parsed (a JSON object, at most 500 members) and handed to
//!    the port, which remembers the sender's `X-Spot-Delivery` for the replay window (`409
//!    connector.replayed` for a repeat; **only a verified request can learn this**) and enqueues
//!    the job that runs as the connector's service principal;
//! 7. `202 {"data":{"accepted":true}}`.
//!
//! The route never echoes the body, a header or the connector name, and logs a refusal with the
//! operation id and the problem code only. It reads no clock: the timestamp window uses the
//! port's injected [`ConnectorsPort::now_unix`].
//!
//! # Mounting
//!
//! The group is public. Mounted through [`registration`] it runs behind the registry's public
//! layer, which **removes the `Authorization` header** before the handler: only the HMAC scheme
//! works there. The bearer scheme needs a host mount without that removal (see the task report).
#![allow(clippy::result_large_err)] // a refusal is a whole response, built once and returned

use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Map, Value as Json, json};
use sha2::{Digest as _, Sha256};

use super::super::{Public, RateClass, Registration, RouteState};
use crate::api::ports::PortKey;
use crate::api::ports::connectors::{ConnectorsPort, InboundCredential, InboundRefusal};
use crate::api::registry::Code;
use crate::api::service::{ApiError, RequestCtx};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::super::super::render_error;

/// The operation id this group serves.
pub const INBOUND_SERVES: &[&str] = &["connector_inbound"];

/// The group is public: no bearer, the `auth` rate class.
pub const INBOUND_PUBLIC: Public = Public {
    rate_class: RateClass::Auth,
};

/// The ports this group needs.
pub fn required_ports() -> &'static [PortKey] {
    static KEYS: [PortKey; 1] = [PortKey::of::<dyn ConnectorsPort>()];
    &KEYS
}

/// The ingress route.
pub fn routes() -> Router<RouteState> {
    Router::new().route("/api/v1/connectors/{name}/inbound", post(connector_inbound))
}

/// The group as the registry's mount function takes it.
pub fn registration() -> Registration {
    Registration {
        name: "connector_inbound",
        serves: INBOUND_SERVES,
        required_ports,
        routes,
        public: Some(INBOUND_PUBLIC),
    }
}

/// The hard cap of a delivery's body: 1 MiB.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// The timestamp window, in seconds, either side of now.
pub const WINDOW_SECS: i64 = 300;
/// The members a delivery's body object may have (the contract's `maxProperties`).
const MAX_BODY_PROPERTIES: usize = 500;
/// The longest signature header the contract allows.
const MAX_SIGNATURE_LEN: usize = 300;
/// The longest bearer value the contract allows.
const MAX_BEARER_LEN: usize = 1024;
/// The longest delivery id the contract allows.
const MAX_DELIVERY_LEN: usize = 200;
/// The operation id, for the refusal log.
const OPERATION: &str = "connector_inbound";

/// How many times the verification ran: once per request that reached it, whatever the cause of
/// the refusal (the tests use it to show an unknown connector costs the same as a bad
/// signature).
pub static VERIFICATIONS: AtomicUsize = AtomicUsize::new(0);

/// The key a verification runs against when the connector is unknown or has no HMAC scheme.
const DUMMY_KEY: [u8; 32] = [0x5a; 32];
/// No bytes.
const EMPTY: &[u8] = &[];

// ---------------------------------------------------------------------------------------------
// The primitives (shared with the adapters and the tests)

/// HMAC-SHA256 (RFC 2104) built on SHA-256 alone.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for ((inner_byte, outer_byte), key_byte) in inner_pad
        .iter_mut()
        .zip(outer_pad.iter_mut())
        .zip(block.iter())
    {
        *inner_byte ^= key_byte;
        *outer_byte ^= key_byte;
    }
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(message);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// Constant-time equality of two equal-length byte strings; different lengths are unequal (the
/// lengths here are fixed by construction, never secret).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing a hex pair into a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn unhex_32(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks(2).enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

/// The `X-Spot-Signature` value for `body` signed with `key` at Unix time `t`:
/// `t=<unix>,v1=<hex hmac-sha256(key, "<t>." + body)>`.
pub fn signature_header(key: &[u8], t: i64, body: &[u8]) -> String {
    let mut message = format!("{t}.").into_bytes();
    message.extend_from_slice(body);
    format!("t={t},v1={}", hex(&hmac_sha256(key, &message)))
}

// ---------------------------------------------------------------------------------------------
// What the request presents

enum Presented {
    /// `t=<unix>,v1=<hex>`.
    Signature { t: i64, mac: [u8; 32] },
    /// `Authorization: Bearer <token>`.
    Bearer(Vec<u8>),
    /// Nothing usable.
    Nothing,
}

/// `t=<digits>,v1=<64 hex>`: exactly this shape and nothing else.
fn parse_signature(raw: &str) -> Option<(i64, [u8; 32])> {
    let (t_part, v_part) = raw.split_once(',')?;
    let t_text = t_part.strip_prefix("t=")?;
    let v_text = v_part.strip_prefix("v1=")?;
    if t_text.is_empty() || t_text.len() > 12 || !t_text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((t_text.parse::<i64>().ok()?, unhex_32(v_text)?))
}

fn single<'h>(headers: &'h HeaderMap, name: &str) -> Result<Option<&'h HeaderValue>, ()> {
    let mut values = headers.get_all(name).iter();
    let first = values.next();
    if values.next().is_some() {
        return Err(());
    }
    Ok(first)
}

/// What the request presents: the signature header when there is exactly one usable value, else
/// the bearer when there is exactly one, else nothing.
fn presented_of(headers: &HeaderMap) -> Presented {
    if let Ok(Some(value)) = single(headers, "x-spot-signature") {
        if let Some((t, mac)) = value
            .to_str()
            .ok()
            .filter(|v| v.len() <= MAX_SIGNATURE_LEN)
            .and_then(parse_signature)
        {
            return Presented::Signature { t, mac };
        }
        return Presented::Nothing;
    }
    if let Ok(Some(value)) = single(headers, "authorization")
        && let Some(token) = value
            .to_str()
            .ok()
            .filter(|v| v.len() <= MAX_BEARER_LEN)
            .and_then(|v| v.strip_prefix("Bearer "))
            .filter(|t| !t.is_empty())
        {
            return Presented::Bearer(token.as_bytes().to_vec());
        }
    Presented::Nothing
}

/// Whether the request carries a delivery id in a form the contract allows, or none at all:
/// `Ok(None)` none, `Ok(Some)` one, `Err` malformed or repeated.
fn delivery_of(headers: &HeaderMap) -> Result<Option<String>, ()> {
    let Some(value) = single(headers, "x-spot-delivery")? else {
        return Ok(None);
    };
    match value.to_str() {
        Ok(text)
            if (1..=MAX_DELIVERY_LEN).contains(&text.len())
                && text.bytes().all(|b| (0x21..=0x7e).contains(&b)) =>
        {
            Ok(Some(text.to_owned()))
        }
        _ => Err(()),
    }
}

/// The whole check, over the same work for every request: one HMAC and two digests, whatever the
/// connector, the headers or the timestamp. The result combines every condition without
/// branching on any of them.
fn verify(
    credential: Option<&InboundCredential>,
    presented: &Presented,
    body: &[u8],
    now: i64,
) -> bool {
    VERIFICATIONS.fetch_add(1, Ordering::Relaxed);
    let flag = |b: bool| u8::from(b);
    let (hmac_key, hmac_known): (&[u8], u8) = match credential {
        Some(InboundCredential::Hmac(key)) => (key.as_slice(), 1),
        _ => (&DUMMY_KEY[..], 0),
    };
    let (bearer_stored, bearer_known): (&[u8], u8) = match credential {
        Some(InboundCredential::Bearer(token)) => (token.as_slice(), 1),
        _ => (&DUMMY_KEY[..], 0),
    };
    let (t, presented_mac, sig_present) = match presented {
        Presented::Signature { t, mac } => (*t, *mac, 1u8),
        _ => (0, [0u8; 32], 0u8),
    };
    let (token, bearer_present): (&[u8], u8) = match presented {
        Presented::Bearer(token) => (token.as_slice(), 1),
        _ => (EMPTY, 0),
    };
    // The HMAC path.
    let mut message = format!("{t}.").into_bytes();
    message.extend_from_slice(body);
    let expected = hmac_sha256(hmac_key, &message);
    let fresh = flag(now.saturating_sub(t).abs() <= WINDOW_SECS);
    let mac_ok = flag(ct_eq(&expected, &presented_mac));
    let hmac_ok = hmac_known & sig_present & fresh & mac_ok;
    // The bearer path: both sides hashed to the same length first.
    let presented_digest = Sha256::digest(token);
    let stored_digest = Sha256::digest(bearer_stored);
    let token_ok = flag(ct_eq(&presented_digest, &stored_digest));
    let bearer_ok = bearer_known & bearer_present & token_ok;
    (hmac_ok | bearer_ok) == 1
}

// ---------------------------------------------------------------------------------------------
// Answers

/// Every answer of the route is private to the caller and never cached.
fn finish(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// A refusal: logged with the operation id and the problem code only, never a value.
fn refuse(error: ApiError, request_id: &RequestId) -> Response {
    tracing::info!(
        operation = OPERATION,
        code = error.code.as_str(),
        "request refused"
    );
    finish(render_error(error, request_id))
}

/// The one rejection of an unverified request: the same for every cause, with no detail.
fn rejected(request_id: &RequestId) -> Response {
    refuse(
        ApiError::from_code(Code::ConnectorSignatureInvalid),
        request_id,
    )
}

/// Whether the request declares `application/json` (any parameters are ignored).
fn is_json(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value
        .to_str()
        .ok()
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
}

/// A connector name usable for a lookup: 1 to 200 characters, no control character. Anything
/// else is looked up as the empty name, which no connector has.
fn lookup_name(raw: &str) -> &str {
    if (1..=200).contains(&raw.chars().count()) && !raw.chars().any(char::is_control) {
        raw
    } else {
        ""
    }
}

async fn connector_inbound(
    State(state): State<RouteState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let request_id = &site.request_id;
    let Some(port) = state.port::<dyn ConnectorsPort>() else {
        return finish(super::not_configured(request_id));
    };
    // No credential: the route is public, and the registry removed any bearer.
    let ctx = RequestCtx {
        site: site.site.clone(),
        credential: None,
        idempotency_key: None,
        language: None,
        request_id: request_id.clone(),
        peer: site.peer,
    };
    if let Err(wait) = port.admit_inbound(&ctx, lookup_name(&name)) {
        return finish(render_error(
            ApiError::from_code(Code::ApiRateLimited).detail("retry_after", Json::from(wait)),
            request_id,
        ));
    }
    if !is_json(&headers) {
        return refuse(
            ApiError::from_code(Code::ApiUnsupportedMediaType),
            request_id,
        );
    }
    let too_large = || refuse(ApiError::from_code(Code::HttpBodyTooLarge), request_id);
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<usize>().ok());
    if declared.is_some_and(|n| n > MAX_BODY_BYTES) {
        return too_large();
    }
    // The read stops at the cap, so an endless body is never buffered past it.
    let Ok(body): Result<Bytes, _> =
        axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await
    else {
        return too_large();
    };

    // The check, before any parse. Every input is read first and every computation runs.
    let delivery = delivery_of(&headers);
    let presented = presented_of(&headers);
    let credential = port.inbound_credential(&ctx, lookup_name(&name)).await;
    let now = port.now_unix();
    let signature_ok = verify(credential.as_ref(), &presented, &body, now);
    let delivery_ok = delivery.is_ok();
    if u8::from(signature_ok) & u8::from(delivery_ok) != 1 {
        return rejected(request_id);
    }
    let delivery_id = delivery.unwrap_or(None);

    // Verified: now the body may be parsed.
    let map: Map<String, Json> = match serde_json::from_slice::<Json>(&body) {
        Ok(Json::Object(map)) if map.len() <= MAX_BODY_PROPERTIES => map,
        Ok(Json::Object(_)) => {
            return refuse(
                ApiError::new(
                    Code::ApiInvalidRequest,
                    "The body is an object of at most 500 members.",
                ),
                request_id,
            );
        }
        Ok(_) => {
            return refuse(
                ApiError::new(
                    Code::ApiInvalidRequest,
                    "The request body must be a JSON object.",
                ),
                request_id,
            );
        }
        Err(_) => return refuse(ApiError::from_code(Code::ApiInvalidJson), request_id),
    };
    match port
        .inbound_receive(&ctx, lookup_name(&name), delivery_id.as_deref(), &map)
        .await
    {
        Ok(()) => finish(
            (
                StatusCode::ACCEPTED,
                axum::Json(json!({ "data": { "accepted": true } })),
            )
                .into_response(),
        ),
        Err(InboundRefusal::Replayed) => {
            refuse(ApiError::from_code(Code::ConnectorReplayed), request_id)
        }
        Err(InboundRefusal::Disabled) => {
            refuse(ApiError::from_code(Code::ConnectorDisabled), request_id)
        }
        Err(InboundRefusal::Unavailable) => {
            refuse(ApiError::from_code(Code::ServerUnavailable), request_id)
        }
        Err(InboundRefusal::Internal) => {
            refuse(ApiError::from_code(Code::HttpInternal), request_id)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn hmac_sha256_matches_the_rfc_4231_vectors() {
        // Test case 1.
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block is hashed first.
        let mac = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn a_signature_header_round_trips_through_the_parser() {
        let header = signature_header(b"k", 1_800_000_000, b"{}");
        let (t, mac) = parse_signature(&header).unwrap();
        assert_eq!(t, 1_800_000_000);
        assert_eq!(mac, hmac_sha256(b"k", b"1800000000.{}"));
        for bad in ["", "t=1", "t=1,v1=zz", "v1=00,t=1", "t=,v1=", "t=-1,v1=00"] {
            assert!(parse_signature(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_comparison_is_exact() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}
