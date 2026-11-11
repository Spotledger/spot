//! Property tests: evaluation is total and deterministic, a report always round-trips through
//! its JSON form, and no check panics on any fact value.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_hardening::{
    Baseline, CheckSpec, FactKey, FactValue, Facts, FakeServer, OsFamily, Report, check_outcome,
    evaluate,
};

fn key() -> FactKey {
    FactKey::try_new("a.flag").unwrap()
}

fn any_fact_value() -> impl Strategy<Value = FactValue> {
    prop_oneof![
        any::<bool>().prop_map(FactValue::Bool),
        "[a-z]{0,8}".prop_map(FactValue::Text),
        any::<i64>().prop_map(FactValue::Int),
        prop::collection::vec("[a-z]{0,4}", 0..4).prop_map(FactValue::TextList),
    ]
}

fn every_check() -> Vec<CheckSpec> {
    vec![
        CheckSpec::Bool {
            fact: key(),
            expect: true,
        },
        CheckSpec::Text {
            fact: key(),
            expect: "yes".to_owned(),
        },
        CheckSpec::TextIn {
            fact: key(),
            allow: vec!["no".to_owned(), "yes".to_owned()],
        },
        CheckSpec::Int {
            fact: key(),
            min: Some(0),
            max: Some(10),
        },
        CheckSpec::Present { fact: key() },
        CheckSpec::Absent { fact: key() },
        CheckSpec::Subset {
            fact: key(),
            allowed_fact: FactKey::try_new("b.allow").unwrap(),
        },
        CheckSpec::All {
            checks: vec![
                CheckSpec::Bool {
                    fact: key(),
                    expect: true,
                },
                CheckSpec::Text {
                    fact: key(),
                    expect: "yes".to_owned(),
                },
            ],
        },
        CheckSpec::Any {
            checks: vec![
                CheckSpec::Bool {
                    fact: key(),
                    expect: false,
                },
                CheckSpec::TextIn {
                    fact: key(),
                    allow: vec!["yes".to_owned()],
                },
            ],
        },
    ]
}

proptest! {
    #[test]
    fn evaluation_is_deterministic(enabled in any::<bool>()) {
        let baseline = Baseline::linux().unwrap();
        let server = FakeServer::new(OsFamily::Linux)
            .with(FactKey::try_new("ssh.enabled").unwrap(), FactValue::Bool(enabled));
        let first = evaluate(&baseline, &server);
        let second = evaluate(&baseline, &server);
        prop_assert_eq!(first, second);
    }

    #[test]
    fn a_report_round_trips_through_json(enabled in any::<bool>()) {
        let baseline = Baseline::windows().unwrap();
        let server = FakeServer::new(OsFamily::Windows)
            .with(FactKey::try_new("windows.uac.enabled").unwrap(), FactValue::Bool(enabled));
        let report = evaluate(&baseline, &server);
        let json = report.to_json().unwrap();
        prop_assert_eq!(Report::from_json(&json).unwrap(), report);
    }

    #[test]
    fn no_check_panics_on_any_fact_value(value in any_fact_value()) {
        let facts = Facts::new().with(key(), value);
        for check in every_check() {
            let _ = check_outcome(&check, &facts);
        }
    }
}
