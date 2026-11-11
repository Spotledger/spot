//! Authentication and sessions: sign-in, sign-out, lockout, the first sign-in secret.

use serde_json::json;

use crate::fixture::{Fixture, MANAGER, MEMBER, OBSERVER, THROTTLED};
use crate::report::CaseResult;
use crate::{ensure, failure};

/// A sign-in answers a token, an expiry and the identity of the user who signed in.
pub async fn sign_in_returns_a_session_and_the_user(fixture: &Fixture) -> CaseResult {
    let user = fixture.user(MANAGER);
    let reply = fixture
        .client()
        .login(&user.email, user.password.as_deref().unwrap_or_default())
        .await?;
    reply.expect(200)?;
    let data = reply.data();
    ensure!(
        data.get("token")
            .and_then(|t| t.as_str())
            .is_some_and(|t| !t.is_empty()),
        "the sign-in answered no token: {}",
        reply.redacted_text()
    );
    ensure!(
        data.get("expires_at").and_then(|t| t.as_str()).is_some(),
        "the sign-in answered no expiry: {}",
        reply.redacted_text()
    );
    ensure!(
        data.pointer("/user/id").and_then(|id| id.as_str()) == Some(user.id.as_str()),
        "the sign-in named another user: {}",
        reply.redacted_text()
    );
    ensure!(
        data.pointer("/user/email").and_then(|e| e.as_str()) == Some(user.email.as_str()),
        "the sign-in named another e-mail: {}",
        reply.redacted_text()
    );
    Ok(())
}

/// Signing out ends the session: the same token no longer answers.
pub async fn sign_out_ends_the_session(fixture: &Fixture) -> CaseResult {
    let client = fixture.sign_in(crate::fixture::VIEWER).await?;
    client.session().await?.expect(200)?;
    client.logout().await?.expect(204)?;
    let after = client.session().await?;
    ensure!(
        after.status == 401,
        "the ended session still answers HTTP {}: {}",
        after.status,
        after.text()
    );
    ensure!(
        after.error_code().as_deref() == Some("auth.session_expired"),
        "the ended session answers `{:?}`, not `auth.session_expired`: {}",
        after.error_code(),
        after.text()
    );
    Ok(())
}

/// The session endpoint reports the same identity the sign-in did.
pub async fn session_reports_the_signed_in_user(fixture: &Fixture) -> CaseResult {
    let user = fixture.user(MEMBER);
    let client = fixture.sign_in(MEMBER).await?;
    let reply = client.session().await?;
    reply.expect(200)?;
    let data = reply.data();
    ensure!(
        data.pointer("/user/id").and_then(|id| id.as_str()) == Some(user.id.as_str()),
        "the session names another user: {}",
        reply.text()
    );
    ensure!(
        data.get("site").and_then(|s| s.as_str()) == Some(fixture.site()),
        "the session names another site: {}",
        reply.text()
    );
    Ok(())
}

/// One wrong password and an unknown user get the same refusal, so the answer never says
/// which part was wrong.
pub async fn wrong_password_is_refused_without_saying_which_part(fixture: &Fixture) -> CaseResult {
    let user = fixture.user(OBSERVER);
    // A loopback alias: the sign-in throttle counts failures per source, and these two failed
    // attempts must not slow down another case's sign-in.
    let client = fixture
        .client()
        .with_source("127.0.0.2".parse().expect("a loopback alias"));
    let wrong = client
        .login(&user.email, "not-the-password-9-quartz")
        .await?;
    wrong.expect_error("auth.invalid_credentials")?;
    ensure!(
        wrong.status == 401,
        "a wrong password answers HTTP {}, not 401: {}",
        wrong.status,
        wrong.text()
    );
    let unknown = client
        .login("nobody@l3.test", "not-the-password-9-quartz")
        .await?;
    unknown.expect_error("auth.invalid_credentials")?;
    let wrong_message = wrong
        .json()
        .pointer("/error/message")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_owned();
    let unknown_message = unknown
        .json()
        .pointer("/error/message")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_owned();
    ensure!(
        wrong_message == unknown_message,
        "a wrong password says `{wrong_message}` and an unknown user says `{unknown_message}`; \
         the answer reveals which part was wrong"
    );
    ensure!(
        !wrong_message.contains(&user.email),
        "the refusal names the account: {wrong_message}"
    );
    Ok(())
}

/// A source that keeps failing is backed off: the refusal is a 429 whose envelope names the
/// wait a client must observe before its next attempt.
pub async fn a_throttled_sign_in_reports_retry_after(fixture: &Fixture) -> CaseResult {
    let user = fixture.user(THROTTLED);
    // The throttle counts per source, so this case owns a loopback alias and an account no
    // other case signs in as; two failures are enough with the fixture's `backoff_after = 2`.
    let client = fixture
        .client()
        .with_source("127.0.0.3".parse().expect("a loopback alias"));
    for attempt in 1..=2 {
        let refused = client
            .login(&user.email, "still-not-the-password-4-birch")
            .await?;
        ensure!(
            refused.status == 401,
            "failed attempt {attempt} answers HTTP {}, not 401: {}",
            refused.status,
            refused.redacted_text()
        );
        refused.expect_error("auth.invalid_credentials")?;
    }
    let throttled = client
        .login(&user.email, "still-not-the-password-4-birch")
        .await?;
    throttled.expect_error("api.rate_limited")?;
    ensure!(
        throttled.status == 429,
        "the backed-off attempt answers HTTP {}, not 429: {}",
        throttled.status,
        throttled.redacted_text()
    );
    ensure!(
        throttled
            .error_details()
            .get("retry_after")
            .and_then(|seconds| seconds.as_u64())
            .is_some_and(|seconds| seconds >= 1),
        "the 429 names no positive `details.retry_after`: {}",
        throttled.redacted_text()
    );
    Ok(())
}

/// A request without a credential is refused before the engine sees it.
pub async fn an_unauthenticated_request_never_reaches_the_engine(fixture: &Fixture) -> CaseResult {
    let before = fixture.engine_ops();
    let reply = fixture.client().anonymous().meta("task").await?;
    reply.expect_error("auth.required")?;
    ensure!(
        reply.status == 401,
        "the anonymous request answers HTTP {}, not 401: {}",
        reply.status,
        reply.text()
    );
    let after = fixture.engine_ops();
    ensure!(
        after == before,
        "the engine served {} operation(s) for a refused request ({before} -> {after})",
        after - before
    );
    Ok(())
}

/// Changing the password keeps the session that changed it.
pub async fn a_password_change_keeps_the_session(fixture: &Fixture) -> CaseResult {
    let user = fixture.user(MEMBER);
    let original = user.password.as_deref().expect("the member has a password");
    let client = fixture.sign_in(MEMBER).await?;
    let changed = client
        .send_json(
            "POST",
            "/api/v1/auth/password",
            &[],
            &json!({"current": original, "new": "meadow-saffron-cedar-pilot-31"}),
        )
        .await?;
    changed.expect(204)?;
    let session = client.session().await?;
    session.expect(200)?;
    ensure!(
        session
            .data()
            .pointer("/user/id")
            .and_then(|id| id.as_str())
            == Some(user.id.as_str()),
        "the kept session names another user: {}",
        session.text()
    );
    let restored = client
        .send_json(
            "POST",
            "/api/v1/auth/password",
            &[],
            &json!({"current": "meadow-saffron-cedar-pilot-31", "new": original}),
        )
        .await?;
    restored.expect(204)?;
    let again = fixture.client().login(&user.email, original).await?;
    if again.status != 200 {
        return Err(failure!(
            "the member's password was not restored, later cases cannot sign in: {}",
            again.text()
        ));
    }
    Ok(())
}
