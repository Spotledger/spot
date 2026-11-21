//! The access questions (WP-IAM-08.T9) over HTTP. The contract has no operation for them yet and
//! the module only provides the in-process port `AccessQuestions`, so the end-to-end behaviours are
//! blocked entries naming what is missing.

use super::CaseDef;

/// After the grant governance case.
pub const ORDER: u32 = 45;

const SECTION: &str = "Access questions";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "access_questions: the_four_questions_answer_from_a_seeded_matrix_for_authorised_askers_only",
            SECTION,
            "what a user can do, who can do an action, who can see a document and what changed between two dates return the expected rows with the producing rule; an unauthorised asker is refused and audited; a document the asker cannot read never appears",
            "WP-CON-03.T41",
            "the native API contract has no operation for the access questions, so no route can carry them; the module provides the `AccessQuestions` port only",
        ),
        CaseDef::blocked(
            "access_questions: emergency_access_starts_expires_and_is_reviewed_by_someone_else",
            SECTION,
            "an emergency grant is requested with a reason, limited in time and roles, started through the governed grant path with source `emergency`, reviewed by a controller who is not the beneficiary, and ends at expiry",
            "WP-IAM-08.T7",
            "emergency access needs the governed grant path with a caller and a stored request; neither exists yet (the gate has no caller and keeps its state in memory)",
        ),
    ]
}
