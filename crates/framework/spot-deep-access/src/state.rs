//! Authoritative replay and irreversible revocation boundaries.
use crate::DeepError;
use spot_signed_command::KeyId;
use std::collections::{BTreeMap, BTreeSet};

/// The authoritative customer-key-scoped receipt nonce store.
/// Consumption must be atomic and shared across all issuer instances.
pub trait ReceiptNonces {
    /// Consume once. False means replay; storage failures must fail closed.
    fn consume(&mut self, tenant: &str, key: &KeyId, nonce: [u8; 16]) -> Result<bool, DeepError>;
}
/// Reference in-memory receipt nonce store. Keep one instance per deployment test.
#[derive(Default)]
pub struct MemoryNonces {
    used: BTreeSet<(String, KeyId, [u8; 16])>,
}
impl ReceiptNonces for MemoryNonces {
    fn consume(&mut self, tenant: &str, key: &KeyId, nonce: [u8; 16]) -> Result<bool, DeepError> {
        Ok(self.used.insert((tenant.to_owned(), key.clone(), nonce)))
    }
}
/// Irreversible revocation details.
#[derive(Clone, Debug)]
pub struct Revocation {
    /// Customer identity or platform anomaly rule.
    pub by: String,
    /// Customer-readable reason.
    pub reason: String,
    /// Unix seconds.
    pub at: i64,
}
/// Authoritative revocation lookup, consulted on every operation.
pub trait RevocationView {
    /// None only when the session is not revoked. Errors refuse access.
    fn lookup(&self, tenant: &str, session: [u8; 16]) -> Result<Option<Revocation>, DeepError>;
}
/// Revocation storage. Implementations must atomically preserve the first revocation.
pub trait RevocationStore: RevocationView {
    /// Record final revocation; no un-revoke operation exists.
    fn insert(
        &mut self,
        tenant: &str,
        session: [u8; 16],
        revocation: Revocation,
    ) -> Result<(), DeepError>;
}
/// Reference irreversible in-memory revocation store.
#[derive(Default)]
pub struct MemoryRevocations {
    entries: BTreeMap<(String, [u8; 16]), Revocation>,
}
impl RevocationView for MemoryRevocations {
    fn lookup(&self, tenant: &str, session: [u8; 16]) -> Result<Option<Revocation>, DeepError> {
        Ok(self.entries.get(&(tenant.to_owned(), session)).cloned())
    }
}
impl RevocationStore for MemoryRevocations {
    fn insert(
        &mut self,
        tenant: &str,
        session: [u8; 16],
        revocation: Revocation,
    ) -> Result<(), DeepError> {
        self.entries
            .entry((tenant.to_owned(), session))
            .or_insert(revocation);
        Ok(())
    }
}
/// Per-session counters and denial tracking. The service must hold one shared instance
/// across every authorise call, serialising access; a counter is not operator input.
#[derive(Default, Debug)]
pub struct UseState {
    pub(crate) sessions: BTreeMap<[u8; 16], SessionState>,
}
#[derive(Default, Debug)]
pub(crate) struct SessionState {
    pub counter: Option<u64>,
    pub denials: u32,
    pub steps: BTreeSet<crate::Hash>,
}
