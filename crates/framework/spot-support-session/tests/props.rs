//! The property: for random instants, a grant is `Active` for exactly
//! `[starts_at, expires_at)` unless revoked, and `check` succeeds only inside that
//! window.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use proptest::prelude::*;
use spot_support_session::check::ActionRequest;
use spot_support_session::model::{DerivedStatus, SessionPolicy};
use spot_support_session::{DerivedStatus as DS, SessionGrant, Tier};

fn read() -> ActionRequest {
    ActionRequest {
        action: "read".to_owned(),
        tier: Tier::T0,
        doctype: Some("Sales Invoice".to_owned()),
        tenant: TENANT.to_owned(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// A grant is `Active` exactly over `[starts_at, expires_at)` unless revoked, and a
    /// check can succeed only inside that window (the boundary is exclusive: at
    /// `expires_at` the grant is expired).
    #[test]
    fn active_exactly_over_the_half_open_window(
        base in -1_000_000i64..1_000_000,
        // The width stays within the policy's 4 h maximum, so the request itself is
        // always valid; the property under test is the window's liveness, not the
        // request validation.
        width in 1i64..14_400,
        offset in -10i64..=10,
        revoked in proptest::bool::ANY,
    ) {
        let starts_at = base;
        let expires_at = base.checked_add(width).unwrap();
        let mut s = spec();
        s.starts_at = starts_at;
        s.expires_at = expires_at;
        let mut g = SessionGrant::request(&s, &SessionPolicy::default(), base.saturating_sub(1))
            .unwrap()
            .approve(&approver(), base.saturating_sub(1), &SessionPolicy::default())
            .unwrap()
            .0;
        if revoked {
            let at = base.saturating_sub(1);
            g.revoke("tenant-admin", "property", at).unwrap();
        }

        // Strictly inside the window: Active and the check succeeds (when not revoked).
        let inside = starts_at + (expires_at - starts_at) / 2;
        if !revoked {
            prop_assert_eq!(g.status_at(inside), DerivedStatus::Active);
            prop_assert!(g.check(&read(), inside).is_ok());
        } else {
            prop_assert!(g.check(&read(), inside).is_err());
        }

        // Before the start: NotYetActive, refused.
        prop_assert_eq!(g.status_at(starts_at - 1), DS::NotYetActive);

        // At the start (inclusive) and one second before the end (exclusive end): live.
        prop_assert_eq!(g.status_at(starts_at), DS::Active);
        prop_assert_eq!(g.status_at(expires_at - 1), DS::Active);

        // At and after the boundary: Expired, refused. The revocation may fire first
        // when one happened, but the boundary still holds for the status derivation
        // (which never reads the revocation).
        prop_assert_eq!(g.status_at(expires_at), DS::Expired);
        prop_assert_eq!(
            g.status_at(expires_at.checked_add(offset.abs()).unwrap()),
            DS::Expired
        );
        prop_assert!(g.check(&read(), expires_at).is_err());
        prop_assert!(
            g.check(&read(), expires_at.checked_add(offset.abs()).unwrap())
                .is_err()
        );
    }

    /// A check can succeed **only** inside the window: at every instant outside it, and
    /// for every tier, the check refuses.
    #[test]
    fn check_succeeds_only_inside_the_window(
        before in 0i64..1_000,
        after in 0i64..1_000,
        tier in proptest::sample::select(vec![Tier::T0, Tier::T1, Tier::T2, Tier::T3]),
        with_doctype in proptest::bool::ANY,
    ) {
        let mut s = spec();
        s.tier_ceiling = Tier::T3;
        s.starts_at = 5_000;
        s.expires_at = 10_000;
        let g = SessionGrant::request(&s, &SessionPolicy::default(), 4_000)
            .unwrap()
            .approve(&owner(), 4_000, &SessionPolicy::default())
            .unwrap()
            .0;
        let mut a = read();
        a.tier = tier;
        if !with_doctype {
            a.doctype = None;
        }
        // Every instant before `starts_at` and every instant at or after `expires_at`
        // refuses, whatever the action.
        prop_assert!(g.check(&a, 5_000 - 1 - before).is_err());
        prop_assert!(g.check(&a, 10_000 + after).is_err());
        prop_assert!(g.check(&a, 10_000).is_err());
        // Sanity: inside, a T3 action on any DocType passes (Any actions, but the
        // doctype must be named for the `Named` scope — the base grant's scope is
        // Named, so the no-doctype diagnostic is the all-actions probe).
        if with_doctype {
            prop_assert_eq!(a.doctype.as_deref(), Some("Sales Invoice"));
        } else {
            prop_assert!(g.check(&a, 7_500).is_ok());
        }
    }
}
