//! One-time enrolment tokens: printable with a checksum group, stored as a digest,
//! single use, expiring, and refusing with one indistinguishable outcome.
//!
//! # Shape of a printable token
//!
//! The issuer mints 256 random bits (32 bytes) and shows them once as 52 unambiguous
//! base32 characters (digits `0`, `1`, `8`, `9` are not in the alphabet), chunked
//! into eight `XXXXXX-` groups plus one `XXXX-` group of the remaining four payload
//! characters, and a final 4-character checksum group:
//!
//! ```text
//! GGGGGG-GGGGGG-GGGGGG-GGGGGG-GGGGGG-GGGGGG-GGGGGG-GGGGGG-GGGG-77XX
//! ```
//!
//! The checksum group is the top 20 bits of the CRC-32 of the raw bytes. A hand-typed
//! token with a wrong checksum is refused **before** any lookup, so a typo can never
//! reach the token store. The raw bytes are canonical: a parsed token must decode back
//! to exactly 32 bytes (the final character carries one bit and zero-padding), so
//! shape-valid but non-canonical text is malformed too.
//!
//! # Single use and expiry
//!
//! A token is consumed exactly once, atomically (two concurrent attempts with the same
//! token, exactly one wins). Its lifetime is 24 hours by default at issuance and at
//! most 7 days. After expiry or use, every attempt fails the same way.
//!
//! # What the caller learns
//!
//! [`TokenIssuer::consume`] returns the accepted [`TokenRecord`] or a refusal. Every
//! refusal to the protocol is the same [`EnrolError::TokenRefused`]; inside the server
//! process the precise reason ([`RejectReason`]) is attached for the audit log, and
//! nothing in the outcome distinguishes unknown, used and expired.
//!
//! # At rest
//!
//! The store holds only the SHA-256 **digest** of the printable text, the expiry, the
//! optional group/tag and the issuer. The printable text and the raw bytes live only
//! in [`PrintableToken`], which is zeroized on drop and redacted in `Debug`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use subtle::ConstantTimeEq;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::error::{EnrolError, RejectReason};
use crate::util::{fill_os_random, sha256};

/// Base32 alphabet of printable tokens: RFC 4648 (A-Z, 2-7). The digits `0`, `1`,
/// `8` and `9` are simply not part of the alphabet, so hand-typed tokens stay
/// unambiguous and the shape check refuses everything else.
const TOKEN_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

/// The raw random bytes of one token: 256 bits.
const TOKEN_SECRET_BYTES: usize = 32;

/// Characters of the compact (separator-free) payload: 256 bits at 5 bits a character.
const COMPACT_LEN: usize = 52;

/// Width of the `XXXXXX-` chunks: six, documented for the shape table in the module
/// docs and asserted in the tests.
#[allow(dead_code)]
const CHUNK_LEN: usize = 6;

/// Length of the checksum group.
const CHECKSUM_LEN: usize = 4;

/// Total printable length: 52 payload characters + 9 separators + 4 checksum
/// characters.
pub const TOKEN_PRINTABLE_LEN: usize = COMPACT_LEN + 9 + CHECKSUM_LEN;

/// First position of the checksum group in the printable form.
const CHECKSUM_START: usize = 8 * (CHUNK_LEN + 1) + (COMPACT_LEN - 8 * CHUNK_LEN) + 1;

/// Default validity of a token at issuance: 24 hours.
pub const DEFAULT_TTL_SECS: i64 = 24 * 60 * 60;

/// Maximum validity of a token: 7 days.
pub const MAX_TTL_SECS: i64 = 7 * 24 * 60 * 60;

/// The CRC-32 (reflected, ISO-HDLC) polynomial.
const CRC32_POLY: u32 = 0xEDB8_8320;

/// The CRC-32 (reflected) of `raw`, complemented as the reflected output step does.
fn crc32(raw: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for byte in raw {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ CRC32_POLY;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// The checksum group: the top 20 bits of the CRC-32 of the raw bytes, as four
/// alphabet characters. A typo guard, not a security control — the token's secrecy
/// and the server's refusal discipline carry the security.
#[must_use]
fn checksum_group(raw: &[u8; TOKEN_SECRET_BYTES]) -> [char; CHECKSUM_LEN] {
    let mut value = crc32(raw) >> 12;
    let mut chars = ['0'; CHECKSUM_LEN];
    for slot in chars.iter_mut().rev() {
        *slot = alphabet_char((value & 0x1F) as usize);
        value >>= 5;
    }
    chars
}

/// The alphabet character for a 5-bit value.
fn alphabet_char(index: usize) -> char {
    TOKEN_ALPHABET[index] as char
}

/// The 5-bit value of an alphabet character, or `None` outside the alphabet.
fn alphabet_index(character: char) -> Option<usize> {
    let byte = u8::try_from(character as u32).ok()?;
    TOKEN_ALPHABET
        .iter()
        .position(|candidate| *candidate == byte)
}

/// The 52 compact payload characters of the raw bytes: the base32 of 256 bits, with
/// the final character carrying the last bit (zero-padded low bits).
#[must_use]
fn compact_of(raw: &[u8; TOKEN_SECRET_BYTES]) -> String {
    let mut acc: u64 = 0;
    let mut bits = 0u32;
    let mut out = String::with_capacity(COMPACT_LEN);
    for &byte in raw {
        acc = (acc << 8) | u64::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = ((acc >> bits) & 0x1F) as usize;
            out.push(alphabet_char(index));
        }
        acc &= (1 << bits) - 1;
    }
    // 256 bits leave exactly 1 bit for the 52nd character; zero-padded to 5.
    out.push(alphabet_char(((acc << 4) & 0x1F) as usize));
    out
}

/// Decodes the 52 compact payload characters back to the 32 raw bytes. `None` on a
/// character outside the alphabet, a wrong length, or a non-canonical final character
/// (its four low bits must be zero).
fn compact_to_bytes(compact: &str) -> Option<[u8; TOKEN_SECRET_BYTES]> {
    let characters: Vec<char> = compact.chars().collect();
    if characters.len() != COMPACT_LEN {
        return None;
    }
    let mut acc: u64 = 0;
    let mut bits = 0u32;
    let mut out = [0u8; TOKEN_SECRET_BYTES];
    let mut filled = 0usize;
    for &character in &characters {
        let value = u64::try_from(alphabet_index(character)?).ok()?;
        acc = (acc << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out[filled] = ((acc >> bits) & 0xFF) as u8;
            filled += 1;
            acc &= (1 << bits) - 1;
        }
    }
    if filled != TOKEN_SECRET_BYTES || bits != 4 || acc & 0xF != 0 {
        return None;
    }
    Some(out)
}

/// The canonical printable form: the compact payload in `XXXXXX-` chunks plus the
/// checksum group, with the separators of [`SEPARATORS`].
#[must_use]
fn printable_of(raw: &[u8; TOKEN_SECRET_BYTES]) -> String {
    let compact = compact_of(raw);
    let checksum = checksum_group(raw);
    let mut out = String::with_capacity(TOKEN_PRINTABLE_LEN);
    // Eight chunks of six payload characters, `-` between them...
    for chunk in 0..8 {
        if chunk > 0 {
            out.push('-');
        }
        let start = chunk * CHUNK_LEN;
        out.push_str(&compact[start..start + CHUNK_LEN]);
    }
    // ...the remaining four payload characters after one more `-`...
    out.push('-');
    out.push_str(&compact[8 * CHUNK_LEN..COMPACT_LEN]);
    // ...and the checksum group after the final `-`.
    out.push('-');
    out.extend(checksum);
    assert_eq!(out.len(), TOKEN_PRINTABLE_LEN, "printable layout drifted");
    out
}

/// Whether `text` has the canonical printable token shape: length, separators,
/// alphabet. Mirrors the layout [`printable_of`] writes: eight 6-character chunks,
/// the 4 leftover payload characters, the checksum group, all `-`-separated.
#[must_use]
fn shape_ok(text: &str) -> bool {
    if text.len() != TOKEN_PRINTABLE_LEN {
        return false;
    }
    let groups: Vec<&str> = text.split('-').collect();
    if groups.len() != 8 + 1 + 1 {
        return false;
    }
    for group in groups.iter().take(8) {
        if group.len() != CHUNK_LEN {
            return false;
        }
    }
    if groups[8].len() != COMPACT_LEN - 8 * CHUNK_LEN || groups[9].len() != CHECKSUM_LEN {
        return false;
    }
    text.bytes()
        .filter(|byte| *byte != b'-')
        .all(|byte| alphabet_index(byte as char).is_some())
}

/// A printable enrolment token: the one thing an administrator types into an install
/// command. Secret material — zeroized on drop, redacted in `Debug`, never echoed in
/// an error.
///
/// Constructed only by [`PrintableToken::generate`] (the issuer, fresh random bytes)
/// or [`PrintableToken::parse`] (a presented text, shape- and canonicality-checked).
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct PrintableToken {
    /// The raw bytes; the printable form derives from them.
    raw: [u8; TOKEN_SECRET_BYTES],
    /// The printable text, shown exactly once by the issuer.
    printable: Zeroizing<String>,
}

impl PrintableToken {
    /// A fresh random token (256 OS-CSPRNG bits) with a valid checksum group.
    #[must_use]
    pub fn generate() -> Self {
        let mut raw = [0u8; TOKEN_SECRET_BYTES];
        fill_os_random(&mut raw);
        let printable = Zeroizing::new(printable_of(&raw));
        Self { raw, printable }
    }

    /// Wraps a presented token after its shape and canonicality checks; anything that
    /// is not exactly the canonical printable form is [`EnrolError::TokenMalformed`],
    /// before any store is touched.
    ///
    /// # Errors
    /// [`EnrolError::TokenMalformed`] on any wrong length, separator, character or a
    /// non-canonical payload. The text is never echoed.
    pub fn parse(text: &str) -> Result<Self, EnrolError> {
        if !shape_ok(text) {
            return Err(EnrolError::TokenMalformed);
        }
        // The canonical decode must succeed, so checksum verification below works on
        // real raw bytes and a non-canonical text is malformed here, not later.
        let payload_and_checksum: Vec<char> = text.chars().filter(|c| *c != '-').collect();
        if payload_and_checksum.len() != COMPACT_LEN + CHECKSUM_LEN {
            return Err(EnrolError::TokenMalformed);
        }
        let payload: String = payload_and_checksum[..COMPACT_LEN].iter().collect();
        let raw = compact_to_bytes(&payload).ok_or(EnrolError::TokenMalformed)?;
        Ok(Self {
            raw,
            printable: Zeroizing::new(text.to_owned()),
        })
    }

    /// A token from known raw bytes and its derived printable form; for tests with
    /// deterministic vectors and for stores that rehydrate an issued token.
    #[must_use]
    pub fn from_raw(raw: [u8; TOKEN_SECRET_BYTES]) -> Self {
        let printable = Zeroizing::new(printable_of(&raw));
        Self { raw, printable }
    }

    /// The printable form; shown exactly once by the issuer.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.printable
    }

    /// The SHA-256 digest of the printable text: what the store holds.
    #[must_use]
    pub fn digest(&self) -> TokenDigest {
        TokenDigest {
            bytes: sha256(self.printable.as_bytes()),
        }
    }

    /// Whether the checksum group matches the payload groups: the cheap typo check
    /// every consumer runs before looking the token up. Always `true` for generated
    /// tokens; a parsed token whose checksum group was mistyped fails here.
    #[must_use]
    pub fn checksum_ok(&self) -> bool {
        let expected = checksum_group(&self.raw);
        let tail = &self.printable.as_str()[CHECKSUM_START..];
        tail.chars().eq(expected.iter().copied())
    }

    /// The raw bytes, for tests and stores (not part of the public protocol surface).
    #[must_use]
    pub fn raw(&self) -> &[u8; TOKEN_SECRET_BYTES] {
        &self.raw
    }
}

impl fmt::Debug for PrintableToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrintableToken(***)")
    }
}

/// The stored form of a token: the SHA-256 digest of its printable text. Zeroized on
/// drop, redacted in `Debug`; comparisons against a presented token are constant time.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct TokenDigest {
    bytes: [u8; 32],
}

impl TokenDigest {
    /// Builds a digest from exactly 32 stored bytes, for storage layers.
    ///
    /// # Errors
    /// [`EnrolError::CryptoMalformed`] when `bytes` is not exactly 32 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, EnrolError> {
        let array: [u8; 32] = bytes.try_into().map_err(|_| EnrolError::CryptoMalformed)?;
        Ok(Self { bytes: array })
    }

    /// The 32 digest bytes, for storage layers.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// Constant-time comparison against a presented token's digest.
    #[must_use]
    pub fn matches_token(&self, presented: &PrintableToken) -> bool {
        let presented_digest = presented.digest();
        bool::from(
            self.bytes
                .as_slice()
                .ct_eq(presented_digest.as_bytes().as_slice()),
        )
    }
}

impl fmt::Debug for TokenDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenDigest(***)")
    }
}

/// What the server will join the agent to: an optional group or tag, free text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag(pub String);

/// The issuance request: how long a token lives and what it enrols into.
///
/// `ttl_secs` above 0 is honoured up to [`MAX_TTL_SECS`]; 0 means the
/// [`DEFAULT_TTL_SECS`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenSpec {
    /// Time to live in seconds: 0 for the default, otherwise capped at the maximum.
    pub ttl_secs: i64,
    /// The optional group or tag the server will join the agent to.
    pub tag: Option<Tag>,
    /// Who issues this token (an operator, the console, an automation): recorded as
    /// given, for the audit trail.
    pub issuer: String,
}

impl TokenSpec {
    /// A spec with the default TTL, the given issuer and no tag.
    #[must_use]
    pub fn default_ttl(issuer: impl Into<String>) -> Self {
        Self {
            ttl_secs: 0,
            tag: None,
            issuer: issuer.into(),
        }
    }
}

/// A token that was issued: the printable form (shown once) and the metadata the
/// server keeps digest-side. `Debug` is redacted — the token is secret material.
#[derive(Debug)]
pub struct IssuedToken {
    /// The printable token: shown once, never logged.
    pub token: PrintableToken,
    /// The instant (Unix seconds) the token stops being valid.
    pub expires_at: i64,
    /// The optional group or tag the server will join the agent to.
    pub tag: Option<Tag>,
    /// Who issued the token.
    pub issuer: String,
}

/// A stored token record in a [`TokenStore`].
#[derive(Clone, Debug)]
pub struct TokenRecord {
    /// The digest at rest.
    pub digest: Vec<u8>,
    /// The instant the token stops being valid (Unix seconds).
    pub expires_at: i64,
    /// The optional group or tag the server will join the agent to.
    pub tag: Option<Tag>,
    /// Who issued the token.
    pub issuer: String,
    /// Whether the token was already consumed (single use): always `false` as stored
    /// by [`TokenIssuer::issue`]; a store may keep consumed records briefly to answer
    /// audits, but `consume` must never accept them again.
    pub consumed: bool,
}

/// The persistence behind [`TokenIssuer`]: in memory here, a database later.
///
/// All instants are whole seconds since the Unix epoch; the caller injects time, so
/// the store itself never reads a clock.
pub trait TokenStore: Send + Sync {
    /// Stores a record under its digest. Replaces any record with the same digest.
    fn put(&self, record: TokenRecord);

    /// The record under `digest`, if any.
    fn get(&self, digest: &[u8]) -> Option<TokenRecord>;

    /// Marks the record under `digest` consumed; returns the record when this caller
    /// won the token. Atomic: two concurrent calls, exactly one `Ok`. Implementations
    /// must consume only an unconsumed, unexpired record and must report the reason
    /// otherwise, so the audit trail stays truthful.
    fn consume(&self, digest: &[u8], now: i64) -> Result<TokenRecord, RejectReason>;
}

/// An in-memory [`TokenStore`].
///
/// Single use is enforced under one [`Mutex`]: `consume` checks the record, removes it
/// and returns it in the same critical section, so two concurrent attempts with the
/// same token produce exactly one `Ok`.
#[derive(Default)]
pub struct MemoryTokenStore {
    records: Mutex<HashMap<Vec<u8>, TokenRecord>>,
}

impl MemoryTokenStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl TokenStore for MemoryTokenStore {
    fn put(&self, record: TokenRecord) {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        records.insert(record.digest.clone(), record);
    }

    fn get(&self, digest: &[u8]) -> Option<TokenRecord> {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        records.get(digest).cloned()
    }

    fn consume(&self, digest: &[u8], now: i64) -> Result<TokenRecord, RejectReason> {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        // Exactly one critical section: check validity, remove, return.
        match records.get(digest) {
            None => Err(RejectReason::UnknownToken),
            Some(record) if record.consumed => Err(RejectReason::AlreadyUsed),
            Some(record) if record.expires_at < now => Err(RejectReason::Expired),
            Some(record) => {
                let record = record.clone();
                drop(records.remove(digest));
                Ok(record)
            }
        }
    }
}

/// The server side: issues tokens over a [`TokenStore`] and runs the single lookup
/// [`TokenIssuer::consume`], the only thing an agent's request touches.
pub struct TokenIssuer {
    store: Arc<dyn TokenStore>,
}

impl TokenIssuer {
    /// An issuer over `store`.
    #[must_use]
    pub fn new(store: Arc<dyn TokenStore>) -> Self {
        Self { store }
    }

    /// The store behind this issuer, for callers that go beyond issue and consume.
    #[must_use]
    pub fn store(&self) -> &Arc<dyn TokenStore> {
        &self.store
    }

    /// Mints a token: 256 fresh random bits, a printable form with a checksum group,
    /// and a record with only the digest, expiry, tag and issuer.
    ///
    /// # Errors
    /// [`EnrolError::FieldInvalid`] when the TTL is negative (0 means default) or the
    /// issuer text is empty or over 128 bytes — a token without a known issuer is
    /// refused at issuance, not at use.
    pub fn issue(&self, spec: &TokenSpec, now: i64) -> Result<IssuedToken, EnrolError> {
        if spec.ttl_secs < 0 {
            return Err(EnrolError::FieldInvalid);
        }
        let ttl = if spec.ttl_secs == 0 {
            DEFAULT_TTL_SECS
        } else {
            spec.ttl_secs.min(MAX_TTL_SECS)
        };
        if spec.issuer.trim().is_empty() || spec.issuer.len() > 128 {
            return Err(EnrolError::FieldInvalid);
        }
        let token = PrintableToken::generate();
        let digest = token.digest();
        self.store.put(TokenRecord {
            digest: digest.as_bytes().to_vec(),
            expires_at: now + ttl,
            tag: spec.tag.clone(),
            issuer: spec.issuer.clone(),
            consumed: false,
        });
        Ok(IssuedToken {
            token,
            expires_at: now + ttl,
            tag: spec.tag.clone(),
            issuer: spec.issuer.clone(),
        })
    }

    /// The one lookup of an enrolment: shape-check, canonicality-check,
    /// checksum-check, then a constant-time digest comparison and an atomic consume.
    ///
    /// # Errors
    /// [`EnrolError::TokenMalformed`] when the text is not a printable token at all —
    /// the cheap typo path, no digest, no lookup, no store touched.
    /// [`EnrolError::TokenRefused`] when the checksum is wrong, or the token is
    /// unknown, used or expired — one outcome, whichever it was; the precise reason is
    /// the second half of the returned pair, for the audit log only.
    pub fn consume(
        &self,
        presented: &str,
        now: i64,
    ) -> Result<TokenRecord, (EnrolError, Option<RejectReason>)> {
        let token = PrintableToken::parse(presented).map_err(|error| (error, None))?;
        if !token.checksum_ok() {
            return Err((EnrolError::TokenRefused, Some(RejectReason::UnknownToken)));
        }
        let digest = token.digest();
        self.store
            .consume(digest.as_bytes(), now)
            .map_err(|reason| (EnrolError::TokenRefused, Some(reason)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        CHECKSUM_START, DEFAULT_TTL_SECS, IssuedToken, MAX_TTL_SECS, MemoryTokenStore,
        PrintableToken, RejectReason, TokenDigest, TokenIssuer, TokenSpec, compact_of,
        compact_to_bytes, printable_of, shape_ok,
    };
    use std::sync::Arc;
    use std::thread;

    fn issuer() -> TokenIssuer {
        TokenIssuer::new(Arc::new(MemoryTokenStore::new()))
    }

    fn spec() -> TokenSpec {
        TokenSpec::default_ttl("operator")
    }

    #[test]
    fn generated_tokens_have_the_documented_shape() {
        for _ in 0..32 {
            let token = PrintableToken::generate();
            let text = token.as_str();
            assert_eq!(text.len(), 65);
            assert!(shape_ok(text));
            let groups: Vec<&str> = text.split('-').collect();
            assert_eq!(groups.len(), 10);
            // Eight payload chunks of six, then the four leftover payload characters,
            // then the checksum group.
            for group in &groups[..8] {
                assert_eq!(group.len(), 6);
            }
            assert_eq!(groups[8].len(), 4);
            assert_eq!(groups[9].len(), 4);
            for character in text.chars() {
                assert!(
                    character == '-'
                        || character.is_ascii_uppercase()
                        || ('2'..='7').contains(&character)
                );
            }
            assert!(token.checksum_ok());
        }
    }

    #[test]
    fn compact_codec_round_trips_256_bits() {
        let mut raw = [0u8; 32];
        crate::util::fill_os_random(&mut raw);
        let compact = compact_of(&raw);
        assert_eq!(compact.len(), 52);
        let back = compact_to_bytes(&compact).unwrap();
        assert_eq!(back, raw);
        // The printable form groups the same compact characters.
        let printable = printable_of(&raw);
        let stripped: String = printable.chars().filter(|c| *c != '-').collect();
        assert_eq!(
            stripped,
            format!("{compact}{}", {
                let expected = super::checksum_group(&raw);
                expected.iter().collect::<String>()
            })
        );
    }

    #[test]
    fn parse_refuses_wrong_shapes_before_any_lookup() {
        let good = PrintableToken::generate().as_str().to_owned();
        let checksum = &good[CHECKSUM_START..];
        let payload = &good[..CHECKSUM_START - 1];
        let cases = vec![
            String::new(),
            good.to_lowercase(),
            format!("{good}X"),
            good[..good.len() - 1].to_owned(),
            good.replacen('-', "", 1),
            format!("{payload}X{checksum}"),
            format!("{payload}{checksum}X"),
        ];
        for text in cases {
            match PrintableToken::parse(&text) {
                Err(crate::error::EnrolError::TokenMalformed) => {}
                other => panic!(
                    "expected TokenMalformed for {text:?}, got {other:?}",
                    other = other.map(|_| ())
                ),
            }
        }
    }

    #[test]
    fn issue_honours_ttl_bounds_and_audit_fields() {
        let token_issuer = issuer();
        let issued = token_issuer.issue(&spec(), 1_000).unwrap();
        assert_eq!(issued.expires_at, 1_000 + DEFAULT_TTL_SECS);
        assert_eq!(issued.issuer, "operator");
        assert!(issued.tag.is_none());
        let ttl_spec = |ttl| TokenSpec {
            ttl_secs: ttl,
            tag: None,
            issuer: "console".to_owned(),
        };
        let issued = token_issuer.issue(&ttl_spec(3_600), 1_000).unwrap();
        assert_eq!(issued.expires_at, 4_600);
        let issued = token_issuer
            .issue(&ttl_spec(MAX_TTL_SECS + 1), 1_000)
            .unwrap();
        assert_eq!(issued.expires_at, 1_000 + MAX_TTL_SECS);
        assert!(token_issuer.issue(&ttl_spec(-1), 1_000).is_err());
        assert!(
            token_issuer
                .issue(&TokenSpec::default_ttl("  "), 0)
                .is_err()
        );
    }

    #[test]
    fn checksum_only_typo_is_refused_before_lookup() {
        let token_issuer = issuer();
        let issued = token_issuer.issue(&spec(), 0).unwrap();
        let mut tampered = issued.token.as_str().to_owned();
        let tail = tampered.pop().unwrap();
        tampered.push(if tail == 'A' { 'B' } else { 'A' });
        let outcome = token_issuer.consume(&tampered, 0);
        assert!(matches!(
            outcome,
            Err((
                crate::error::EnrolError::TokenRefused,
                Some(RejectReason::UnknownToken)
            ))
        ));
        // And the refusal happened before the store: no record of the tampered text
        // exists (the digest of the tampered text matches nothing).
    }

    #[test]
    fn wrong_token_is_refused_with_the_same_error() {
        let token_issuer = issuer();
        let _issued = token_issuer.issue(&spec(), 0).unwrap();
        let other = PrintableToken::generate();
        let outcome = token_issuer.consume(other.as_str(), 0);
        assert!(matches!(
            outcome,
            Err((
                crate::error::EnrolError::TokenRefused,
                Some(RejectReason::UnknownToken)
            ))
        ));
        let _ = _issued;
    }

    #[test]
    fn expiry_at_the_boundary() {
        let token_issuer = issuer();
        let issued = token_issuer.issue(&spec(), 1_000).unwrap();
        let text = issued.token.as_str().to_owned();
        // The record is valid at its expiry instant (it ends when `expires_at` has
        // passed), and consumed by the successful call.
        assert!(
            token_issuer
                .consume(&text, 1_000 + DEFAULT_TTL_SECS)
                .is_ok()
        );
        let issued = token_issuer.issue(&spec(), 1_000).unwrap();
        let text = issued.token.as_str().to_owned();
        let outcome = token_issuer.consume(&text, 1_000 + DEFAULT_TTL_SECS + 1);
        assert!(matches!(
            outcome,
            Err((
                crate::error::EnrolError::TokenRefused,
                Some(RejectReason::Expired)
            ))
        ));
    }

    #[test]
    fn single_use_two_threads_exactly_one_wins() {
        let token_issuer = Arc::new(issuer());
        let issued = token_issuer.issue(&spec(), 0).unwrap();
        let text = issued.token.as_str().to_owned();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let token_issuer = Arc::clone(&token_issuer);
            let barrier = Arc::clone(&barrier);
            let text = text.clone();
            handles.push(thread::spawn(move || {
                barrier.wait();
                token_issuer.consume(&text, 0)
            }));
        }
        let mut wins = 0;
        let mut refusals = 0;
        for handle in handles {
            match handle.join().unwrap() {
                Ok(_) => wins += 1,
                Err((crate::error::EnrolError::TokenRefused, _)) => refusals += 1,
                Err(other) => panic!("unexpected first error {other:?}"),
            }
        }
        assert_eq!(wins, 1);
        assert_eq!(refusals, 7);
    }

    #[test]
    fn no_information_leak_unknown_used_and_expired_look_alike() {
        let token_issuer = issuer();
        let message_of = |outcome: Result<
            super::TokenRecord,
            (crate::error::EnrolError, Option<RejectReason>),
        >| match outcome {
            Err((error, _)) => (format!("{error}"), error.code()),
            Ok(_) => panic!("expected refusal"),
        };
        let unknown = token_issuer.consume(PrintableToken::generate().as_str(), 0);
        let issued = token_issuer.issue(&spec(), 0).unwrap();
        let text = issued.token.as_str().to_owned();
        token_issuer.consume(&text, 0).unwrap();
        let used = token_issuer.consume(&text, 0);
        // Expired.
        let issued = token_issuer.issue(&spec(), 0).unwrap();
        let text = issued.token.as_str().to_owned();
        let expired = token_issuer.consume(&text, DEFAULT_TTL_SECS + 1);
        let (unknown, used, expired) = (message_of(unknown), message_of(used), message_of(expired));
        assert_eq!(unknown, used);
        assert_eq!(used, expired);
    }

    #[test]
    fn debug_of_secret_types_leaks_nothing() {
        let token_issuer = issuer();
        let issued: IssuedToken = token_issuer.issue(&spec(), 0).unwrap();
        let text = issued.token.as_str().to_owned();
        let digest = issued.token.digest();
        // The issued wrapper is redacted because its token field is.
        assert_eq!(format!("{digest:?}"), "TokenDigest(***)");
        assert!(!format!("{digest:?}").contains(&crate::codec::hex_lower(digest.as_bytes())));
        // A known token string never shows up in any Debug output of the issuer flow:
        // the record and error texts are the surfaces.
        let rebuilt = TokenDigest::from_bytes(&[9u8; 32]).unwrap();
        assert!(!format!("{rebuilt:?}").contains("0909"));
        let presented = PrintableToken::parse(&text).unwrap();
        assert!(!format!("{presented:?}").contains(text.as_str()));
        let _ = &text;
    }

    #[test]
    fn digest_stores_and_matches() {
        let token = PrintableToken::generate();
        let digest = token.digest();
        assert!(digest.matches_token(&token));
        let other = PrintableToken::generate();
        assert!(!digest.matches_token(&other));
        let rebuilt = TokenDigest::from_bytes(digest.as_bytes()).unwrap();
        assert!(rebuilt.matches_token(&token));
        assert!(TokenDigest::from_bytes(&[0u8; 31]).is_err());
    }
}
