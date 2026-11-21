//! The fresh revocation list (domain `SPOT-REVOCATION-LIST-V2\n`): the store's revocation list
//! with `next_update` inside the signed body, the key behind a port, and one wire codec and one
//! verifier shared by the store server and the site client.
//!
//! This is additive: [`crate::revocation::RevocationBody`] and its V1 domain are unchanged.
//!
//! - **Key custody.** The issuer is the store's *revocation-issuing key*; it signs only
//!   revocation lists (and licences elsewhere). It is reached through [`RevocationSigner`], so a
//!   deployment may hold it in hardware.
//! - **Freshness.** A list is valid from `issued_at` until `next_update`, at most
//!   [`MAX_LIST_LIFETIME_SECONDS`] (7 days) later. [`FreshRevocationList::is_stale`] drives the
//!   fail-closed install rule; [`verify_fresh_revocation_list`] refuses a stale list.
//! - **Wire form.** [`FreshRevocationList::to_wire_json`] and
//!   [`FreshRevocationList::from_wire_json`] speak the JSON of `contracts/store-api`: times as
//!   RFC 3339 UTC with `Z`, the bundle digest as `digest`, the signature as base64. Parsing is
//!   strict, and the signed bytes are always rebuilt from the parsed value, never from the
//!   received text.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::anchor::TrustAnchor;
use crate::canonical::{self, FRESH_REVOCATION_DOMAIN, b64, unb64};
use crate::error::SigningError;
use crate::keys::{KeyId, KeyPair, SIGNATURE_BYTES};
use crate::revocation::{RevokedBundle, RevokedKey, RevokedPublisher};

/// The longest a list may be valid: `next_update - issued_at` is at most 7 days.
pub const MAX_LIST_LIFETIME_SECONDS: i64 = 7 * 24 * 60 * 60;
/// The largest `generation`: 2^53, the largest integer every JSON implementation reads exactly.
pub const MAX_GENERATION: u64 = 1 << 53;
/// The largest wire document this crate parses (8 MiB).
pub const MAX_WIRE_BYTES: usize = 8 * 1024 * 1024;
/// The longest `reason`, in bytes.
pub const MAX_REASON_BYTES: usize = 2000;
/// The longest `publisher_id`, in bytes.
pub const MAX_PUBLISHER_ID_BYTES: usize = 128;

const DOC: &str = "revocation list";
/// 0000-01-01T00:00:00Z.
const MIN_SECONDS: i64 = -62_167_219_200;
/// 9999-12-31T23:59:59Z.
const MAX_SECONDS: i64 = 253_402_300_799;

fn malformed(detail: impl Into<String>) -> SigningError {
    SigningError::DocumentMalformed {
        document: DOC,
        detail: detail.into(),
    }
}

/// Where the store's revocation-issuing key lives. The crate never sees the secret: a
/// deployment may implement this over a key file, a service or a hardware module.
pub trait RevocationSigner {
    /// The id of the key that signs; it must equal the list's `issued_by`.
    fn key_id(&self) -> &KeyId;
    /// Signs `message` (the domain-separated bytes of a list) with Ed25519.
    fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_BYTES];
}

impl RevocationSigner for KeyPair {
    fn key_id(&self) -> &KeyId {
        KeyPair::id(self)
    }

    fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_BYTES] {
        KeyPair::sign(self, message)
    }
}

/// The unsigned body of a fresh revocation list. Times are UTC seconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreshRevocationBody {
    /// Strictly increasing per issued list, at most [`MAX_GENERATION`].
    pub generation: u64,
    /// When the list was issued.
    pub issued_at: i64,
    /// The first instant the list is stale; after `issued_at`, at most 7 days later.
    pub next_update: i64,
    /// The key that signs it.
    pub issued_by: KeyId,
    /// Revoked keys, sorted by key id.
    pub keys: Vec<RevokedKey>,
    /// Revoked publishers, sorted by publisher id.
    pub publishers: Vec<RevokedPublisher>,
    /// Revoked bundle digests, sorted.
    pub bundles: Vec<RevokedBundle>,
}

fn in_range(label: &str, seconds: i64) -> Result<(), SigningError> {
    if (MIN_SECONDS..=MAX_SECONDS).contains(&seconds) {
        Ok(())
    } else {
        Err(malformed(format!("{label} is outside years 0000 to 9999")))
    }
}

fn reason_ok(reason: &str) -> Result<(), SigningError> {
    if reason.len() > MAX_REASON_BYTES {
        return Err(malformed(format!(
            "a reason is longer than {MAX_REASON_BYTES} bytes"
        )));
    }
    Ok(())
}

impl FreshRevocationBody {
    /// Checks everything the format requires: ranges, the lifetime rule and field shapes.
    ///
    /// # Errors
    /// [`SigningError::RevocationLifetimeInvalid`] when `next_update` is not after `issued_at`
    /// or is more than 7 days later; [`SigningError::DocumentMalformed`] for any other
    /// field out of range or in the wrong shape.
    pub fn validate(&self) -> Result<(), SigningError> {
        if self.generation > MAX_GENERATION {
            return Err(malformed("generation is above 2^53"));
        }
        in_range("issued_at", self.issued_at)?;
        in_range("next_update", self.next_update)?;
        if self.next_update <= self.issued_at
            || self.next_update - self.issued_at > MAX_LIST_LIFETIME_SECONDS
        {
            return Err(SigningError::RevocationLifetimeInvalid);
        }
        for key in &self.keys {
            in_range("a key's effective_from", key.effective_from)?;
            reason_ok(&key.reason)?;
        }
        for publisher in &self.publishers {
            in_range("a publisher's effective_from", publisher.effective_from)?;
            reason_ok(&publisher.reason)?;
            let id = &publisher.publisher_id;
            let shape = !id.is_empty()
                && id.len() <= MAX_PUBLISHER_ID_BYTES
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
            if !shape {
                return Err(malformed(
                    "a publisher_id is not 1 to 128 of A-Z a-z 0-9 _ -",
                ));
            }
        }
        for bundle in &self.bundles {
            reason_ok(&bundle.reason)?;
            let hex = &bundle.digest_hex;
            let shape = hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
            if !shape {
                return Err(malformed("a digest is not 64 lower-case hex characters"));
            }
        }
        Ok(())
    }

    /// The domain-separated canonical bytes the issuer signs (domain `V2`): the same objects as
    /// the V1 bytes plus `next_update`, every object's keys in lexicographic order, times as
    /// integer UTC seconds, a bundle's digest under `digest`.
    #[must_use]
    pub fn signed_bytes(&self) -> Vec<u8> {
        let keys = self
            .keys
            .iter()
            .map(|k| {
                let mut o = BTreeMap::new();
                o.insert("compromised", k.compromised.into());
                o.insert("effective_from", k.effective_from.into());
                o.insert("key_id", k.key_id.as_str().into());
                o.insert("reason", k.reason.as_str().into());
                canonical::object(o)
            })
            .collect::<Vec<_>>();
        let publishers = self
            .publishers
            .iter()
            .map(|p| {
                let mut o = BTreeMap::new();
                o.insert("effective_from", p.effective_from.into());
                o.insert("publisher_id", p.publisher_id.as_str().into());
                o.insert("reason", p.reason.as_str().into());
                canonical::object(o)
            })
            .collect::<Vec<_>>();
        let bundles = self
            .bundles
            .iter()
            .map(|b| {
                let mut o = BTreeMap::new();
                o.insert("digest", b.digest_hex.as_str().into());
                o.insert("reason", b.reason.as_str().into());
                canonical::object(o)
            })
            .collect::<Vec<_>>();
        let mut fields = BTreeMap::new();
        fields.insert("bundles", bundles.into());
        fields.insert("generation", self.generation.into());
        fields.insert("issued_at", self.issued_at.into());
        fields.insert("issued_by", self.issued_by.as_str().into());
        fields.insert("keys", keys.into());
        fields.insert("next_update", self.next_update.into());
        fields.insert("publishers", publishers.into());
        canonical::encode(FRESH_REVOCATION_DOMAIN, &fields)
    }
}

/// A fresh revocation list with the issuer's signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FreshRevocationList {
    /// The body.
    pub body: FreshRevocationBody,
    signature: [u8; SIGNATURE_BYTES],
}

impl FreshRevocationList {
    /// The signature bytes.
    #[must_use]
    pub fn signature(&self) -> &[u8; SIGNATURE_BYTES] {
        &self.signature
    }

    /// Assembles a list from its parts without checking anything. Only
    /// [`verify_fresh_revocation_list`] checks it.
    #[must_use]
    pub fn from_parts(body: FreshRevocationBody, signature: [u8; SIGNATURE_BYTES]) -> Self {
        Self { body, signature }
    }

    /// Whether the list is stale at `now`: `next_update` has been reached. A stale list never
    /// permits a new install or update; already-installed bundles keep running.
    #[must_use]
    pub fn is_stale(&self, now: i64) -> bool {
        self.body.next_update <= now
    }
}

/// Issues a fresh revocation list. Entries are sorted here, so equal content always signs the
/// same bytes.
///
/// # Errors
/// [`SigningError::KeyMismatch`] when `signer` is not the key named in `body.issued_by`;
/// whatever [`FreshRevocationBody::validate`] refuses, including a lifetime above 7 days.
pub fn issue_fresh_revocation_list(
    mut body: FreshRevocationBody,
    signer: &dyn RevocationSigner,
) -> Result<FreshRevocationList, SigningError> {
    if signer.key_id() != &body.issued_by {
        return Err(SigningError::KeyMismatch);
    }
    body.validate()?;
    body.keys.sort_by(|a, b| a.key_id.cmp(&b.key_id));
    body.publishers
        .sort_by(|a, b| a.publisher_id.cmp(&b.publisher_id));
    body.bundles.sort_by(|a, b| a.digest_hex.cmp(&b.digest_hex));
    let signature = signer.sign(&body.signed_bytes());
    Ok(FreshRevocationList { body, signature })
}

/// Verifies a fresh list as a site does before using it, failing closed.
///
/// In order: the issuer is a root of `anchor` and valid at `now`; the signature verifies over
/// bytes rebuilt from `list.body`; the body is well formed and its lifetime is within the cap;
/// `generation` is not lower than `held_generation` (replay); the list is not stale at `now`.
/// An equal generation is accepted.
///
/// # Errors
/// [`SigningError::UnknownRoot`], [`SigningError::RootOutsideWindow`],
/// [`SigningError::RevocationSignatureInvalid`], [`SigningError::RevocationLifetimeInvalid`] or
/// [`SigningError::DocumentMalformed`], [`SigningError::StaleRevocationList`] (a replay) and
/// [`SigningError::RevocationListExpired`].
pub fn verify_fresh_revocation_list(
    list: &FreshRevocationList,
    anchor: &TrustAnchor,
    now: i64,
    held_generation: Option<u64>,
) -> Result<(), SigningError> {
    let root = anchor
        .root(&list.body.issued_by)
        .ok_or(SigningError::UnknownRoot)?;
    if !root.valid_at(now) {
        return Err(SigningError::RootOutsideWindow);
    }
    root.public()
        .verify(&list.body.signed_bytes(), &list.signature)
        .map_err(|_| SigningError::RevocationSignatureInvalid)?;
    list.body.validate()?;
    if held_generation.is_some_and(|held| list.body.generation < held) {
        return Err(SigningError::StaleRevocationList);
    }
    if list.is_stale(now) {
        return Err(SigningError::RevocationListExpired);
    }
    Ok(())
}

// --- RFC 3339 UTC, by hand ---------------------------------------------------------------------

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn is_leap(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// Formats UTC seconds as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// # Errors
/// [`SigningError::DocumentMalformed`] outside years 0000 to 9999.
pub fn format_rfc3339(seconds: i64) -> Result<String, SigningError> {
    in_range("a time", seconds)?;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let rem = seconds.rem_euclid(86_400);
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    ))
}

/// Parses exactly `YYYY-MM-DDTHH:MM:SSZ` (upper-case `T` and `Z`, no offset, no fraction, no
/// leap second) into UTC seconds.
///
/// # Errors
/// [`SigningError::DocumentMalformed`] for any other text or an impossible date.
pub fn parse_rfc3339(text: &str) -> Result<i64, SigningError> {
    let bytes = text.as_bytes();
    let bad = || malformed("a time is not YYYY-MM-DDTHH:MM:SSZ in UTC");
    if bytes.len() != 20 {
        return Err(bad());
    }
    for (index, expected) in [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'Z'),
    ] {
        if bytes.get(index) != Some(&expected) {
            return Err(bad());
        }
    }
    let number = |from: usize, to: usize| -> Result<i64, SigningError> {
        let digits = bytes.get(from..to).ok_or_else(bad)?;
        if !digits.iter().all(u8::is_ascii_digit) {
            return Err(bad());
        }
        let mut value = 0i64;
        for digit in digits {
            value = value * 10 + i64::from(digit - b'0');
        }
        Ok(value)
    };
    let (year, month, day) = (number(0, 4)?, number(5, 7)?, number(8, 10)?);
    let (hour, minute, second) = (number(11, 13)?, number(14, 16)?, number(17, 19)?);
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(bad());
    }
    Ok(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

// --- the wire form -----------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireKey {
    key_id: String,
    effective_from: String,
    compromised: bool,
    reason: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WirePublisher {
    publisher_id: String,
    effective_from: String,
    reason: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireBundle {
    digest: String,
    reason: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireList {
    generation: u64,
    issued_at: String,
    next_update: String,
    issued_by: String,
    signature: String,
    keys: Vec<WireKey>,
    publishers: Vec<WirePublisher>,
    bundles: Vec<WireBundle>,
}

impl FreshRevocationList {
    /// The list as the JSON object the store API carries in `data`: compact, `digest` for a
    /// bundle's digest, RFC 3339 UTC times, base64 signature.
    ///
    /// # Errors
    /// [`SigningError::DocumentMalformed`] when the body fails [`FreshRevocationBody::validate`]
    /// (for example a list assembled with [`FreshRevocationList::from_parts`]).
    pub fn to_wire_json(&self) -> Result<String, SigningError> {
        let b = &self.body;
        b.validate()?;
        let keys = b
            .keys
            .iter()
            .map(|k| {
                Ok(WireKey {
                    key_id: k.key_id.as_str().to_owned(),
                    effective_from: format_rfc3339(k.effective_from)?,
                    compromised: k.compromised,
                    reason: k.reason.clone(),
                })
            })
            .collect::<Result<Vec<_>, SigningError>>()?;
        let publishers = b
            .publishers
            .iter()
            .map(|p| {
                Ok(WirePublisher {
                    publisher_id: p.publisher_id.clone(),
                    effective_from: format_rfc3339(p.effective_from)?,
                    reason: p.reason.clone(),
                })
            })
            .collect::<Result<Vec<_>, SigningError>>()?;
        let wire = WireList {
            generation: b.generation,
            issued_at: format_rfc3339(b.issued_at)?,
            next_update: format_rfc3339(b.next_update)?,
            issued_by: b.issued_by.as_str().to_owned(),
            signature: b64(&self.signature),
            keys,
            publishers,
            bundles: b
                .bundles
                .iter()
                .map(|x| WireBundle {
                    digest: x.digest_hex.clone(),
                    reason: x.reason.clone(),
                })
                .collect(),
        };
        serde_json::to_string(&wire).map_err(|e| malformed(e.to_string()))
    }

    /// Parses the wire JSON strictly: unknown fields, duplicate keys, non-`Z` offsets,
    /// fractional seconds, out-of-range dates, trailing text and documents over
    /// [`MAX_WIRE_BYTES`] are refused. The result is not yet trusted; call
    /// [`verify_fresh_revocation_list`], which rebuilds the signed bytes from this value.
    ///
    /// # Errors
    /// [`SigningError::DocumentMalformed`], [`SigningError::SignatureMalformed`] or
    /// [`SigningError::RevocationLifetimeInvalid`].
    pub fn from_wire_json(bytes: &[u8]) -> Result<Self, SigningError> {
        if bytes.len() > MAX_WIRE_BYTES {
            return Err(malformed("the document is over 8 MiB"));
        }
        let wire: WireList = serde_json::from_slice(bytes).map_err(|e| malformed(e.to_string()))?;
        let signature = unb64(&wire.signature)
            .and_then(|raw| <[u8; SIGNATURE_BYTES]>::try_from(raw.as_slice()).ok())
            .ok_or(SigningError::SignatureMalformed)?;
        let keys = wire
            .keys
            .into_iter()
            .map(|k| {
                Ok(RevokedKey {
                    key_id: KeyId::parse(&k.key_id)?,
                    effective_from: parse_rfc3339(&k.effective_from)?,
                    compromised: k.compromised,
                    reason: k.reason,
                })
            })
            .collect::<Result<Vec<_>, SigningError>>()?;
        let publishers = wire
            .publishers
            .into_iter()
            .map(|p| {
                Ok(RevokedPublisher {
                    publisher_id: p.publisher_id,
                    effective_from: parse_rfc3339(&p.effective_from)?,
                    reason: p.reason,
                })
            })
            .collect::<Result<Vec<_>, SigningError>>()?;
        let body = FreshRevocationBody {
            generation: wire.generation,
            issued_at: parse_rfc3339(&wire.issued_at)?,
            next_update: parse_rfc3339(&wire.next_update)?,
            issued_by: KeyId::parse(&wire.issued_by)?,
            keys,
            publishers,
            bundles: wire
                .bundles
                .into_iter()
                .map(|x| RevokedBundle {
                    digest_hex: x.digest,
                    reason: x.reason,
                })
                .collect(),
        };
        body.validate()?;
        Ok(Self { body, signature })
    }
}
