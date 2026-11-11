#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Customer visibility: the view read from the audit chain, and the append-only archive
//! the vendor cannot delete from.

mod common;

use std::sync::Arc;

use common::*;
use spot_audit_chain::{AuditChain, AuditStore, CheckpointPolicy, InMemoryStore, LocalSigner};
use spot_break_glass::{
    BreakGlassError, BreakGlassSession, ChainLog, CustomerConsent, CustomerView, FileRecorder,
    FrameKind, Grant, Principal, Recorder, RecordingArchive, audit,
};
use spot_deep_access::AuditExecutor;
use spot_support_session::{Approver, SessionGrant, SessionPolicy};

fn chain_log() -> ChainLog<InMemoryStore, Ready> {
    let signer = Arc::new(LocalSigner::from_seed([9; 32]));
    let store = InMemoryStore::new();
    let chain = AuditChain::new(store, TENANT, signer, CheckpointPolicy { every: 1 });
    ChainLog::new(chain, Ready)
}

fn run_session(log: &mut ChainLog<InMemoryStore, Ready>) -> BreakGlassSession {
    let (mut session, requested) = BreakGlassSession::new(request(), policy(), 1000).unwrap();
    audit(log, &requested).unwrap();
    let consent = session
        .record_consent(
            &CustomerConsent {
                approver: Principal::customer("customer-owner"),
                at: 1000,
                receipt_hash: Some([9; 32]),
            },
            1000,
        )
        .unwrap();
    audit(log, &consent).unwrap();
    for approver in ["vendor-a", "vendor-b"] {
        let event = session.approve(&vendor(approver), 1000).unwrap();
        audit(log, &event).unwrap();
    }
    let spec = session.request().session_spec();
    let pending = SessionGrant::request(&spec, &SessionPolicy::default(), 1000).unwrap();
    let (grant, _) = pending
        .approve(
            &Approver::new("customer-owner", true),
            1000,
            &SessionPolicy::default(),
        )
        .unwrap();
    let active = session
        .activate(Grant::Scoped(Box::new(grant)), 1000)
        .unwrap();
    audit(log, &active).unwrap();
    let command = session.command(1500).unwrap();
    audit(log, &command).unwrap();
    let recording = session.record_recording([5; 32], 1600).unwrap();
    audit(log, &recording).unwrap();
    let ended = session.end("work finished", 2000).unwrap();
    audit(log, &ended).unwrap();
    session
}

#[test]
fn the_customer_view_is_complete_against_the_audit_chain() {
    let mut log = chain_log();
    run_session(&mut log);

    let entries = AuditExecutor::run(&Ready, log.chain().store().range(TENANT, 1, u64::MAX))
        .unwrap()
        .unwrap();
    assert!(entries.len() >= 8, "one entry per transition");

    let view = CustomerView::collect(&entries).unwrap();
    assert_eq!(view.sessions().len(), 1);
    let summary = view.session(SESSION).unwrap();
    assert_eq!(summary.who, "support-agent");
    assert_eq!(summary.reason, "Repair a stuck invoice workflow");
    assert_eq!(summary.requested_at, 1000);
    assert_eq!(summary.started_at, Some(1000));
    assert_eq!(summary.ended_at, Some(2000));
    assert_eq!(summary.duration_secs, Some(1000));
    assert_eq!(summary.commands, 1);
    assert_eq!(summary.state, "ended");
    assert_eq!(
        summary.recording_digest.as_deref(),
        Some(spot_break_glass::hex(&[5; 32]).as_str())
    );
}

#[test]
fn an_empty_or_unrelated_chain_yields_no_sessions() {
    let view = CustomerView::collect(&[]).unwrap();
    assert!(view.sessions().is_empty());
}

#[test]
fn recordings_are_retrievable_and_a_vendor_cannot_delete_them() {
    let dir = TempDir::new("archive");
    let archive = RecordingArchive::new(dir.path());
    let mut recorder =
        FileRecorder::create(archive.root(), SESSION, TENANT, 1000, 1 << 20).unwrap();
    recorder
        .append(FrameKind::Output, 1000, b"$ id\n", true)
        .unwrap();
    recorder.finish().unwrap();

    assert!(!archive.read(SESSION).unwrap().is_empty());
    assert!(archive.replay(SESSION).verified());

    assert_eq!(
        archive
            .erase(&Principal::vendor("vendor-a"), SESSION)
            .unwrap_err(),
        BreakGlassError::VendorDeleteRefused
    );
    assert_eq!(
        archive
            .erase(&Principal::customer("customer-admin"), SESSION)
            .unwrap_err(),
        BreakGlassError::AppendOnly
    );
    // The recording survives the refusal: the archive is append-only.
    assert!(archive.replay(SESSION).verified());
    assert!(!archive.read(SESSION).unwrap().is_empty());
}
