//! The OIDC flow end to end, and one refusal per check.

mod support;

use serde_json::json;
use spot_sso::{
    CallbackParams, CompleteError, Discovery, Jwa, SsoIdentity, begin, callback, complete,
    parse_jwks,
};

use support::*;

/// A pending sign-in for the fake provider.
fn pending(now: i64, seed: u8) -> (spot_sso::PendingAuth, support::TestIdp) {
    let idp = support::TestIdp::new();
    let config = idp.config();
    let discovery = idp.discovery();
    let started = ok(
        begin(&config, &discovery, &rng_bytes(seed), now, "/app/invoices"),
        "begin",
    );
    (started.1, idp)
}

/// The refusal code of a [`CompleteError`].
fn refusal_code(error: CompleteError) -> &'static str {
    error.code()
}

#[test]
fn happy_path_rs256() {
    let now = 1_700_000_000;
    let (pending, idp) = pending(now, 1);
    let discovery = idp.discovery();
    let config = idp.config();

    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |_| {});
    let identity = ok(
        complete(
            &config,
            &discovery,
            &keys(&idp),
            &pending,
            &token_response(&token),
            now,
        ),
        "complete",
    );
    assert_eq!(identity.subject, "subject-1");
    assert_eq!(identity.issuer, ISSUER);
    assert_eq!(identity.email.as_deref(), Some("ada@example.com"));
    assert_eq!(identity.display_name.as_deref(), Some("Ada Lovelace"));
    assert_eq!(identity.groups, vec!["engineering".to_owned()]);
    assert_eq!(
        identity.acr.as_deref(),
        Some("urn:mace:incommon:iap:silver")
    );
    assert_eq!(identity.amr, vec!["pwd".to_owned(), "otp".to_owned()]);
    // The raw claims are not kept; only a hash.
    assert!(!identity.raw_claims_hash.is_empty());
}

#[test]
fn happy_path_es256() {
    let now = 1_700_000_100;
    let (pending, idp) = pending(now, 2);
    let token = idp.id_token(Jwa::Es256, &pending.nonce, now, |_| {});
    let identity = ok(
        complete(
            &idp.config(),
            &idp.discovery(),
            &keys(&idp),
            &pending,
            &token_response(&token),
            now,
        ),
        "complete",
    );
    assert_eq!(identity.subject, "subject-1");
}

#[test]
fn callback_exchanges_the_code() {
    let now = 1_700_000_200;
    let (pending, idp) = pending(now, 3);
    let params = CallbackParams {
        state: Some(pending.state.clone()),
        code: Some("auth-code-123".to_owned()),
        error: None,
        error_description: None,
    };
    let next = ok(
        callback(&idp.config(), &idp.discovery(), &pending, &params, now),
        "callback",
    );
    let spot_sso::CallbackNext::ExchangeCode { request } = next;
    assert_eq!(request.endpoint, format!("{ISSUER}/token"));
    assert!(request.form.iter().any(|(k, _)| k == "code_verifier"));
    // The form is a secret and never shown.
    assert!(format!("{request:?}").contains("<redacted>"));
    assert!(!format!("{request:?}").contains("auth-code-123"));
}

#[test]
fn discovery_is_checked_against_the_config() {
    let idp = support::TestIdp::new();
    let config = idp.config();
    let mut document: serde_json::Value =
        serde_json::from_slice(&idp.discovery_bytes()).unwrap_or_default();
    document["issuer"] = json!("https://other.example.com");
    let bytes = serde_json::to_vec(&document).unwrap_or_default();
    let error = err(Discovery::parse(&bytes, &config));
    assert_eq!(error.code(), "sso.discovery_mismatch");
}

#[test]
fn discovery_must_offer_s256() {
    let idp = support::TestIdp::new();
    let config = idp.config();
    let mut document: serde_json::Value =
        serde_json::from_slice(&idp.discovery_bytes()).unwrap_or_default();
    document["code_challenge_methods_supported"] = json!(["plain"]);
    let bytes = serde_json::to_vec(&document).unwrap_or_default();
    let error = err(Discovery::parse(&bytes, &config));
    assert_eq!(error.code(), "sso.discovery_mismatch");
}

#[test]
fn begin_refuses_an_open_redirect() {
    let idp = support::TestIdp::new();
    for bad in ["https://evil.example", "//evil.example", "app"] {
        let error = err(begin(
            &idp.config(),
            &idp.discovery(),
            &rng_bytes(1),
            0,
            bad,
        ));
        assert_eq!(error.code(), "sso.bad_return_to", "for {bad:?}");
    }
}

#[test]
fn begin_needs_enough_random_bytes() {
    let idp = support::TestIdp::new();
    let error = err(begin(&idp.config(), &idp.discovery(), &[0u8; 8], 0, "/app"));
    assert_eq!(error.code(), "sso.rng_insufficient");
}

#[test]
fn pkce_verifier_and_challenge_agree() {
    let now = 0;
    let (pending, idp) = pending(now, 7);
    assert_eq!(pending.code_verifier.len(), 43);
    assert_eq!(pending.code_challenge.len(), 43);
    let params = CallbackParams {
        state: Some(pending.state.clone()),
        code: Some("c".to_owned()),
        error: None,
        error_description: None,
    };
    assert!(callback(&idp.config(), &idp.discovery(), &pending, &params, now).is_ok());
}

#[test]
fn callback_refuses_a_wrong_state() {
    let now = 0;
    let (pending, idp) = pending(now, 8);
    let params = CallbackParams {
        state: Some("wrong".to_owned()),
        code: Some("c".to_owned()),
        error: None,
        error_description: None,
    };
    let error = err(callback(
        &idp.config(),
        &idp.discovery(),
        &pending,
        &params,
        now,
    ));
    assert_eq!(error.code(), "sso.state_mismatch");
}

#[test]
fn callback_refuses_an_expired_attempt() {
    let now = 0;
    let (pending, idp) = pending(now, 9);
    let params = CallbackParams {
        state: Some(pending.state.clone()),
        code: Some("c".to_owned()),
        error: None,
        error_description: None,
    };
    let error = err(callback(
        &idp.config(),
        &idp.discovery(),
        &pending,
        &params,
        now + spot_sso::PENDING_TTL_SECONDS + 1,
    ));
    assert_eq!(error.code(), "sso.state_expired");
}

#[test]
fn callback_refuses_a_replayed_attempt() {
    let now = 0;
    let (pending, idp) = pending(now, 10);
    let params = CallbackParams {
        state: Some(pending.state.clone()),
        code: Some("c".to_owned()),
        error: None,
        error_description: None,
    };
    let used = ok(pending.consume(now), "consume");
    let error = err(callback(
        &idp.config(),
        &idp.discovery(),
        &used,
        &params,
        now,
    ));
    assert_eq!(error.code(), "sso.state_replayed");
}

#[test]
fn callback_surfaces_a_provider_error() {
    let now = 0;
    let (pending, idp) = pending(now, 11);
    let params = CallbackParams {
        state: Some(pending.state.clone()),
        code: None,
        error: Some("access_denied".to_owned()),
        error_description: None,
    };
    let error = err(callback(
        &idp.config(),
        &idp.discovery(),
        &pending,
        &params,
        now,
    ));
    assert_eq!(error.code(), "sso.idp_error");
}

#[test]
fn callback_requires_a_code() {
    let now = 0;
    let (pending, idp) = pending(now, 12);
    let params = CallbackParams {
        state: Some(pending.state.clone()),
        code: None,
        error: None,
        error_description: None,
    };
    let error = err(callback(
        &idp.config(),
        &idp.discovery(),
        &pending,
        &params,
        now,
    ));
    assert_eq!(error.code(), "sso.missing_param");
}

#[test]
fn callback_refuses_a_tampered_verifier() {
    let now = 0;
    let (pending, idp) = pending(now, 13);
    let tampered = spot_sso::PendingAuth {
        code_verifier: spot_sso::PendingAuth { ..pending.clone() }
            .code_verifier
            .chars()
            .rev()
            .collect(),
        ..pending.clone()
    };
    let params = CallbackParams {
        state: Some(pending.state.clone()),
        code: Some("c".to_owned()),
        error: None,
        error_description: None,
    };
    let error = err(callback(
        &idp.config(),
        &idp.discovery(),
        &tampered,
        &params,
        now,
    ));
    assert_eq!(error.code(), "sso.pkce_mismatch");
}

/// Runs `complete` with a mutated token for the current pending sign-in.
fn complete_with(
    idp: &support::TestIdp,
    pending: &spot_sso::PendingAuth,
    now: i64,
    token: &str,
) -> Result<SsoIdentity, CompleteError> {
    complete(
        &idp.config(),
        &idp.discovery(),
        &keys(idp),
        pending,
        &token_response(token),
        now,
    )
}

#[test]
fn alg_none_is_refused() {
    let now = 1_700_000_300;
    let (pending, idp) = pending(now, 20);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |_| {});
    // Replace the header with alg=none and keep the signature segment.
    let tampered = replace_header_alg(&token, "none", Some(KID_RSA));
    let error = err(complete_with(&idp, &pending, now, &tampered));
    assert_eq!(refusal_code(error), "sso.alg_unsupported");
}

#[test]
fn alg_hs256_is_refused() {
    let now = 1_700_000_310;
    let (pending, idp) = pending(now, 21);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |_| {});
    let tampered = replace_header_alg(&token, "HS256", Some(KID_RSA));
    let error = err(complete_with(&idp, &pending, now, &tampered));
    assert_eq!(refusal_code(error), "sso.alg_unsupported");
}

#[test]
fn alg_key_mismatch_is_refused() {
    let now = 1_700_000_320;
    let (pending, idp) = pending(now, 22);
    // Sign RS256 but point the header at the EC key, whose algorithm is ES256.
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |_| {});
    let tampered = replace_header_alg(&token, "RS256", Some(KID_EC));
    let error = err(complete_with(&idp, &pending, now, &tampered));
    assert_eq!(refusal_code(error), "sso.alg_mismatch");
}

#[test]
fn unknown_kid_asks_for_a_fresh_key_set() {
    let now = 1_700_000_330;
    let (pending, idp) = pending(now, 23);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |_| {});
    let tampered = replace_header_alg(&token, "RS256", Some("unknown-key"));
    let error = err(complete_with(&idp, &pending, now, &tampered));
    assert!(matches!(error, CompleteError::NeedFreshJwks { .. }));
    assert_eq!(refusal_code(error), "sso.jwks_stale");
}

#[test]
fn a_rotated_key_still_verifies_until_its_ttl_passes() {
    let now = 1_700_000_340;
    let (pending, idp) = pending(now, 24);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |_| {});
    let mut cache = keys(&idp);
    // Rotate to an empty set; the old keys stay for the TTL.
    cache.rotate(spot_sso::Jwks::default(), now);
    let identity = ok(
        complete(
            &idp.config(),
            &idp.discovery(),
            &cache,
            &pending,
            &token_response(&token),
            now,
        ),
        "rotated",
    );
    assert_eq!(identity.subject, "subject-1");
    // After the TTL the old key is gone, so the token needs a fresh key set.
    cache.prune(now + 301);
    let error = err(complete(
        &idp.config(),
        &idp.discovery(),
        &cache,
        &pending,
        &token_response(&token),
        now,
    ));
    assert_eq!(refusal_code(error), "sso.jwks_stale");
}

#[test]
fn tampered_signature_is_refused() {
    let now = 1_700_000_350;
    let (pending, idp) = pending(now, 25);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |_| {});
    // Rewrite the payload without re-signing: the signature no longer covers it.
    let tampered = replace_claim(&token, "name", json!("Someone Else"));
    let error = err(complete_with(&idp, &pending, now, &tampered));
    assert_eq!(refusal_code(error), "sso.signature_invalid");
}

#[test]
fn wrong_issuer_is_refused() {
    let now = 1_700_000_360;
    let (pending, idp) = pending(now, 26);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |c| {
        c.insert("iss".to_owned(), json!("https://evil.example.com"));
    });
    let error = err(complete_with(&idp, &pending, now, &token));
    assert_eq!(refusal_code(error), "sso.issuer_mismatch");
}

#[test]
fn wrong_audience_is_refused() {
    let now = 1_700_000_370;
    let (pending, idp) = pending(now, 27);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |c| {
        c.insert("aud".to_owned(), json!("someone-else"));
    });
    let error = err(complete_with(&idp, &pending, now, &token));
    assert_eq!(refusal_code(error), "sso.audience_mismatch");
}

#[test]
fn multiple_audiences_need_azp() {
    let now = 1_700_000_380;
    let (pending, idp) = pending(now, 28);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |c| {
        c.insert("aud".to_owned(), json!(["spotledger", "other"]));
    });
    let error = err(complete_with(&idp, &pending, now, &token));
    assert_eq!(refusal_code(error), "sso.azp_mismatch");
}

#[test]
fn expired_token_is_refused() {
    let now = 1_700_000_390;
    let (pending, idp) = pending(now, 29);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |c| {
        c.insert("exp".to_owned(), json!(now - 1000));
    });
    let error = err(complete_with(&idp, &pending, now, &token));
    assert_eq!(refusal_code(error), "sso.token_expired");
}

#[test]
fn not_yet_valid_beyond_skew_is_refused() {
    let now = 1_700_000_400;
    let (pending, idp) = pending(now, 30);
    // The skew is 60 seconds; nbf 120 seconds ahead is beyond it.
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |c| {
        c.insert("nbf".to_owned(), json!(now + 120));
    });
    let error = err(complete_with(&idp, &pending, now, &token));
    assert_eq!(refusal_code(error), "sso.token_not_yet_valid");
}

#[test]
fn future_issued_at_is_refused() {
    let now = 1_700_000_410;
    let (pending, idp) = pending(now, 31);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |c| {
        c.insert("iat".to_owned(), json!(now + 1000));
    });
    let error = err(complete_with(&idp, &pending, now, &token));
    assert_eq!(refusal_code(error), "sso.issued_at_invalid");
}

#[test]
fn nonce_mismatch_is_refused() {
    let now = 1_700_000_420;
    let (pending, idp) = pending(now, 32);
    let token = idp.id_token(Jwa::Rs256, "another-nonce", now, |_| {});
    let error = err(complete_with(&idp, &pending, now, &token));
    assert_eq!(refusal_code(error), "sso.nonce_mismatch");
}

#[test]
fn at_hash_mismatch_is_refused() {
    let now = 1_700_000_430;
    let (pending, idp) = pending(now, 33);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |c| {
        c.insert("at_hash".to_owned(), json!("not-the-right-hash"));
    });
    let error = err(complete(
        &idp.config(),
        &idp.discovery(),
        &keys(&idp),
        &pending,
        &token_response_with_access(&token, "an-access-token"),
        now,
    ));
    assert_eq!(refusal_code(error), "sso.at_hash_mismatch");
}

#[test]
fn a_matching_at_hash_is_accepted() {
    let now = 1_700_000_440;
    let (pending, idp) = pending(now, 34);
    let access_token = "an-access-token";
    let expected = expected_at_hash(access_token);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |c| {
        c.insert("at_hash".to_owned(), json!(expected));
    });
    let identity = ok(
        complete(
            &idp.config(),
            &idp.discovery(),
            &keys(&idp),
            &pending,
            &token_response_with_access(&token, access_token),
            now,
        ),
        "at_hash",
    );
    assert_eq!(identity.subject, "subject-1");
}

#[test]
fn acr_insufficient_is_refused() {
    let now = 1_700_000_450;
    let (pending, idp) = pending(now, 35);
    let mut config = idp.config();
    config.acr_values = vec!["urn:gold".to_owned()];
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |_| {});
    let error = err(complete(
        &config,
        &idp.discovery(),
        &keys(&idp),
        &pending,
        &token_response(&token),
        now,
    ));
    assert_eq!(refusal_code(error), "sso.acr_insufficient");
}

#[test]
fn unverified_email_is_refused_when_required() {
    let now = 1_700_000_460;
    let (pending, idp) = pending(now, 36);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |c| {
        c.insert("email_verified".to_owned(), json!(false));
    });
    let error = err(complete_with(&idp, &pending, now, &token));
    assert_eq!(refusal_code(error), "sso.email_not_verified");
}

#[test]
fn missing_subject_is_refused() {
    let now = 1_700_000_470;
    let (pending, idp) = pending(now, 37);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |c| {
        c.remove("sub");
    });
    let error = err(complete_with(&idp, &pending, now, &token));
    assert_eq!(refusal_code(error), "sso.claim_missing");
}

#[test]
fn a_truncated_token_is_refused() {
    let now = 1_700_000_480;
    let (pending, idp) = pending(now, 38);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |_| {});
    let truncated = token.split('.').take(2).collect::<Vec<_>>().join(".");
    let error = err(complete_with(&idp, &pending, now, &truncated));
    assert_eq!(refusal_code(error), "sso.token_malformed");
}

#[test]
fn a_token_with_two_signature_segments_is_refused() {
    let now = 1_700_000_490;
    let (pending, idp) = pending(now, 39);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |_| {});
    let four_segments = format!("{token}.extra");
    let error = err(complete_with(&idp, &pending, now, &four_segments));
    assert_eq!(refusal_code(error), "sso.token_malformed");
}

#[test]
fn an_embedded_key_header_is_refused() {
    let now = 1_700_000_500;
    let (pending, idp) = pending(now, 40);
    let token = idp.id_token(Jwa::Rs256, &pending.nonce, now, |_| {});
    let with_jku = replace_header_extra(&token, json!({ "jku": "https://evil.example/jwks" }));
    let error = err(complete_with(&idp, &pending, now, &with_jku));
    assert_eq!(refusal_code(error), "sso.header_untrusted");
}

#[test]
fn logout_url_uses_the_end_session_endpoint() {
    let idp = support::TestIdp::new();
    let url = ok(
        spot_sso::end_session_url(
            &idp.config(),
            &idp.discovery(),
            Some("the-id-token"),
            Some(REDIRECT_URI),
        ),
        "logout",
    );
    assert!(url.starts_with(&format!("{ISSUER}/logout?")));
    assert!(url.contains("post_logout_redirect_uri="));
}

#[test]
fn logout_refuses_a_wildcard_redirect() {
    let idp = support::TestIdp::new();
    let error = err(spot_sso::end_session_url(
        &idp.config(),
        &idp.discovery(),
        None,
        Some("https://ledger.example.com/anything"),
    ));
    assert_eq!(error.code(), "sso.logout_redirect");
}

#[test]
fn token_response_ignores_unknown_members() {
    let bytes = br#"{"id_token":"x","access_token":"y","token_type":"Bearer","expires_in":3600,"scope":"openid"}"#;
    let response = ok(spot_sso::TokenResponse::from_json(bytes), "token response");
    assert_eq!(response.id_token.as_deref(), Some("x"));
    assert_eq!(response.access_token.as_deref(), Some("y"));
}

#[test]
fn keys_parse_and_skip_small_rsa() {
    let small = json!({
        "keys": [
            {
                "kty": "RSA",
                "kid": "small",
                "n": "AQAB",
                "e": "AQAB"
            },
            {
                "kty": "RSA",
                "kid": "good",
                "n": RSA_N_B64,
                "e": RSA_E_B64
            }
        ]
    });
    let bytes = serde_json::to_vec(&small).unwrap_or_default();
    let jwks = ok(parse_jwks(&bytes), "jwks");
    assert_eq!(jwks.keys.len(), 1);
    assert_eq!(jwks.skipped.len(), 1);
    assert_eq!(jwks.skipped[0].reason, "sso.key_too_small");
}

// --- helpers that mutate a compact token without the private helpers above ---

/// Rewrites a token's payload claim, keeping header and signature (so the signature no longer
/// covers the payload).
fn replace_claim(token: &str, key: &str, value: serde_json::Value) -> String {
    let parts: Vec<&str> = token.split('.').collect();
    let mut claims: serde_json::Value =
        serde_json::from_slice(&base64_url_decode(parts[1])).unwrap_or_default();
    if let Some(object) = claims.as_object_mut() {
        object.insert(key.to_owned(), value);
    }
    let payload = base64_url_encode(&serde_json::to_vec(&claims).unwrap_or_default());
    format!("{}.{}.{}", parts[0], payload, parts[2])
}

/// Replaces the `alg` (and optionally `kid`) in a token's header, keeping the signature.
fn replace_header_alg(token: &str, alg: &str, kid: Option<&str>) -> String {
    let mut extra = serde_json::Map::new();
    extra.insert("alg".to_owned(), json!(alg));
    if let Some(kid) = kid {
        extra.insert("kid".to_owned(), json!(kid));
    }
    replace_header_extra(token, serde_json::Value::Object(extra))
}

/// Rewrites a token's header, merging `extra`, keeping the payload and signature segments.
fn replace_header_extra(token: &str, extra: serde_json::Value) -> String {
    let parts: Vec<&str> = token.split('.').collect();
    let mut header: serde_json::Value =
        serde_json::from_slice(&base64_url_decode(parts[0])).unwrap_or_default();
    if let (Some(object), Some(extra)) = (header.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            object.insert(key.clone(), value.clone());
        }
    }
    let header = base64_url_encode(&serde_json::to_vec(&header).unwrap_or_default());
    format!("{header}.{}.{}", parts[1], parts[2])
}

/// Base64url-decodes a JOSE segment in test code.
fn base64_url_decode(text: &str) -> Vec<u8> {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    URL_SAFE_NO_PAD.decode(text.as_bytes()).unwrap_or_default()
}

/// Base64url-encodes bytes in test code.
fn base64_url_encode(bytes: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The `at_hash` of an access token, for a matching-at-hash test.
fn expected_at_hash(access_token: &str) -> String {
    use ring::digest::{SHA256, digest};
    let digest = digest(&SHA256, access_token.as_bytes());
    base64_url_encode(&digest.as_ref()[..16])
}
