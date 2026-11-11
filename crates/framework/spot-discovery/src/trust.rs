//! The trust model behind "TLS optional on LAN" (pure; no TLS stack here).
//!
//! Three TLS modes exist (`TlsMode` in [`crate::record`]); the client-side state a
//! connection carries is [`TlsModeState`]:
//!
//! - `Provided` → `Secured`: a system-trusted certificate; nothing to pair.
//! - `SelfSigned` → `Verified` only after **pairing**: the server shows a short
//!   pairing code derived from its key fingerprint and a one-time nonce; the client
//!   enters it; both compute an HMAC-SHA-256 confirmation MAC over the fingerprint so
//!   a man in the middle cannot present another key undetected; on success the client
//!   stores [`TrustedServer`] in its [`TrustStore`]. A later connection whose
//!   fingerprint differs is `discovery.fingerprint_changed` and refused until an
//!   administrator re-pairs.
//! - `Off` → `Unprotected`: a typed warning flag the client **must show the user** —
//!   not a log line (this type is deliberately distinct so the UI cannot lose it).
//!
//! ## The constructions
//!
//! Both server and client derive the same values from the server's 32-byte public key
//! (or any canonical key encoding both sides hold), a fresh 16-byte nonce, and the
//! shared label constants:
//!
//! ```text
//! fingerprint = hex( SHA-256(key)[ ..16 ] )
//! H           = HMAC-SHA-256(key, "spot-pair-code-v1"     || nonce)   -- 32 bytes
//! pairing_code          = 8 chars of H[0..8], each mapped into the 32-character
//!   unambiguous alphabet "ABCDEFGHJKLMNPQRSTUVWXYZ23456789" (no 0/O/1/I/L-lookalikes;
//!   the map is `alphabet[byte & 0x1F]`)
//! confirmation = HMAC-SHA-256(key, "spot-pair-confirm-v1" || nonce || pairing_code)
//! ```
//!
//! The MAC binds the code to the key and the nonce: entering the code proves the user
//! can see the server's display, and the confirmation proves both sides hashed the
//! *same key* — a man in the middle with a different key cannot produce it. The nonce
//! is one-time: a recorded confirmation is refused on replay.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use ::serde::{Deserialize, Serialize};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::error::DiscoveryError;
use crate::record::{Fingerprint, ServerId};

/// HMAC-SHA-256, spelled out once.
type HmacSha256 = Hmac<Sha256>;

/// Domain separation label for the code derivation.
const CODE_LABEL: &[u8] = b"spot-pair-code-v1";
/// Domain separation label for the confirmation MAC.
const CONFIRM_LABEL: &[u8] = b"spot-pair-confirm-v1";
/// Nonce length in bytes.
pub const NONCE_LEN: usize = 16;
/// Raw key length this module hashes (a 32-byte Ed25519 public key or an equivalent
/// canonical encoding; the caller supplies the bytes it will actually bind TLS to).
pub const KEY_LEN: usize = 32;
/// The unambiguous alphabet: no `0/O`, no `1/I/L`, no `5/S` lookalike pairs that
/// survive a bad phone-photo read.
pub const CODE_ALPHABET: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// Length of a pairing code in characters.
pub const CODE_LEN: usize = 8;
/// How long a pairing code is valid.
pub const CODE_VALIDITY: Duration = Duration::from_secs(5 * 60);
/// Failed attempts allowed before lockout.
pub const MAX_ATTEMPTS: u32 = 3;
/// How long pairing is locked after [`MAX_ATTEMPTS`] failures.
pub const LOCKOUT_DURATION: Duration = Duration::from_secs(15 * 60);

/// Derives the 8-character pairing code from the server key and nonce.
///
/// # Errors
/// [`DiscoveryError::PairingExpired`] when the key or nonce has the wrong length
/// (re-tagged as `PairingCodeInvalid` by callers that surface it as user input).
pub fn pairing_code(key: &[u8], nonce: &[u8]) -> Result<String, DiscoveryError> {
    if key.len() != KEY_LEN {
        return Err(DiscoveryError::PairingCodeInvalid);
    }
    if nonce.len() != NONCE_LEN {
        return Err(DiscoveryError::PairingCodeInvalid);
    }
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| DiscoveryError::PairingCodeInvalid)?;
    mac.update(CODE_LABEL);
    mac.update(nonce);
    let out = mac.finalize().into_bytes();
    let mut code = String::with_capacity(CODE_LEN);
    for &b in &out[..CODE_LEN] {
        let idx = (b & 0x1f) as usize;
        let ch = CODE_ALPHABET.as_bytes().get(idx).copied().unwrap_or(b'A');
        code.push(ch as char);
    }
    Ok(code)
}

/// Computes the confirmation MAC both sides compare: over the key, the nonce and the
/// code the user entered.
///
/// # Errors
/// [`DiscoveryError::PairingCodeInvalid`] when `key` or `nonce` has the wrong length.
pub fn pairing_confirm_mac(
    key: &[u8],
    nonce: &[u8],
    code: &str,
) -> Result<[u8; 32], DiscoveryError> {
    if key.len() != KEY_LEN {
        return Err(DiscoveryError::PairingCodeInvalid);
    }
    if nonce.len() != NONCE_LEN {
        return Err(DiscoveryError::PairingCodeInvalid);
    }
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|_| DiscoveryError::PairingCodeInvalid)?;
    mac.update(CONFIRM_LABEL);
    mac.update(nonce);
    mac.update(code.as_bytes());
    let out = mac.finalize().into_bytes();
    let mut fixed = [0u8; 32];
    fixed.copy_from_slice(&out);
    Ok(fixed)
}

/// Computes the [`Fingerprint`] over a raw public key (SHA-256, first 16 bytes, hex).
#[must_use]
pub fn key_fingerprint(key: &[u8]) -> Fingerprint {
    Fingerprint::from_public_key_der(key)
}

/// The client-side security state of a connection attempt. `Unprotected` is a value
/// the UI must render, not a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsModeState {
    /// TLS with a system-trusted certificate.
    Secured,
    /// TLS with a self-signed certificate the client paired and stored.
    Verified,
    /// Plaintext: show a warning to the user before connecting.
    Unprotected,
}

/// The typed `Unprotected` warning flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unprotected(pub TlsModeState);

impl Unprotected {
    /// The flag is carried only by plaintext mode; this constructor makes the intent
    /// explicit at the call site.
    #[must_use]
    pub const fn from_tls_off() -> Self {
        Self(TlsModeState::Unprotected)
    }
}

/// The server-side pairing session: a code derived from this server's key and a fresh
/// nonce, valid [`CODE_VALIDITY`], with a rate-limited attempt counter.
#[derive(Debug)]
pub struct PairingSession {
    /// The server's key the code and confirmation are derived from (kept for the
    /// confirmation recompute on `expected_confirmation`).
    key: [u8; KEY_LEN],
    /// The one-time nonce from the injected random source.
    nonce: [u8; NONCE_LEN],
    /// The fingerprint the client stores after pairing.
    fingerprint: Fingerprint,
    /// The service instance being paired.
    service_instance: ServerId,
    /// Monotonic time the session began (the code window is measured from it).
    created_monotonic: Duration,
    /// The displayed code.
    code: String,
    /// Set when a correct code was verified; the session is one-shot.
    consumed: bool,
    /// The rate-limited attempt counter.
    attempts: PairingAttemptCounter,
}

impl PairingSession {
    /// Starts a session from the server's key bytes and a nonce drawn from the
    /// injected [`crate::random::RandomSource`].
    ///
    /// # Errors
    /// [`DiscoveryError::PairingCodeInvalid`] when the key is not [`KEY_LEN`] bytes.
    pub fn start(
        service_instance: ServerId,
        key: &[u8],
        random: &dyn crate::random::RandomSource,
        now_monotonic: Duration,
    ) -> Result<Self, DiscoveryError> {
        let mut key_fixed = [0u8; KEY_LEN];
        let mut nonce = [0u8; NONCE_LEN];
        if key.len() != KEY_LEN {
            return Err(DiscoveryError::PairingCodeInvalid);
        }
        key_fixed.copy_from_slice(key);
        random.fill(&mut nonce);
        let code = pairing_code(key, &nonce)?;
        Ok(Self {
            fingerprint: key_fingerprint(key),
            key: key_fixed,
            nonce,
            service_instance,
            created_monotonic: now_monotonic,
            code,
            consumed: false,
            attempts: PairingAttemptCounter::Fresh,
        })
    }

    /// The pairing code to show on the server console.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The server's key fingerprint (the value the client will store after pairing).
    #[must_use]
    pub fn fingerprint(&self) -> &Fingerprint {
        &self.fingerprint
    }

    /// The service instance being paired.
    #[must_use]
    pub fn service_instance(&self) -> &ServerId {
        &self.service_instance
    }

    /// Whether the session has been consumed or locked.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        !self.consumed && !matches!(self.attempts, PairingAttemptCounter::Locked { .. })
    }

    /// Verifies a code entered on the client (the caller relays it). Rate-limited:
    /// three wrong attempts lock the session for [`LOCKOUT_DURATION`].
    ///
    /// # Errors
    /// - [`DiscoveryError::PairingExpired`] when the window passed or a consumed
    ///   confirmation is replayed.
    /// - [`DiscoveryError::PairingLocked`] after [`MAX_ATTEMPTS`] failures.
    /// - [`DiscoveryError::PairingCodeInvalid`] when the code is wrong.
    pub fn verify_client_code(
        &mut self,
        code: &str,
        now_monotonic: Duration,
    ) -> Result<(), DiscoveryError> {
        if self.consumed {
            return Err(DiscoveryError::PairingExpired);
        }
        if now_monotonic.saturating_sub(self.created_monotonic) > CODE_VALIDITY {
            return Err(DiscoveryError::PairingExpired);
        }
        match self.attempts.status(now_monotonic) {
            PairingStatus::Locked => {
                return Err(DiscoveryError::PairingLocked {
                    attempts: MAX_ATTEMPTS,
                });
            }
            PairingStatus::Open => {}
        }
        if constant_time_eq(code.as_bytes(), self.code.as_bytes()) {
            self.consumed = true;
            Ok(())
        } else {
            self.attempts.record_failure(now_monotonic);
            if self.attempts.status(now_monotonic) == PairingStatus::Locked {
                Err(DiscoveryError::PairingLocked {
                    attempts: MAX_ATTEMPTS,
                })
            } else {
                Err(DiscoveryError::PairingCodeInvalid)
            }
        }
    }

    /// The confirmation MAC this server expects for this session, recomputed from the
    /// key and nonce (the key is never handed out).
    #[must_use]
    pub fn expected_confirmation(&self) -> [u8; 32] {
        // The inputs were validated at `start`; the recompute is total.
        pairing_confirm_mac(&self.key, &self.nonce, &self.code).unwrap_or([0u8; 32])
    }
}

/// The client-side view of one pairing exchange. Created with the server's advertised
/// fingerprint; completed when the displayed confirmation MAC matches the one the
/// client computes from the code it entered.
#[derive(Debug)]
pub struct PairingClient {
    service_instance: ServerId,
    /// The key bytes the client learned during the exchange (from the TLS handshake
    /// with the advertised fingerprint).
    key: [u8; KEY_LEN],
    nonce: [u8; NONCE_LEN],
    code_entered: Option<String>,
}

impl PairingClient {
    /// Prepares the client side of a pairing exchange.
    ///
    /// # Errors
    /// [`DiscoveryError::PairingCodeInvalid`] when the key length is wrong.
    pub fn begin(
        service_instance: ServerId,
        key: &[u8],
        nonce: &[u8],
    ) -> Result<Self, DiscoveryError> {
        if key.len() != KEY_LEN {
            return Err(DiscoveryError::PairingCodeInvalid);
        }
        if nonce.len() != NONCE_LEN {
            return Err(DiscoveryError::PairingCodeInvalid);
        }
        let mut key_fixed = [0u8; KEY_LEN];
        key_fixed.copy_from_slice(key);
        let mut nonce_fixed = [0u8; NONCE_LEN];
        nonce_fixed.copy_from_slice(nonce);
        Ok(Self {
            service_instance,
            key: key_fixed,
            nonce: nonce_fixed,
            code_entered: None,
        })
    }

    /// Records the code the user read off the server console.
    ///
    /// # Errors
    /// [`DiscoveryError::PairingCodeInvalid`] when the code is not [`CODE_LEN`]
    /// characters of the unambiguous alphabet.
    pub fn enter_code(&mut self, code: &str) -> Result<(), DiscoveryError> {
        let ok =
            code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.as_bytes().contains(&b));
        if !ok {
            return Err(DiscoveryError::PairingCodeInvalid);
        }
        self.code_entered = Some(code.to_string());
        Ok(())
    }

    /// Completes the exchange: recomputes the confirmation MAC over the entered code
    /// and compares it to the server-displayed MAC.
    ///
    /// # Errors
    /// [`DiscoveryError::PairingCodeInvalid`] when no code was entered or the MACs
    /// differ.
    pub fn complete(
        &self,
        server_confirmation: &[u8; 32],
    ) -> Result<TrustedServer, DiscoveryError> {
        let Some(code) = self.code_entered.as_ref() else {
            return Err(DiscoveryError::PairingCodeInvalid);
        };
        let mine = pairing_confirm_mac(&self.key, &self.nonce, code)?;
        if !constant_time_eq(&mine, server_confirmation) {
            return Err(DiscoveryError::PairingCodeInvalid);
        }
        Ok(TrustedServer {
            server_id: self.service_instance.clone(),
            fingerprint: key_fingerprint(&self.key),
            first_seen: 0,
        })
    }
}

/// Attempt counter behind pairing rate limiting. The caller owns persistence if it
/// wants lockout to survive a restart; this type is the documented behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingAttemptCounter {
    /// No failures recorded.
    Fresh,
    /// Failures so far, with the last failure's monotonic time.
    Failures {
        /// Failed attempts since the last successful (or expired) session.
        count: u32,
        /// Monotonic time of the most recent failure.
        last: Duration,
    },
    /// Locked since `since`, until `until`.
    Locked {
        /// Monotonic time the lockout began.
        since: Duration,
        /// Monotonic time the lockout ends.
        until: Duration,
    },
}

impl PairingAttemptCounter {
    /// Current status at `now`.
    #[must_use]
    pub fn status(&self, now: Duration) -> PairingStatus {
        match self {
            Self::Fresh | Self::Failures { .. } => PairingStatus::Open,
            Self::Locked { until, .. } => {
                if *until > now {
                    PairingStatus::Locked
                } else {
                    PairingStatus::Open
                }
            }
        }
    }

    /// Records one failure at `now`, locking when [`MAX_ATTEMPTS`] is reached.
    pub fn record_failure(&mut self, now: Duration) {
        let count = match self {
            Self::Fresh => 1u32,
            Self::Failures { count, .. } => count.saturating_add(1),
            Self::Locked { until, .. } => {
                if *until > now {
                    return;
                }
                1
            }
        };
        if count >= MAX_ATTEMPTS {
            *self = Self::Locked {
                since: now,
                until: now + LOCKOUT_DURATION,
            };
        } else {
            *self = Self::Failures { count, last: now };
        }
    }
}

/// Whether attempts may proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingStatus {
    /// Attempts allowed.
    Open,
    /// Locked out.
    Locked,
}

/// Constant-time equality so a wrong code costs the same as a right one (side-channel
/// hygiene; the code window is 5 minutes, the counter 3).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// A server the client has paired with and must trust.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedServer {
    /// The stable [`ServerId`].
    pub server_id: ServerId,
    /// The paired key fingerprint.
    pub fingerprint: Fingerprint,
    /// Wall-clock seconds since the Unix epoch when pairing happened (u64; a `Clock`
    /// converts to a date for display). Storing seconds keeps the file form stable
    /// across time crates.
    pub first_seen: u64,
}

/// The client's store of paired servers. Implementations: [`InMemoryTrustStore`] and
/// [`FileTrustStore`].
pub trait TrustStore: Send + Sync + 'static {
    /// Looks up the stored fingerprint for `server_id`.
    ///
    /// # Errors
    /// [`DiscoveryError::TrustStoreUnavailable`] when a backing file exists but cannot
    /// be read.
    fn lookup(&self, server_id: &ServerId) -> Result<Option<TrustedServer>, DiscoveryError>;

    /// Stores or replaces the pairing for `server_id`.
    ///
    /// # Errors
    /// [`DiscoveryError::TrustStoreUnavailable`] when the backing store cannot be
    /// written.
    fn store(&self, trusted: TrustedServer) -> Result<(), DiscoveryError>;

    /// Removes the pairing for `server_id` (administrator-initiated re-pair).
    ///
    /// # Errors
    /// [`DiscoveryError::TrustStoreUnavailable`] when the backing store cannot be
    /// written.
    fn remove(&self, server_id: &ServerId) -> Result<(), DiscoveryError>;
}

/// An in-memory store: the default for tests and for clients without a profile dir.
#[derive(Debug, Default)]
pub struct InMemoryTrustStore {
    entries: std::sync::Mutex<HashMap<ServerId, TrustedServer>>,
}

impl InMemoryTrustStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl TrustStore for InMemoryTrustStore {
    fn lookup(&self, server_id: &ServerId) -> Result<Option<TrustedServer>, DiscoveryError> {
        Ok(self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(server_id)
            .cloned())
    }

    fn store(&self, trusted: TrustedServer) -> Result<(), DiscoveryError> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(trusted.server_id.clone(), trusted);
        Ok(())
    }

    fn remove(&self, server_id: &ServerId) -> Result<(), DiscoveryError> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(server_id);
        Ok(())
    }
}

/// A file store written atomically: the new file is written fully, then renamed over
/// the old one, so a crash mid-write never truncates the store.
#[derive(Debug)]
pub struct FileTrustStore {
    path: PathBuf,
}

impl FileTrustStore {
    /// Opens (not creates) a store at `path`.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Reads the whole store file; a missing file is an empty store, a corrupt one is
    /// an error (never silently discarded trust).
    fn read_all(&self) -> Result<Vec<TrustedServer>, DiscoveryError> {
        let bytes = match std::fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(DiscoveryError::TrustStoreUnavailable {
                    path: self.path.display().to_string(),
                    detail: format!("the store file could not be read: {e}"),
                });
            }
        };
        serde_json::from_slice::<Vec<TrustedServer>>(&bytes).map_err(|_| {
            DiscoveryError::TrustStoreUnavailable {
                path: self.path.display().to_string(),
                detail: "the store file is corrupt".to_string(),
            }
        })
    }

    /// Writes the whole store atomically (temp file in the same directory, fsync,
    /// rename).
    fn write_all(&self, entries: &[TrustedServer]) -> Result<(), DiscoveryError> {
        let bytes = serde_json::to_vec_pretty(entries).map_err(|e| {
            DiscoveryError::TrustStoreUnavailable {
                path: self.path.display().to_string(),
                detail: format!("the store could not be serialised: {e}"),
            }
        })?;
        let tmp = self.path.with_extension("tmp-atom");
        std::fs::write(&tmp, &bytes).map_err(|e| DiscoveryError::TrustStoreUnavailable {
            path: tmp.display().to_string(),
            detail: format!("the store could not be written: {e}"),
        })?;
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(DiscoveryError::TrustStoreUnavailable {
                path: self.path.display().to_string(),
                detail: format!("the store could not be replaced: {e}"),
            });
        }
        Ok(())
    }
}

impl TrustStore for FileTrustStore {
    fn lookup(&self, server_id: &ServerId) -> Result<Option<TrustedServer>, DiscoveryError> {
        Ok(self
            .read_all()?
            .into_iter()
            .find(|t| &t.server_id == server_id))
    }

    fn store(&self, trusted: TrustedServer) -> Result<(), DiscoveryError> {
        let mut entries = self
            .read_all()?
            .into_iter()
            .filter(|t| t.server_id != trusted.server_id)
            .collect::<Vec<_>>();
        entries.push(trusted);
        entries.sort_by(|a, b| a.server_id.cmp(&b.server_id));
        self.write_all(&entries)
    }

    fn remove(&self, server_id: &ServerId) -> Result<(), DiscoveryError> {
        let mut entries = self.read_all()?;
        let before = entries.len();
        entries.retain(|t| &t.server_id != server_id);
        if entries.len() != before {
            self.write_all(&entries)?;
        }
        Ok(())
    }
}

/// The full first-connection flow for `TlsMode::SelfSigned`, as a function so callers
/// cannot skip the trust decision: with no stored fingerprint the pairing must
/// complete first; with a stored one a mismatch is refused.
///
/// # Errors
/// - [`DiscoveryError::FingerprintChanged`] when the stored fingerprint differs.
/// - [`TrustStoreUnavailable`](DiscoveryError::TrustStoreUnavailable) on store failure.
pub fn verify_paired(
    store: &dyn TrustStore,
    server_id: &ServerId,
    presented: &Fingerprint,
) -> Result<TrustDecision, DiscoveryError> {
    let stored = store.lookup(server_id)?;
    match stored {
        None => Ok(TrustDecision::PairingRequired),
        Some(trusted) if &trusted.fingerprint == presented => Ok(TrustDecision::Trusted(trusted)),
        Some(_) => Err(DiscoveryError::FingerprintChanged),
    }
}

/// The outcome of [`verify_paired`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustDecision {
    /// No pairing stored yet: run the pairing exchange before any data flows.
    PairingRequired,
    /// The presented fingerprint matches the stored pairing.
    Trusted(TrustedServer),
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        CODE_ALPHABET, CODE_LEN, CODE_VALIDITY, InMemoryTrustStore, LOCKOUT_DURATION, MAX_ATTEMPTS,
        PairingClient, PairingSession, TlsModeState, TrustDecision, TrustStore, Unprotected,
        constant_time_eq, key_fingerprint, pairing_code, pairing_confirm_mac, verify_paired,
    };
    use crate::error::DiscoveryError;
    use crate::random::FixedRandom;
    use crate::record::ServerId;

    /// The key the fixed nonce makes deterministic: SHA-256("spot-discovery pairing
    /// test key v1") and v2, computed with the same python3 used to build the vectors
    /// (the test asserts against those exact hex strings).
    const KEY_V1: [u8; 32] = [
        0xea, 0x97, 0x91, 0x6a, 0x50, 0x1d, 0x91, 0x80, 0x49, 0x89, 0x98, 0x03, 0x70, 0x87, 0xab,
        0x84, 0x75, 0xc6, 0x13, 0x7c, 0xf0, 0x3e, 0xdc, 0x70, 0x22, 0x79, 0x71, 0xc8, 0x2f, 0xea,
        0x35, 0xb1,
    ];
    const NONCE_V1: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];

    const VEC_CODE: &str = "437EYX54";
    const VEC_CONFIRM: [u8; 32] = [
        0xe4, 0x8c, 0x7d, 0xe6, 0x48, 0xf6, 0x0c, 0xd6, 0xd2, 0x18, 0x15, 0x13, 0x78, 0x40, 0x92,
        0x64, 0x96, 0x7d, 0xa0, 0x23, 0x74, 0x89, 0x47, 0x89, 0xd4, 0xc1, 0x28, 0x25, 0xf1, 0x18,
        0x00, 0x6f,
    ];

    #[test]
    fn hmac_test_vector_code_and_confirmation() {
        let code = pairing_code(&KEY_V1, &NONCE_V1).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(code, VEC_CODE);
        let mac =
            pairing_confirm_mac(&KEY_V1, &NONCE_V1, &code).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(mac, VEC_CONFIRM);
    }

    #[test]
    fn test_vector_uses_unambiguous_alphabet() {
        assert_eq!(CODE_ALPHABET.len(), 32);
        assert!(VEC_CODE.chars().all(|c| CODE_ALPHABET.contains(c)));
        assert_eq!(VEC_CODE.len(), CODE_LEN);
    }

    #[test]
    fn different_key_changes_code_and_confirmation() {
        let key2 = {
            let mut k = KEY_V1;
            k[0] ^= 0x01;
            k
        };
        let c1 = pairing_code(&KEY_V1, &NONCE_V1).unwrap_or_else(|e| panic!("{e:?}"));
        let c2 = pairing_code(&key2, &NONCE_V1).unwrap_or_else(|e| panic!("{e:?}"));
        assert_ne!(c1, c2);
        let m1 = pairing_confirm_mac(&KEY_V1, &NONCE_V1, &c1).unwrap_or_else(|e| panic!("{e:?}"));
        let m2 = pairing_confirm_mac(&key2, &NONCE_V1, &c1).unwrap_or_else(|e| panic!("{e:?}"));
        assert_ne!(m1, m2);
    }

    #[test]
    fn code_length_and_character_set_hold_for_stream_of_nonces() {
        let mut nonce = [0u8; 16];
        for b in 0..64u8 {
            nonce[0] = b;
            let code = pairing_code(&KEY_V1, &nonce).unwrap_or_else(|e| panic!("{e:?}"));
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.chars().all(|c| CODE_ALPHABET.contains(c)));
        }
    }

    #[test]
    fn session_start_yields_code_and_confirmation() {
        let rnd = FixedRandom::new(NONCE_V1.to_vec());
        let mut session =
            PairingSession::start(ServerId("srv-1".to_string()), &KEY_V1, &rnd, Duration::ZERO)
                .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(session.code(), VEC_CODE);
        let ok = session.verify_client_code(VEC_CODE, Duration::from_secs(10));
        assert!(ok.is_ok(), "{ok:?}");
        assert_eq!(session.expected_confirmation(), VEC_CONFIRM);
    }

    #[test]
    fn correct_code_succeeds_and_consumes_session() {
        let rnd = FixedRandom::new(NONCE_V1.to_vec());
        let mut session =
            PairingSession::start(ServerId("srv-1".to_string()), &KEY_V1, &rnd, Duration::ZERO)
                .unwrap_or_else(|e| panic!("{e:?}"));
        session
            .verify_client_code(VEC_CODE, Duration::ZERO)
            .unwrap_or_else(|e| panic!("{e:?}"));
        // Replay of the same code is refused (already consumed).
        let replay = session.verify_client_code(VEC_CODE, Duration::from_secs(1));
        assert!(
            matches!(replay, Err(DiscoveryError::PairingExpired)),
            "{replay:?}"
        );
        assert!(!session.is_alive());
    }

    #[test]
    fn wrong_code_three_times_locks_out() {
        let rnd = FixedRandom::new(NONCE_V1.to_vec());
        let mut session =
            PairingSession::start(ServerId("srv-1".to_string()), &KEY_V1, &rnd, Duration::ZERO)
                .unwrap_or_else(|e| panic!("{e:?}"));
        let mut last = None;
        for i in 0..MAX_ATTEMPTS {
            let now = Duration::from_secs(u64::from(i));
            let res = session.verify_client_code("AAAAAAAA", now);
            last = Some(res);
        }
        let third = last.unwrap_or_else(|| panic!("expected three attempts"));
        // The third failure reports the lockout (the counter locked on the third).
        assert!(
            matches!(third, Err(DiscoveryError::PairingLocked { .. })),
            "third attempt must report lockout, got {third:?}"
        );
        // Even the correct code is refused during the lockout.
        let during = session.verify_client_code(VEC_CODE, Duration::from_secs(60));
        assert!(matches!(during, Err(DiscoveryError::PairingLocked { .. })));
        // After the lockout the code's 5-minute window has long passed: the old
        // session is dead and the server must start a fresh one. (That is the
        // documented behaviour: lockout never re-opens a stale code.)
        let after = session.verify_client_code(VEC_CODE, LOCKOUT_DURATION + Duration::from_secs(1));
        assert!(
            matches!(after, Err(DiscoveryError::PairingExpired)),
            "{after:?}"
        );
    }

    #[test]
    fn lockout_ends_and_new_session_accepts_attempts() {
        let rnd = FixedRandom::new(NONCE_V1.to_vec());
        let mut first =
            PairingSession::start(ServerId("srv-1".to_string()), &KEY_V1, &rnd, Duration::ZERO)
                .unwrap_or_else(|e| panic!("{e:?}"));
        for i in 0..MAX_ATTEMPTS {
            let now = Duration::from_secs(u64::from(i));
            let _ = first.verify_client_code("AAAAAAAA", now);
        }
        // A fresh session (the server restarts pairing after the lockout) works
        // again immediately.
        let mut fresh = PairingSession::start(
            ServerId("srv-1".to_string()),
            &KEY_V1,
            &rnd,
            LOCKOUT_DURATION + Duration::from_secs(1),
        )
        .unwrap_or_else(|e| panic!("{e:?}"));
        let ok = fresh.verify_client_code(VEC_CODE, LOCKOUT_DURATION + Duration::from_secs(2));
        assert!(ok.is_ok(), "{ok:?}");
    }

    #[test]
    fn expired_code_refused() {
        let rnd = FixedRandom::new(NONCE_V1.to_vec());
        let mut session =
            PairingSession::start(ServerId("srv-1".to_string()), &KEY_V1, &rnd, Duration::ZERO)
                .unwrap_or_else(|e| panic!("{e:?}"));
        let late = session.verify_client_code(VEC_CODE, CODE_VALIDITY + Duration::from_secs(1));
        assert!(
            matches!(late, Err(DiscoveryError::PairingExpired)),
            "{late:?}"
        );
    }

    #[test]
    fn replayed_confirmation_refused_by_client_flow() {
        // The client computes the confirmation from the code it entered; a recorded
        // confirmation from an *earlier* exchange (other nonce) does not verify.
        let rnd = FixedRandom::new(NONCE_V1.to_vec());
        let session_old =
            PairingSession::start(ServerId("srv-1".to_string()), &KEY_V1, &rnd, Duration::ZERO)
                .unwrap_or_else(|e| panic!("{e:?}"));
        let old_confirm = session_old.expected_confirmation();

        // A new session with a different nonce:
        let mut different_nonce = NONCE_V1;
        different_nonce[0] = 0x7f;
        let mut client =
            PairingClient::begin(ServerId("srv-1".to_string()), &KEY_V1, &different_nonce)
                .unwrap_or_else(|e| panic!("{e:?}"));
        client
            .enter_code(VEC_CODE)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let res = client.complete(&old_confirm);
        assert!(
            matches!(res, Err(DiscoveryError::PairingCodeInvalid)),
            "replayed confirmation must be refused, got {res:?}"
        );
    }

    #[test]
    fn client_flow_stores_trusted_server_on_success() {
        let rnd = FixedRandom::new(NONCE_V1.to_vec());
        let session =
            PairingSession::start(ServerId("srv-1".to_string()), &KEY_V1, &rnd, Duration::ZERO)
                .unwrap_or_else(|e| panic!("{e:?}"));
        let mut client = PairingClient::begin(ServerId("srv-1".to_string()), &KEY_V1, &NONCE_V1)
            .unwrap_or_else(|e| panic!("{e:?}"));
        client
            .enter_code(VEC_CODE)
            .unwrap_or_else(|e| panic!("{e:?}"));
        let trusted = client
            .complete(&session.expected_confirmation())
            .unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(trusted.server_id.as_str(), "srv-1");
        assert_eq!(trusted.fingerprint.0, key_fingerprint(&KEY_V1).0);
    }

    #[test]
    fn fingerprint_change_is_detected_and_refused() {
        let store = InMemoryTrustStore::new();
        let id = ServerId("srv-1".to_string());
        let fp_a = key_fingerprint(&KEY_V1);
        let key2 = {
            let mut k = KEY_V1;
            k[31] ^= 0x01;
            k
        };
        let fp_b = key_fingerprint(&key2);
        store
            .store(crate::trust::TrustedServer {
                server_id: id.clone(),
                fingerprint: fp_a.clone(),
                first_seen: 1_000,
            })
            .unwrap_or_else(|e| panic!("{e:?}"));
        // Same fingerprint: trusted.
        let same = verify_paired(&store, &id, &fp_a).unwrap_or_else(|e| panic!("{e:?}"));
        assert!(matches!(same, TrustDecision::Trusted(_)));
        // A different key under the same instance id:
        let changed = verify_paired(&store, &id, &fp_b);
        assert!(
            matches!(changed, Err(DiscoveryError::FingerprintChanged)),
            "{changed:?}"
        );
    }

    #[test]
    fn pairing_required_when_store_empty() {
        let store = InMemoryTrustStore::new();
        let id = ServerId("srv-9".to_string());
        let fp = key_fingerprint(&KEY_V1);
        let d = verify_paired(&store, &id, &fp).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(d, TrustDecision::PairingRequired);
    }

    #[test]
    fn file_store_round_trip_and_atomicity_shape() {
        let dir = std::env::temp_dir().join(format!("spot-discovery-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("{e}"));
        let path = dir.join("trust.json");
        let store = super::FileTrustStore::new(&path);
        let id = ServerId("srv-file".to_string());
        let fp = key_fingerprint(&KEY_V1);
        store
            .store(crate::trust::TrustedServer {
                server_id: id.clone(),
                fingerprint: fp.clone(),
                first_seen: 42,
            })
            .unwrap_or_else(|e| panic!("{e:?}"));
        let got = store
            .lookup(&id)
            .unwrap_or_else(|e| panic!("{e:?}"))
            .unwrap_or_else(|| panic!("entry missing"));
        assert_eq!(got.fingerprint, fp);
        assert_eq!(got.first_seen, 42);
        // Re-store with a different fingerprint: the new pairing replaces the old.
        let key2 = {
            let mut k = KEY_V1;
            k[31] ^= 0x02;
            k
        };
        store
            .store(crate::trust::TrustedServer {
                server_id: id.clone(),
                fingerprint: key_fingerprint(&key2),
                first_seen: 100,
            })
            .unwrap_or_else(|e| panic!("{e:?}"));
        let replaced = store
            .lookup(&id)
            .unwrap_or_else(|e| panic!("{e:?}"))
            .unwrap_or_else(|| panic!("entry missing after replace"));
        assert_eq!(replaced.fingerprint, key_fingerprint(&key2));
        assert_eq!(replaced.first_seen, 100);
        store.remove(&id).unwrap_or_else(|e| panic!("{e:?}"));
        let after = store.lookup(&id).unwrap_or_else(|e| panic!("{e:?}"));
        assert!(after.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unprotected_flag_is_typed_and_distinct() {
        let flag = Unprotected::from_tls_off();
        assert_eq!(flag.0, TlsModeState::Unprotected);
        assert_ne!(flag.0, TlsModeState::Secured);
    }

    #[test]
    fn wrong_length_inputs_are_refused() {
        let short_key = [0u8; 31];
        assert!(matches!(
            pairing_code(&short_key, &NONCE_V1),
            Err(DiscoveryError::PairingCodeInvalid)
        ));
        let short_nonce = [0u8; 8];
        assert!(matches!(
            pairing_code(&KEY_V1, &short_nonce),
            Err(DiscoveryError::PairingCodeInvalid)
        ));
    }

    #[test]
    fn constant_time_eq_matches_std_for_equal_and_unequal() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"diff"));
        assert!(!constant_time_eq(b"same", b"sam"));
    }
}
