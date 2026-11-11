//! Every code this crate raises has a catalogue entry, and every catalogue entry is raised.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use spot_offline_identity::*;

fn codes() -> BTreeSet<&'static str> {
    let node = NodeId::new("n").unwrap();
    let errors = vec![
        OfflineError::InvalidId { kind: "x" },
        OfflineError::CertificateOffline,
        OfflineError::CertificateLifetime,
        OfflineError::CertificateSerial,
        OfflineError::JournalTampered { seq: 1 },
        OfflineError::JournalSequence {
            previous: 1,
            got: 3,
        },
        OfflineError::JournalStore,
        OfflineError::OverrideRefused { reason: "x" },
        OfflineError::AuditFailed,
        OfflineError::KeyReleaseRefused {
            node: node.to_string(),
            reason: "x",
        },
        OfflineError::UnknownNode,
        OfflineError::StampMalformed,
        OfflineError::QuarantineStore,
        OfflineError::QuarantineConflict {
            node: node.to_string(),
            journal_seq: 1,
        },
    ];
    let rejections = vec![
        Rejection::Rollback { got: 1, held: 2 },
        Rejection::Fork { epoch: 1 },
        Rejection::Replay { epoch: 1 },
        Rejection::BadSignature,
        Rejection::BadDelegation,
        Rejection::DelegationRollback { got: 1, held: 2 },
        Rejection::WrongTenant,
        Rejection::WrongNode,
        Rejection::Expired,
        Rejection::BoundsOutOfRange { tier: Tier::One },
        Rejection::Malformed { detail: "x" },
        Rejection::StaleTimestamp,
        Rejection::TimestampMismatch,
        Rejection::PersistFailed,
    ];
    let denials = [
        DenyReason::NoSnapshot,
        DenyReason::NodeSuspended,
        DenyReason::CertificateLapsed,
        DenyReason::ClockFault,
        DenyReason::UnknownUser,
        DenyReason::UserDisabled,
        DenyReason::NotOfflineAllowed,
        DenyReason::UserRevoked,
        DenyReason::TierNotAllowedForUser,
        DenyReason::TierLapsed(Tier::One),
        DenyReason::PermissionNotGranted,
    ];
    let quarantines = [
        QuarantineReason::TenantMismatch,
        QuarantineReason::NodeMismatch,
        QuarantineReason::SenderSuspended,
        QuarantineReason::SequenceReplay,
        QuarantineReason::UnknownEpoch,
        QuarantineReason::AssignmentMismatch,
        QuarantineReason::AuthorRevoked,
        QuarantineReason::Tier1AfterRevocation,
        QuarantineReason::BeyondBound,
        QuarantineReason::JournalTampered,
    ];
    let requests = [
        spot_offline_identity::signing::RequestRefusal::ReportMissing,
        spot_offline_identity::signing::RequestRefusal::BadSignature,
    ];
    let mut all = BTreeSet::new();
    let mut add = |code: &'static str, unique: bool| {
        assert!(code.starts_with("offline_identity."), "{code}");
        assert!(all.insert(code) || !unique, "duplicate {code}");
    };
    for e in &errors {
        add(e.code(), true);
        assert!(e.to_string().contains(e.code()), "{e}");
    }
    for r in &rejections {
        add(r.code(), true);
        assert!(r.to_string().contains(r.code()), "{r}");
    }
    for d in &denials {
        add(d.code(), true);
    }
    for q in &quarantines {
        add(q.code(), true);
    }
    for r in &requests {
        add(r.code(), true);
        assert!(r.to_string().contains(r.code()), "{r}");
    }
    assert_eq!(
        all.len(),
        errors.len() + rejections.len() + denials.len() + quarantines.len() + requests.len()
    );
    all
}

#[test]
fn every_code_has_a_catalogue_entry_and_every_entry_is_raised() {
    let catalogue = spot_errors::Catalogue::load();
    assert_eq!(
        catalogue.load_error(),
        None,
        "the catalogue must load clean"
    );
    let raised = codes();
    for code in &raised {
        assert!(
            catalogue.lookup(code).is_some(),
            "no catalogue entry for `{code}`"
        );
    }
    let file = include_str!("../../spot-errors/catalogue/offline_identity.toml");
    let listed: BTreeSet<&str> = file
        .lines()
        .filter_map(|l| l.strip_prefix("code = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .collect();
    // Audit kinds written by the hub's replay guard (spot-lease-grants); they ride the audit
    // chain, so this crate raises none of them but the file catalogues them.
    let mut raised = raised;
    raised.extend([
        "offline_identity.node_regressed",
        "offline_identity.node_report_ahead",
        "offline_identity.node_reenrolled",
    ]);
    assert_eq!(
        listed, raised,
        "the catalogue file and the raised codes differ"
    );
}

#[test]
fn messages_carry_no_secret_material() {
    let e = OfflineError::KeyReleaseRefused {
        node: "branch-1".into(),
        reason: "the node is suspended",
    };
    let text = e.to_string();
    assert!(!text.contains("key="));
    assert!(text.contains("branch-1"));
}
