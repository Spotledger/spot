//! Replay protection: the nonce contract and the in-memory bounded-window store
//! (`WP-ADM-01.T2`).
//!
//! # The contract
//!
//! [`NonceStore::check_and_insert`] is one operation: consult the window for
//! `(issuer key, nonce)` and, when absent, insert it — **atomically**. It returns
//! `true` when the pair is fresh (the caller accepts the command), `false` when the
//! pair is already recorded (a replay), and the error when the store cannot serve (its
//! cap is full, a backend failed). A verifier calls it **last**, after every other
//! check passed, so a command rejected for any other reason never burns a nonce.
//!
//! # The window
//!
//! Replay protection is only as durable as the store behind this port. The window must
//! be **shared and durable in any deployment with more than one node** (or any that
//! restarts): [`MemoryNonceStore`] is for one process and for tests, and the
//! PostgreSQL store (`spot-signed-command-pg`, `PgNonceStore`) is the durable one. A
//! store that cannot answer must return an error, never `true`: the port is
//! fail-closed.
//!
//! [`MemoryNonceStore`] keeps entries until `expires_at + max_skew` (a live entry may
//! still be presented under skew; dropping it earlier reopens the window), sweeps them
//! lazily on each call, and enforces a hard cap [`DEFAULT_NONCE_CAP`]: at the cap a new
//! command is refused with [`StoreError::Full`] rather than evicting a live entry —
//! evicting would reopen the replay window the store exists to close. The cap bound is
//! why the window needs no external memory: after `expires_at + max_skew` no verifier
//! accepts the command any more, so its nonce can never be replayed into acceptance.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use crate::command::KeyId;
use crate::error::StoreError;

/// A boxed, `Send` future: the return type of the nonce port, so the trait stays object
/// safe (`Box<dyn NonceStore>`, `Arc<dyn NonceStore>`).
pub type NonceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The replay-protection contract, implemented by callers' own stores (and
/// [`MemoryNonceStore`] here, which is for one process and for tests).
///
/// Implementations must be atomic per call: two racing calls on the same key and nonce
/// must return exactly one `true`. The store may drop entries once they are past
/// `expires_at + skew`; dropping a live entry is forbidden (it reopens the replay
/// window).
pub trait NonceStore: Send + Sync {
    /// Consults the window and records the pair atomically.
    ///
    /// # Errors
    /// The implementation's own failure mode (full, backend) as [`StoreError`]; a
    /// failing store refuses the command.
    fn check_and_insert<'a>(
        &'a self,
        key: &'a KeyId,
        nonce: [u8; 16],
        expires_at: i64,
        now: i64,
    ) -> NonceFuture<'a, Result<bool, StoreError>>;
}

/// The default hard cap of [`MemoryNonceStore`]: 100,000 live entries.
pub const DEFAULT_NONCE_CAP: usize = 100_000;

/// One recorded nonce: the issuer key it came with, and when it may be dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
struct NonceEntry {
    /// The command's expiry instant (UTC seconds), as the store received it.
    expires_at: i64,
}

/// The window map: `(key id bytes, nonce)` to the entry.
type WindowMap = HashMap<(Vec<u8>, [u8; 16]), NonceEntry>;

/// The in-memory nonce store: thread-safe, bounded, lazy-sweeping.
///
/// **For one process and for tests.** A restart empties it and a second node never sees
/// it, so a deployment that must refuse replays across restarts or nodes uses a durable
/// store (`spot-signed-command-pg`) instead.
///
/// The window entries live in a [`Mutex`]'d map keyed by `(key id bytes, nonce)`; the
/// map is swept of expired entries on every call before the cap is consulted.
pub struct MemoryNonceStore {
    /// The live window, guarded by the mutex.
    entries: Mutex<WindowMap>,
    /// The skew seconds entries survive past their `expires_at`.
    skew: i64,
    /// The hard cap on live entries.
    cap: usize,
}

impl MemoryNonceStore {
    /// A store with the default skew (60 s) and the default cap (100,000).
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(crate::envelope::DEFAULT_MAX_SKEW, DEFAULT_NONCE_CAP)
    }

    /// A store with explicit skew and cap: `skew` extends every entry's life past its
    /// `expires_at` (it must match the verifier policy's skew for the window bound to
    /// be exact), `cap` is the maximum number of live entries before new commands are
    /// refused.
    #[must_use]
    pub fn with_limits(skew: i64, cap: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            skew,
            cap,
        }
    }

    /// How many entries are live right now (tests and monitoring).
    ///
    /// # Errors
    /// [`StoreError::Backend`] when the mutex is poisoned.
    pub fn len(&self) -> Result<usize, StoreError> {
        Ok(self.lock()?.len())
    }

    /// Whether the window is empty.
    ///
    /// # Errors
    /// [`StoreError::Backend`] when the mutex is poisoned.
    pub fn is_empty(&self) -> Result<bool, StoreError> {
        Ok(self.lock()?.is_empty())
    }

    /// Drops entries whose `expires_at + skew` instant has passed, returning how many
    /// were dropped. An entry is live until `now` is strictly past
    /// `expires_at + skew`: a verifier whose clock disagrees by at most `skew` can no
    /// longer accept the command at that point, so the nonce can never be replayed
    /// into an acceptance.
    ///
    /// # Errors
    /// [`StoreError::Backend`] when the mutex is poisoned.
    pub fn sweep(&self, now: i64) -> Result<usize, StoreError> {
        let mut entries = self.lock()?;
        let before = entries.len();
        entries.retain(|_, entry| entry.expires_at + self.skew >= now);
        Ok(before - entries.len())
    }

    /// Locks the map, mapping a poisoned lock to the backend error.
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, WindowMap>, StoreError> {
        self.entries.lock().map_err(|poisoned| {
            StoreError::Backend(format!("nonce store lock poisoned: {poisoned}"))
        })
    }
}

impl Default for MemoryNonceStore {
    fn default() -> Self {
        Self::new()
    }
}

impl NonceStore for MemoryNonceStore {
    fn check_and_insert<'a>(
        &'a self,
        key: &'a KeyId,
        nonce: [u8; 16],
        expires_at: i64,
        now: i64,
    ) -> NonceFuture<'a, Result<bool, StoreError>> {
        // The whole decision runs synchronously under the lock; the future is ready at
        // once, and the lock is never held across an await point.
        let result = self.claim(key, nonce, expires_at, now);
        Box::pin(std::future::ready(result))
    }
}

impl MemoryNonceStore {
    /// The atomic check-and-insert, under the lock.
    fn claim(
        &self,
        key: &KeyId,
        nonce: [u8; 16],
        expires_at: i64,
        now: i64,
    ) -> Result<bool, StoreError> {
        let mut entries = self.lock()?;
        // Sweep first: entries past `expires_at + skew` must not count against the cap.
        entries.retain(|_, entry| entry.expires_at + self.skew >= now);
        let slot = (key.as_bytes().to_vec(), nonce);
        if entries.contains_key(&slot) {
            return Ok(false);
        }
        if entries.len() >= self.cap {
            // Refuse rather than evict: a live entry dropped here reopens the replay
            // window the store exists to close.
            return Err(StoreError::Full);
        }
        entries.insert(slot, NonceEntry { expires_at });
        Ok(true)
    }
}

impl fmt::Debug for MemoryNonceStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The entries are freshness state, not secrets; the count is what monitoring
        // needs. The mutex may be poisoned, so the count reports through a Result-style
        // fallback: show the cap and skew, not the map.
        f.debug_struct("MemoryNonceStore")
            .field("skew", &self.skew)
            .field("cap", &self.cap)
            .finish()
    }
}
