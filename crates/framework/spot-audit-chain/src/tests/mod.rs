//! Tests of the audit chain. Unit tests so they can reach the in-memory store's entries to tamper
//! with them; no public API can.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod chain;
mod golden;
mod prop;
mod tamper;

use std::sync::Arc;

use crate::{
    AuditChain, AuditRecord, ChainEntry, CheckpointPolicy, InMemoryStore, KeyRing, LocalSigner,
    Payload, PayloadValue, Subject,
};

pub(crate) const SITE: &str = "site-1";

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn record(n: i64) -> AuditRecord {
    let mut payload = Payload::new();
    payload.insert("n".to_owned(), PayloadValue::Int(n));
    payload.insert("note".to_owned(), PayloadValue::Text(format!("entry {n}")));
    payload.insert("ok".to_owned(), PayloadValue::Bool(n % 2 == 0));
    AuditRecord {
        kind: "doc.update".to_owned(),
        actor: "user:u1".to_owned(),
        on_behalf_of: Some("user:boss".to_owned()),
        subject: Some(Subject {
            doctype: "Invoice".to_owned(),
            id: format!("INV-{n}"),
        }),
        at: 1_000_000 + n,
        payload,
        masked: true,
    }
}

pub(crate) fn signer(seed: u8) -> Arc<LocalSigner> {
    Arc::new(LocalSigner::from_seed([seed; 32]))
}

pub(crate) fn ring(signers: &[&LocalSigner]) -> KeyRing {
    let mut ring = KeyRing::new();
    for s in signers {
        ring.add(&s.public_key()).unwrap();
    }
    ring
}

/// A chain of `n` records with a checkpoint every `every` entries, and the ring that verifies it.
pub(crate) fn built(n: i64, every: u64) -> (AuditChain<InMemoryStore>, KeyRing) {
    let s = signer(1);
    let ring = ring(&[&s]);
    let chain = AuditChain::new(InMemoryStore::new(), SITE, s, CheckpointPolicy { every });
    for i in 1..=n {
        futures::executor::block_on(chain.append(record(i))).unwrap();
    }
    (chain, ring)
}

/// Edits the stored entries in place (what an attacker with database access can do).
pub(crate) fn mutate(chain: &AuditChain<InMemoryStore>, f: impl FnOnce(&mut Vec<ChainEntry>)) {
    let mut state = chain.store().state.lock().unwrap();
    f(state.entries.get_mut(SITE).unwrap());
}
