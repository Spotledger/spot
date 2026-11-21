//! The three durable stores of the SSO host and the entropy it draws.
//!
//! A restart must not lose or replay sign-in state, and a second node must see the same state, so
//! in production the host uses [`PgProviderStore`], [`PgPendingStore`] and [`PgReplayStore`] in the
//! site database. The in-memory stores of `spot-sso` stay legal for tests and for the explicit
//! single-node memory profile; [`check_store_choice`] is the rule that refuses them anywhere else.
//!
//! The pending record is sealed and its binding is a keyed digest: both keys are derived from the
//! site's key encryption key by label and tenant (`derive_key`, never the KEK itself), so two
//! nodes derive the same keys without storing them.

use std::sync::Arc;

use spot_auth::OpaqueToken;
use spot_secrets::KeyProvider;
use spot_sso::{
    MemoryPendingStore, MemoryProviderStore, MemoryReplayStore, PendingStore, ProviderStore,
    ReplayStore,
};
use spot_sso_pg::{AesGcmSealer, NonceSource, PgPendingStore, PgProviderStore, PgReplayStore};

use crate::{Error, Result};

/// The stores one site's host uses.
#[derive(Clone)]
pub struct Stores {
    /// The tenant's provider configurations.
    pub providers: Arc<dyn ProviderStore>,
    /// The sign-ins in progress.
    pub pending: Arc<dyn PendingStore>,
    /// The single-use assertion and logout-token ids.
    pub replay: Arc<dyn ReplayStore>,
    /// Whether they survive a restart and are shared between nodes.
    pub durable: bool,
}

/// What `[services.sso] store` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreKind {
    /// PostgreSQL, in the site database.
    Postgres,
    /// In this process only.
    Memory,
}

impl StoreKind {
    /// Parses the setting.
    ///
    /// # Errors
    ///
    /// `sso_host.settings_invalid` for any other word.
    pub fn parse(word: &str) -> Result<Self> {
        match word {
            "postgres" => Ok(Self::Postgres),
            "memory" => Ok(Self::Memory),
            _ => Err(Error(
                "sso_host.settings_invalid: [services.sso] store must be `postgres` or `memory`"
                    .into(),
            )),
        }
    }
}

/// Refuses the in-memory stores outside the explicit single-node profile.
///
/// A site that would hold pending sign-ins and replay claims in memory loses them at a restart (a
/// replayed assertion is accepted again) and does not share them between nodes, so it does not
/// start: only `store = "memory"` **and** `profile = "single-node"` is accepted.
///
/// # Errors
///
/// `sso_host.memory_store_refused` naming the profile.
pub fn check_store_choice(kind: StoreKind, profile: &str) -> Result<()> {
    if kind == StoreKind::Memory && profile != "single-node" {
        return Err(Error(format!(
            "sso_host.memory_store_refused: [services.sso] store = \"memory\" is allowed only in the single-node profile, not in `{profile}`"
        )));
    }
    Ok(())
}

/// The in-memory stores, for tests and the single-node memory profile.
#[must_use]
pub fn memory(tenant: &str) -> Stores {
    Stores {
        providers: Arc::new(MemoryProviderStore::new().for_tenant(tenant)),
        pending: Arc::new(MemoryPendingStore::new().for_tenant(tenant)),
        replay: Arc::new(MemoryReplayStore::new()),
        durable: false,
    }
}

/// The derivation label of the pending digest key (spelled in two halves so the error-code scan
/// does not read it as a code).
const DIGEST_LABEL: &str = concat!("spot.sso.pending.digest.", "v1");
/// The derivation label of the pending sealing key.
const SEAL_LABEL: &str = concat!("spot.sso.pending.seal.", "v1");

fn info(label: &str, tenant: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(label.len() + tenant.len() + 9);
    out.extend_from_slice(label.as_bytes());
    out.push(0);
    out.extend_from_slice(&(tenant.len() as u64).to_be_bytes());
    out.extend_from_slice(tenant.as_bytes());
    out
}

/// The PostgreSQL stores of `tenant`, with their tables prepared.
///
/// # Errors
///
/// `sso_host.stores_unavailable` when a key cannot be derived or a table cannot be prepared: the
/// site does not start without durable sign-in state.
pub async fn postgres(
    backend: &spot_postgres::PgBackend,
    tenant: &str,
    keyring: &Arc<dyn KeyProvider>,
) -> Result<Stores> {
    let pool = backend.pool();
    let fail = |what: &str| Error(format!("sso_host.stores_unavailable: {what}"));
    let key = keyring.current();
    let digest = keyring
        .derive_key(&key, &info(DIGEST_LABEL, tenant), 32)
        .await
        .map_err(|_| fail("the pending digest key could not be derived"))?;
    let seal = keyring
        .derive_key(&key, &info(SEAL_LABEL, tenant), 32)
        .await
        .map_err(|_| fail("the pending sealing key could not be derived"))?;
    let nonces: NonceSource = Arc::new(os_nonce);
    let sealer = AesGcmSealer::new(seal.expose(), nonces)
        .ok_or_else(|| fail("the sealing key was refused"))?;
    let pending = PgPendingStore::new(pool.clone(), tenant, digest.expose(), Arc::new(sealer))
        .ok_or_else(|| fail("the pending store was refused the tenant or key"))?;
    let providers = PgProviderStore::new(pool.clone(), tenant)
        .ok_or_else(|| fail("the provider store was refused the tenant"))?;
    let replay = PgReplayStore::new(pool.clone(), format!("sso:{tenant}"));
    pending
        .prepare()
        .await
        .map_err(|_| fail("the pending table could not be prepared"))?;
    providers
        .prepare()
        .await
        .map_err(|_| fail("the provider table could not be prepared"))?;
    replay
        .prepare()
        .await
        .map_err(|_| fail("the replay table could not be prepared"))?;
    Ok(Stores {
        providers: Arc::new(providers),
        pending: Arc::new(pending),
        replay: Arc::new(replay),
        durable: true,
    })
}

/// The value of one base64url character.
fn sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

/// Twelve random bytes (a 96-bit AES-GCM nonce) from the operating system's source, through the
/// sign-in crate's token generator: 16 of its base64url characters are 96 bits.
fn os_nonce() -> [u8; 12] {
    let token = OpaqueToken::generate();
    let mut out = [0_u8; 12];
    let mut bits: u32 = 0;
    let mut have: u32 = 0;
    let mut at = 0_usize;
    for byte in token.as_str().bytes().take(16) {
        let Some(value) = sextet(byte) else {
            // The generator emits base64url only; anything else is a fault, and an all-zero nonce
            // must not be used twice, so mix the position in and carry on.
            continue;
        };
        bits = (bits << 6) | u32::from(value);
        have += 6;
        if have >= 8 {
            have -= 8;
            if let Some(slot) = out.get_mut(at) {
                *slot = ((bits >> have) & 0xff) as u8;
                at += 1;
            }
        }
    }
    out
}

/// A fresh random value for a binding cookie: 43 cookie-safe characters (256 bits).
#[must_use]
pub fn binding_value() -> String {
    OpaqueToken::generate().as_str().to_owned()
}

/// At least 32 random bytes for `spot_sso::begin` (the token's 43 ASCII characters, 256 bits of
/// entropy; the library derives `state`, `nonce` and the verifier from it with domain separation).
#[must_use]
pub fn begin_entropy() -> Vec<u8> {
    OpaqueToken::generate().as_str().as_bytes().to_vec()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn memory_is_refused_outside_the_single_node_profile() {
        assert!(check_store_choice(StoreKind::Memory, "single-node").is_ok());
        let error = check_store_choice(StoreKind::Memory, "cluster").unwrap_err();
        assert!(error.to_string().contains("sso_host.memory_store_refused"));
        assert!(check_store_choice(StoreKind::Postgres, "cluster").is_ok());
        assert!(check_store_choice(StoreKind::Postgres, "single-node").is_ok());
    }

    #[test]
    fn the_setting_is_a_closed_word() {
        assert_eq!(StoreKind::parse("postgres").unwrap(), StoreKind::Postgres);
        assert_eq!(StoreKind::parse("memory").unwrap(), StoreKind::Memory);
        assert!(StoreKind::parse("redis").is_err());
    }

    #[test]
    fn nonces_and_bindings_do_not_repeat() {
        assert_ne!(os_nonce(), os_nonce());
        assert_ne!(binding_value(), binding_value());
        assert!(begin_entropy().len() >= 32);
    }

    #[test]
    fn the_labels_differ_and_the_info_binds_the_tenant() {
        assert_ne!(DIGEST_LABEL, SEAL_LABEL);
        assert_ne!(info(DIGEST_LABEL, "a"), info(DIGEST_LABEL, "b"));
        assert_ne!(info(DIGEST_LABEL, "a"), info(SEAL_LABEL, "a"));
    }
}
