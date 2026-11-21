//! One tier type for the agent authoriser and the support session (WP-AI-06.T3): the shared
//! ladder, the stored `t0`..`t3` wire form, and the old-grant golden fixtures.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_support_session::model::SessionPolicy;
use spot_support_session::tier::tier_snake;
use spot_support_session::{ActionRequest, SessionGrant, Tier};

fn action(tier: Tier) -> ActionRequest {
    ActionRequest {
        action: "update".to_owned(),
        tier,
        doctype: Some("Sales Invoice".to_owned()),
        tenant: TENANT.to_owned(),
    }
}

/// Compile-time proof that both crates name one type: this only builds if they are identical.
fn same_type(_: spot_agent_tiers::Tier, _: spot_support_session::Tier) {}

#[test]
fn the_session_tier_is_the_agent_tier() {
    same_type(spot_agent_tiers::Tier::T2, Tier::T2);
    let agent: spot_agent_tiers::Tier = Tier::T3;
    assert_eq!(agent, spot_agent_tiers::Tier::T3);
}

#[test]
fn a_t1_ceiling_refuses_a_t2_action_and_allows_t0_and_t1() {
    let g = grant();
    assert_eq!(g.tier_ceiling, Tier::T1);
    let refusal = g.check(&action(Tier::T2), START + 1).unwrap_err();
    assert_eq!(refusal.code(), "session.above_ceiling");
    assert!(g.check(&action(Tier::T0), START + 1).is_ok());
    assert!(g.check(&action(Tier::T1), START + 1).is_ok());
}

#[test]
fn every_ceiling_refuses_exactly_the_tiers_above_it() {
    for ceiling in Tier::all() {
        let mut s = spec();
        s.tier_ceiling = ceiling;
        s.scope.doctypes =
            spot_support_session::model::DocTypeScope::Named(vec!["Sales Invoice".to_owned()]);
        let policy = SessionPolicy {
            max_tier_without_owner_approval: Tier::T3,
            ..SessionPolicy::default()
        };
        let g = SessionGrant::request(&s, &policy, START)
            .unwrap()
            .approve(&approver(), START, &policy)
            .unwrap()
            .0;
        for tier in Tier::all() {
            let outcome = g.check(&action(tier), START + 1);
            assert_eq!(
                outcome.is_ok(),
                tier <= ceiling,
                "ceiling {ceiling} action {tier}"
            );
        }
    }
}

/// The session check and the agent authoriser share the T3 rule: a T3 action is not
/// unattended for an agent that is not registered deep, and a session allows it only because
/// a human approved a T3 ceiling (the attendance the agent side asks for).
#[test]
fn the_session_and_the_agent_authoriser_agree_on_unattended_t3() {
    assert!(!Tier::T3.unattended_allowed(false));
    assert!(Tier::T3.unattended_allowed(true));
    for t in [Tier::T0, Tier::T1, Tier::T2] {
        assert!(t.unattended_allowed(false));
    }
    let owner_policy = SessionPolicy::default();
    let mut s = spec();
    s.tier_ceiling = Tier::T3;
    // Without an owner, a T3 ceiling cannot be approved: the attended path is guarded.
    let pending = SessionGrant::request(&s, &owner_policy, START).unwrap();
    assert!(pending.approve(&approver(), START, &owner_policy).is_err());
}

#[test]
fn tier_snake_writes_lower_case_and_reads_either_case() {
    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Holder {
        #[serde(with = "tier_snake")]
        tier: Tier,
    }
    for t in Tier::all() {
        let lower = t.as_str().to_lowercase();
        let h = Holder { tier: t };
        assert_eq!(
            serde_json::to_string(&h).unwrap(),
            format!("{{\"tier\":\"{lower}\"}}")
        );
        let from_lower: Holder =
            serde_json::from_str(&format!("{{\"tier\":\"{lower}\"}}")).unwrap();
        let from_upper: Holder =
            serde_json::from_str(&format!("{{\"tier\":\"{}\"}}", t.as_str())).unwrap();
        assert_eq!(from_lower, h);
        assert_eq!(from_upper, h);
        assert_eq!(tier_snake::wire(t), lower);
    }
    for bad in ["T4", "t4", "", "t", "tier1", " t1", "T1 "] {
        assert!(
            serde_json::from_str::<Holder>(&format!("{{\"tier\":\"{bad}\"}}")).is_err(),
            "{bad:?} must be refused"
        );
    }
}

/// Reads a fixture written by the code before the two tier types were unified and checks that
/// it loads and writes back byte for byte.
fn golden<T>(file: &str)
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let path = format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap();
    let value: T = serde_json::from_str(&text).unwrap();
    assert_eq!(serde_json::to_string(&value).unwrap(), text, "{file}");
}

#[test]
fn grants_written_by_the_old_code_round_trip_byte_for_byte() {
    golden::<SessionGrant>("grant_with_extension.json");
    golden::<spot_support_session::PendingGrant>("pending_grant.json");
    golden::<SessionPolicy>("session_policy.json");
    golden::<ActionRequest>("action_request.json");
}

#[test]
fn an_old_grant_loads_with_the_expected_tier() {
    let path = format!(
        "{}/tests/fixtures/grant_with_extension.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let g: SessionGrant = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(g.tier_ceiling, Tier::T1);
}

#[test]
fn a_grant_with_upper_case_tiers_loads_and_is_written_back_lower_case() {
    let path = format!(
        "{}/tests/fixtures/grant_with_extension.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(path).unwrap();
    let upper = text.replace("\"tier_ceiling\":\"t1\"", "\"tier_ceiling\":\"T1\"");
    assert_ne!(upper, text);
    let g: SessionGrant = serde_json::from_str(&upper).unwrap();
    assert_eq!(serde_json::to_string(&g).unwrap(), text);
}
