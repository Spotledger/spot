//! Sign-in decision table, second factor, API keys and throttling.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;
#[allow(unused_imports)]
use common::*;

#[tokio::test]
async fn unknown_user_and_wrong_password_do_the_same_hash_work() {
    let fixture = fixture();
    let hasher = fixture.hasher.clone();
    let _known = fixture.dir.add(
        StoredUser::new("ada", UserState::Active).with_password(hasher.as_ref(), "right-password"),
    );

    let before = hasher.verify_count();
    let unknown = fixture
        .service
        .login_with_source(&ctx(None), &body("nobody", "whatever"), "10.0.0.1")
        .await;
    assert_code(&err(unknown), "auth.invalid_credentials");
    let after_unknown = hasher.verify_count();
    assert_eq!(after_unknown - before, 1, "an unknown user does one hash");

    let wrong = fixture
        .service
        .login_with_source(&ctx(None), &body("ada", "wrong-password"), "10.0.0.1")
        .await;
    assert_code(&err(wrong), "auth.invalid_credentials");
    assert_eq!(
        hasher.verify_count() - after_unknown,
        1,
        "a wrong password does one hash"
    );
}

#[tokio::test]
async fn right_password_signs_in_and_the_session_round_trips() {
    let fixture = fixture();
    let _id = fixture.dir.add(
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
    assert_eq!(session.user.full_name, "ada");

    let principal = ok(fixture
        .service
        .authenticate(&ctx(Some(&session.token)))
        .await);
    assert_eq!(principal.user_name, "ada");

    let info = ok(fixture.service.session(&ctx(Some(&session.token))).await);
    assert_eq!(info.user.id, principal.user_id);
    assert_eq!(info.site, "acme.test");
}

#[tokio::test]
async fn disabled_invited_and_anonymised_all_read_as_invalid_credentials() {
    let fixture = fixture();
    let hasher = fixture.hasher.clone();
    for (name, state) in [
        ("disabled-user", UserState::Disabled),
        ("invited-user", UserState::Invited),
        ("anonymised-user", UserState::Anonymised),
    ] {
        fixture
            .dir
            .add(StoredUser::new(name, state).with_password(hasher.as_ref(), "right-password"));
        let result = fixture
            .service
            .login_with_source(&ctx(None), &body(name, "right-password"), "10.0.0.1")
            .await;
        assert_code(&err(result), "auth.invalid_credentials");
    }
    // The true reason is kept in the audit record, never in the response.
    let codes: Vec<String> = lock(&fixture.events.attempts)
        .iter()
        .map(|attempt| attempt.result_code.clone())
        .collect();
    assert!(codes.contains(&"login.account_disabled".to_owned()));
    assert!(codes.contains(&"login.account_invited".to_owned()));
    assert!(codes.contains(&"login.account_anonymised".to_owned()));
}

#[tokio::test]
async fn totp_required_then_a_valid_code_signs_in_and_a_replay_is_refused() {
    // The RFC 6238 SHA-1 seed, six-digit codes; at unix 60 the current step is 2.
    let clock = ManualClock::new(ok(OffsetDateTime::from_unix_timestamp(60)));
    let fixture = fixture_at(clock, LoginConfig::for_tests());
    let id = fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );
    fixture.dir.mutate(id, |user| {
        user.totp_base32 = Some("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ".to_owned());
    });

    let missing = fixture
        .service
        .login_with_source(
            &ctx(None),
            &body("ada", "correct horse battery staple"),
            "10.0.0.1",
        )
        .await;
    assert_code(&err(missing), "auth.totp_required");

    let wrong = fixture
        .service
        .login_with_source(
            &ctx(None),
            &LoginBody {
                totp: Some("000000".to_owned()),
                ..body("ada", "correct horse battery staple")
            },
            "10.0.0.1",
        )
        .await;
    assert_code(&err(wrong), "auth.totp_invalid");

    let good = ok(fixture
        .service
        .login_with_source(
            &ctx(None),
            &LoginBody {
                totp: Some("359152".to_owned()),
                ..body("ada", "correct horse battery staple")
            },
            "10.0.0.1",
        )
        .await);
    assert!(!good.token.is_empty());

    let replay = fixture
        .service
        .login_with_source(
            &ctx(None),
            &LoginBody {
                totp: Some("359152".to_owned()),
                ..body("ada", "correct horse battery staple")
            },
            "10.0.0.1",
        )
        .await;
    assert_code(&err(replay), "auth.totp_invalid");
}

#[tokio::test]
async fn a_recovery_code_signs_in_once() {
    let fixture = fixture();
    let id = fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );
    let codes = generate_recovery_codes(2);
    let display = codes[0].display();
    let digests: Vec<StoredDigest> = codes.iter().map(|code| code.digest()).collect();
    fixture.dir.mutate(id, |user| {
        user.totp_base32 = Some("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ".to_owned());
        user.recovery = digests.iter().map(|digest| *digest.as_bytes()).collect();
    });

    let first = fixture
        .service
        .login_with_source(
            &ctx(None),
            &LoginBody {
                recovery_code: Some(display.clone()),
                ..body("ada", "correct horse battery staple")
            },
            "10.0.0.1",
        )
        .await;
    assert!(!ok(first).token.is_empty());

    let second = fixture
        .service
        .login_with_source(
            &ctx(None),
            &LoginBody {
                recovery_code: Some(display),
                ..body("ada", "correct horse battery staple")
            },
            "10.0.0.1",
        )
        .await;
    assert_code(&err(second), "auth.totp_invalid");
}

#[tokio::test]
async fn api_keys_authenticate_and_revoked_or_expired_keys_do_not() {
    let fixture = fixture();
    let id = fixture
        .dir
        .add(StoredUser::new("service-account", UserState::Active));
    let (key, stored) = ApiKey::generate();
    fixture.keys.add(StoredKey {
        key_id: stored.key_id.clone(),
        digest: *stored.digest.as_bytes(),
        owner: id,
        state: KeyState::Active,
        expires_at: None,
    });

    let principal = ok(fixture.service.authenticate(&ctx(Some(&key.text()))).await);
    assert_eq!(principal.user_id, id);
    assert_eq!(principal.via, spot_login::Via::ApiKey);

    fixture.keys.add(StoredKey {
        key_id: stored.key_id.clone(),
        digest: *stored.digest.as_bytes(),
        owner: id,
        state: KeyState::Revoked,
        expires_at: None,
    });
    assert_code(
        &err(fixture.service.authenticate(&ctx(Some(&key.text()))).await),
        "auth.invalid_credentials",
    );

    fixture.keys.add(StoredKey {
        key_id: stored.key_id.clone(),
        digest: *stored.digest.as_bytes(),
        owner: id,
        state: KeyState::Active,
        expires_at: Some(ok(OffsetDateTime::from_unix_timestamp(1))),
    });
    assert_code(
        &err(fixture.service.authenticate(&ctx(Some(&key.text()))).await),
        "auth.invalid_credentials",
    );
}

#[tokio::test]
async fn a_locked_account_is_rate_limited_after_the_threshold() {
    let mut config = LoginConfig::for_tests();
    config.throttle = throttle_config(2, 2);
    let fixture = fixture_at(
        ManualClock::new(ok(OffsetDateTime::from_unix_timestamp(1_000_000))),
        config,
    );
    fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );

    for _ in 0..2 {
        let result = fixture
            .service
            .login_with_source(&ctx(None), &body("ada", "wrong"), "10.0.0.1")
            .await;
        assert_code(&err(result), "auth.invalid_credentials");
    }
    let locked = fixture
        .service
        .login_with_source(
            &ctx(None),
            &body("ada", "correct horse battery staple"),
            "10.0.0.1",
        )
        .await;
    assert_code(&err(locked), "api.rate_limited");
}

#[tokio::test]
async fn the_source_throttle_backs_off_a_second_account() {
    let mut config = LoginConfig::for_tests();
    config.throttle = throttle_config(1, 10);
    let fixture = fixture_at(
        ManualClock::new(ok(OffsetDateTime::from_unix_timestamp(1_000_000))),
        config,
    );
    fixture.dir.add(
        StoredUser::new("ada", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );
    fixture.dir.add(
        StoredUser::new("grace", UserState::Active)
            .with_password(fixture.hasher.as_ref(), "correct horse battery staple"),
    );

    let first = fixture
        .service
        .login_with_source(&ctx(None), &body("ada", "wrong"), "10.0.0.1")
        .await;
    assert_code(&err(first), "auth.invalid_credentials");

    let second = fixture
        .service
        .login_with_source(&ctx(None), &body("grace", "wrong"), "10.0.0.1")
        .await;
    assert_code(&err(second), "api.rate_limited");
}

/// Recorded security finding (the architecture owner, 2026-11-10): an API key never reads the session, with or without an expiry
/// (the latter used to be `http.internal`); the refusal is `permission.denied` in both cases.
#[tokio::test]
async fn an_api_key_reading_the_session_is_refused_with_or_without_an_expiry() {
    let fixture = fixture();
    let id = fixture
        .dir
        .add(StoredUser::new("service-account", UserState::Active));
    let (key, stored) = ApiKey::generate();
    let store = |expires_at| {
        fixture.keys.add(StoredKey {
            key_id: stored.key_id.clone(),
            digest: *stored.digest.as_bytes(),
            owner: id,
            state: KeyState::Active,
            expires_at,
        });
    };
    store(None);
    let refused = err(fixture.service.session(&ctx(Some(&key.text()))).await);
    assert_code(&refused, "permission.denied");

    store(Some(ok(OffsetDateTime::from_unix_timestamp(4_000_000_000))));
    let refused = err(fixture.service.session(&ctx(Some(&key.text()))).await);
    assert_code(&refused, "permission.denied");
}
