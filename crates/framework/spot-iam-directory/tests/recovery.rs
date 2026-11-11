//! The recovery matrix, read from `tests/recovery.toml`.
#![allow(missing_docs)]

mod common;

use common::id;
use serde::Deserialize;
use spot_iam_directory::{
    RecoveryKind, RecoveryPolicy, RecoveryRequest, Requester, Step, Target, UserType,
    required_steps,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    kind: Kind,
    #[serde(default)]
    self_service: bool,
    #[serde(default)]
    requester_admin: bool,
    #[serde(default = "internal")]
    target_type: UserType,
    #[serde(default = "yes")]
    target_enabled: bool,
    #[serde(default)]
    target_admin: bool,
    #[serde(default = "yes")]
    policy_password: bool,
    #[serde(default)]
    policy_second_factor: bool,
    steps: Option<Vec<StepName>>,
    refused: Option<String>,
}

fn yes() -> bool {
    true
}

fn internal() -> UserType {
    UserType::Internal
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Password,
    SecondFactor,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum StepName {
    StepUp,
    SecondApprover,
    NotifyAllAddresses,
}

#[derive(Deserialize)]
struct File {
    case: Vec<Case>,
}

#[test]
fn the_matrix_holds() {
    let text = include_str!("recovery.toml");
    let file: File = toml::from_str(text).unwrap_or_else(|e| panic!("recovery.toml: {e}"));
    assert!(file.case.len() >= 12, "the matrix needs at least 12 cases");
    for case in file.case {
        let target = id();
        let initiated_by = if case.self_service { target } else { id() };
        let request = RecoveryRequest {
            target,
            initiated_by,
            kind: match case.kind {
                Kind::Password => RecoveryKind::Password,
                Kind::SecondFactor => RecoveryKind::SecondFactor,
            },
        };
        let policy = RecoveryPolicy {
            self_service_password: case.policy_password,
            self_service_second_factor: case.policy_second_factor,
        };
        let requester = Requester {
            is_administrator: case.requester_admin,
        };
        let target = Target {
            user_type: case.target_type,
            enabled: case.target_enabled,
            is_administrator: case.target_admin,
        };
        let got = required_steps(&policy, &request, &requester, &target);
        match (&case.steps, &case.refused) {
            (Some(steps), None) => {
                let want: Vec<Step> = steps
                    .iter()
                    .map(|s| match s {
                        StepName::StepUp => Step::StepUp,
                        StepName::SecondApprover => Step::SecondApprover,
                        StepName::NotifyAllAddresses => Step::NotifyAllAddresses,
                    })
                    .collect();
                assert_eq!(got.ok(), Some(want), "{}", case.name);
            }
            (None, Some(code)) => {
                let e = got
                    .err()
                    .unwrap_or_else(|| panic!("{}: must be refused", case.name));
                assert_eq!(e.codes(), [code.as_str()], "{}", case.name);
            }
            _ => panic!("{}: a case has either `steps` or `refused`", case.name),
        }
    }
}
