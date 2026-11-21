//! The tenant recovery key and its printed recovery phrase.
//!
//! The recovery key is 32 random bytes that never leave the customer's possession. It is
//! shown **once**, as a recovery phrase, when the tenant first enables cloud backup; the
//! service never sees it and cannot decrypt a backup without it (zero knowledge). A new
//! PC restores from the phrase alone: no local state, no server-side key.
//!
//! # The phrase
//!
//! The phrase is the recovery key in Crockford base32 — digits and letters without the
//! look-alikes `I`, `L`, `O` and `U` — followed by a two-byte SHA-256 checksum, printed
//! as eleven groups of five symbols (`ABCDE-FGHJK-...`). Parsing ignores case, spaces
//! and dashes and folds the classic transcription mistakes `O` to `0` and `I`/`L` to
//! `1`; a mistyped or truncated phrase fails its checksum instead of opening a different
//! key. [`RecoveryPhrase`]'s `Debug` is redacted: the phrase is a secret and must not
//! reach a log.
//!
//! # Escrow
//!
//! Escrow is **opt-in and explicit**: the customer grants it for one backup at a time by
//! passing a provider they control (a company key server, an offline key). Without that
//! provider the backup is recoverable only from the phrase.

use std::fmt;
use std::sync::Arc;

use spot_secrets::{
    KeyProvider, KeyRef as SecretKeyRef, ProviderFuture, SecretBytes, SecretError, WrappedKey,
};
use zeroize::Zeroizing;

use crate::crypto::{self, KEY_LEN};
use crate::error::{CloudBackupError, Result};

/// The alphabet of the recovery phrase: Crockford base32, 32 symbols without look-alikes.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Groups of this many symbols are separated by a dash when a phrase is printed.
const GROUP: usize = 5;
/// Symbols in a phrase: 34 bytes (key plus checksum) as base32.
const SYMBOLS: usize = 55;
/// The key id every [`RecoveryKeyProvider`] wraps under.
pub const RECOVERY_KEY_ID: &str = "tenant-recovery";

/// A tenant recovery key. Redacted in `Debug` and zeroed on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryKey(Zeroizing<[u8; KEY_LEN]>);

impl RecoveryKey {
    /// A fresh random recovery key from the operating system RNG.
    ///
    /// # Errors
    /// [`CloudBackupError::Store`] when the OS RNG is unavailable.
    pub fn generate() -> Result<Self> {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        crypto::random_bytes(key.as_mut())?;
        Ok(Self(key))
    }

    /// Wraps 32 raw bytes, for example after unwrapping the data key from a provider.
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// The key bytes. Callers must not log or return them.
    pub fn expose(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// The phrase that prints this key once. Treat the result as the secret itself.
    pub fn phrase(&self) -> RecoveryPhrase {
        RecoveryPhrase::from_key(self)
    }

    /// A key provider that wraps data keys under this recovery key.
    pub fn provider(&self) -> RecoveryKeyProvider {
        RecoveryKeyProvider::new(self.clone())
    }
}

impl fmt::Debug for RecoveryKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RecoveryKey([redacted])")
    }
}

/// A recovery phrase: the printed, checksummed form of a [`RecoveryKey`].
///
/// `Debug` prints `[redacted]`. [`RecoveryPhrase::as_str`] is the only way to read the
/// text, which is deliberate: the phrase is shown to the customer once and never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryPhrase {
    key: RecoveryKey,
    canonical: String,
}

impl RecoveryPhrase {
    /// Encodes `key` as a grouped, checksummed phrase.
    pub fn from_key(key: &RecoveryKey) -> Self {
        let mut payload = [0u8; KEY_LEN + 2];
        payload[..KEY_LEN].copy_from_slice(key.expose());
        let checksum = crypto::sha256(key.expose());
        payload[KEY_LEN..].copy_from_slice(&checksum[..2]);
        let encoded = encode_base32(&payload);
        let mut canonical = String::with_capacity(encoded.len() + encoded.len() / GROUP);
        for (index, ch) in encoded.chars().enumerate() {
            if index > 0 && index % GROUP == 0 {
                canonical.push('-');
            }
            canonical.push(ch);
        }
        Self {
            key: key.clone(),
            canonical,
        }
    }

    /// Parses a printed phrase, checking the checksum.
    ///
    /// # Errors
    /// [`CloudBackupError::RecoveryPhrase`] when the phrase has an unknown symbol, the
    /// wrong length or a checksum that does not match; the message never echoes it.
    pub fn parse(text: &str) -> Result<Self> {
        let mut symbols = Vec::with_capacity(SYMBOLS);
        for ch in text.chars() {
            match ch {
                '-' | ' ' | '\t' | '\n' | '\r' => continue,
                'O' | 'o' => symbols.push(0u8),
                'I' | 'i' | 'L' | 'l' => symbols.push(1u8),
                other => {
                    let upper = other.to_ascii_uppercase();
                    let Some(value) = ALPHABET
                        .iter()
                        .position(|candidate| char::from(*candidate) == upper)
                    else {
                        return Err(CloudBackupError::RecoveryPhrase {
                            reason: "it contains a symbol outside the recovery alphabet",
                        });
                    };
                    // The index is < 32 because `position` found it in a 32-byte table.
                    symbols.push(u8::try_from(value).unwrap_or(0));
                }
            }
        }
        if symbols.len() != SYMBOLS {
            return Err(CloudBackupError::RecoveryPhrase {
                reason: "it does not have 55 symbols",
            });
        }
        let payload = decode_base32(&symbols).ok_or(CloudBackupError::RecoveryPhrase {
            reason: "it is not valid base32",
        })?;
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        key.copy_from_slice(&payload[..KEY_LEN]);
        let expected = crypto::sha256(key.as_ref());
        if !crypto::constant_time_eq(&payload[KEY_LEN..], &expected[..2]) {
            return Err(CloudBackupError::RecoveryPhrase {
                reason: "its checksum does not match",
            });
        }
        // Re-encode rather than echoing the input: the canonical text is always the
        // alphabet's own spelling, never the caller's look-alike substitutions.
        Ok(Self::from_key(&RecoveryKey(key)))
    }

    /// The recovery key this phrase encodes.
    pub fn key(&self) -> RecoveryKey {
        self.key.clone()
    }

    /// The printable phrase, in canonical groups.
    pub fn as_str(&self) -> &str {
        &self.canonical
    }
}

impl fmt::Debug for RecoveryPhrase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RecoveryPhrase([redacted])")
    }
}

impl fmt::Display for RecoveryPhrase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[redacted]")
    }
}

/// Encodes bytes as Crockford base32, zero-padded to a whole symbol at the end.
fn encode_base32(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for byte in bytes {
        buffer = (buffer << 8) | u32::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let value = (buffer >> bits) & 0x1f;
            out.push(char::from(ALPHABET[usize::try_from(value).unwrap_or(0)]));
        }
    }
    if bits > 0 {
        let value = (buffer << (5 - bits)) & 0x1f;
        out.push(char::from(ALPHABET[usize::try_from(value).unwrap_or(0)]));
    }
    out
}

/// Decodes [`SYMBOLS`] base32 symbols, rejecting non-zero padding bits.
fn decode_base32(symbols: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(KEY_LEN + 2);
    let mut buffer: u32 = 0;
    let mut bits = 0u32;
    for symbol in symbols {
        if *symbol >= 32 {
            return None;
        }
        buffer = (buffer << 5) | u32::from(*symbol);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).ok()?);
        }
    }
    // 55 symbols carry 275 bits; the payload is 272, so the last three bits must be zero.
    if bits >= 5 || (buffer & ((1 << bits) - 1)) != 0 {
        return None;
    }
    Some(out)
}

/// Wraps per-backup data keys under a tenant recovery key, through the `spot-secrets`
/// envelope. It never exposes the recovery key: only [`KeyProvider`]'s wrap/unwrap.
pub struct RecoveryKeyProvider {
    key: RecoveryKey,
    id: String,
}

impl RecoveryKeyProvider {
    /// A provider for `key`, under the standard recovery key id.
    pub fn new(key: RecoveryKey) -> Self {
        Self {
            key,
            id: RECOVERY_KEY_ID.to_owned(),
        }
    }

    /// The recovery key this provider wraps under.
    pub fn recovery_key(&self) -> &RecoveryKey {
        &self.key
    }
}

impl fmt::Debug for RecoveryKeyProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecoveryKeyProvider")
            .field("key_id", &self.id)
            .finish_non_exhaustive()
    }
}

impl KeyProvider for RecoveryKeyProvider {
    fn current(&self) -> SecretKeyRef {
        SecretKeyRef {
            id: self.id.clone(),
            version: 1,
        }
    }

    fn wrap<'a>(&'a self, data_key: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey> {
        Box::pin(async move {
            if data_key.len() != KEY_LEN {
                return Err(SecretError::Provider(
                    "a backup data key must be 32 bytes".to_owned(),
                ));
            }
            let bytes = crypto::encrypt(self.key.expose(), aad, data_key)
                .ok_or_else(|| SecretError::Provider("the AEAD refused the key wrap".to_owned()))?;
            Ok(WrappedKey {
                key: self.current(),
                bytes,
            })
        })
    }

    fn unwrap<'a>(
        &'a self,
        key: &'a SecretKeyRef,
        wrapped: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move {
            if key.id != self.id || key.version != 1 {
                return Err(SecretError::UnknownKey {
                    id: key.id.clone(),
                    version: key.version,
                });
            }
            let plain = crypto::decrypt(self.key.expose(), aad, &wrapped.bytes)
                .ok_or(SecretError::Tampered)?;
            if plain.len() != KEY_LEN {
                return Err(SecretError::Malformed("data key has the wrong length"));
            }
            Ok(SecretBytes::new(plain.to_vec()))
        })
    }
}

/// Whether a backup is additionally recoverable through an escrow key.
///
/// Escrow is off unless the customer grants it explicitly, for this backup, with a
/// provider they control. The service never holds the escrow key either.
#[derive(Clone)]
pub enum Escrow {
    /// No escrow: the recovery phrase is the only way back.
    None,
    /// The data key is also wrapped under `provider`, under `label`.
    Granted {
        /// The customer's escrow key provider.
        provider: Arc<dyn KeyProvider>,
        /// A short label recorded in the manifest (for example `company-key-2026`).
        label: String,
    },
}

impl Escrow {
    /// Grants escrow under `provider` and `label`.
    ///
    /// # Errors
    /// [`CloudBackupError::ManifestInvalid`] when the label is empty, longer than 64
    /// bytes or contains anything but ASCII letters, digits, `-`, `_` and `.`.
    pub fn granted(provider: Arc<dyn KeyProvider>, label: impl Into<String>) -> Result<Self> {
        let label = label.into();
        if label.is_empty()
            || label.len() > 64
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(CloudBackupError::ManifestInvalid {
                backup_id: String::new(),
                reason: "the escrow label must be 1 to 64 ASCII letters, digits, '-', '_' or '.'"
                    .to_owned(),
            });
        }
        Ok(Self::Granted { provider, label })
    }

    /// The escrow label, when escrow is granted.
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Granted { label, .. } => Some(label),
        }
    }

    /// The escrow provider, when escrow is granted.
    pub fn provider(&self) -> Option<&Arc<dyn KeyProvider>> {
        match self {
            Self::None => None,
            Self::Granted { provider, .. } => Some(provider),
        }
    }
}

impl fmt::Debug for Escrow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => formatter.write_str("Escrow::None"),
            Self::Granted { label, .. } => formatter
                .debug_struct("Escrow::Granted")
                .field("label", label)
                .finish_non_exhaustive(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrase_round_trips_and_is_grouped() {
        let key = RecoveryKey::generate().expect("rng");
        let phrase = key.phrase();
        assert_eq!(phrase.as_str().len(), SYMBOLS + SYMBOLS / GROUP - 1);
        assert_eq!(phrase.as_str().matches('-').count(), SYMBOLS / GROUP - 1);
        let parsed = RecoveryPhrase::parse(phrase.as_str()).expect("parse");
        assert_eq!(parsed.key().expose(), key.expose());
    }

    #[test]
    fn parsing_ignores_case_spaces_dashes_and_look_alikes() {
        let key = RecoveryKey::generate().expect("rng");
        let phrase = key.phrase();
        let lowered = phrase.as_str().to_lowercase().replace('-', " ");
        assert_eq!(
            RecoveryPhrase::parse(&lowered)
                .expect("parse")
                .key()
                .expose(),
            key.expose()
        );
        // `0` typed as `O` and `1` typed as `I` are folded, not rejected.
        let with_o = phrase.as_str().replacen('0', "O", 1);
        let with_i = with_o.replacen('1', "I", 1);
        assert_eq!(
            RecoveryPhrase::parse(&with_i)
                .expect("parse")
                .key()
                .expose(),
            key.expose()
        );
    }

    #[test]
    fn a_changed_symbol_fails_the_checksum() {
        // A fixed key keeps the tampered phrase's checksum outcome deterministic.
        let key = RecoveryKey::from_bytes([7u8; KEY_LEN]);
        let phrase = key.phrase();
        let mut chars: Vec<char> = phrase.as_str().chars().collect();
        let position = chars
            .iter()
            .position(|ch| ALPHABET.contains(&u8::try_from(*ch).unwrap_or(0)))
            .unwrap_or(0);
        chars[position] = if chars[position] == 'Z' { 'Y' } else { 'Z' };
        let changed: String = chars.into_iter().collect();
        let error = RecoveryPhrase::parse(&changed).expect_err("checksum");
        assert_eq!(error.code(), "cloud_backup.recovery_phrase");
    }

    #[test]
    fn truncated_and_unknown_symbols_are_refused() {
        let key = RecoveryKey::generate().expect("rng");
        let phrase = key.phrase();
        let short = &phrase.as_str()[..phrase.as_str().len() - 2];
        assert!(RecoveryPhrase::parse(short).is_err());
        assert!(RecoveryPhrase::parse("!!!!!").is_err());
    }

    #[test]
    fn secrets_are_redacted_in_debug_and_display() {
        let key = RecoveryKey::generate().expect("rng");
        let phrase = key.phrase();
        assert_eq!(format!("{key:?}"), "RecoveryKey([redacted])");
        assert_eq!(format!("{phrase:?}"), "RecoveryPhrase([redacted])");
        assert_eq!(format!("{phrase}"), "[redacted]");
        let provider = key.provider();
        assert!(!format!("{provider:?}").contains(phrase.as_str()));
    }

    #[tokio::test]
    async fn provider_wraps_and_unwraps_under_the_right_aad() {
        let key = RecoveryKey::generate().expect("rng");
        let provider = key.provider();
        let data = [7u8; KEY_LEN];
        let wrapped = provider.wrap(&data, b"aad").await.expect("wrap");
        let opened = provider
            .unwrap(&wrapped.key, &wrapped, b"aad")
            .await
            .expect("unwrap");
        assert_eq!(opened.expose(), &data);
        assert!(
            provider
                .unwrap(&wrapped.key, &wrapped, b"other")
                .await
                .is_err()
        );
    }
}
