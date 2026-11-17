//! Request grants: the HMAC keys, the production authority over a rotating key list, the request
//! table the authority reads, and the issuing side the engine's steps call.
//!
//! # Keys
//!
//! A grant is signed under a key **derived from the site's key-encryption key** with
//! `KeyProvider::derive_key` (HKDF, per-purpose label, 32 bytes): the KEK never leaves the
//! provider, the derived key is held zeroed-on-drop and never logged, and there is no key to put
//! in a settings file. The label carries the key id from `[services.process_governance]
//! key_ids`, so rotating is adding a new id at the front: new grants are signed under it and
//! grants signed under an older listed id still verify until they expire (15 minutes at most).
//! The derivation is async and the authority's port is not, so the derived keys are refreshed
//! into a small in-memory table by [`DerivedSecrets::refresh`] at start and on every tick of
//! the module's service; a key that cannot be derived is removed, so the authority fails closed.
//!
//! # One-time use
//!
//! The authority answers "is this a grant I signed, for this person, for the current attempt of
//! an open step". Whether the grant was already **used** is the ledger's: the write that
//! carries it inserts the use row in its own transaction (see `participant.rs`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, RwLock};

use spot_bpm_governance::{GovernanceError, RequestAuthority, VerifiedRequest};
use spot_bpm_request_authority::{
    GrantError, GrantSecrets, ProductionRequestAuthority, RequestStore, SecretRef,
};
use spot_core::Clock;
use spot_secrets::{KeyProvider, SecretBytes};
use time::Duration;

use super::ledger::{Attempt, Ledger, LedgerError, Slot};

/// The label prefix of a derived grant key. A label is not a secret and is not a dotted code.
const LABEL_PREFIX: &str = "spot-request-grant/";

/// The length of a derived grant key, bytes.
const KEY_LEN: usize = 32;

/// The grant keys derived from the site's key provider, by key id.
#[derive(Default)]
pub struct DerivedSecrets {
    keys: RwLock<BTreeMap<String, SecretBytes>>,
}

impl DerivedSecrets {
    /// An empty table: nothing can be signed or verified until a refresh succeeds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A table holding fixed keys, for tests and hosts with their own key source.
    #[must_use]
    pub fn fixed(entries: &[(&str, &[u8])]) -> Self {
        let table = Self::new();
        for (id, value) in entries {
            table.set(id, SecretBytes::new(value.to_vec()));
        }
        table
    }

    fn set(&self, id: &str, value: SecretBytes) {
        self.keys
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_owned(), value);
    }

    fn remove(&self, id: &str) {
        self.keys
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
    }

    /// Derives the key of every id in `ids` from `provider`'s current key-encryption key. An id
    /// that cannot be derived is removed, never left stale. Returns how many are usable.
    pub async fn refresh(&self, provider: &dyn KeyProvider, ids: &[String]) -> usize {
        let key = provider.current();
        let mut usable = 0;
        for id in ids {
            let label = format!("{LABEL_PREFIX}{id}");
            match provider.derive_key(&key, label.as_bytes(), KEY_LEN).await {
                Ok(value) => {
                    self.set(id, value);
                    usable += 1;
                }
                Err(_) => {
                    self.remove(id);
                    tracing::warn!(key_id = %id, "a request grant key could not be derived");
                }
            }
        }
        usable
    }

    /// How many keys are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Whether no key is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl GrantSecrets for DerivedSecrets {
    fn resolve(&self, reference: &SecretRef) -> Result<SecretBytes, GrantError> {
        self.keys
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(reference.as_str())
            .map(|value| SecretBytes::new(value.expose().to_vec()))
            .ok_or(GrantError::SecretUnavailable)
    }
}

/// The production authority over a list of key ids: it signs under the first and verifies under
/// any of them.
pub struct KeyedAuthority {
    current: ProductionRequestAuthority,
    older: Vec<ProductionRequestAuthority>,
}

impl KeyedAuthority {
    /// An authority over `store`, signing under `key_ids[0]` and also verifying the rest.
    ///
    /// # Errors
    /// No key id was given.
    pub fn new(
        store: &Arc<dyn RequestStore>,
        secrets: &Arc<dyn GrantSecrets>,
        key_ids: &[String],
        clock: &Arc<dyn Clock>,
    ) -> Result<Self, &'static str> {
        let mut all = key_ids.iter().map(|id| {
            ProductionRequestAuthority::new(
                Arc::clone(store),
                Arc::clone(secrets),
                SecretRef::new(id.as_str()),
                Arc::clone(clock),
            )
        });
        let current = all.next().ok_or("key_ids")?;
        Ok(Self {
            current,
            older: all.collect(),
        })
    }

    /// Issues a grant valid until `expires_at` under the current key.
    ///
    /// # Errors
    /// The authority's refusal.
    pub fn issue_until(
        &self,
        request: &str,
        node: &str,
        attempt: u32,
        actor: &str,
        expires_at: time::OffsetDateTime,
    ) -> Result<String, GrantError> {
        self.current
            .issue_until(request, node, attempt, actor, expires_at)
    }
}

impl RequestAuthority for KeyedAuthority {
    fn issue(
        &self,
        request: &str,
        node: &str,
        attempt: u32,
        actor: &str,
    ) -> Result<String, GovernanceError> {
        self.current.issue(request, node, attempt, actor)
    }

    fn authenticate(&self, command_id: &str, actor: &str) -> Option<VerifiedRequest> {
        self.current.authenticate(command_id, actor).or_else(|| {
            self.older
                .iter()
                .find_map(|authority| authority.authenticate(command_id, actor))
        })
    }

    fn process_of(&self, request: &str) -> Option<String> {
        self.current.process_of(request)
    }

    fn may_create(&self, step: &VerifiedRequest, doctype: &str) -> bool {
        self.current.may_create(step, doctype)
    }
}

/// What the cache knows of one open step.
#[derive(Clone, Debug)]
struct Entry {
    process: String,
    attempt: u32,
    doctypes: BTreeSet<String>,
}

/// The authority's request store: a cache of the ledger's open steps. The ledger is the truth
/// (the write that uses a grant checks it inside its own transaction); the cache answers the
/// authority's synchronous questions without leaving the runtime thread.
#[derive(Default)]
pub struct LedgerRequests {
    steps: RwLock<BTreeMap<(String, String), Entry>>,
}

impl LedgerRequests {
    /// An empty cache: no request is open.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the cache by the ledger's open steps.
    pub fn replace(&self, slots: Vec<Slot>) {
        let mut next = BTreeMap::new();
        for slot in slots {
            next.insert(
                (slot.request, slot.node),
                Entry {
                    process: slot.process,
                    attempt: slot.attempt,
                    doctypes: slot.doctypes.into_iter().collect(),
                },
            );
        }
        *self
            .steps
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
    }

    /// Notes one step as open, as soon as its grant is issued (a refresh would take a tick).
    pub fn open(&self, attempt: &Attempt) {
        self.steps
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                (attempt.request.clone(), attempt.node.clone()),
                Entry {
                    process: attempt.process.clone(),
                    attempt: attempt.attempt,
                    doctypes: attempt.doctypes.iter().cloned().collect(),
                },
            );
    }

    /// Notes every step of `request` as closed.
    pub fn close(&self, request: &str) {
        self.steps
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .retain(|(r, _), _| r != request);
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<(String, String), Entry>> {
        self.steps
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl RequestStore for LedgerRequests {
    fn process_of(&self, request: &str) -> Option<String> {
        self.read()
            .iter()
            .find(|((r, _), _)| r == request)
            .map(|(_, entry)| entry.process.clone())
    }

    fn current_attempt(&self, request: &str, node: &str) -> Option<u32> {
        self.read()
            .get(&(request.to_owned(), node.to_owned()))
            .map(|entry| entry.attempt)
    }

    fn may_create(&self, request: &str, node: &str, doctype: &str) -> bool {
        self.read()
            .get(&(request.to_owned(), node.to_owned()))
            .is_some_and(|entry| entry.doctypes.contains(doctype))
    }
}

/// Why a grant was not issued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IssueError {
    /// The request is closed or the step is already at a newer attempt.
    NotOpen,
    /// The authority refused (a blank id, an actor that is not a typed reference, no key).
    Authority(GrantError),
    /// The ledger could not be written.
    Ledger(LedgerError),
}

impl IssueError {
    /// The stable code the refusal carries.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Authority(error) => error.code(),
            Self::NotOpen => spot_bpm_request_authority::codes::ATTEMPT_NOT_CURRENT,
            Self::Ledger(_) => spot_bpm_governance::codes::SETTINGS,
        }
    }
}

/// The issuing side: what an engine step calls to be handed the grant its document writes run
/// under, and to close a request whose grants must die.
///
/// Offered to later modules as a port (`ctx.require_port::<GrantHost>()`).
pub struct GrantHost {
    ledger: Ledger,
    authority: Arc<KeyedAuthority>,
    requests: Arc<LedgerRequests>,
    clock: Arc<dyn Clock>,
    ttl: Duration,
}

impl GrantHost {
    /// The host over `ledger`, signing with `authority` and caching through `requests`.
    #[must_use]
    pub fn new(
        ledger: Ledger,
        authority: Arc<KeyedAuthority>,
        requests: Arc<LedgerRequests>,
        clock: Arc<dyn Clock>,
        ttl_secs: u32,
    ) -> Self {
        Self {
            ledger,
            authority,
            requests,
            clock,
            ttl: Duration::seconds(i64::from(ttl_secs)),
        }
    }

    /// The authority the guards verify with.
    #[must_use]
    pub fn authority(&self) -> Arc<dyn RequestAuthority> {
        Arc::clone(&self.authority) as Arc<dyn RequestAuthority>
    }

    /// The cache the authority reads.
    #[must_use]
    pub fn requests(&self) -> &Arc<LedgerRequests> {
        &self.requests
    }

    /// Hands step `node` of `request` a grant for `attempt`, valid for `doctypes`, to `actor`
    /// (a typed principal reference: `user:<id>`, `consumer:<id>` or `system`).
    ///
    /// The step's row is written first, so the grant is never worth anything without it; a
    /// newer attempt replaces the row, which kills every older grant of the step.
    ///
    /// # Errors
    /// [`IssueError`].
    pub async fn open_attempt(
        &self,
        request: &str,
        process: &str,
        node: &str,
        attempt: u32,
        doctypes: &[String],
        actor: &str,
    ) -> Result<String, IssueError> {
        let now = self.clock.now_utc();
        let expires = now.checked_add(self.ttl).ok_or(IssueError::Authority(
            GrantError::LifetimeTooLong { max_minutes: 15 },
        ))?;
        let step = Attempt {
            request: request.to_owned(),
            process: process.to_owned(),
            node: node.to_owned(),
            attempt,
            doctypes: doctypes.to_vec(),
            actor: actor.to_owned(),
            opened_us: micros(now),
            expires_us: micros(expires),
        };
        // Blank ids and untyped actors are refused before anything is written.
        if request.trim().is_empty() || node.trim().is_empty() {
            return Err(IssueError::Authority(GrantError::IdentifierBlank));
        }
        if !spot_bpm_request_authority::is_typed_actor(actor) {
            return Err(IssueError::Authority(GrantError::ActorNotTyped));
        }
        if !self
            .ledger
            .open_attempt(&step)
            .await
            .map_err(IssueError::Ledger)?
        {
            return Err(IssueError::NotOpen);
        }
        self.requests.open(&step);
        self.authority
            .issue_until(request, node, attempt, actor, expires)
            .map_err(IssueError::Authority)
    }

    /// Closes `request`: every grant issued for it stops working, at once on this node and on
    /// the next refresh on another.
    ///
    /// # Errors
    /// The ledger could not be written.
    pub async fn close_request(&self, request: &str) -> Result<(), LedgerError> {
        self.ledger
            .close_request(request, micros(self.clock.now_utc()))
            .await?;
        self.requests.close(request);
        Ok(())
    }

    /// Reloads the cache from the ledger and removes what is long dead. Returns how many steps
    /// are open.
    ///
    /// # Errors
    /// The ledger could not be read.
    pub async fn refresh(&self) -> Result<usize, LedgerError> {
        let now = self.clock.now_utc();
        let slots = self.ledger.open_slots(micros(now)).await?;
        let open = slots.len();
        self.requests.replace(slots);
        // Rows closed or expired more than a day ago are of no use to anyone.
        let cutoff = now.checked_sub(Duration::days(1)).map_or(i64::MIN, micros);
        let _ = self.ledger.reap(cutoff).await;
        Ok(open)
    }
}

/// Unix microseconds of an instant.
#[must_use]
pub fn micros(at: time::OffsetDateTime) -> i64 {
    i64::try_from(at.unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_cache_answers_the_three_questions_and_forgets_a_closed_request() {
        let cache = LedgerRequests::new();
        cache.replace(vec![Slot {
            request: "r1".to_owned(),
            node: "n1".to_owned(),
            process: "p".to_owned(),
            attempt: 2,
            doctypes: vec!["invoice".to_owned()],
        }]);
        assert_eq!(cache.process_of("r1").as_deref(), Some("p"));
        assert_eq!(cache.current_attempt("r1", "n1"), Some(2));
        assert!(cache.may_create("r1", "n1", "invoice"));
        assert!(!cache.may_create("r1", "n1", "item"));
        assert!(!cache.may_create("r1", "other", "invoice"));
        cache.close("r1");
        assert_eq!(cache.process_of("r1"), None);
    }

    #[test]
    fn a_missing_key_fails_closed() {
        let secrets = DerivedSecrets::new();
        assert!(secrets.is_empty());
        assert_eq!(
            secrets.resolve(&SecretRef::new("grant-1")).unwrap_err(),
            GrantError::SecretUnavailable
        );
    }

    #[test]
    fn micros_of_the_epoch_is_zero() {
        assert_eq!(micros(time::OffsetDateTime::UNIX_EPOCH), 0);
    }
}
