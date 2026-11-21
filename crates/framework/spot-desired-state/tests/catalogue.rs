//! Every code this crate raises resolves in the compiled-in error catalogue.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_desired_state::{Error, codes};

fn every_error() -> Vec<Error> {
    vec![
        Error::DuplicateLayer {
            layer: "group:group:prod".to_owned(),
        },
        Error::EmptyGroup,
        Error::InvalidKey {
            key: "logging".to_owned(),
            reason: "it needs two segments".to_owned(),
        },
        Error::UnknownSetting {
            key: "runtime.nope".to_owned(),
        },
        Error::ScopeNotAllowed {
            key: "tenant.default_currency".to_owned(),
            scope: "group".to_owned(),
        },
        Error::ReadOnly {
            key: "os.kernel.version".to_owned(),
        },
        Error::SecretSetting {
            key: "integrations.api_token".to_owned(),
        },
        Error::InvalidValue {
            key: "runtime.jobs.worker_concurrency".to_owned(),
            reason: "above the maximum".to_owned(),
        },
        Error::GroupMismatch {
            group: "prod".to_owned(),
            scope: "group:other".to_owned(),
        },
        Error::UnsupportedScope {
            scope: "company:acme/C1".to_owned(),
        },
        Error::InvalidOperation {
            id: "not an id".to_owned(),
        },
        Error::EmptyPlan,
        Error::NotApproved {
            decision: "pending".to_owned(),
        },
        Error::PlanChanged,
        Error::RehearsalFailed { problems: 2 },
        Error::InvalidExemption {
            reason: "no reason".to_owned(),
        },
        Error::Rollout {
            message: "the rollout is not paused".to_owned(),
        },
        Error::UnknownServer {
            server: "srv-999".to_owned(),
        },
    ]
}

#[test]
fn every_raised_code_has_a_catalogue_entry() {
    let catalogue = spot_errors::Catalogue::load();
    for error in every_error() {
        let entry = catalogue.lookup(error.code());
        assert!(
            entry.is_some(),
            "code {} has no catalogue entry",
            error.code()
        );
        assert_eq!(
            entry.map(|entry| entry.since.as_str()),
            Some("0.1.0"),
            "code {} has the wrong since",
            error.code()
        );
    }
}

#[test]
fn the_code_constants_match_the_raised_codes() {
    let raised: std::collections::BTreeSet<&str> = every_error().iter().map(Error::code).collect();
    let constants = [
        codes::DUPLICATE_LAYER,
        codes::EMPTY_GROUP,
        codes::INVALID_KEY,
        codes::UNKNOWN_SETTING,
        codes::SCOPE_NOT_ALLOWED,
        codes::READ_ONLY,
        codes::SECRET_SETTING,
        codes::INVALID_VALUE,
        codes::GROUP_MISMATCH,
        codes::UNSUPPORTED_SCOPE,
        codes::INVALID_OPERATION,
        codes::EMPTY_PLAN,
        codes::NOT_APPROVED,
        codes::PLAN_CHANGED,
        codes::REHEARSAL_FAILED,
        codes::INVALID_EXEMPTION,
        codes::ROLLOUT,
        codes::UNKNOWN_SERVER,
    ];
    for constant in constants {
        assert!(
            raised.contains(constant),
            "constant {constant} is never raised"
        );
    }
}
