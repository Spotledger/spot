//! The customer-side view: completeness against what was executed, extension history,
//! range filtering, and tamper detection on the tenant's chain.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use common::*;
use spot_audit_chain::InMemoryStore;
use spot_audit_chain::{AppendOutcome, AuditStore, ChainEntry, Checkpoint, StoreFuture};
use spot_support_grants::{CustomerLog, GrantStore, codes};
use spot_support_session::Tier;

#[test]
fn the_view_shows_who_what_scope_what_was_done_and_what_changed() {
    let mut h = Harness::new();
    h.grant(
        "g-view",
        Tier::T1,
        scope(&["Factura", "Payment"], &["read", "update"]),
        100,
        400,
    );
    let session = session("g-view");
    h.plane().start_session(TENANT, "g-view", 100).unwrap();
    assert!(
        h.plane()
            .attempt(&session, &read("Factura"), 110)
            .is_allowed()
    );
    assert!(
        h.plane()
            .attempt(
                &session,
                &action("update", Tier::T1, Some("Payment")).changing("set Payment.status to Paid"),
                120,
            )
            .is_allowed()
    );
    assert_eq!(
        h.plane().attempt(&session, &read("Secret"), 130).code(),
        "session.doctype_not_in_scope"
    );
    h.plane().extend(TENANT, "g-view", 600, ADMIN, 140).unwrap();
    h.plane()
        .stop_session(TENANT, "g-view", "done", 150)
        .unwrap();
    h.plane()
        .revoke(TENANT, "g-view", ADMIN, "customer request", 160)
        .unwrap();

    let sessions = h.sessions();
    assert_eq!(sessions.len(), 1);
    let record = &sessions[0];
    assert_eq!(record.session, "g-view");
    assert_eq!(record.tenant, TENANT);
    assert_eq!(record.who, AGENT);
    assert_eq!(record.who_kind, "agent");
    assert_eq!(record.support_org, ORG);
    assert_eq!(record.tier, "T1");
    assert!(record.scope.contains("Factura"));
    assert_eq!(record.requested_at, Some(100));
    assert_eq!(record.approved_at, Some(100));
    assert_eq!(record.started_at, Some(100));
    assert_eq!(record.stopped_at, Some(150));
    assert_eq!(record.revoked_at, Some(160));
    assert_eq!(record.revocation_reason, "customer request");

    assert_eq!(record.actions.len(), 3);
    assert_eq!(record.actions[0].action, "read");
    assert_eq!(record.actions[0].tier, "T0");
    assert_eq!(record.actions[0].outcome, "allowed");
    assert_eq!(record.actions[1].tier, "T1");
    assert_eq!(record.actions[1].outcome, "allowed");
    assert_eq!(record.actions[1].doctype.as_deref(), Some("Payment"));
    assert_eq!(record.actions[2].outcome, "denied");
    assert_eq!(record.actions[2].code, "session.doctype_not_in_scope");

    assert_eq!(record.changes, ["set Payment.status to Paid"]);
    assert_eq!(record.extensions.len(), 1);
    assert_eq!(record.extensions[0].to, 600);
    assert_eq!(record.extensions[0].by, ADMIN);
    assert!(record.chain_verified);
    assert!(record.failures.is_empty());
}

#[test]
fn the_view_filters_by_the_requested_range() {
    let mut h = Harness::new();
    h.grant(
        "g-range",
        Tier::T1,
        scope(&["Factura"], &["read"]),
        100,
        400,
    );
    let session = session("g-range");
    h.plane().start_session(TENANT, "g-range", 100).unwrap();
    h.plane().attempt(&session, &read("Factura"), 110);
    h.plane().attempt(&session, &read("Factura"), 220);
    h.plane()
        .stop_session(TENANT, "g-range", "done", 230)
        .unwrap();

    let early = h.view().sessions(TENANT, 0..=150).unwrap();
    assert_eq!(early.len(), 1);
    assert_eq!(early[0].actions.len(), 1);
    assert_eq!(early[0].stopped_at, None);

    let late = h.view().sessions(TENANT, 200..=300).unwrap();
    assert_eq!(late.len(), 1);
    assert_eq!(late[0].actions.len(), 1);
    assert_eq!(late[0].actions[0].at, 220);
    assert_eq!(late[0].stopped_at, Some(230));

    let empty = h.view().sessions(TENANT, 1000..=2000).unwrap();
    assert!(empty.is_empty());

    // The view only reads its own tenant's chain.
    assert_eq!(
        h.view().sessions("tenant-b", 0..=1000).unwrap_err().code(),
        codes::WRONG_TENANT
    );
}

#[test]
fn extension_is_a_new_customer_approval_with_a_history() {
    let mut h = Harness::new();
    h.grant("g-ext", Tier::T1, scope(&["Factura"], &["read"]), 0, 3000);

    // A non-administrator cannot extend, and neither can the subject or the requester.
    assert_eq!(
        h.plane()
            .extend(TENANT, "g-ext", 3400, "stranger", 100)
            .unwrap_err()
            .code(),
        codes::NOT_ADMIN
    );
    h.admins.add(TENANT, AGENT, true);
    h.admins.add(TENANT, REQUESTER, true);
    assert_eq!(
        h.plane()
            .extend(TENANT, "g-ext", 3400, AGENT, 100)
            .unwrap_err()
            .code(),
        codes::SELF_APPROVAL
    );
    assert_eq!(
        h.plane()
            .extend(TENANT, "g-ext", 3400, REQUESTER, 100)
            .unwrap_err()
            .code(),
        codes::SELF_APPROVAL
    );

    // A customer administrator may, within the total duration cap (4 hours from start).
    h.plane()
        .extend(TENANT, "g-ext", 3400, ADMIN2, 100)
        .unwrap();
    assert!(
        h.grants
            .get(TENANT, "g-ext")
            .unwrap()
            .unwrap()
            .is_running(3399)
    );
    assert_eq!(
        h.plane()
            .extend(TENANT, "g-ext", 20_000, ADMIN, 110)
            .unwrap_err()
            .code(),
        codes::DURATION_OVER_POLICY
    );

    // Once expired, an extension is refused: the way back is a new grant.
    assert_eq!(
        h.plane()
            .extend(TENANT, "g-ext", 14_000, ADMIN, 3700)
            .unwrap_err()
            .code(),
        codes::EXTENSION_REFUSED
    );
    h.plane().start_session(TENANT, "g-ext", 2000).unwrap();
    let record = h
        .sessions()
        .into_iter()
        .find(|s| s.session == "g-ext")
        .unwrap();
    assert_eq!(record.extensions.len(), 1);
    assert_eq!(record.extensions[0].to, 3400);
    assert!(record.chain_verified);
}

/// A store wrapper that tampers with what a reader sees, to prove the view notices.
#[derive(Clone, Default)]
struct TamperStore {
    inner: Arc<InMemoryStore>,
    tamper: Arc<AtomicU8>,
}

impl TamperStore {
    fn set(&self, mode: u8) {
        self.tamper.store(mode, Ordering::SeqCst);
    }
}

impl AuditStore for TamperStore {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        self.inner.head(site)
    }

    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        self.inner.append_if_head(site, expected_seq, entry)
    }

    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        Box::pin(async move {
            let mut entries = self.inner.range(site, from, to).await?;
            match self.tamper.load(Ordering::SeqCst) {
                1 => {
                    if let Some(entry) = entries.first_mut() {
                        entry.record.actor = "user:attacker".to_owned();
                    }
                }
                2 => {
                    if !entries.is_empty() {
                        entries.remove(0);
                    }
                }
                3 => {
                    entries.pop();
                }
                _ => {}
            }
            Ok(entries)
        })
    }

    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        self.inner.put_checkpoint(checkpoint)
    }

    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.inner.latest_checkpoint(site)
    }
}

#[test]
fn a_tampered_chain_is_reported_on_every_session() {
    let tamper = TamperStore::default();
    let mut h = Harness::with_store(tamper.clone());
    h.grant(
        "g-tamper",
        Tier::T1,
        scope(&["Factura"], &["read"]),
        100,
        400,
    );
    let session = session("g-tamper");
    h.plane().start_session(TENANT, "g-tamper", 100).unwrap();
    h.plane().attempt(&session, &read("Factura"), 110);
    h.plane()
        .stop_session(TENANT, "g-tamper", "done", 120)
        .unwrap();

    // Untouched, the view verifies.
    let clean = h.sessions();
    assert!(clean.iter().all(|record| record.chain_verified));

    for mode in 1..=3 {
        tamper.set(mode);
        let sessions = h.view().sessions(TENANT, 0..=i64::MAX).unwrap();
        assert!(!sessions.is_empty());
        for record in &sessions {
            assert!(
                !record.chain_verified,
                "tamper mode {mode} must be detected"
            );
            assert!(
                !record.failures.is_empty(),
                "tamper mode {mode} must name a failure"
            );
        }
    }
}

#[test]
fn a_vendor_cannot_write_into_another_tenants_record() {
    let mut h = Harness::new();
    h.grant("g-own", Tier::T1, scope(&["Factura"], &["read"]), 100, 400);
    let mut event = spot_support_grants::GrantEvent {
        kind: spot_support_grants::EventKind::Action,
        tenant: "tenant-b".to_owned(),
        session: "g-own".to_owned(),
        who: AGENT.to_owned(),
        who_kind: spot_support_session::SubjectKind::Agent,
        subject: AGENT.to_owned(),
        subject_kind: spot_support_session::SubjectKind::Agent,
        support_org: ORG.to_owned(),
        tier: Tier::T0,
        scope: String::new(),
        action: "read".to_owned(),
        doctype: None,
        outcome: "allowed".to_owned(),
        code: codes::ALLOWED.to_owned(),
        change: String::new(),
        reason: String::new(),
        extended_to: None,
        at: 1,
    };
    assert_eq!(
        h.log.append(event.clone()).unwrap_err().code(),
        codes::WRONG_TENANT
    );
    event.tenant = TENANT.to_owned();
    h.log.append(event).expect("the tenant's own event appends");
    // The trait offers append only: no update, no delete, no truncate.
    assert_eq!(h.log.tenant(), TENANT);
}
