//! The [`PendingStore`] and [`ProviderStore`] contract suites (feature `testing`).
//!
//! The in-memory stores here and the PostgreSQL ones (WP-IAM-05.T9) must pass [`run_pending`]
//! and [`run_providers`]. Each takes a factory returning **two stores over one fresh backend**:
//! the first is tenant A's, the second tenant B's, which is how tenant isolation is proved.
//! The suites are synchronous and drive the futures with [`block_on`].

use std::sync::Arc;
use std::thread;

use spot_core::Audience;

use crate::contract::block_on;
use crate::pending::{PendingAuth, PendingError, PendingStore};
use crate::provider::{ClaimsMap, ClientAuth, Jwa, ProviderConfig, ProviderId};
use crate::provider_store::{ProviderStore, ProviderStoreError};
use crate::replay::StoreFuture;

/// A pending store that is always down: every call is a [`PendingError`].
#[derive(Clone, Copy, Debug, Default)]
pub struct FailingPendingStore;

impl PendingStore for FailingPendingStore {
    fn put<'a>(
        &'a self,
        _binding: &'a str,
        _pending: &'a PendingAuth,
    ) -> StoreFuture<'a, Result<(), PendingError>> {
        Box::pin(std::future::ready(Err(PendingError::unavailable())))
    }

    fn take<'a>(
        &'a self,
        _binding: &'a str,
        _now: i64,
    ) -> StoreFuture<'a, Result<Option<PendingAuth>, PendingError>> {
        Box::pin(std::future::ready(Err(PendingError::unavailable())))
    }
}

/// A pending authorisation whose secrets are recognisable by `tag`, valid from 0 to 600.
#[must_use]
pub fn sample_pending(tag: &str) -> PendingAuth {
    PendingAuth {
        provider: ProviderId::new("acme"),
        state: format!("state-{tag}"),
        nonce: format!("nonce-{tag}"),
        code_verifier: format!("verifier-{tag}"),
        code_challenge: format!("challenge-{tag}"),
        redirect_uri: "https://ledger.example.com/sso/callback".to_owned(),
        return_to: "/app".to_owned(),
        created_at: 0,
        expires_at: 600,
        used: false,
    }
}

/// A valid provider configuration with id `id` and the given audience.
#[must_use]
pub fn sample_provider(id: &str, audience: Audience) -> ProviderConfig {
    ProviderConfig {
        id: ProviderId::new(id),
        issuer: "https://idp.example.com".to_owned(),
        client_id: "spotledger".to_owned(),
        auth: ClientAuth::ClientSecretRef {
            secret_ref: format!("providers/{id}/client-secret"),
        },
        scopes: vec!["openid".to_owned(), "email".to_owned()],
        redirect_uri: "https://ledger.example.com/sso/callback".to_owned(),
        discovery_url: "https://idp.example.com/.well-known/openid-configuration".to_owned(),
        allowed_algs: vec![Jwa::Rs256],
        clock_skew_seconds: 60,
        max_logout_token_lifetime_seconds: 120,
        claims_map: ClaimsMap::default(),
        require_email_verified: true,
        acr_values: Vec::new(),
        audience,
        user_options: crate::provider::UserOptions::default(),
    }
}

/// Runs every pending-store case. `make` returns tenant A's and tenant B's store over one fresh
/// backend.
///
/// # Panics
///
/// When a store breaks the contract, naming the case.
pub fn run_pending<S, F>(make: F)
where
    S: PendingStore + 'static,
    F: Fn() -> (S, S),
{
    put_then_take_once(&make().0);
    take_of_an_unknown_binding_is_none(&make().0);
    an_expired_entry_is_not_returned_and_is_gone(&make().0);
    put_replaces_the_entry(&make().0);
    bindings_are_independent(&make().0);
    tenants_are_isolated(&make());
    concurrent_takes_give_one_winner(Arc::new(make().0));
}

fn put(store: &impl PendingStore, binding: &str, pending: &PendingAuth) {
    if let Err(error) = block_on(store.put(binding, pending)) {
        panic!("the store failed a put: {error}");
    }
}

fn take(store: &impl PendingStore, binding: &str, now: i64) -> Option<PendingAuth> {
    match block_on(store.take(binding, now)) {
        Ok(entry) => entry,
        Err(error) => panic!("the store failed a take: {error}"),
    }
}

/// An entry comes back once and only once, and comes back unchanged.
fn put_then_take_once(store: &impl PendingStore) {
    let pending = sample_pending("a");
    put(store, "binding-1", &pending);
    assert_eq!(take(store, "binding-1", 10), Some(pending), "first take");
    assert_eq!(take(store, "binding-1", 10), None, "second take");
}

fn take_of_an_unknown_binding_is_none(store: &impl PendingStore) {
    assert_eq!(take(store, "never-stored", 10), None);
}

/// At and after `expires_at` the entry is not returned, and it is removed by the attempt.
fn an_expired_entry_is_not_returned_and_is_gone(store: &impl PendingStore) {
    put(store, "binding-1", &sample_pending("a"));
    assert_eq!(take(store, "binding-1", 600), None, "at expires_at");
    assert_eq!(
        take(store, "binding-1", 1),
        None,
        "gone even for an earlier instant"
    );
    put(store, "binding-2", &sample_pending("b"));
    assert!(
        take(store, "binding-2", 599).is_some(),
        "just before expiry"
    );
}

fn put_replaces_the_entry(store: &impl PendingStore) {
    put(store, "binding-1", &sample_pending("old"));
    let newer = sample_pending("new");
    put(store, "binding-1", &newer);
    assert_eq!(take(store, "binding-1", 10), Some(newer));
    assert_eq!(take(store, "binding-1", 10), None);
}

fn bindings_are_independent(store: &impl PendingStore) {
    put(store, "binding-1", &sample_pending("a"));
    put(store, "binding-2", &sample_pending("b"));
    assert_eq!(take(store, "binding-2", 10), Some(sample_pending("b")));
    assert_eq!(take(store, "binding-1", 10), Some(sample_pending("a")));
}

/// Tenant B cannot take tenant A's binding, and trying does not consume it.
fn tenants_are_isolated<S: PendingStore>(stores: &(S, S)) {
    let (a, b) = stores;
    put(a, "shared-binding", &sample_pending("a"));
    assert_eq!(take(b, "shared-binding", 10), None, "another tenant");
    assert_eq!(
        take(a, "shared-binding", 10),
        Some(sample_pending("a")),
        "the owner still has it"
    );
    put(b, "shared-binding", &sample_pending("b"));
    put(a, "shared-binding", &sample_pending("a2"));
    assert_eq!(take(b, "shared-binding", 10), Some(sample_pending("b")));
    assert_eq!(take(a, "shared-binding", 10), Some(sample_pending("a2")));
}

/// 32 threads take one binding at once: exactly one gets it.
fn concurrent_takes_give_one_winner<S: PendingStore + 'static>(store: Arc<S>) {
    const THREADS: usize = 32;
    put(&*store, "contended", &sample_pending("a"));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let store = Arc::clone(&store);
            thread::spawn(move || block_on(store.take("contended", 10)))
        })
        .collect();
    let mut winners = 0;
    for handle in handles {
        match handle.join() {
            Ok(Ok(Some(_))) => winners += 1,
            Ok(Ok(None)) => {}
            Ok(Err(error)) => panic!("the store failed a take: {error}"),
            Err(_) => panic!("a taking thread panicked"),
        }
    }
    assert_eq!(winners, 1, "exactly one concurrent take wins");
}

/// Runs every provider-store case. `make` returns tenant A's and tenant B's store over one fresh
/// backend.
///
/// # Panics
///
/// When a store breaks the contract, naming the case.
pub fn run_providers<S, F>(make: F)
where
    S: ProviderStore,
    F: Fn() -> (S, S),
{
    put_get_list_delete(&make().0);
    the_audience_is_stored_with_the_row(&make().0);
    put_replaces(&make().0);
    an_invalid_configuration_is_refused_and_not_stored(&make().0);
    provider_tenants_are_isolated(&make());
}

fn must<T>(outcome: Result<T, ProviderStoreError>, what: &str) -> T {
    match outcome {
        Ok(value) => value,
        Err(error) => panic!("the store failed {what}: {error}"),
    }
}

fn put_get_list_delete(store: &impl ProviderStore) {
    let id = ProviderId::new("acme");
    assert_eq!(must(block_on(store.get(&id)), "an empty get"), None);
    assert!(
        must(block_on(store.list()), "an empty list")
            .providers
            .is_empty()
    );
    let config = sample_provider("acme", Audience::Staff);
    must(block_on(store.put(&config)), "a put");
    assert_eq!(
        must(block_on(store.get(&id)), "a get"),
        Some(config.clone())
    );
    let other = sample_provider("beta", Audience::Partner);
    must(block_on(store.put(&other)), "a second put");
    let listing = must(block_on(store.list()), "a list");
    assert_eq!(listing.providers, vec![config, other], "ordered by id");
    assert!(listing.skipped.is_empty());
    assert!(
        must(block_on(store.delete(&id)), "a delete"),
        "there was one"
    );
    assert!(!must(block_on(store.delete(&id)), "a second delete"));
    assert_eq!(must(block_on(store.get(&id)), "a get after delete"), None);
}

fn the_audience_is_stored_with_the_row(store: &impl ProviderStore) {
    for audience in Audience::ALL {
        let config = sample_provider(audience.as_str(), audience);
        must(block_on(store.put(&config)), "a put");
        let back = must(block_on(store.get(&config.id)), "a get").unwrap_or_else(|| {
            panic!("the {audience} provider vanished");
        });
        assert_eq!(back.audience, audience);
    }
}

fn put_replaces(store: &impl ProviderStore) {
    let mut config = sample_provider("acme", Audience::Staff);
    must(block_on(store.put(&config)), "a put");
    config.client_id = "changed".to_owned();
    must(block_on(store.put(&config)), "a replacing put");
    let listing = must(block_on(store.list()), "a list");
    assert_eq!(listing.providers, vec![config]);
}

fn an_invalid_configuration_is_refused_and_not_stored(store: &impl ProviderStore) {
    let mut config = sample_provider("acme", Audience::Staff);
    config.issuer = "http://idp.example.com".to_owned();
    match block_on(store.put(&config)) {
        Err(ProviderStoreError::Invalid(errors)) => {
            assert!(errors.has_code("sso.issuer_invalid"));
        }
        other => panic!("expected an invalid refusal, got {other:?}"),
    }
    assert_eq!(must(block_on(store.get(&config.id)), "a get"), None);
    let mut consumer = sample_provider("market", Audience::Consumer);
    consumer.user_options.link_by_email = true;
    assert!(matches!(
        block_on(store.put(&consumer)),
        Err(ProviderStoreError::Invalid(_))
    ));
}

/// Tenant B cannot read, list or delete tenant A's provider, and the same id may exist in both.
fn provider_tenants_are_isolated<S: ProviderStore>(stores: &(S, S)) {
    let (a, b) = stores;
    let config = sample_provider("acme", Audience::Staff);
    must(block_on(a.put(&config)), "a put");
    assert_eq!(
        must(block_on(b.get(&config.id)), "a cross-tenant get"),
        None
    );
    assert!(
        must(block_on(b.list()), "a cross-tenant list")
            .providers
            .is_empty()
    );
    assert!(!must(
        block_on(b.delete(&config.id)),
        "a cross-tenant delete"
    ));
    assert_eq!(
        must(block_on(a.get(&config.id)), "the owner's get"),
        Some(config.clone())
    );
    let mut theirs = sample_provider("acme", Audience::Partner);
    theirs.client_id = "tenant-b-client".to_owned();
    must(block_on(b.put(&theirs)), "tenant B's put");
    assert_eq!(must(block_on(a.get(&config.id)), "a get"), Some(config));
    assert_eq!(must(block_on(b.get(&theirs.id)), "b get"), Some(theirs));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pending::MemoryPendingStore;
    use crate::provider_store::MemoryProviderStore;

    #[test]
    fn the_memory_pending_store_passes_the_contract() {
        run_pending(|| {
            let a = MemoryPendingStore::new().for_tenant("a");
            let b = a.for_tenant("b");
            (a, b)
        });
    }

    #[test]
    fn the_memory_provider_store_passes_the_contract() {
        run_providers(|| {
            let a = MemoryProviderStore::new().for_tenant("a");
            let b = a.for_tenant("b");
            (a, b)
        });
    }

    #[test]
    fn a_failing_pending_store_refuses_with_the_stable_code() {
        let error = block_on(FailingPendingStore.take("x", 0));
        assert_eq!(
            error.map_err(|e| e.code()),
            Err("sso.pending_store_unavailable")
        );
        assert!(block_on(FailingPendingStore.put("x", &sample_pending("a"))).is_err());
    }

    #[test]
    fn an_invalid_stored_row_is_skipped_by_list_and_reported_by_get() {
        let store = MemoryProviderStore::new();
        let mut broken = sample_provider("old", Audience::Staff);
        broken.scopes = vec!["email".to_owned()];
        store.put_unchecked(broken);
        let good = sample_provider("good", Audience::Staff);
        must(block_on(store.put(&good)), "a put");
        let listing = must(block_on(store.list()), "a list");
        assert_eq!(listing.providers, vec![good]);
        assert_eq!(listing.skipped, vec![ProviderId::new("old")]);
        assert!(matches!(
            block_on(store.get(&ProviderId::new("old"))),
            Err(ProviderStoreError::Invalid(_))
        ));
    }
}
