//! The wire forms of the two types serde does not know: a record id (the canonical lower-case
//! hyphenated UUID text, as in the events contract) and an instant (RFC 3339, always UTC).

use serde::{Deserialize, Deserializer, Serializer};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// `#[serde(with = "wire::record_id")]`.
pub(crate) mod record_id {
    use super::*;

    pub fn serialize<S: Serializer>(id: &RecordId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&id.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<RecordId, D::Error> {
        let text = String::deserialize(d)?;
        parse(&text).map_err(serde::de::Error::custom)
    }

    /// Parses the canonical form: 36 characters, lower-case hex, hyphens at 8, 13, 18 and 23.
    pub fn parse(text: &str) -> Result<RecordId, String> {
        let bad = || "a record id must be a lower-case hyphenated UUIDv7".to_owned();
        if text.len() != 36 {
            return Err(bad());
        }
        let mut bytes = [0_u8; 16];
        let (mut at, mut high) = (0_usize, None);
        for (i, c) in text.bytes().enumerate() {
            if matches!(i, 8 | 13 | 18 | 23) {
                if c != b'-' {
                    return Err(bad());
                }
                continue;
            }
            let value = match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                _ => return Err(bad()),
            };
            match high.take() {
                None => high = Some(value),
                Some(h) => {
                    if let Some(slot) = bytes.get_mut(at) {
                        *slot = (h << 4) | value;
                    }
                    at += 1;
                }
            }
        }
        RecordId::from_bytes(bytes).map_err(|e| e.to_string())
    }
}

/// `#[serde(with = "wire::opt_record_id")]`.
pub(crate) mod opt_record_id {
    use super::*;

    pub fn serialize<S: Serializer>(id: &Option<RecordId>, s: S) -> Result<S::Ok, S::Error> {
        match id {
            Some(id) => s.serialize_some(&id.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<RecordId>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(text) => record_id::parse(&text)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

/// `#[serde(with = "wire::utc")]`.
pub(crate) mod utc {
    use super::*;

    pub fn serialize<S: Serializer>(t: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
        let text = t
            .to_offset(time::UtcOffset::UTC)
            .format(&Rfc3339)
            .map_err(serde::ser::Error::custom)?;
        s.serialize_str(&text)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<OffsetDateTime, D::Error> {
        let text = String::deserialize(d)?;
        OffsetDateTime::parse(&text, &Rfc3339)
            .map(|t| t.to_offset(time::UtcOffset::UTC))
            .map_err(serde::de::Error::custom)
    }
}

/// `#[serde(with = "wire::opt_utc")]`.
pub(crate) mod opt_utc {
    use super::*;

    pub fn serialize<S: Serializer>(t: &Option<OffsetDateTime>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => utc::serialize(t, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(text) => OffsetDateTime::parse(&text, &Rfc3339)
                .map(|t| Some(t.to_offset(time::UtcOffset::UTC)))
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}
