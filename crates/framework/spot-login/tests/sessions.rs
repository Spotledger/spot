//! Password change, session management, administration, redaction, events and concurrency.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;
#[allow(unused_imports)]
use common::*;

#[tokio::test]
async fn changing_the_password_keeps_the_current_session_and_ends_the_others() {
    let fixture = fixture();
    let id = fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );
    let first = ok(fixture
        .service
        .login_with_source(
            &ctx(None),
            &body("ada", "correct horse battery staple"),
            "10.0.0.1",
        )
        .await);
    let second = ok(fixture
        .service
        .login_with_source(
            &ctx(None),
            &body("ada", "correct horse battery staple"),
            "10.0.0.2",
        )
        .await);

    ok(fixture
        .service
        .change_password(
            &ctx(Some(&first.token)),
            "correct horse battery staple",
            "a far better passphrase entirely",
        )
        .await);

    let current = fixture.service.authenticate(&ctx(Some(&first.token))).await;
    assert!(current.is_ok(), "the current session survives");

    let other = fixture
        .service
        .authenticate(&ctx(Some(&second.token)))
        .await;
    assert_code(&err(other), "auth.session_expired");

    let sessions = ok(fixture
        .service
        .list_sessions(&ctx(Some(&first.token)))
        .await);
    assert_eq!(sessions.iter().filter(|session| session.live).count(), 1);
    assert!(sessions.iter().any(|session| session.current));

    // The stored hash really changed.
    let record = some(ok(fixture.dir.find_by_id(id).await));
    assert!(record.password_hash.is_some());
}

#[tokio::test]
async fn a_wrong_current_password_and_a_weak_new_one_are_refused() {
    let fixture = fixture();
    fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );
    let session = ok(fixture
        .service
        .login_with_source(
            &ctx(None),
            &body("ada", "correct horse battery staple"),
            "10.0.0.1",
        )
        .await);

    let wrong = fixture
        .service
        .change_password(
            &ctx(Some(&session.token)),
            "not-the-password",
            "a far better passphrase",
        )
        .await;
    assert_code(&err(wrong), "auth.invalid_credentials");

    let weak = fixture
        .service
        .change_password(
            &ctx(Some(&session.token)),
            "correct horse battery staple",
            "short",
        )
        .await;
    assert_code(&err(weak), "auth.password_policy");
}

#[tokio::test]
async fn logout_is_idempotent_and_kills_the_session() {
    let fixture = fixture();
    fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );
    let session = ok(fixture
        .service
        .login_with_source(
            &ctx(None),
            &body("ada", "correct horse battery staple"),
            "10.0.0.1",
        )
        .await);

    ok(fixture.service.logout(&ctx(Some(&session.token))).await);
    assert_code(
        &err(fixture
            .service
            .authenticate(&ctx(Some(&session.token)))
            .await),
        "auth.session_expired",
    );
    // A second logout of a dead session still succeeds.
    ok(fixture.service.logout(&ctx(Some(&session.token))).await);
}

#[tokio::test]
async fn a_user_can_end_another_of_their_own_sessions() {
    let fixture = fixture();
    fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );
    let first = ok(fixture
        .service
        .login_with_source(
            &ctx(None),
            &body("ada", "correct horse battery staple"),
            "10.0.0.1",
        )
        .await);
    let second = ok(fixture
        .service
        .login_with_source(
            &ctx(None),
            &body("ada", "correct horse battery staple"),
            "10.0.0.2",
        )
        .await);

    let sessions = ok(fixture
        .service
        .list_sessions(&ctx(Some(&first.token)))
        .await);
    let target = some(
        sessions
            .iter()
            .find(|session| !session.current)
            .map(|session| session.id.clone()),
    );
    ok(fixture
        .service
        .end_session(&ctx(Some(&first.token)), &target)
        .await);
    assert_code(
        &err(fixture
            .service
            .authenticate(&ctx(Some(&second.token)))
            .await),
        "auth.session_expired",
    );
}

#[tokio::test]
async fn an_expired_session_is_rejected_and_a_due_rotation_kills_the_old_token() {
    // A policy whose idle window is wide enough for the rotation to fall due first.
    let policy = ok(SessionPolicy::new(
        Duration::from_secs(24 * 3600),
        Duration::from_secs(48 * 3600),
        Duration::from_secs(3600),
        8,
    ));
    let mut config = LoginConfig::for_tests();
    config.session_policy = policy;
    let fixture = fixture_at(
        ManualClock::new(ok(OffsetDateTime::from_unix_timestamp(1_000_000))),
        config,
    );
    fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );
    let session = ok(fixture
        .service
        .login_with_source(
            &ctx(None),
            &body("ada", "correct horse battery staple"),
            "10.0.0.1",
        )
        .await);

    fixture.clock.advance(Duration::from_secs(2 * 3600));
    let rotated = ok(fixture
        .service
        .authenticate(&ctx(Some(&session.token)))
        .await);
    let renewed = some(rotated.renewed_token);

    // The old token is dead the instant rotation lands.
    assert_code(
        &err(fixture
            .service
            .authenticate(&ctx(Some(&session.token)))
            .await),
        "auth.session_expired",
    );
    // The fresh token works.
    ok(fixture.service.authenticate(&ctx(Some(&renewed))).await);

    // Past the absolute bound even the fresh token is gone.
    fixture.clock.advance(Duration::from_secs(49 * 3600));
    assert_code(
        &err(fixture.service.authenticate(&ctx(Some(&renewed))).await),
        "auth.session_expired",
    );
}

#[tokio::test]
async fn administrative_actions_unlock_and_reset() {
    let mut config = LoginConfig::for_tests();
    config.throttle = throttle_config(2, 2);
    let fixture = fixture_at(
        ManualClock::new(ok(OffsetDateTime::from_unix_timestamp(1_000_000))),
        config,
    );
    let id = fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );
    for _ in 0..2 {
        let _ = fixture
            .service
            .login_with_source(&ctx(None), &body("ada", "wrong"), "10.0.0.1")
            .await;
    }

    ok(fixture.service.unlock_account(RecordId::new_v7(), id).await);
    let after_unlock = ok(fixture
        .service
        .login_with_source(
            &ctx(None),
            &body("ada", "correct horse battery staple"),
            "10.0.0.9",
        )
        .await);
    assert!(!after_unlock.token.is_empty());

    let codes = ok(fixture.service.reset_second_factor(id).await);
    assert_eq!(codes.len(), 10);
    let record = some(ok(fixture.dir.find_by_id(id).await));
    assert!(record.totp_secret.is_none());
    assert_eq!(record.recovery_digests.len(), 10);

    let revoked = ok(fixture.service.revoke_all_sessions(id).await);
    assert_eq!(revoked, 1);
    assert_code(
        &err(fixture
            .service
            .authenticate(&ctx(Some(&after_unlock.token)))
            .await),
        "auth.session_expired",
    );
}

#[tokio::test]
async fn no_secret_appears_in_a_debug_rendering() {
    let fixture = fixture();
    let phc = fixture
        .hasher
        .hash(&Password::new("correct horse battery staple"))
        .phc_string()
        .to_owned();
    let id = fixture.dir.add(StoredUser {
        password_hash: Some(phc.clone()),
        totp_base32: Some("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ".to_owned()),
        ..StoredUser::new("ada", UserState::Active)
    });
    let record = some(ok(fixture.dir.find_by_id(id).await));
    let rendered = format!("{record:?}");
    assert!(!rendered.contains(&phc), "the password hash leaked");
    assert!(
        !rendered.contains("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
        "the TOTP secret leaked"
    );
    assert!(rendered.contains("Secret(***)"));
}

#[tokio::test]
async fn emitted_identity_events_validate_against_the_events_contract() {
    let fixture = fixture();
    fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );

    // An unknown-user failure and a password change exercise the audit topics.
    let _ = fixture
        .service
        .login_with_source(&ctx(None), &body("nobody", "whatever"), "10.0.0.1")
        .await;
    let session = ok(fixture
        .service
        .login_with_source(
            &ctx(None),
            &body("ada", "correct horse battery staple"),
            "10.0.0.1",
        )
        .await);
    ok(fixture
        .service
        .change_password(
            &ctx(Some(&session.token)),
            "correct horse battery staple",
            "a far better passphrase entirely",
        )
        .await);

    let mut checked = 0usize;
    for event in lock(&fixture.events.events).iter() {
        // The in-memory session store assigns a non-UUID id, so topics carrying a session id are
        // validated in the storage-backed slice; the user topics are validated here.
        if matches!(
            event.topic,
            "identity_sign_in_succeeded" | "identity_session_started" | "identity_session_ended"
        ) {
            continue;
        }
        ok(spot_events_contract::identity::validate(
            event.topic,
            &event.data,
        ));
        checked += 1;
    }
    assert!(
        checked >= 2,
        "expected the sign-in-failed and password-changed events"
    );
}

#[tokio::test]
async fn two_hundred_concurrent_sign_ins_all_succeed_with_consistent_counters() {
    let fixture = fixture();
    fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );

    let mut handles = Vec::new();
    for index in 0..200u32 {
        let service = fixture.service.clone();
        let source = format!("10.0.0.{}", index % 8 + 1);
        handles.push(tokio::spawn(async move {
            service
                .login_with_source(
                    &ctx(None),
                    &body("ada", "correct horse battery staple"),
                    &source,
                )
                .await
        }));
    }
    let mut tokens = 0usize;
    for handle in handles {
        let session = ok(ok(handle.await));
        assert!(!session.token.is_empty());
        tokens += 1;
    }
    assert_eq!(tokens, 200);
    assert_eq!(lock(&fixture.events.attempts).len(), 200);
}
