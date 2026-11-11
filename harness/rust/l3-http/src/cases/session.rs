//! A user's own sessions (`GET /api/v1/auth/sessions`, `DELETE /api/v1/auth/sessions/{id}`,
//! WP-IAM-07.T2): the list is the caller's own live sessions with the current one first and no
//! token in it; ending another own session or the current one stops that token at once; another
//! user's id answers exactly like an unknown id; a first-sign-in session is denied (not an empty list).
//!
//! A registered case runs after the hand-written block, whose last case stops the shared host,
//! so these cases start their own fixture (one more database and host) and finish it.

use serde_json::Value as Json;

use super::{CaseDef, CaseFuture};
use crate::client::Reply;
use crate::fixture::{Fixture, INVITED, MANAGER, VIEWER};
use crate::report::CaseResult;
use crate::{ensure, failure};

/// After the reports and before the events cases.
pub const ORDER: u32 = 17;

const SECTION: &str = "Sessions";

fn run_own(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start().await else {
            return Err(failure!("the session case could not start its own fixture"));
        };
        let result = a_user_lists_and_ends_only_their_own_sessions(&own).await;
        own.finish().await;
        result
    })
}

fn run_key(_shared: &Fixture) -> CaseFuture<'_> {
    Box::pin(async {
        let Some(mut own) = Fixture::start().await else {
            return Err(failure!("the session case could not start its own fixture"));
        };
        let result = a_first_sign_in_session_and_a_bearer_secret_cannot_manage_sessions(&own).await;
        own.finish().await;
        result
    })
}

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "sessions: a_user_lists_and_ends_only_their_own_sessions_and_a_foreign_id_is_not_found",
            SECTION,
            "`GET /auth/sessions` lists the caller's own live sessions, the current one first, with no token in the answer; `DELETE /auth/sessions/{id}` is 204 for an own session (that token is 401 on the next request, the current one included) and the same 404 `auth.session_not_found` for another user's id as for an unknown one",
            false,
            run_own,
        ),
        CaseDef::run(
            "sessions: a_first_sign_in_session_and_a_bearer_secret_cannot_manage_sessions",
            SECTION,
            "`GET /auth/sessions` and `DELETE /auth/sessions/{id}` from a session that has not chosen its password are 403, never an empty list; the one-time secret as a bearer and an unsigned request are 401; reading the session with a refused bearer is not a 500 (the API-key refusal itself, `403 permission.denied`, is pinned in `spot-cli`'s `tests/session.rs`)",
            false,
            run_key,
        ),
    ]
}

fn ids(reply: &Reply) -> Vec<String> {
    reply
        .data()
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("id").and_then(Json::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn current(reply: &Reply) -> Option<String> {
    reply.data().as_array().and_then(|rows| {
        rows.iter()
            .find(|row| row.get("current").and_then(Json::as_bool) == Some(true))
            .and_then(|row| row.get("id").and_then(Json::as_str).map(str::to_owned))
    })
}

fn same_answer(a: &Reply, b: &Reply) -> bool {
    let strip = |reply: &Reply| {
        let mut body = reply.json();
        if let Some(error) = body.get_mut("error").and_then(Json::as_object_mut) {
            error.remove("request_id");
        }
        (reply.status, body)
    };
    strip(a) == strip(b)
}

async fn a_user_lists_and_ends_only_their_own_sessions(fixture: &Fixture) -> CaseResult {
    let first = fixture.sign_in(MANAGER).await?;
    let second = fixture.sign_in(MANAGER).await?;
    let other_user = fixture.sign_in(VIEWER).await?;

    let listed = second.get("/api/v1/auth/sessions").await?;
    listed.expect(200)?;
    ensure!(
        ids(&listed).len() >= 2,
        "two sign-ins list fewer than two sessions: {}",
        listed.redacted_text()
    );
    ensure!(
        listed
            .data()
            .pointer("/0/current")
            .and_then(Json::as_bool)
            == Some(true),
        "the current session is not first: {}",
        listed.redacted_text()
    );
    for token in [first.bearer(), second.bearer(), other_user.bearer()]
        .into_iter()
        .flatten()
    {
        ensure!(
            !listed.text().contains(token),
            "a session token is in the session list"
        );
    }
    let mine = current(&listed).ok_or_else(|| failure!("no current session in the list"))?;

    // Another user's session is invisible, and its id answers like an unknown id.
    let theirs_list = other_user.get("/api/v1/auth/sessions").await?;
    theirs_list.expect(200)?;
    let theirs = current(&theirs_list).ok_or_else(|| failure!("the viewer has no current session"))?;
    ensure!(
        !ids(&listed).contains(&theirs),
        "another user's session is in the list"
    );
    let foreign = second
        .send("DELETE", &format!("/api/v1/auth/sessions/{theirs}"), &[], None)
        .await?;
    foreign.expect_error("auth.session_not_found")?;
    ensure!(foreign.status == 404, "a foreign id answers HTTP {}", foreign.status);
    let unknown = second
        .send(
            "DELETE",
            "/api/v1/auth/sessions/an-id-that-never-existed",
            &[],
            None,
        )
        .await?;
    ensure!(
        same_answer(&foreign, &unknown),
        "a foreign id and an unknown id answer differently: {} / {}",
        foreign.redacted_text(),
        unknown.redacted_text()
    );
    other_user.session().await?.expect(200)?;

    // Ending the first session (from the second) stops that token at once.
    let first_id = current(&first.get("/api/v1/auth/sessions").await?)
        .ok_or_else(|| failure!("the first session has no current entry"))?;
    let ended = second
        .send("DELETE", &format!("/api/v1/auth/sessions/{first_id}"), &[], None)
        .await?;
    ended.expect(204)?;
    let after = first.session().await?;
    ensure!(
        after.status == 401,
        "the ended session still answers HTTP {}",
        after.status
    );

    // Ending the current session equals logout.
    let ended_self = second
        .send("DELETE", &format!("/api/v1/auth/sessions/{mine}"), &[], None)
        .await?;
    ended_self.expect(204)?;
    let gone = second.session().await?;
    ensure!(
        gone.status == 401,
        "the session that ended itself still answers HTTP {}",
        gone.status
    );
    Ok(())
}

async fn a_first_sign_in_session_and_a_bearer_secret_cannot_manage_sessions(
    fixture: &Fixture,
) -> CaseResult {
    let user = fixture.user(INVITED);
    let Some(secret) = user.secret.clone() else {
        return Err(failure!("the invited user holds no one-time secret"));
    };
    let first = fixture.client().login(&user.email, &secret).await?;
    first.expect(200)?;
    let token = first
        .data()
        .get("token")
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| failure!("the first sign-in answered no token"))?;
    let restricted = fixture.client().with_token(token);
    // A session that has not chosen its password may do nothing here: denied, never a list.
    let listed = restricted.get("/api/v1/auth/sessions").await?;
    ensure!(
        listed.status == 403 && listed.data().is_null(),
        "a first-sign-in session listing sessions answers HTTP {}, not a 403 refusal: {}",
        listed.status,
        listed.redacted_text()
    );
    let ended = restricted
        .send("DELETE", "/api/v1/auth/sessions/anything", &[], None)
        .await?;
    ensure!(
        ended.status == 403,
        "a first-sign-in session ending a session answers HTTP {}, not 403",
        ended.status
    );
    // The one-time secret is not a bearer credential (the API-key path is refused with the same
    // 401 a wrong key gets), and an unsigned request is refused.
    let as_bearer = fixture.client().with_token(secret);
    let bearer_list = as_bearer.get("/api/v1/auth/sessions").await?;
    ensure!(
        bearer_list.status == 401,
        "the secret as a bearer answers HTTP {}, not 401: {}",
        bearer_list.status,
        bearer_list.redacted_text()
    );
    let anonymous = fixture
        .client()
        .anonymous()
        .get("/api/v1/auth/sessions")
        .await?;
    ensure!(
        anonymous.status == 401,
        "an unsigned session list answers HTTP {}, not 401",
        anonymous.status
    );
    // Recorded security finding: reading the session with a refused bearer is a refusal, never a 500.
    let read = as_bearer.session().await?;
    ensure!(
        read.status != 500,
        "reading the session with a bearer secret is a server error: {}",
        read.redacted_text()
    );
    Ok(())
}
