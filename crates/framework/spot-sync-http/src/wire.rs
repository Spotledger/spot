//! The wire: the request and response bodies and the newline-delimited framing of a batch.
//!
//! ADR-078 section 2 keeps one operation layer behind many bindings; the sync binding is a
//! plain HTTP/1.1-or-h2 binding, so it reuses the platform's JSON value encoding and streams a
//! batch as **newline-delimited JSON** (NDJSON): the first line is the batch header, and each
//! following line is one record.
//!
//! The header-first framing is deliberate. ADR-062 G1 says the receiver refuses a batch whose
//! source partition is not owned by the authenticated node, and the brief says that check
//! happens *before any decoding of records beyond the header*: the receiver reads the header
//! line, checks ownership, and only then parses record lines. A heartbeat line
//! (`{"heartbeat":true}`) may appear between a long-poll's request and its batch; a reader skips
//! it.
//!
//! A heartbeat may also carry one opaque payload under `hub_time`
//! (`{"heartbeat":true,"hub_time":<any JSON value>}`): the streamed form of a pre-signed hub
//! time. It stays a heartbeat to every reader — [`is_heartbeat`] ignores the field, so
//! an older reader skips the line exactly as before — and this module neither signs, verifies nor
//! interprets the payload. [`heartbeat_payload`] hands it to a caller that will; the node's
//! snapshot store is the only place allowed to trust it.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Bytes;
use serde::{Deserialize, Serialize};
use spot_events_contract::SourceId;
use spot_sync_protocol::{Batch, Checksum, Hello, ProtocolVersion, Record};

use crate::error::{Error, Result};

/// The content type of an NDJSON sync body.
pub const CONTENT_TYPE: &str = "application/x-ndjson";

/// The largest batch body this binding will read or write, in bytes.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// The largest record payload, matching the state machine's own limit.
pub const MAX_PAYLOAD_BYTES: usize = 1 << 20;

/// The first line of a streamed batch: identity, the sequence range, and the checksum over the
/// records that follow.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchHeader {
    /// The stream the batch belongs to.
    pub source: SourceId,
    /// The first sequence the batch carries.
    pub from_seq: u64,
    /// The last sequence the batch carries.
    pub to_seq: u64,
    /// The checksum over the header and the records.
    pub checksum: Checksum,
}

impl BatchHeader {
    /// The header of a batch.
    pub fn of(batch: &Batch) -> Self {
        Self {
            source: batch.source.clone(),
            from_seq: batch.from_seq,
            to_seq: batch.to_seq,
            checksum: batch.checksum,
        }
    }

    /// The partition this batch's source belongs to, if any.
    pub fn partition(&self) -> Option<&str> {
        self.source.partition()
    }
}

/// The hub's answer to a handshake: the negotiated version and the hub's own [`Hello`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloReply {
    /// The highest protocol version both peers speak.
    pub protocol: ProtocolVersion,
    /// The hub's identity and range.
    pub hub: Hello,
}

/// The hub's answer to a push: an acknowledgement or a refusal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PushReply {
    /// The batch was applied (or was already applied); `through_seq` is durable.
    Ack(spot_sync_protocol::Ack),
    /// The batch was refused; the sender resumes from the named sequence.
    Nack(spot_sync_protocol::Nack),
}

impl PushReply {
    /// The acknowledgement, if this reply is one.
    pub fn into_ack(self) -> Option<spot_sync_protocol::Ack> {
        match self {
            Self::Ack(ack) => Some(ack),
            Self::Nack(_) => None,
        }
    }
}

/// A heartbeat line's marker: a reader skips a line whose object has `heartbeat: true`.
///
/// The struct carries only the marker, so a line that also carries a `hub_time` payload parses
/// as a heartbeat too: a consumer ignores fields it does not know (ADR-056).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Heartbeat {
    heartbeat: bool,
}

/// The field a heartbeat carries a pre-signed hub time under.
const HUB_TIME_FIELD: &str = "hub_time";

/// The largest `hub_time` payload a heartbeat carries, in bytes once serialised.
///
/// A payload above this is never sent (the sender falls back to a bare heartbeat) and is ignored
/// by a reader, so an end-point cannot be made to buffer an unbounded value in a keep-alive line.
pub const MAX_HEARTBEAT_PAYLOAD_BYTES: usize = 4_096;

/// A heartbeat line that may carry a payload.
#[derive(Serialize)]
struct HeartbeatWith<'a> {
    heartbeat: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    hub_time: Option<&'a serde_json::Value>,
}

/// Whether an oversize payload has already been warned about (one warning, not one per beat).
static OVERSIZE_WARNED: AtomicBool = AtomicBool::new(false);

/// Encodes one batch as NDJSON: the header line, then a line per record.
pub fn encode_batch(batch: &Batch) -> Result<Bytes> {
    let mut out = Vec::new();
    push_line(&mut out, &BatchHeader::of(batch))?;
    for record in &batch.records {
        push_line(&mut out, record)?;
    }
    if out.len() > MAX_BODY_BYTES {
        return Err(Error::batch_too_large(format!(
            "the encoded batch is {} bytes, above the {MAX_BODY_BYTES}-byte limit",
            out.len()
        )));
    }
    Ok(Bytes::from(out))
}

/// Encodes a batch's header line alone, for tests and for a sender that writes incrementally.
pub fn encode_header(header: &BatchHeader) -> Result<Bytes> {
    let mut out = Vec::new();
    push_line(&mut out, header)?;
    Ok(Bytes::from(out))
}

/// Appends one JSON value as a line.
fn push_line<T: Serialize>(out: &mut Vec<u8>, value: &T) -> Result<()> {
    serde_json::to_writer(&mut *out, value).map_err(|error| {
        Error::invalid_request(format!("could not encode a batch line: {error}"))
    })?;
    out.push(b'\n');
    Ok(())
}

/// A heartbeat line.
pub fn heartbeat_line() -> Bytes {
    Bytes::from_static(b"{\"heartbeat\":true}\n")
}

/// A heartbeat line carrying `hub_time` as an opaque payload, or a bare one when there is none.
///
/// The payload is copied into the line verbatim and never inspected: this crate does not know,
/// sign or verify what a signed hub time is. A payload above [`MAX_HEARTBEAT_PAYLOAD_BYTES`] is
/// refused here, at the sender: the line is then the bare heartbeat, so the pull keeps working
/// and the value is simply not carried (one `tracing::warn!` per process).
pub fn heartbeat_line_with(hub_time: Option<&serde_json::Value>) -> Bytes {
    let Some(payload) = hub_time else {
        return heartbeat_line();
    };
    if !payload_fits(payload) {
        warn_oversize_once();
        return heartbeat_line();
    }
    let line = HeartbeatWith {
        heartbeat: true,
        hub_time: Some(payload),
    };
    let mut out = Vec::new();
    // A `serde_json::Value` is always encodable; the fallback is here so the pull never fails on
    // a heartbeat.
    if serde_json::to_writer(&mut out, &line).is_err() {
        warn_oversize_once();
        return heartbeat_line();
    }
    out.push(b'\n');
    Bytes::from(out)
}

/// Whether a line is a heartbeat.
pub fn is_heartbeat(line: &str) -> bool {
    serde_json::from_str::<Heartbeat>(line).is_ok_and(|heartbeat| heartbeat.heartbeat)
}

/// The `hub_time` payload of a heartbeat line.
///
/// `None` for a heartbeat that carries none, for a payload above
/// [`MAX_HEARTBEAT_PAYLOAD_BYTES`] (ignored, exactly as the sender refuses to send one), and for
/// a line that is not a heartbeat at all — a record or a batch header that happens to contain a
/// `hub_time` field is never read as a payload.
///
/// The value is whatever the hub put there and is **untrusted**: only the node's snapshot store,
/// after verifying the signature against its pinned keys, may treat it as a hub time.
pub fn heartbeat_payload(line: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let object = value.as_object()?;
    if object.get("heartbeat").and_then(serde_json::Value::as_bool) != Some(true) {
        return None;
    }
    let payload = object.get(HUB_TIME_FIELD)?.clone();
    payload_fits(&payload).then_some(payload)
}

/// Whether a payload is inside the heartbeat's size limit, when serialised.
fn payload_fits(payload: &serde_json::Value) -> bool {
    serde_json::to_vec(payload).is_ok_and(|bytes| bytes.len() <= MAX_HEARTBEAT_PAYLOAD_BYTES)
}

/// Warns that a payload was too large to carry, once per process.
fn warn_oversize_once() {
    if !OVERSIZE_WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            limit = MAX_HEARTBEAT_PAYLOAD_BYTES,
            "a heartbeat payload is above the size limit; the bare heartbeat is sent instead"
        );
    }
}

/// The non-empty lines of a body, in order, as UTF-8 text.
fn lines(body: &[u8]) -> Result<Vec<&str>> {
    let text = std::str::from_utf8(body)
        .map_err(|_| Error::invalid_request("a batch body is UTF-8 JSON lines"))?;
    Ok(text
        .split('\n')
        .filter(|line| !line.trim().is_empty())
        .collect())
}

/// Whether a body carries a batch at all (a non-heartbeat, non-empty line).
///
/// A long-poll that reached its cap with no records answers with heartbeats only, or nothing; the
/// client reads that as "nothing yet" rather than a malformed batch.
pub fn contains_batch(body: &[u8]) -> bool {
    lines(body)
        .map(|lines| lines.into_iter().any(|line| !is_heartbeat(line)))
        .unwrap_or(false)
}

/// Parses a body's first line as a batch header, skipping any leading heartbeat lines.
pub fn decode_header(body: &[u8]) -> Result<BatchHeader> {
    for line in lines(body)? {
        if is_heartbeat(line) {
            continue;
        }
        return serde_json::from_str(line).map_err(|error| {
            Error::invalid_request(format!("a batch header did not parse: {error}"))
        });
    }
    Err(Error::invalid_request("a batch body is empty"))
}

/// Parses a body's record lines (everything after the header) into records.
///
/// The first non-heartbeat line is the header and is skipped; every later non-heartbeat line is
/// a record. Heartbeats may appear anywhere.
pub fn decode_records(body: &[u8]) -> Result<Vec<Record>> {
    let mut header_seen = false;
    let mut records = Vec::new();
    for line in lines(body)? {
        if is_heartbeat(line) {
            continue;
        }
        if !header_seen {
            header_seen = true;
            continue;
        }
        let record: Record = serde_json::from_str(line).map_err(|error| {
            Error::invalid_request(format!("a batch record did not parse: {error}"))
        })?;
        if record.payload.len() > MAX_PAYLOAD_BYTES {
            return Err(Error::batch_too_large(format!(
                "a record payload is {} bytes, above the {MAX_PAYLOAD_BYTES}-byte limit",
                record.payload.len()
            )));
        }
        records.push(record);
    }
    Ok(records)
}

/// Rebuilds a batch from a header and its records, refusing a checksum or range mismatch.
///
/// The records are re-encoded and their checksum recomputed by [`Batch::new`]; a header whose
/// checksum or range does not match the records is refused as `sync_http.corrupt_batch` before any
/// record is applied.
pub fn batch_from_parts(header: &BatchHeader, records: Vec<Record>) -> Result<Batch> {
    let batch = Batch::new(header.source.clone(), records)
        .map_err(|error| Error::corrupt_batch(error.to_string()))?;
    if batch.from_seq != header.from_seq || batch.to_seq != header.to_seq {
        return Err(Error::corrupt_batch(format!(
            "the header claims {}..={} but the records are {}..={}",
            header.from_seq, header.to_seq, batch.from_seq, batch.to_seq
        )));
    }
    if batch.checksum != header.checksum {
        return Err(Error::corrupt_batch(format!(
            "the checksum of {} sequences {}..={} does not match the header",
            header.source, header.from_seq, header.to_seq
        )));
    }
    Ok(batch)
}

/// Parses a whole NDJSON batch body, header and records together.
pub fn decode_batch(body: &[u8]) -> Result<Batch> {
    let header = decode_header(body)?;
    let records = decode_records(body)?;
    batch_from_parts(&header, records)
}

/// Percent-encodes a query value, leaving the unreserved set (`A-Z a-z 0-9 - _ . ~`) alone.
///
/// `SourceId` carries `#`, which must be encoded in a query string.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(&mut out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use spot_sync_protocol::RecordName;

    fn source(partition: &str) -> SourceId {
        SourceId::new(&format!("edge-khi#{partition}")).unwrap()
    }

    fn batch(from: u64, count: u64) -> Batch {
        let records = (0..count)
            .map(|index| {
                let seq = from + index;
                Record::new(
                    RecordName::new(&format!("doc_{seq}")).unwrap(),
                    1,
                    seq,
                    format!("v{seq}"),
                )
                .unwrap()
            })
            .collect();
        Batch::new(source("branch_01"), records).unwrap()
    }

    #[test]
    fn a_batch_round_trips_through_ndjson() {
        let batch = batch(1, 3);
        let body = encode_batch(&batch).unwrap();
        let decoded = decode_batch(&body).unwrap();
        assert_eq!(decoded, batch);
        decoded.verify().unwrap();
    }

    #[test]
    fn the_header_is_readable_before_the_records() {
        let batch = batch(5, 2);
        let body = encode_batch(&batch).unwrap();
        let header = decode_header(&body).unwrap();
        assert_eq!((header.from_seq, header.to_seq), (5, 6));
        assert_eq!(header.partition(), Some("branch_01"));
        // The header check needs no record decode: the first line alone carries it.
        let first_line = body.split(|b| *b == b'\n').next().unwrap();
        assert_eq!(decode_header(first_line).unwrap(), header);
    }

    #[test]
    fn a_tampered_record_fails_the_checksum() {
        let batch = batch(1, 2);
        let body = encode_batch(&batch).unwrap();
        let text = String::from_utf8(body.to_vec()).unwrap();
        let tampered = text.replace("\"v2\"", "\"tampered\"");
        let error = decode_batch(tampered.as_bytes()).unwrap_err();
        assert_eq!(error.code, crate::error::CORRUPT_BATCH);
    }

    #[test]
    fn heartbeats_are_skipped() {
        let batch = batch(1, 1);
        let mut body = heartbeat_line().to_vec();
        body.extend_from_slice(&encode_batch(&batch).unwrap());
        body.extend_from_slice(&heartbeat_line());
        assert_eq!(decode_batch(&body).unwrap(), batch);
        assert!(is_heartbeat("{\"heartbeat\":true}"));
        assert!(!is_heartbeat("{\"heartbeat\":false}"));
    }

    #[test]
    fn a_heartbeat_may_carry_a_payload_and_stays_a_heartbeat() {
        let payload = serde_json::json!({"signed": "value", "n": 7});
        let line = heartbeat_line_with(Some(&payload));
        let text = std::str::from_utf8(&line).unwrap();
        assert_eq!(
            heartbeat_payload(text),
            Some(payload),
            "the payload round-trips through the line"
        );
        // An older reader still skips it: the marker is unchanged and `hub_time` is an unknown
        // field to it.
        assert!(is_heartbeat(text));
        // A bare heartbeat has no payload, and an oversize one is ignored on both sides.
        assert_eq!(heartbeat_payload("{\"heartbeat\":true}"), None);
        let long = serde_json::json!("x".repeat(MAX_HEARTBEAT_PAYLOAD_BYTES + 1));
        assert_eq!(heartbeat_line_with(Some(&long)), heartbeat_line());
        let oversized = format!("{{\"heartbeat\":true,\"hub_time\":{long}}}");
        assert_eq!(heartbeat_payload(&oversized), None);
    }

    #[test]
    fn query_values_are_percent_encoded() {
        assert_eq!(percent_encode("edge-khi#branch_01"), "edge-khi%23branch_01");
        assert_eq!(percent_encode("a.b-c_d"), "a.b-c_d");
    }
}
