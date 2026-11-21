//! Properties: an allow is always inside the window, tier and scope, and the expiry
//! boundary always refuses.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::*;
use proptest::prelude::*;
use spot_support_grants::check;
use spot_support_session::Tier;

fn tier_of(n: u8) -> Tier {
    match n % 4 {
        0 => Tier::T0,
        1 => Tier::T1,
        2 => Tier::T2,
        _ => Tier::T3,
    }
}

proptest! {
    #[test]
    fn an_allow_is_inside_the_window_tier_and_scope(
        offset in -100_i64..300,
        tier_n in 0_u8..4,
        action_n in 0_u8..3,
    ) {
        let mut h = Harness::new();
        h.grant(
            "g-prop",
            Tier::T2,
            scope(&["Factura"], &["read", "update"]),
            100,
            200,
        );
        let action = match action_n {
            0 => read("Factura"),
            1 => action("update", tier_of(tier_n), Some("Factura")),
            _ => action("read", tier_of(tier_n), Some("Payment")),
        };
        let now = 100 + offset;
        let decision = check(&h.grants, &session("g-prop"), &action, now);
        if decision.is_allowed() {
            prop_assert!((100..200).contains(&now));
            prop_assert!(action.tier <= Tier::T2);
            prop_assert_eq!(action.doctype.as_deref(), Some("Factura"));
        }
    }

    #[test]
    fn the_expiry_instant_always_refuses(tier_n in 0_u8..4) {
        let mut h = Harness::new();
        h.grant(
            "g-boundary",
            Tier::T3,
            scope(&["Anything"], &["read"]),
            100,
            200,
        );
        let action = action("read", tier_of(tier_n), Some("Anything"));
        let decision = check(&h.grants, &session("g-boundary"), &action, 200);
        prop_assert_eq!(decision.code(), "session.expired");
    }
}
