//! Fail-closed authorisation per tier, and the audited, bounded override.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_audit_chain::PayloadValue;
use spot_offline_identity::*;

fn allow() -> Decision {
    Decision::Allow
}

fn deny(r: DenyReason) -> Decision {
    Decision::Deny(r)
}

#[test]
fn an_enabled_allowed_user_is_allowed_inside_the_bound() {
    let w = World::new();
    let s = w.store_with(snapshot(1));
    assert_eq!(
        s.authorise_offline("clerk", Tier::Three, clock(1_000, 1)),
        allow()
    );
    assert_eq!(s.sign_in("clerk", clock(1_000, 1)), allow());
}

#[test]
fn a_user_the_node_cannot_confirm_is_denied() {
    let w = World::new();
    let s = w.store_with(snapshot(1));
    let c = clock(1_000, 1);
    assert_eq!(s.sign_in("nobody", c), deny(DenyReason::UnknownUser));
    assert_eq!(s.sign_in("gone", c), deny(DenyReason::UserDisabled));
    assert_eq!(s.sign_in("admin", c), deny(DenyReason::NotOfflineAllowed));
}

#[test]
fn a_user_cannot_act_above_their_highest_offline_tier() {
    let w = World::new();
    let s = w.store_with(snapshot(1));
    let c = clock(1_000, 1);
    assert_eq!(
        s.authorise_offline("clerk", Tier::Two, c),
        deny(DenyReason::TierNotAllowedForUser)
    );
    assert_eq!(
        s.authorise_offline("reader", Tier::One, c),
        deny(DenyReason::TierNotAllowedForUser)
    );
    assert_eq!(s.authorise_offline("reader", Tier::Three, c), allow());
}

#[test]
fn tier_boundaries_are_exact_and_each_tier_fails_closed_on_its_own() {
    let w = World::new();
    let mut s = w.store_with(snapshot(1));
    // The node certificate is renewed while connected, so it outlives the tier-3 bound here.
    s.renew_certificate(2, MAX_CERT_TTL_MS, true, clock(HOUR - 1, 1))
        .unwrap();
    // Tier 1: one hour.
    assert_eq!(
        s.authorise_offline("payer", Tier::One, clock(HOUR - 1, 1)),
        allow()
    );
    assert_eq!(
        s.authorise_offline("payer", Tier::One, clock(HOUR, 1)),
        deny(DenyReason::TierLapsed(Tier::One))
    );
    // Tier 2: twelve hours; its users stop signing in, tier 3 users carry on.
    assert_eq!(s.sign_in("reader", clock(12 * HOUR - 1, 1)), allow());
    assert_eq!(
        s.sign_in("reader", clock(12 * HOUR, 1)),
        deny(DenyReason::TierLapsed(Tier::Two))
    );
    assert_eq!(
        s.authorise_offline("clerk", Tier::Three, clock(12 * HOUR, 1)),
        allow()
    );
    // Tier 3: twenty-four hours.
    assert_eq!(
        s.authorise_offline("clerk", Tier::Three, clock(24 * HOUR - 1, 1)),
        allow()
    );
    assert_eq!(
        s.authorise_offline("clerk", Tier::Three, clock(24 * HOUR, 1)),
        deny(DenyReason::TierLapsed(Tier::Three))
    );
}

#[test]
fn a_timestamp_moves_the_boundary() {
    let w = World::new();
    let snap = snapshot(1);
    let mut s = w.store_with(snap.clone());
    let at = T0 + 50 * 60_000;
    s.accept(
        &w.timestamp(&snap, at, [HOUR, 12 * HOUR, 24 * HOUR]),
        rx(50 * 60_000, 1),
    )
    .unwrap();
    assert_eq!(
        s.authorise_offline("payer", Tier::One, clock(HOUR + 10 * 60_000, 1)),
        allow()
    );
    assert_eq!(
        s.authorise_offline("payer", Tier::One, clock(50 * 60_000 + HOUR, 1)),
        deny(DenyReason::TierLapsed(Tier::One))
    );
}

#[test]
fn nothing_is_allowed_without_a_snapshot_or_with_a_lapsed_certificate() {
    let w = World::new();
    let empty = SnapshotStore::open(w.anchor(), MemoryPersistence::default(), clock(0, 1));
    assert_eq!(
        empty.sign_in("clerk", clock(0, 1)),
        deny(DenyReason::NoSnapshot)
    );

    let mut s = SnapshotStore::open(w.anchor(), MemoryPersistence::default(), clock(0, 1));
    s.accept(&w.signed(snapshot(1)), rx(0, 1)).unwrap();
    assert_eq!(
        s.sign_in("clerk", clock(1, 1)),
        deny(DenyReason::CertificateLapsed)
    );
    s.renew_certificate(1, HOUR, true, clock(0, 1)).unwrap();
    assert_eq!(s.sign_in("clerk", clock(HOUR - 1, 1)), allow());
    assert_eq!(
        s.sign_in("clerk", clock(HOUR, 1)),
        deny(DenyReason::CertificateLapsed)
    );
}

#[test]
fn a_snapshot_that_suspends_this_node_denies_everyone() {
    let w = World::new();
    for kind in [
        NodeRevocationKind::Suspended,
        NodeRevocationKind::Decommissioned,
    ] {
        let mut snap = snapshot(1);
        snap.revocations.push(Revocation::Node {
            node: "branch-1".to_owned(),
            kind,
            effective_epoch: 1,
        });
        let s = w.store_with(snap);
        assert_eq!(
            s.sign_in("clerk", clock(1, 1)),
            deny(DenyReason::NodeSuspended)
        );
    }
}

#[test]
fn a_revocation_of_another_node_does_not_stop_this_one() {
    let w = World::new();
    let mut snap = snapshot(1);
    snap.revocations.push(Revocation::Node {
        node: "branch-9".to_owned(),
        kind: NodeRevocationKind::Suspended,
        effective_epoch: 1,
    });
    assert_eq!(w.store_with(snap).sign_in("clerk", clock(1, 1)), allow());
}

#[test]
fn a_revoked_user_is_denied_even_if_the_directory_says_enabled() {
    let w = World::new();
    let mut snap = snapshot(1);
    snap.revocations.push(Revocation::User {
        user: "clerk".to_owned(),
        assignment_id: None,
        effective_epoch: 1,
    });
    let s = w.store_with(snap);
    assert_eq!(
        s.sign_in("clerk", clock(1, 1)),
        deny(DenyReason::UserRevoked)
    );
    assert_eq!(
        s.authorise_action("clerk", Tier::Three, "invoice:read", clock(1, 1)),
        deny(DenyReason::UserRevoked)
    );
}

#[test]
fn a_permission_needs_a_grant_from_an_assignment_that_is_not_revoked() {
    let w = World::new();
    let s = w.store_with(snapshot(1));
    let c = clock(1, 1);
    assert_eq!(
        s.authorise_action("clerk", Tier::Three, "invoice:write", c),
        allow()
    );
    assert_eq!(
        s.authorise_action("clerk", Tier::Three, "payment:post", c),
        deny(DenyReason::PermissionNotGranted)
    );
    let mut snap = snapshot(1);
    snap.revocations.push(Revocation::User {
        user: "clerk".to_owned(),
        assignment_id: Some("a-clerk".to_owned()),
        effective_epoch: 1,
    });
    let s = w.store_with(snap);
    assert_eq!(
        s.authorise_action("clerk", Tier::Three, "invoice:write", c),
        deny(DenyReason::PermissionNotGranted)
    );
    assert_eq!(
        s.sign_in("clerk", c),
        allow(),
        "only the assignment is revoked"
    );
}

#[test]
fn tenant_bounds_are_clamped_to_the_floor_and_ceiling() {
    let b = TierBounds::clamped(0, 10, u64::MAX);
    assert_eq!(b.get(Tier::One), FLOOR_MS);
    assert_eq!(b.get(Tier::Two), FLOOR_MS);
    assert_eq!(b.get(Tier::Three), CEILING_MS);
    assert!(b.in_range());
    let ok = TierBounds::clamped(15 * 60_000, HOUR, 48 * HOUR);
    assert_eq!(ok.get(Tier::One), 15 * 60_000);
    assert_eq!(TierBounds::default().get(Tier::Two), 12 * HOUR);
}

// ---- the override ----

fn lapsed_two(w: &World) -> SnapshotStore<MemoryPersistence> {
    let mut s = w.store_with(snapshot(1));
    // Keep the certificate good through the test window.
    s.renew_certificate(2, MAX_CERT_TTL_MS, true, clock(0, 1))
        .unwrap();
    s
}

fn request<'a>(who: &'a str, tier: Tier, ms: u64) -> OverrideRequest<'a> {
    OverrideRequest {
        supervisor: who,
        tier,
        duration_ms: ms,
        reason: "hub unreachable after the storm; month-end close",
        at_micros: 1,
    }
}

#[test]
fn a_supervisor_override_is_audited_bounded_and_ends() {
    let w = World::new();
    let mut s = lapsed_two(&w);
    let at = clock(13 * HOUR, 1);
    assert_eq!(
        s.sign_in("reader", at),
        deny(DenyReason::TierLapsed(Tier::Two))
    );
    let mut audit = MemoryAuditSink::default();
    let grant = s
        .request_override(&request("sup", Tier::Two, 30 * 60_000), at, &mut audit)
        .unwrap();
    assert_eq!(
        grant,
        OverrideGrant {
            tier: Tier::Two,
            duration_ms: 30 * 60_000
        }
    );
    assert_eq!(audit.records.len(), 1);
    assert_eq!(audit.records[0].kind, "offline_identity.override_granted");
    assert_eq!(audit.records[0].actor, "offline-identity:user:sup");
    assert_eq!(
        audit.records[0].payload.get("tier"),
        Some(&PayloadValue::Text("tier2".into()))
    );
    assert_eq!(
        s.sign_in("reader", clock(13 * HOUR + 29 * 60_000, 1)),
        allow()
    );
    assert_eq!(
        s.sign_in("reader", clock(13 * HOUR + 30 * 60_000, 1)),
        deny(DenyReason::TierLapsed(Tier::Two)),
        "the override is bounded"
    );
}

#[test]
fn an_override_does_not_reach_disabled_revoked_or_unlisted_users_or_other_tiers() {
    let w = World::new();
    let mut s = lapsed_two(&w);
    let at = clock(13 * HOUR, 1);
    let mut audit = MemoryAuditSink::default();
    s.request_override(&request("sup", Tier::Two, 60_000), at, &mut audit)
        .unwrap();
    assert_eq!(s.sign_in("gone", at), deny(DenyReason::UserDisabled));
    assert_eq!(s.sign_in("admin", at), deny(DenyReason::NotOfflineAllowed));
    assert_eq!(s.sign_in("nobody", at), deny(DenyReason::UnknownUser));
    // Tier 1 stays lapsed for the payer: the override was for tier 2.
    assert_eq!(
        s.sign_in("payer", at),
        deny(DenyReason::TierLapsed(Tier::One))
    );
}

#[test]
fn only_a_supervisor_holding_the_permission_may_override() {
    let w = World::new();
    let mut s = lapsed_two(&w);
    let at = clock(13 * HOUR, 1);
    let mut audit = MemoryAuditSink::default();
    for who in ["clerk", "gone", "admin", "nobody"] {
        let err = s
            .request_override(&request(who, Tier::Two, 60_000), at, &mut audit)
            .unwrap_err();
        assert!(matches!(err, OfflineError::OverrideRefused { .. }), "{who}");
    }
    assert_eq!(
        s.sign_in("reader", at),
        deny(DenyReason::TierLapsed(Tier::Two))
    );
    assert!(
        audit
            .records
            .iter()
            .all(|r| r.kind == "offline_identity.override_refused")
    );
    assert_eq!(audit.records.len(), 4, "refusals are audited too");
}

#[test]
fn tier_one_zero_and_overlong_overrides_and_missing_reasons_are_refused() {
    let w = World::new();
    let mut s = lapsed_two(&w);
    let at = clock(13 * HOUR, 1);
    let mut audit = MemoryAuditSink::default();
    assert!(
        s.request_override(&request("sup", Tier::One, 60_000), at, &mut audit)
            .is_err()
    );
    assert!(
        s.request_override(&request("sup", Tier::Two, 0), at, &mut audit)
            .is_err()
    );
    assert!(
        s.request_override(&request("sup", Tier::Two, HOUR + 1), at, &mut audit)
            .is_err()
    );
    let mut no_reason = request("sup", Tier::Two, 60_000);
    no_reason.reason = "";
    assert!(s.request_override(&no_reason, at, &mut audit).is_err());
    let long = "x".repeat(257);
    no_reason.reason = &long;
    assert!(s.request_override(&no_reason, at, &mut audit).is_err());
    // A tenant that decides otherwise can allow tier 1, still bounded.
    s.set_override_policy(OverridePolicy {
        max_duration_ms: 10 * 60_000,
        tier1_overridable: true,
    });
    assert!(
        s.request_override(&request("sup", Tier::One, 10 * 60_000), at, &mut audit)
            .is_ok()
    );
    assert!(
        s.request_override(&request("sup", Tier::One, 10 * 60_000 + 1), at, &mut audit)
            .is_err()
    );
}

#[test]
fn if_the_audit_cannot_be_written_nothing_is_overridden() {
    let w = World::new();
    let mut s = lapsed_two(&w);
    let at = clock(13 * HOUR, 1);
    let mut audit = MemoryAuditSink {
        records: vec![],
        fail: true,
    };
    let err = s
        .request_override(&request("sup", Tier::Two, 60_000), at, &mut audit)
        .unwrap_err();
    assert_eq!(err, OfflineError::AuditFailed);
    assert_eq!(
        s.sign_in("reader", at),
        deny(DenyReason::TierLapsed(Tier::Two))
    );
}

#[test]
fn a_reboot_ends_an_override() {
    let w = World::new();
    let mut s = lapsed_two(&w);
    let at = clock(13 * HOUR, 1);
    let mut audit = MemoryAuditSink::default();
    s.request_override(&request("sup", Tier::Two, 30 * 60_000), at, &mut audit)
        .unwrap();
    s.checkpoint(at).unwrap();
    let mut r = SnapshotStore::open(w.anchor(), s.into_persistence(), clock(0, 2));
    r.renew_certificate(3, HOUR, true, clock(0, 2)).unwrap();
    assert_eq!(
        r.sign_in("reader", clock(1, 2)),
        deny(DenyReason::TierLapsed(Tier::Two))
    );
}

// ---- the tier bound on an override ----

fn override_bounds() -> OverridePolicy {
    // A policy generous enough that the tier's own bound is what decides.
    OverridePolicy {
        max_duration_ms: 7 * 24 * HOUR,
        tier1_overridable: true,
    }
}

fn grant_ok(s: &mut SnapshotStore<MemoryPersistence>, tier: Tier, ms: u64, at: NodeClock) -> bool {
    let mut audit = MemoryAuditSink::default();
    let granted = s
        .request_override(&request("sup", tier, ms), at, &mut audit)
        .is_ok();
    if granted {
        // Every override leaves its record, in the offline-identity actor namespace.
        assert_eq!(audit.records.len(), 1);
        assert_eq!(audit.records[0].kind, "offline_identity.override_granted");
        assert_eq!(audit.records[0].actor, "offline-identity:user:sup");
        assert_eq!(
            audit.records[0].payload.get("tier"),
            Some(&PayloadValue::Text(tier.as_str().into()))
        );
    }
    granted
}

#[test]
fn an_override_grant_is_bounded_by_its_tier_bound_at_the_bound() {
    // Tier 1's signed bound is one hour: a grant of exactly one hour is inside it, one
    // millisecond more is not.
    let w = World::new();
    let mut s = lapsed_two(&w);
    s.set_override_policy(override_bounds());
    let at = clock(13 * HOUR, 1);
    assert!(grant_ok(&mut s, Tier::One, HOUR, at), "at the bound");
    assert!(
        grant_ok(&mut s, Tier::One, HOUR - 1, at),
        "just inside the bound"
    );
    assert!(
        !grant_ok(&mut s, Tier::One, HOUR + 1, at),
        "a payments-tier grant outside its short bound is refused"
    );
}

#[test]
fn an_override_grant_is_bounded_by_its_tier_bound_just_inside_and_outside() {
    let w = World::new();
    let mut s = lapsed_two(&w);
    s.set_override_policy(override_bounds());
    let at = clock(13 * HOUR, 1);
    // Tier 2: twelve hours.
    assert!(grant_ok(&mut s, Tier::Two, 12 * HOUR, at), "at the bound");
    assert!(
        grant_ok(&mut s, Tier::Two, 12 * HOUR - 1, at),
        "just inside the bound"
    );
    assert!(
        !grant_ok(&mut s, Tier::Two, 12 * HOUR + 1, at),
        "just outside the bound"
    );
    // Tier 3: twenty-four hours.
    assert!(grant_ok(&mut s, Tier::Three, 24 * HOUR, at), "at the bound");
    assert!(
        grant_ok(&mut s, Tier::Three, 24 * HOUR - 1, at),
        "just inside the bound"
    );
    assert!(
        !grant_ok(&mut s, Tier::Three, 24 * HOUR + 1, at),
        "just outside the bound"
    );
}

#[test]
fn the_tier_bound_narrows_with_the_snapshot_and_still_bounds_the_grant() {
    // A tenant that tightens tier 1 to fifteen minutes also tightens what an override may
    // grant: the bound is the snapshot's own lease for the tier, not a platform constant.
    let w = World::new();
    let mut s = w.store_with(snapshot_at(1, T0, [15 * 60_000, 12 * HOUR, 24 * HOUR]));
    s.renew_certificate(2, MAX_CERT_TTL_MS, true, clock(0, 1))
        .unwrap();
    s.set_override_policy(override_bounds());
    let at = clock(16 * 60_000, 1);
    assert_eq!(
        s.sign_in("payer", at),
        deny(DenyReason::TierLapsed(Tier::One))
    );
    // A refused request grants nothing, so the tier stays lapsed.
    assert!(
        !grant_ok(&mut s, Tier::One, 15 * 60_000 + 1, at),
        "just outside the bound"
    );
    assert_eq!(
        s.sign_in("payer", at),
        deny(DenyReason::TierLapsed(Tier::One)),
        "a refused override grants nothing"
    );
    assert!(grant_ok(&mut s, Tier::One, 15 * 60_000, at), "at the bound");
    assert_eq!(s.sign_in("payer", at), allow());
}
