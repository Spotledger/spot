//! Every code this crate raises is catalogued (the completeness rule of
//! `crates/framework/spot-errors/README.md`).
//!
//! Test code may unwrap what it built itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_iam_questions::QuestionsError;

fn all_errors() -> Vec<QuestionsError> {
    vec![
        QuestionsError::RequestInvalid {
            subject: "(request)".into(),
            detail: "x".into(),
        },
        QuestionsError::NotPermitted {
            asker: "ana".into(),
            detail: "x".into(),
        },
        QuestionsError::UnknownDoctype {
            doctype: "invoice".into(),
            detail: "x".into(),
        },
        QuestionsError::UnknownPermission {
            permission: "invoice.x".into(),
            detail: "x".into(),
        },
        QuestionsError::ModelInvalid {
            subject: "snapshot".into(),
            detail: "x".into(),
        },
        QuestionsError::PageInvalid {
            subject: "(page)".into(),
            detail: "x".into(),
        },
        QuestionsError::EmergencyState {
            grant: "eg-1".into(),
            detail: "x".into(),
        },
        QuestionsError::EmergencyDuration {
            grant: "eg-1".into(),
            detail: "x".into(),
        },
        QuestionsError::ReviewSelf {
            review: "rv-1".into(),
            detail: "x".into(),
        },
        QuestionsError::ReviewWrongPerson {
            review: "rv-1".into(),
            detail: "x".into(),
        },
        QuestionsError::Recording {
            subject: "eg-1".into(),
            detail: "x".into(),
        },
        QuestionsError::Approval {
            request: "eg-1".into(),
            detail: "x".into(),
        },
    ]
}

#[test]
fn every_code_has_a_catalogue_entry() {
    let catalogue = spot_errors::Catalogue::load();
    for error in all_errors() {
        let code = error.code();
        assert!(
            catalogue.lookup(code).is_some(),
            "code {code} has no catalogue entry"
        );
    }
}
