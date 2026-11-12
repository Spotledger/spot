//! The signed revocation list: issuing, keeping and serving it.
//!
//! The list is `spot-bundle-signing`'s fresh (V2) list, built, signed and encoded only through
//! that crate (`issue_fresh_revocation_list`, `to_wire_json`); nothing here writes the JSON.
//! The signer is the one store key behind the `RevocationSigner` port.
//!
//! Entries: revoked signing keys and bundles come from the state store; revoked publishers are
//! the `revoke_publisher` entries of its audit log (neither port lists suspended publishers, and
//! the audit log is append-only, so it cannot lose one). A key revoked by the operator is listed
//! as compromised, which refuses every signature it ever made.
//!
//! A list is issued at start when the stored one is missing, stale, due or different, whenever
//! an entry changes (the revoke operations call [`Revocations::reissue`] before they answer),
//! and by the maintenance task at the refresh interval. The last list is kept with
//! `record_issued_list`, so a restart serves the same bytes and the same `ETag`. The server
//! also keeps an in-process V1 list for its own publish checks (`RegistryTrust`); it is never
//! served.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use spot_bundle_signing::{
    FreshRevocationBody, FreshRevocationList, KeyId, RevocationBody, RevocationList,
    RevocationSigner, RevokedBundle, RevokedKey, RevokedPublisher, TrustAnchor, TrustedRoot,
    issue_fresh_revocation_list, issue_revocation_list, verify_fresh_revocation_list,
};
use spot_registry::RegistryTrust;
use spot_store_state::{AuditKind, IssuedList, MAX_AUDIT_PAGE, StoreStateStore};

use crate::config::StartupError;
use crate::error::ApiError;
use crate::keys::StoreKey;

/// The revocation entries of one list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Entries {
    pub keys: Vec<RevokedKey>,
    pub publishers: Vec<RevokedPublisher>,
    pub bundles: Vec<RevokedBundle>,
}

impl Entries {
    fn sorted(mut self) -> Self {
        self.keys.sort_by(|a, b| a.key_id.cmp(&b.key_id));
        self.publishers
            .sort_by(|a, b| a.publisher_id.cmp(&b.publisher_id));
        self.bundles.sort_by(|a, b| a.digest_hex.cmp(&b.digest_hex));
        self
    }
}

/// The list as served: the wire JSON of `FreshRevocationList::to_wire_json`.
#[derive(Clone, Debug)]
pub(crate) struct Served {
    pub generation: u64,
    pub issued_at: i64,
    pub next_update: i64,
    pub body: Arc<String>,
}

impl Served {
    /// The strong entity tag of the list: its generation in quotes.
    pub fn etag(&self) -> String {
        format!("\"{}\"", self.generation)
    }
}

/// Revoked publishers found in the audit log so far.
#[derive(Default)]
struct PublisherCache {
    after_id: u64,
    /// publisher id to (time, reason); the first record wins.
    revoked: BTreeMap<String, (i64, String)>,
}

/// Reads the audit entries after the cache's position into the cache.
fn scan_audit(state: &dyn StoreStateStore, cache: &mut PublisherCache) -> Result<(), ApiError> {
    loop {
        let page = state.read_audit(cache.after_id, MAX_AUDIT_PAGE)?;
        for record in &page {
            cache.after_id = record.id;
            let entry = &record.entry;
            // A refused attempt is audited under the same kind with a `refused:` reason; it
            // revokes nothing.
            if entry.kind == AuditKind::RevokePublisher
                && !entry.reason.starts_with("refused:")
                && let Some(id) = &entry.publisher_id
            {
                cache
                    .revoked
                    .entry(id.clone())
                    .or_insert_with(|| (entry.at, entry.reason.clone()));
            }
        }
        if page.len() < usize::try_from(MAX_AUDIT_PAGE).unwrap_or(usize::MAX) {
            return Ok(());
        }
    }
}

fn collect(state: &dyn StoreStateStore, cache: &PublisherCache) -> Result<Entries, ApiError> {
    let mut keys = Vec::new();
    for revoked in state.revoked_keys()? {
        // The operator route validates the id; a stored id that does not parse is skipped.
        if let Ok(key_id) = KeyId::parse(&revoked.key_id) {
            keys.push(RevokedKey {
                key_id,
                effective_from: revoked.at,
                compromised: true,
                reason: revoked.reason,
            });
        }
    }
    let publishers = cache
        .revoked
        .iter()
        .map(|(id, (at, reason))| RevokedPublisher {
            publisher_id: id.clone(),
            effective_from: *at,
            reason: reason.clone(),
        })
        .collect();
    let bundles = state
        .revoked_bundles()?
        .into_iter()
        .map(|revoked| RevokedBundle {
            digest_hex: revoked.digest.to_hex(),
            reason: revoked.reason,
        })
        .collect();
    Ok(Entries {
        keys,
        publishers,
        bundles,
    }
    .sorted())
}

/// Issues, encodes and records a V2 list with the next generation.
fn issue_v2(
    state: &dyn StoreStateStore,
    key: &StoreKey,
    entries: &Entries,
    now: i64,
    lifetime: i64,
) -> Result<Served, ApiError> {
    let generation = state.next_generation(now)?;
    let next_update = now.saturating_add(lifetime);
    let body = FreshRevocationBody {
        generation,
        issued_at: now,
        next_update,
        issued_by: key.id().clone(),
        keys: entries.keys.clone(),
        publishers: entries.publishers.clone(),
        bundles: entries.bundles.clone(),
    };
    let signer: &dyn RevocationSigner = key;
    let list = issue_fresh_revocation_list(body, signer)?;
    let json = list.to_wire_json()?;
    state.record_issued_list(&IssuedList {
        generation,
        issued_at: now,
        next_update,
        body_json: json.clone(),
    })?;
    Ok(Served {
        generation,
        issued_at: now,
        next_update,
        body: Arc::new(json),
    })
}

/// The in-process V1 list the registry's own checks use, same entries, same key.
fn v1_list(key: &StoreKey, entries: &Entries, generation: u64, issued_at: i64) -> RevocationList {
    issue_revocation_list(
        RevocationBody {
            generation,
            issued_at,
            issued_by: key.id().clone(),
            keys: entries.keys.clone(),
            publishers: entries.publishers.clone(),
            bundles: entries.bundles.clone(),
        },
        key.pair(),
    )
}

/// The stored list, when it can be served again unchanged: it parses, verifies under the store
/// key, is not due for a refresh, and carries exactly the current entries.
fn reuse(
    state: &dyn StoreStateStore,
    key: &StoreKey,
    entries: &Entries,
    now: i64,
    refresh: i64,
) -> Option<Served> {
    let last = state.last_issued_list().ok()??;
    let list = FreshRevocationList::from_wire_json(last.body_json.as_bytes()).ok()?;
    let own = TrustAnchor::new(vec![TrustedRoot::new(key.public(), 0, None)]);
    verify_fresh_revocation_list(&list, &own, now, None).ok()?;
    let body = &list.body;
    if body.issued_by != *key.id()
        || body.generation != last.generation
        || body.issued_at != last.issued_at
        || body.next_update != last.next_update
        || now < last.issued_at
        || now - last.issued_at >= refresh
    {
        return None;
    }
    let stored = Entries {
        keys: body.keys.clone(),
        publishers: body.publishers.clone(),
        bundles: body.bundles.clone(),
    }
    .sorted();
    if stored != *entries {
        return None;
    }
    Some(Served {
        generation: last.generation,
        issued_at: last.issued_at,
        next_update: last.next_update,
        body: Arc::new(last.body_json),
    })
}

/// The revocation service: the served list and the way to replace it.
pub(crate) struct Revocations {
    state: Arc<dyn StoreStateStore>,
    key: Arc<StoreKey>,
    trust: Arc<RegistryTrust>,
    lifetime: i64,
    refresh: i64,
    served: Mutex<Served>,
    cache: Mutex<PublisherCache>,
    issue_lock: Mutex<()>,
}

impl Revocations {
    /// Loads or issues the first list and builds the registry's trust from it.
    pub fn open(
        state: Arc<dyn StoreStateStore>,
        key: Arc<StoreKey>,
        roots: Vec<TrustedRoot>,
        lifetime: i64,
        refresh: i64,
        now: i64,
    ) -> Result<Self, StartupError> {
        let failed = |what: &str, e: &ApiError| {
            StartupError::startup(format!(
                "could not prepare the revocation list ({what}): {}",
                e.message
            ))
        };
        let mut cache = PublisherCache::default();
        scan_audit(&*state, &mut cache).map_err(|e| failed("audit", &e))?;
        let entries = collect(&*state, &cache).map_err(|e| failed("entries", &e))?;
        let served = match reuse(&*state, &key, &entries, now, refresh) {
            Some(served) => served,
            None => {
                issue_v2(&*state, &key, &entries, now, lifetime).map_err(|e| failed("issue", &e))?
            }
        };
        let v1 = v1_list(&key, &entries, served.generation, served.issued_at);
        let mut all = vec![TrustedRoot::new(key.public(), 0, None)];
        all.extend(roots);
        let trust = RegistryTrust::new(TrustAnchor::new(all), v1, now).map_err(|e| {
            StartupError::startup(format!(
                "the trust anchor refused the first revocation list ({})",
                e.code
            ))
        })?;
        Ok(Self {
            state,
            key,
            trust: Arc::new(trust),
            lifetime,
            refresh,
            served: Mutex::new(served),
            cache: Mutex::new(cache),
            issue_lock: Mutex::new(()),
        })
    }

    /// The trust the registry core checks every publish against.
    pub fn trust(&self) -> Arc<RegistryTrust> {
        Arc::clone(&self.trust)
    }

    /// The list being served.
    pub fn served(&self) -> Served {
        self.served
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Whether the served list has reached its refresh time.
    pub fn refresh_due(&self, now: i64) -> bool {
        let served = self.served();
        now < served.issued_at || now - served.issued_at >= self.refresh
    }

    /// Whether the served list has passed its `next_update`.
    pub fn is_stale(&self, now: i64) -> bool {
        self.served().next_update <= now
    }

    /// Issues a new list from the current state, registers it with the registry's trust and
    /// starts serving it; returns its generation. Synchronous: the revoke operations call it
    /// before they answer.
    pub fn reissue(&self, now: i64) -> Result<u64, ApiError> {
        let _serial = self
            .issue_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let entries = {
            let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
            scan_audit(&*self.state, &mut cache)?;
            collect(&*self.state, &cache)?
        };
        let served = issue_v2(&*self.state, &self.key, &entries, now, self.lifetime)?;
        let v1 = v1_list(&self.key, &entries, served.generation, now);
        self.trust.update_revocations(v1, now)?;
        let generation = served.generation;
        *self.served.lock().unwrap_or_else(PoisonError::into_inner) = served;
        Ok(generation)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_store_state::{AuditEntry, BundleRevocation, KeyRevocation, MemoryStoreState};

    const NOW: i64 = 1_800_000_000;

    fn open(state: &Arc<MemoryStoreState>, key: &Arc<StoreKey>, now: i64) -> Revocations {
        let shared: Arc<dyn StoreStateStore> = state.clone();
        Revocations::open(shared, Arc::clone(key), Vec::new(), 86_400, 21_600, now).unwrap()
    }

    fn parse(served: &Served) -> FreshRevocationList {
        FreshRevocationList::from_wire_json(served.body.as_bytes()).unwrap()
    }

    #[test]
    fn the_first_open_issues_generation_one_and_a_restart_serves_the_same_bytes() {
        let state = Arc::new(MemoryStoreState::new());
        let key = Arc::new(StoreKey::from_seed(&[5; 32]));
        let first = open(&state, &key, NOW);
        let served = first.served();
        assert_eq!(served.generation, 1);
        assert_eq!(served.etag(), "\"1\"");
        assert_eq!(served.next_update, NOW + 86_400);
        let anchor = TrustAnchor::new(vec![TrustedRoot::new(key.public(), 0, None)]);
        verify_fresh_revocation_list(&parse(&served), &anchor, NOW, None).unwrap();

        let restarted = open(&state, &key, NOW + 60).served();
        assert_eq!(restarted.generation, 1);
        assert_eq!(
            restarted.body, served.body,
            "the same bytes and the same ETag"
        );
    }

    #[test]
    fn a_changed_entry_or_a_due_refresh_issues_a_higher_generation() {
        let state = Arc::new(MemoryStoreState::new());
        let key = Arc::new(StoreKey::from_seed(&[5; 32]));
        let service = open(&state, &key, NOW);
        assert!(!service.refresh_due(NOW + 100));
        assert!(service.refresh_due(NOW + 21_600));
        assert!(!service.is_stale(NOW + 100));
        assert!(service.is_stale(NOW + 86_400));

        state
            .revoke_key(&KeyRevocation {
                key_id: "6a3803d5f059902a1c6dafbc9ba47292".to_owned(),
                reason: "leaked".to_owned(),
                at: NOW + 5,
            })
            .unwrap();
        assert_eq!(service.reissue(NOW + 10).unwrap(), 2);
        let list = parse(&service.served());
        assert_eq!(list.body.keys.len(), 1);
        assert!(list.body.keys[0].compromised);

        // A restart with a changed entry does not reuse the stored list.
        state
            .revoke_bundle(&BundleRevocation {
                digest: "ab".repeat(32).parse().unwrap(),
                reason: "malware".to_owned(),
                at: NOW + 20,
            })
            .unwrap();
        assert_eq!(open(&state, &key, NOW + 30).served().generation, 3);
    }

    #[test]
    fn a_revoked_publisher_comes_from_the_audit_log_and_a_refused_attempt_revokes_nothing() {
        let state = Arc::new(MemoryStoreState::new());
        let key = Arc::new(StoreKey::from_seed(&[5; 32]));
        let service = open(&state, &key, NOW);
        let entry = |reason: &str, publisher: &str| AuditEntry {
            kind: AuditKind::RevokePublisher,
            actor: "operator".to_owned(),
            publisher_id: Some(publisher.to_owned()),
            app: None,
            version: None,
            digest: None,
            reason: reason.to_owned(),
            at: NOW + 3,
        };
        state.append_audit(&entry("takedown", "acme")).unwrap();
        state
            .append_audit(&entry(
                "refused: store_server.invalid_admin_request",
                "zeta",
            ))
            .unwrap();
        service.reissue(NOW + 4).unwrap();
        let list = parse(&service.served());
        assert_eq!(list.body.publishers.len(), 1);
        assert_eq!(list.body.publishers[0].publisher_id, "acme");
        assert_eq!(list.body.publishers[0].effective_from, NOW + 3);
        assert!(list.body.publishers[0].reason == "takedown");
    }

    #[test]
    fn consecutive_reissues_keep_the_registry_trust_in_step() {
        let state = Arc::new(MemoryStoreState::new());
        let key = Arc::new(StoreKey::from_seed(&[5; 32]));
        let service = open(&state, &key, NOW);
        // The V1 update only accepts a strictly higher generation: two reissues in a row work.
        service.reissue(NOW + 1).unwrap();
        service.reissue(NOW + 2).unwrap();
        assert_eq!(service.served().generation, 3);
    }
}
