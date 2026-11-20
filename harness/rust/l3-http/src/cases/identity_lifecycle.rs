//! End-to-end identity lifecycle over HTTP (WP-IAM-12.T6): the acceptance rows of WP-IAM-02,
//! IAM-03, IAM-04 and IAM-07 driven the way an administrator drives them.
//!
//! One case runs for real and gates the rest: the identity administration surface is served and
//! authenticated (a list answers an administrator `200` with a `data` array, and a caller with no
//! credential `401`, never `404`, which is how an unserved operation answers). The nine
//! behaviour cases of the brief are registered as **blocked**, each with the task that owns what
//! it needs and the reason: the contract requires a recent step-up for the enable and
//! impersonation operations (`403 auth.step_up_required`), the fixture has no helper that
//! performs one, and no helper that gives an administration-created user a password. Neither
//! lives in an owned path, so the lead widens the paths (see the report's open questions).

use super::{CaseDef, CaseFuture};
use crate::ensure;
use crate::fixture::Fixture;
use crate::report::CaseResult;

/// After the shared groups; independent of their data.
pub const ORDER: u32 = 50;

const SECTION: &str = "Identity lifecycle";

const NEEDS_HELPERS: &str = "the case needs a harness helper to perform a step-up and to give a user created through `identity_user_create` a password; `fixture.rs` has neither and is not an owned path of this task";

/// The lists an administrator can read without a path parameter.
const LISTS: [&str; 3] = [
    "/api/v1/identity/users",
    "/api/v1/identity/invitations",
    "/api/v1/identity/api-keys",
];

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "identity.administration_surface_is_served_and_authenticated",
            SECTION,
            "the user, invitation and API-key lists answer an administrator 200 with a `data` array and a caller with no credential 401 (never 404), a Viewer is refused 403",
            false,
            run_surface,
        ),
        CaseDef::blocked(
            "identity.leaver_ends_everything",
            SECTION,
            "disabling a user ends two sessions, a key and an invitation within one second; enabling does not bring them back; the actions are audited",
            "WP-IAM-12.T2",
            NEEDS_HELPERS,
        ),
        CaseDef::blocked(
            "identity.last_administrator_protected",
            SECTION,
            "the only Tenant Administrator cannot be disabled, demoted or left by an assignment end or a rule change; with a second one it can; two concurrent demotions leave one",
            "WP-IAM-12.T2",
            NEEDS_HELPERS,
        ),
        CaseDef::blocked(
            "identity.anonymise_keeps_id",
            SECTION,
            "an anonymised user keeps its id, document links still resolve, name, e-mail and linked identities are gone from the directory, the lists and the audit read model, and sign-in fails",
            "WP-IAM-12.T3",
            NEEDS_HELPERS,
        ),
        CaseDef::blocked(
            "identity.scoped_roles",
            SECTION,
            "one user is Manager for Company A and Viewer for Company B and sees exactly that; a write on a Company B document is refused; the permission explanation agrees",
            "WP-IAM-03.T2",
            NEEDS_HELPERS,
        ),
        CaseDef::blocked(
            "identity.upgrade_diff_leaves_copies",
            SECTION,
            "a newer reference pack shows a diff, the customised copy stays byte-identical and the reference role changes",
            "WP-IAM-03.T2",
            NEEDS_HELPERS,
        ),
        CaseDef::blocked(
            "identity.request_finishes_under_its_snapshot",
            SECTION,
            "a slow request that started before a role change finishes under the old rights and the next request sees the new ones",
            "WP-IAM-03.T2",
            "the case needs a controllable delay in the harness's host, which the fixture does not offer",
        ),
        CaseDef::blocked(
            "identity.assignment_sources",
            SECTION,
            "a rule-owned assignment cannot be edited or removed by hand, ends when its rule stops matching, and a manual assignment of the same role survives; the same for an identity-provider-owned one",
            "WP-IAM-04.T3",
            NEEDS_HELPERS,
        ),
        CaseDef::blocked(
            "identity.key_never_exceeds_owner",
            SECTION,
            "a key wider than its owner is refused, a narrower one works only in its scope, loses a right when the owner does, and stops when the owner is disabled",
            "WP-IAM-07.T3",
            NEEDS_HELPERS,
        ),
        CaseDef::blocked(
            "identity.impersonation_limits",
            SECTION,
            "support impersonation of a normal user works and the trail carries both identities; an administrator or a break-glass user is refused; an impersonated session cannot create a key, change a password or end the real user's other sessions",
            "WP-IAM-12.T3",
            NEEDS_HELPERS,
        ),
    ]
}

/// The registered cases run after the shared host was shut down (the shutdown case), so this one
/// starts a host of its own.
fn run_surface(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut fixture) = Fixture::start_secondary().await else {
            return Err(crate::failure!(
                "the identity case could not start its own fixture"
            ));
        };
        let result = surface(&fixture).await;
        fixture.finish().await;
        result
    })
}

async fn surface(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let anonymous = fixture.client().anonymous();
    let viewer = fixture.sign_in(crate::fixture::VIEWER).await?;
    for path in LISTS {
        let listed = admin.get(path).await?;
        ensure!(
            listed.status != 404,
            "{path} answers 404: the operation is not served ({})",
            listed.redacted_text()
        );
        listed.expect(200)?;
        ensure!(
            listed
                .json()
                .get("data")
                .is_some_and(|data| data.is_array()),
            "{path} does not answer a `data` array: {}",
            listed.redacted_text()
        );
        let unsigned = anonymous.get(path).await?;
        ensure!(
            unsigned.status == 401,
            "{path} without a credential answers {} instead of 401",
            unsigned.status
        );
        let refused = viewer.get(path).await?;
        ensure!(
            refused.status == 403,
            "{path} for a Viewer answers {} instead of 403: {}",
            refused.status,
            refused.redacted_text()
        );
    }
    // A user id that does not exist answers the same 404 for an administrator as an id of another
    // tenant would; the shape of that answer is `identity.not_found`.
    let missing = admin
        .get("/api/v1/identity/users/00000000-0000-7000-8000-000000000000")
        .await?;
    ensure!(
        missing.status == 404 && missing.error_code().as_deref() == Some("identity.not_found"),
        "an unknown user answers {} {:?} instead of 404 identity.not_found",
        missing.status,
        missing.error_code()
    );
    Ok(())
}
