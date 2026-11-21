//! Keys and sessions administered over HTTP (WP-IAM-12.T3): the surfaces a host without a secrets
//! vault can prove: a list of keys, a malformed key request, ending a user's sessions (the next
//! request of that user is 401), and impersonation refused by default. Key creation with a sealed
//! secret needs the vault and is covered by the bundle's spot-cli tests (see the report).

use serde_json::json;

use super::{CaseDef, CaseFuture};
use crate::fixture::{Fixture, VIEWER};
use crate::report::CaseResult;
use crate::{ensure, failure};

/// After user administration; the case owns its fixture.
pub const ORDER: u32 = 43;

const SECTION: &str = "Key and session administration";
const BASE: &str = "/api/v1/identity";

fn run_case(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!("the key case could not start its own fixture"));
        };
        let result = keys_and_sessions(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "key_admin: keys_and_sessions_are_administered_and_impersonation_is_refused",
        SECTION,
        "the key list is `200` for an administrator and `403` for a Viewer, a key request with a bad scope is `422 identity.invalid`, ending a Viewer's sessions makes the Viewer's next request `401`, a session list never carries a token, and impersonation is refused for everyone (`403`) because no support grant exists",
        false,
        run_case,
    )]
}

async fn keys_and_sessions(fixture: &Fixture) -> CaseResult {
    // A fresh sign-in is not a step-up: the administration below needs a recent one.
    let admin = fixture.admin_step_up().await?;
    admin.get(&format!("{BASE}/api-keys")).await?.expect(200)?;
    let viewer = fixture.sign_in(VIEWER).await?;
    viewer.get(&format!("{BASE}/api-keys")).await?.expect(403)?;
    let owner = fixture.admin_id();
    let bad = admin
        .send_json(
            "POST",
            &format!("{BASE}/api-keys"),
            &[("Idempotency-Key", "k-bad-scope")],
            &json!({"name": "ci", "owner_id": owner, "scopes": ["not a scope"]}),
        )
        .await?;
    bad.expect(422)?.expect_error("identity.invalid")?;
    ensure!(
        !bad.text().contains("not a scope"),
        "the refusal echoes the submitted scope: {}",
        bad.text()
    );

    let viewer_id = fixture.user(VIEWER).id.clone();
    let listed = admin
        .get(&format!("{BASE}/sessions?user={viewer_id}"))
        .await?;
    listed.expect(200)?;
    ensure!(
        !listed.text().contains("token"),
        "a session list names a token: {}",
        listed.text()
    );
    let ended = admin
        .send_json(
            "POST",
            &format!("{BASE}/users/{viewer_id}/sessions/end-all"),
            &[],
            &json!({}),
        )
        .await?;
    ended.expect(200)?;
    viewer.session().await?.expect(401)?;

    let start = admin
        .send_json(
            "POST",
            &format!("{BASE}/impersonation"),
            &[],
            &json!({"user_id": viewer_id, "reason": "support case"}),
        )
        .await?;
    ensure!(
        start.status == 403,
        "impersonation was not refused: HTTP {} {}",
        start.status,
        start.text()
    );
    Ok(())
}
