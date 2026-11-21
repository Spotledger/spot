//! How the workload classifies what the engine returned.
//!
//! | Engine result | Class | Why |
//! | --- | --- | --- |
//! | `Ok` | `Committed` | the engine answered after its commit |
//! | `Unavailable` with `storage_code = storage.commit_outcome_unknown` | `Indeterminate` | the commit message went out and the reply is lost |
//! | `Unavailable` with any other storage code (connection refused or lost, timeout, retries exhausted) | `Indeterminate` | conservative: the commit may or may not have happened; the re-send with the same command id settles it |
//! | `StaleVersion`, `InvalidState`, `NotFound`, `Duplicate`, `Linked`, `Validation`, `MethodNotFound`, `UnknownDocType` | `Refused` | a clean refusal that rolled back |
//! | `PermissionDenied` | `Denied` | a refusal by permission |
//! | `Internal` | `Unclassified` | a failure nobody planned for: the run fails and names the code |

use spot_app_api::{AppError, ErrorClass};

/// What an engine result means for the audit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Class {
    /// Committed.
    Committed,
    /// Refused cleanly; nothing happened.
    Refused(String),
    /// Refused by permission; nothing happened.
    Denied(String),
    /// The commit state is unknown: re-send with the same command id.
    Indeterminate(String),
    /// Neither: the run fails with this code.
    Unclassified(String),
}

/// The classification table, for the README and the tests.
pub const TABLE: [(&str, &str); 4] = [
    ("Unavailable (any storage code)", "Indeterminate"),
    (
        "StaleVersion, InvalidState, NotFound, Duplicate, Linked, Validation, MethodNotFound, UnknownDocType",
        "Refused",
    ),
    ("PermissionDenied", "Denied"),
    ("Internal", "Unclassified (the run fails)"),
];

/// Classifies an error.
pub fn classify(e: &AppError) -> Class {
    match e.class {
        ErrorClass::Unavailable => Class::Indeterminate(
            e.details
                .get("storage_code")
                .and_then(|v| v.as_str())
                .unwrap_or(e.code.as_str())
                .to_owned(),
        ),
        ErrorClass::PermissionDenied => Class::Denied(e.code.clone()),
        ErrorClass::StaleVersion
        | ErrorClass::InvalidState
        | ErrorClass::NotFound
        | ErrorClass::Duplicate
        | ErrorClass::Linked
        | ErrorClass::Validation
        | ErrorClass::MethodNotFound
        | ErrorClass::UnknownDocType => Class::Refused(e.code.clone()),
        ErrorClass::Internal => Class::Unclassified(e.code.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_class_of_error_has_one_classification() {
        let mk = |class, code: &str| AppError::new(class, code, "m");
        assert!(matches!(
            classify(&mk(ErrorClass::Unavailable, "server.unavailable")),
            Class::Indeterminate(_)
        ));
        assert!(matches!(
            classify(&mk(ErrorClass::StaleVersion, "document.stale_version")),
            Class::Refused(_)
        ));
        assert!(matches!(
            classify(&mk(ErrorClass::PermissionDenied, "permission.denied")),
            Class::Denied(_)
        ));
        assert!(matches!(
            classify(&mk(ErrorClass::Internal, "http.internal")),
            Class::Unclassified(_)
        ));
    }

    #[test]
    fn the_storage_code_of_an_unavailable_error_is_kept() {
        let e = AppError::new(ErrorClass::Unavailable, "server.unavailable", "m")
            .detail("storage_code", "storage.commit_outcome_unknown");
        assert_eq!(
            classify(&e),
            Class::Indeterminate("storage.commit_outcome_unknown".into())
        );
    }
}
