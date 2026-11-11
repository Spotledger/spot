//! The opaque, signed export cursor.
//!
//! A [`Cursor`] is a token the client stores and hands back to resume. It carries the plan
//! digest and the principal, so a client cannot reuse it on another plan or another principal,
//! and a keyset position (the order-key values and the record id of the last delivered row),
//! so `resume` continues exactly after it. The whole payload is authenticated with
//! HMAC-SHA-256 under a key the caller supplies ([`CursorKey`]), so a client cannot forge a
//! position.
//!
//! The token is versioned: the first payload byte is the format version, and a token of an
//! unknown version is refused.

use base64::Engine as _;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use spot_storage_contract::{RecordId, Value};

use crate::error::{BulkError, BulkResult};

type HmacSha256 = Hmac<Sha256>;

/// The cursor format version this build writes and reads.
pub const CURSOR_VERSION: u8 = 1;

/// The caller's signing key for cursors. Kept out of logs; the caller owns its lifecycle.
#[derive(Clone)]
pub struct CursorKey(Vec<u8>);

impl CursorKey {
    /// A signing key from raw bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> CursorKey {
        CursorKey(bytes.into())
    }

    /// The bytes (for a caller that stores the key itself).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for CursorKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CursorKey(<redacted>)")
    }
}

/// An opaque, signed position in an export.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    token: String,
}

impl Cursor {
    /// Wraps a token a client handed back (validated by [`Cursor::decode`]).
    #[must_use]
    pub fn from_token(token: impl Into<String>) -> Cursor {
        Cursor {
            token: token.into(),
        }
    }

    /// The token a client stores.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.token
    }

    /// Encodes and signs a position under `key`.
    ///
    /// # Errors
    ///
    /// [`bulk.cursor_invalid`](crate::error::CURSOR_INVALID) when the key is unusable (HMAC
    /// accepts every key length, so this is a guard against a future digest).
    pub fn encode(key: &CursorKey, body: &CursorBody) -> BulkResult<Cursor> {
        let payload = body.encode();
        let mut mac = HmacSha256::new_from_slice(key.as_bytes())
            .map_err(|_| BulkError::cursor_invalid("the cursor key is unusable"))?;
        mac.update(&payload);
        let tag = mac.finalize().into_bytes();
        let mut all = payload;
        all.extend_from_slice(&tag);
        Ok(Cursor {
            token: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(all),
        })
    }

    /// Verifies the signature and decodes the position.
    ///
    /// # Errors
    ///
    /// [`bulk.cursor_invalid`](crate::error::CURSOR_INVALID) when the token is not valid
    /// base64url, is truncated, has a bad signature or an unknown version.
    pub fn decode(&self, key: &CursorKey) -> BulkResult<CursorBody> {
        let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(self.token.as_bytes())
            .map_err(|_| BulkError::cursor_invalid("the cursor is not valid base64url"))?;
        if raw.len() < 32 + 1 {
            return Err(BulkError::cursor_invalid("the cursor is too short"));
        }
        let (payload, tag) = raw.split_at(raw.len() - 32);
        let mut mac = HmacSha256::new_from_slice(key.as_bytes())
            .map_err(|_| BulkError::cursor_invalid("the cursor key is unusable"))?;
        mac.update(payload);
        mac.verify_slice(tag)
            .map_err(|_| BulkError::cursor_invalid("the cursor signature is wrong"))?;
        CursorBody::decode(payload)
    }
}

/// The signed contents of a cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorBody {
    /// The plan the cursor belongs to (refuses cross-plan reuse).
    pub plan_digest: [u8; 32],
    /// The principal the cursor belongs to (refuses cross-principal reuse).
    pub principal: String,
    /// Which part of the export (0 is the parent, then each child table in order).
    pub part: u32,
    /// How many rows have been delivered (progress).
    pub rows: u64,
    /// The keyset position: the order-key values and the record id of the last delivered row.
    pub keys: Vec<Value>,
}

impl CursorBody {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(CURSOR_VERSION);
        out.extend_from_slice(&self.plan_digest);
        put_bytes(&mut out, self.principal.as_bytes());
        out.extend_from_slice(&self.part.to_le_bytes());
        out.extend_from_slice(&self.rows.to_le_bytes());
        out.extend_from_slice(
            &u16::try_from(self.keys.len())
                .unwrap_or(u16::MAX)
                .to_le_bytes(),
        );
        for key in &self.keys {
            encode_value(&mut out, key);
        }
        out
    }

    fn decode(payload: &[u8]) -> BulkResult<CursorBody> {
        let mut cur = Reader::new(payload);
        let version = cur.u8()?;
        if version != CURSOR_VERSION {
            return Err(BulkError::cursor_invalid(format!(
                "the cursor version {version} is not supported"
            )));
        }
        let plan_digest = cur.array32()?;
        let principal = cur.string()?;
        let part = cur.u32()?;
        let rows = cur.u64()?;
        let count = cur.u16()? as usize;
        let mut keys = Vec::with_capacity(count);
        for _ in 0..count {
            keys.push(decode_value(&mut cur)?);
        }
        if !cur.done() {
            return Err(BulkError::cursor_invalid("the cursor has trailing bytes"));
        }
        Ok(CursorBody {
            plan_digest,
            principal,
            part,
            rows,
            keys,
        })
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Value tags.
mod tag {
    pub const NULL: u8 = 0;
    pub const BOOL: u8 = 1;
    pub const INT: u8 = 2;
    pub const DECIMAL: u8 = 3;
    pub const TEXT: u8 = 4;
    pub const DATE: u8 = 5;
    pub const TIME: u8 = 6;
    pub const DATETIME: u8 = 7;
    pub const ID: u8 = 8;
    pub const JSON: u8 = 9;
    pub const BYTES: u8 = 10;
}

fn encode_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        // ADR-087: no field kind produces these yet (WP-MET-05.T1); the exact debug text under
        // the text tag, which is enough for a cursor to compare positions.
        Value::GeoPoint(_) | Value::Range(_) => {
            out.push(tag::TEXT);
            put_bytes(out, format!("{value:?}").as_bytes());
        }
        Value::Null => out.push(tag::NULL),
        Value::Bool(b) => {
            out.push(tag::BOOL);
            out.push(u8::from(*b));
        }
        Value::Int(n) => {
            out.push(tag::INT);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Decimal(d) => {
            out.push(tag::DECIMAL);
            put_bytes(out, d.to_string().as_bytes());
        }
        Value::Text(s) => {
            out.push(tag::TEXT);
            put_bytes(out, s.as_bytes());
        }
        Value::Date(d) => {
            out.push(tag::DATE);
            out.extend_from_slice(&d.year().to_le_bytes());
            out.push(d.month() as u8);
            out.push(d.day());
        }
        Value::Time(t) => {
            out.push(tag::TIME);
            out.push(t.hour());
            out.push(t.minute());
            out.push(t.second());
            out.extend_from_slice(&t.nanosecond().to_le_bytes());
        }
        Value::DateTime(dt) => {
            out.push(tag::DATETIME);
            out.extend_from_slice(&dt.unix_timestamp().to_le_bytes());
            out.extend_from_slice(&dt.nanosecond().to_le_bytes());
        }
        Value::Id(id) => {
            out.push(tag::ID);
            out.extend_from_slice(id.as_bytes());
        }
        Value::Json(j) => {
            out.push(tag::JSON);
            put_bytes(out, j.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(tag::BYTES);
            put_bytes(out, b);
        }
    }
}

fn decode_value(cur: &mut Reader<'_>) -> BulkResult<Value> {
    let tag = cur.u8()?;
    Ok(match tag {
        tag::NULL => Value::Null,
        tag::BOOL => Value::Bool(cur.u8()? != 0),
        tag::INT => Value::Int(cur.i64()?),
        tag::DECIMAL => {
            let text = cur.string()?;
            Value::Decimal(
                rust_decimal::Decimal::from_str_exact(&text)
                    .map_err(|_| BulkError::cursor_invalid("a decimal key does not parse"))?,
            )
        }
        tag::TEXT => Value::Text(cur.string()?),
        tag::DATE => {
            let year = cur.i32()?;
            let month = cur.u8()?;
            let day = cur.u8()?;
            let month = time::Month::try_from(month)
                .map_err(|_| BulkError::cursor_invalid("a date key does not parse"))?;
            Value::Date(
                time::Date::from_calendar_date(year, month, day)
                    .map_err(|_| BulkError::cursor_invalid("a date key does not parse"))?,
            )
        }
        tag::TIME => {
            let hour = cur.u8()?;
            let minute = cur.u8()?;
            let second = cur.u8()?;
            let nanos = cur.u32()?;
            Value::Time(
                time::Time::from_hms_nano(hour, minute, second, nanos)
                    .map_err(|_| BulkError::cursor_invalid("a time key does not parse"))?,
            )
        }
        tag::DATETIME => {
            let seconds = cur.i64()?;
            let nanos = cur.u32()?;
            let instant = time::OffsetDateTime::from_unix_timestamp(seconds)
                .map_err(|_| BulkError::cursor_invalid("an instant key does not parse"))?;
            let instant = instant
                .replace_nanosecond(nanos)
                .map_err(|_| BulkError::cursor_invalid("an instant key does not parse"))?;
            Value::DateTime(instant)
        }
        tag::ID => Value::Id(
            RecordId::from_bytes(cur.array16()?)
                .map_err(|_| BulkError::cursor_invalid("an id key is not a UUIDv7"))?,
        ),
        tag::JSON => Value::Json(cur.string()?),
        tag::BYTES => Value::Bytes(cur.bytes()?.to_vec()),
        other => {
            return Err(BulkError::cursor_invalid(format!(
                "the cursor holds an unknown value tag {other}"
            )));
        }
    })
}

/// A minimal bounds-checked reader over the payload.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, at: 0 }
    }

    fn take(&mut self, n: usize) -> BulkResult<&'a [u8]> {
        let end = self
            .at
            .checked_add(n)
            .ok_or_else(|| BulkError::cursor_invalid("the cursor is malformed"))?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| BulkError::cursor_invalid("the cursor is truncated"))?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> BulkResult<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> BulkResult<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> BulkResult<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> BulkResult<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn i32(&mut self) -> BulkResult<i32> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i64(&mut self) -> BulkResult<i64> {
        let b = self.take(8)?;
        Ok(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn array16(&mut self) -> BulkResult<[u8; 16]> {
        let b = self.take(16)?;
        let mut out = [0u8; 16];
        out.copy_from_slice(b);
        Ok(out)
    }

    fn array32(&mut self) -> BulkResult<[u8; 32]> {
        let b = self.take(32)?;
        let mut out = [0u8; 32];
        out.copy_from_slice(b);
        Ok(out)
    }

    fn bytes(&mut self) -> BulkResult<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn string(&mut self) -> BulkResult<String> {
        let bytes = self.bytes()?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| BulkError::cursor_invalid("a cursor text field is not UTF-8"))
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use rust_decimal::Decimal;
    use time::{Date, Month, OffsetDateTime, Time};

    fn every_value() -> Vec<Value> {
        let instant = OffsetDateTime::from_unix_timestamp(1_769_853_600)
            .unwrap()
            .replace_nanosecond(123_456_000)
            .unwrap();
        vec![
            Value::Null,
            Value::Bool(true),
            Value::Int(-3),
            Value::Decimal(Decimal::from_str_exact("1.50").unwrap()),
            Value::Text("text".into()),
            Value::Date(Date::from_calendar_date(2026, Month::January, 31).unwrap()),
            Value::Time(Time::from_hms_nano(9, 30, 15, 500_000_000).unwrap()),
            Value::DateTime(instant),
            Value::Id(RecordId::new_v7()),
            Value::Json("{\"a\":1}".into()),
            Value::Bytes(vec![1, 2, 3]),
        ]
    }

    #[test]
    fn round_trips_every_value_shape() {
        let key = CursorKey::new(b"key".to_vec());
        let body = CursorBody {
            plan_digest: [7u8; 32],
            principal: "alice".into(),
            part: 2,
            rows: 9,
            keys: every_value(),
        };
        let token = Cursor::encode(&key, &body).unwrap();
        assert_eq!(token.decode(&key).unwrap(), body);
    }

    #[test]
    fn a_forged_or_foreign_cursor_is_refused() {
        let key = CursorKey::new(b"key".to_vec());
        let body = CursorBody {
            plan_digest: [1u8; 32],
            principal: "alice".into(),
            part: 0,
            rows: 0,
            keys: vec![Value::Int(1)],
        };
        let token = Cursor::encode(&key, &body).unwrap();
        // The wrong key.
        assert_eq!(
            token
                .decode(&CursorKey::new(b"other".to_vec()))
                .unwrap_err()
                .code(),
            crate::error::CURSOR_INVALID
        );
        // A tampered token.
        let mut tampered = token.as_str().to_owned();
        tampered.push('A');
        assert!(Cursor::from_token(tampered).decode(&key).is_err());
        // A truncated token.
        let short = &token.as_str()[..4];
        assert!(Cursor::from_token(short).decode(&key).is_err());
    }
}
