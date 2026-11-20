//! Deterministic ids and the declared users: no clock, no randomness.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_app_api::BoxFuture;
use spot_app_api::prelude::RecordId;
use spot_document::engine::{IdGenerator, PrincipalSource};
use spot_permissions::Principal;

/// A UUIDv7-shaped id from a counter: the same run gives the same ids.
pub(crate) fn id_from(counter: u64) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[0] = 0x01;
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    let tail = counter.to_be_bytes();
    // The counter fills bytes 9..16 (56 bits); byte 8 keeps the variant bits.
    bytes[9..16].copy_from_slice(&tail[1..8]);
    RecordId::from_bytes(bytes).unwrap_or_else(|_| RecordId::new_v7())
}

/// The engine's id generator: 1, 2, 3, ... as UUIDv7-shaped ids.
#[derive(Debug, Default)]
pub struct SeqIds {
    next: AtomicU64,
}

impl IdGenerator for SeqIds {
    fn next_id(&self) -> RecordId {
        id_from(self.next.fetch_add(1, Ordering::SeqCst) + 1)
    }
}

/// The declared users: name, record id and principal. Declaring a user takes effect at once.
#[derive(Debug, Default)]
pub struct TestPrincipals {
    by_id: Mutex<BTreeMap<RecordId, Principal>>,
}

impl TestPrincipals {
    pub(crate) fn set(&self, id: RecordId, principal: Principal) {
        self.by_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, principal);
    }
}

impl PrincipalSource for TestPrincipals {
    fn resolve<'a>(&'a self, user: &'a RecordId) -> BoxFuture<'a, Principal> {
        let found = self
            .by_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(user)
            .cloned();
        // An unknown user has no assignments, never an error (the engine's own rule).
        Box::pin(async move { found.unwrap_or_else(|| Principal::with_roles("unknown", &[])) })
    }
}
