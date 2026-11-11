//! The recorded override: the auditable record that makes an overridden conflict stand
//! (ADR-070 section 8, rule 1). It covers exactly one (rule, user, scope), carries the reason
//! and the mitigating control, and appears in the access review.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use spot_sod::SodOverride;

use crate::conflict::Conflict;
use crate::error::GovError;
use crate::request::{GrantRequest, MitigatingControl};

/// One recorded override of one SoD conflict: it covers exactly the (rule, user, scope) triple
/// it names, and carries the reason and mitigating control that justified it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SodOverrideRecord {
    /// The rule id the override covers.
    pub rule_id: String,
    /// The user the override covers.
    pub user: String,
    /// The scope the override covers; `None` covers the unscoped conflict and only that.
    pub scope: Option<String>,
    /// Why the conflict is tolerated; non-blank after trimming.
    pub reason: String,
    /// The compensating control that makes it tolerable.
    pub mitigating_control: MitigatingControl,
    /// Who granted the override (the grantor's user name).
    pub granted_by: String,
    /// When it was granted, in integer epoch seconds; the caller stamps it.
    pub granted_at: i64,
}

impl SodOverrideRecord {
    /// The `spot-sod` override this record is, so the SoD analysis treats the conflict as
    /// covered. The mitigating control's id and text are joined as the control text, so the
    /// SoD core (which knows only text) still sees both.
    #[must_use]
    pub fn to_sod_override(&self) -> SodOverride {
        SodOverride {
            rule_id: self.rule_id.clone(),
            user: self.user.clone(),
            scope: self.scope.clone(),
            reason: self.reason.clone(),
            mitigating_control: format!(
                "{}: {}",
                self.mitigating_control.id.trim(),
                self.mitigating_control.text.trim()
            ),
            granted_by: self.granted_by.clone(),
            granted_at: self.granted_at,
        }
    }

    /// The note an access review shows for this override (the `spot-access-review` input):
    /// the reason and the mitigating control, so a reviewer sees why the conflict stands.
    #[must_use]
    pub fn access_review_note(&self) -> String {
        format!(
            "{} (control {}: {})",
            self.reason.trim(),
            self.mitigating_control.id.trim(),
            self.mitigating_control.text.trim()
        )
    }

    /// Whether the record is complete: a non-blank rule, user, reason, grantor and control.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        !self.rule_id.trim().is_empty()
            && !self.user.trim().is_empty()
            && !self.reason.trim().is_empty()
            && !self.granted_by.trim().is_empty()
            && self.mitigating_control.is_complete()
    }
}

/// Builds the auditable override records for a grant's conflicting rules, from the request's
/// override submission.
///
/// One submission yields one record per conflicting rule (the same reason and control cover
/// them all). Every conflicting rule must be a SoD conflict; a refusal cannot be overridden.
///
/// # Errors
///
/// - [`GovError::OverrideIncomplete`] when the request carries no submission, or its reason or
///   control is blank;
/// - [`GovError::OverrideNotPermitted`] when the grantor does not hold the `override_sod`
///   right.
pub fn record_overrides(
    request: &GrantRequest,
    conflicts: &[Conflict],
    at: i64,
) -> Result<Vec<SodOverrideRecord>, GovError> {
    let Some(submission) = &request.override_submission else {
        return Err(GovError::OverrideIncomplete {
            subject: request.id.as_str().to_owned(),
            detail: "an override needs a reason and a mitigating control".to_owned(),
        });
    };
    if !request.grantor.may_override_sod() {
        return Err(GovError::OverrideNotPermitted {
            user: request.grantor.user.clone(),
            detail: "recording an SoD override needs the `override_sod` right".to_owned(),
        });
    }
    if submission.reason.trim().is_empty() {
        return Err(GovError::OverrideIncomplete {
            subject: request.id.as_str().to_owned(),
            detail: "the override reason is blank".to_owned(),
        });
    }
    if !submission.mitigating_control.is_complete() {
        return Err(GovError::OverrideIncomplete {
            subject: request.id.as_str().to_owned(),
            detail: "the mitigating control needs a non-blank id and text".to_owned(),
        });
    }
    let mut seen: BTreeSet<(String, String, Option<String>)> = BTreeSet::new();
    let mut records: Vec<SodOverrideRecord> = Vec::new();
    for conflict in conflicts {
        let Conflict::Sod(sod) = conflict else {
            continue;
        };
        let key = (sod.rule_id.clone(), sod.user.clone(), sod.scope.clone());
        if !seen.insert(key) {
            continue;
        }
        records.push(SodOverrideRecord {
            rule_id: sod.rule_id.clone(),
            user: sod.user.clone(),
            scope: sod.scope.clone(),
            reason: submission.reason.clone(),
            mitigating_control: submission.mitigating_control.clone(),
            granted_by: request.grantor.user.clone(),
            granted_at: at,
        });
    }
    Ok(records)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::request::{GrantRequestId, Grantor, OverrideSubmission};

    fn request(grantor: Grantor) -> GrantRequest {
        let id = GrantRequestId::new("req-1").expect("id");
        GrantRequest::new(&id, "ben", "Finance Approver", grantor)
    }

    #[test]
    fn a_complete_submission_yields_one_record_per_rule() {
        let grantor = Grantor::new("ana", ["override_sod"]);
        let req = request(grantor).with_override(OverrideSubmission::new(
            "month-end only",
            MitigatingControl::new("C-1", "second reviewer"),
        ));
        let conflicts = vec![
            Conflict::Sod(spot_sod::Conflict {
                rule_id: "sod.a".to_owned(),
                severity: spot_sod::Severity::High,
                user: "ben".to_owned(),
                scope: Some("A".to_owned()),
                held: Vec::new(),
                overridden: false,
            }),
            Conflict::Sod(spot_sod::Conflict {
                rule_id: "sod.b".to_owned(),
                severity: spot_sod::Severity::Low,
                user: "ben".to_owned(),
                scope: None,
                held: Vec::new(),
                overridden: false,
            }),
        ];
        let records = record_overrides(&req, &conflicts, 100).expect("records");
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(SodOverrideRecord::is_complete));
        assert_eq!(records[0].granted_by, "ana");
        assert_eq!(records[0].granted_at, 100);
        assert_eq!(
            records[0].to_sod_override().mitigating_control,
            "C-1: second reviewer"
        );
        assert!(records[0].access_review_note().contains("C-1"));
    }

    #[test]
    fn a_grantor_without_the_right_is_refused() {
        let req = request(Grantor::plain("ana")).with_override(OverrideSubmission::new(
            "why",
            MitigatingControl::new("C-1", "control"),
        ));
        let err = record_overrides(&req, &[], 0).expect_err("refused");
        assert_eq!(err.code(), crate::error::OVERRIDE_NOT_PERMITTED);
    }
}
