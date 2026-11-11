//! The table-driven risk test: every operation the catalogue ships, against every
//! signature count, must produce exactly the outcome its risk level dictates.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::Harness;
use futures::executor::block_on;
use spot_agent_core::{Policy, Refusal};
use spot_mgmt_ops::Risk;

#[test]
fn every_operation_and_signature_count_matches_its_risk_rule() {
    let harness = Harness::new();
    let agent = harness.agent();
    let pool = [&harness.admin_a, &harness.admin_b, &harness.admin_c];

    let mut checked = 0usize;
    for spec in harness.catalogue.iter() {
        let op = spec.id.as_str().to_owned();
        let risk = spec.risk;
        let required = Policy::required_distinct(risk);
        assert!(
            required >= 1,
            "every risk level needs at least one signature"
        );
        for count in 0..=pool.len() {
            let now = 1_000;
            let command = harness.command(&op, common::valid_params(&op), now - 30, now + 600);
            let envelope = harness.envelope(&command, &pool[..count]);
            let decision = block_on(agent.handle(&envelope, now));
            if count < required {
                assert_eq!(
                    decision.refusal(),
                    Some(&Refusal::InsufficientSignatures {
                        have: count,
                        need: required
                    }),
                    "{op} ({risk:?}) with {count} signatures"
                );
            } else {
                let plan = decision.plan().unwrap_or_else(|| {
                    panic!("{op} ({risk:?}) with {count} signatures should run")
                });
                assert_eq!(plan.risk(), risk, "the plan records the catalogue's risk");
                assert_eq!(plan.signers().len(), count);
            }
            checked += 1;
        }
    }
    assert!(checked >= 20, "the table covers every operation and count");
}

#[test]
fn every_risk_level_has_a_rule_and_the_ops_cover_them() {
    // The exhaustive match in `Policy::approval_for` already makes a new `Risk` variant a
    // compile error; this proves the shipped test catalogue exercises all five.
    for risk in [
        Risk::Read,
        Risk::Low,
        Risk::Medium,
        Risk::High,
        Risk::Critical,
    ] {
        assert!(Policy::required_distinct(risk) >= 1);
    }
    let harness = Harness::new();
    for risk in [
        Risk::Read,
        Risk::Low,
        Risk::Medium,
        Risk::High,
        Risk::Critical,
    ] {
        assert!(
            harness.catalogue.iter().any(|spec| spec.risk == risk),
            "the test catalogue has an operation at {risk:?}"
        );
    }
}
