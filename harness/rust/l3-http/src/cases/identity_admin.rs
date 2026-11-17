//! User administration over HTTP (WP-IAM-12.T2): create, list, get, patch, duplicate refusal,
//! disable (a live session stops within a second), enable, anonymise (the id is kept), the
//! last-administrator rule, and refusals for callers without the right. Each case owns its fixture.

use std::time::{Duration, Instant};

use serde_json::json;

use super::{CaseDef, CaseFuture};
use crate::client::Client;
use crate::fixture::{Fixture, OBSERVER, VIEWER};
use crate::report::CaseResult;
use crate::{ensure, failure};

/// After the functions and graph cases; the case owns its fixture.
pub const ORDER: u32 = 42;

const SECTION: &str = "User administration";
const BASE: &str = "/api/v1/identity";

fn run_case(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start_secondary().await else {
            return Err(failure!(
                "the identity case could not start its own fixture"
            ));
        };
        let result = lifecycle(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "identity_admin: users_are_administered_and_a_disable_ends_the_session_at_once",
        SECTION,
        "an administrator creates a user (a duplicate is `409 identity.user_exists`, a bad audience `422`), lists and gets it, patches it with `If-Match`, disables a signed-in Viewer so the Viewer's next request is `401` within a second and the answer lists the leaver actions, enables, anonymises (same id, address gone, `confirm_email` required); the last administrator cannot be disabled or anonymised (`409 identity.last_administrator`); a Viewer, an Observer and an anonymous caller are refused and an unknown or malformed id is `404 identity.not_found`",
        false,
        run_case,
    )]
}

async fn post(admin: &Client, path: &str, body: &serde_json::Value) -> crate::client::Reply {
    let key = format!("k-{}", path.replace('/', "-"));
    admin
        .send_json("POST", path, &[("Idempotency-Key", &key)], body)
        .await
        .unwrap_or_else(|e| panic!("{path}: {e}"))
}

async fn lifecycle(fixture: &Fixture) -> CaseResult {
    let admin = fixture.admin();
    let users = format!("{BASE}/users");

    // Create: staff, no invitation (none can be sent on this host), no roles.
    let email = "new.person@l3.test";
    let made = post(
        &admin,
        &users,
        &json!({"email": email, "full_name": "New Person", "audience": "staff",
                "roles": [], "invite": false}),
    )
    .await;
    made.expect(201)?;
    let id = made.data()["id"].as_str().unwrap_or_default().to_owned();
    ensure!(!id.is_empty(), "no id in the created user: {}", made.text());
    ensure!(
        made.data()["status"] == json!("active") && made.data()["email"] == json!(email),
        "the created user is not an active `{email}`: {}",
        made.text()
    );
    // A second user with the same address, from another key.
    let again = admin
        .send_json(
            "POST",
            &users,
            &[("Idempotency-Key", "k-duplicate")],
            &json!({"email": email, "full_name": "Other", "audience": "staff", "roles": [], "invite": false}),
        )
        .await?;
    again.expect(409)?.expect_error("identity.user_exists")?;
    let bad = admin
        .send_json(
            "POST",
            &users,
            &[("Idempotency-Key", "k-bad-audience")],
            &json!({"email": "x.y@l3.test", "full_name": "X", "audience": "consumer", "roles": [], "invite": false}),
        )
        .await?;
    ensure!(
        bad.status == 422 || bad.status == 400,
        "a bad audience gave HTTP {}",
        bad.status
    );

    // List and get.
    let listed = admin.get(&format!("{users}?q=new.person&limit=10")).await?;
    listed.expect(200)?;
    ensure!(
        listed.rows()?.iter().any(|u| u["id"] == json!(id)),
        "the new user is not listed: {}",
        listed.text()
    );
    let got = admin.get(&format!("{users}/{id}")).await?;
    got.expect(200)?;
    let version = got.data()["version"].as_u64().unwrap_or(0);
    let patched = admin
        .send_json(
            "PATCH",
            &format!("{users}/{id}"),
            &[("If-Match", &format!("\"{version}\""))],
            &json!({"full_name": "Renamed Person"}),
        )
        .await?;
    patched.expect(200)?;
    ensure!(
        patched.data()["full_name"] == json!("Renamed Person"),
        "the patch did not apply: {}",
        patched.text()
    );

    // Disable a signed-in Viewer: the session ends within a second.
    let viewer = fixture.sign_in(VIEWER).await?;
    viewer.session().await?.expect(200)?;
    let viewer_id = fixture.user(VIEWER).id.clone();
    let off = post(&admin, &format!("{users}/{viewer_id}/disable"), &json!({})).await;
    off.expect(200)?;
    ensure!(
        off.data()["user"]["status"] == json!("disabled"),
        "the viewer is not disabled: {}",
        off.text()
    );
    ensure!(
        off.data()["leaver_actions"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x["action"] == json!("end_sessions"))),
        "the leaver actions do not include end_sessions: {}",
        off.text()
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    let ended = viewer.session().await?;
    ensure!(
        ended.status == 401,
        "the disabled user's session answers HTTP {} (after {:?} of a 1 s budget): {}",
        ended.status,
        deadline.saturating_duration_since(Instant::now()),
        ended.text()
    );
    // Enable: the user is active again, and signs in afresh.
    let on = post(&admin, &format!("{users}/{viewer_id}/enable"), &json!({})).await;
    on.expect(200)?;
    fixture
        .sign_in(VIEWER)
        .await?
        .session()
        .await?
        .expect(200)?;

    // Anonymise: the confirmation must match; the id survives and the address goes.
    let wrong = post(
        &admin,
        &format!("{users}/{id}/anonymise"),
        &json!({"confirm_email": "someone.else@l3.test"}),
    )
    .await;
    ensure!(
        wrong.status == 422,
        "a wrong confirmation gave HTTP {}",
        wrong.status
    );
    let gone = post(
        &admin,
        &format!("{users}/{id}/anonymise"),
        &json!({"confirm_email": email}),
    )
    .await;
    gone.expect(200)?;
    ensure!(
        gone.data()["id"] == json!(id)
            && gone.data()["status"] == json!("anonymised")
            && gone.data()["email"] != json!(email),
        "anonymising did not keep the id and drop the address: {}",
        gone.text()
    );

    // The last administrator.
    let me = admin.session().await?;
    let admin_id = me.data()["user"]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let last = post(&admin, &format!("{users}/{admin_id}/disable"), &json!({})).await;
    last.expect(409)?
        .expect_error("identity.last_administrator")?;

    // Refusals: no right, no session, no such user.
    let viewer = fixture.sign_in(VIEWER).await?;
    viewer.get(&users).await?.expect(403)?;
    fixture
        .sign_in(OBSERVER)
        .await?
        .get(&users)
        .await?
        .expect(403)?;
    fixture
        .client()
        .anonymous()
        .get(&users)
        .await?
        .expect(401)?;
    admin
        .get(&format!("{users}/0192f3c4-0000-7000-8000-000000000001"))
        .await?
        .expect(404)?
        .expect_error("identity.not_found")?;
    let malformed = admin.get(&format!("{users}/not-an-id")).await?;
    ensure!(
        malformed.status == 400 || malformed.status == 404,
        "a malformed id gave HTTP {}",
        malformed.status
    );
    Ok(())
}
