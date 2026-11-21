//! OIDC back-channel logout: a valid token names a subject, a session, or both, and every
//! check in the profile has its own refusal. A token is never an ID token, and a `jti` ends
//! nothing twice.

mod support;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value, json};
use spot_sso::contract::block_on;
use spot_sso::{Jwa, MemoryReplayStore, Result, SsoError, validate_logout_token};

use support::*;

const NOW: i64 = 1_700_000_000;
const BACKCHANNEL: &str = "http://schemas.openid.net/event/backchannel-logout";

/// The claims every logout token carries before a test changes them.
fn base_claims(now: i64) -> Map<String, Value> {
    let mut claims = Map::new();
    claims.insert("iss".to_owned(), json!(ISSUER));
    claims.insert("aud".to_owned(), json!("spotledger"));
    claims.insert("iat".to_owned(), json!(now));
    claims.insert("exp".to_owned(), json!(now + 60));
    claims.insert("jti".to_owned(), json!("logout-1"));
    claims.insert("sub".to_owned(), json!("subject-1"));
    claims.insert("events".to_owned(), json!({ (BACKCHANNEL): {} }));
    claims
}

/// Mints a token with the provider's key for `alg`.
fn mint(idp: &TestIdp, alg: Jwa, claims: &Map<String, Value>) -> String {
    let kid = match alg {
        Jwa::Rs256 => KID_RSA,
        Jwa::Es256 => KID_EC,
    };
    idp.mint(alg, kid, &Value::Object(claims.clone()))
}

/// Mints a token with a header the crate must never trust, signed with the RSA key only so
/// the token is syntactically complete.
fn mint_with_header(idp: &TestIdp, alg: &str, kid: &str, claims: &Map<String, Value>) -> String {
    let header = json!({ "alg": alg, "kid": kid, "typ": "JWT" });
    let header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap_or_default());
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap_or_default());
    let signing_input = format!("{header}.{payload}");
    let signature = idp.sign_rs256(signing_input.as_bytes());
    format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature))
}

/// Validates `token` at `now` through the fake provider.
fn validate(
    idp: &TestIdp,
    token: &str,
    now: i64,
    replay: &MemoryReplayStore,
) -> Result<spot_sso::LogoutTarget> {
    block_on(validate_logout_token(
        &idp.config(),
        &idp.discovery(),
        &keys(idp),
        token,
        now,
        replay,
    ))
}

/// The refusal code of a failed validation.
fn refusal(outcome: Result<spot_sso::LogoutTarget>) -> &'static str {
    let error: SsoError = err(outcome);
    error.code()
}

#[test]
fn a_subject_only_token_names_the_subject() {
    let idp = TestIdp::new();
    let token = mint(&idp, Jwa::Rs256, &base_claims(NOW));
    let target = ok(
        validate(&idp, &token, NOW, &MemoryReplayStore::new()),
        "validate",
    );
    assert_eq!(target.provider.as_str(), "acme");
    assert_eq!(target.issuer, ISSUER);
    assert_eq!(target.subject.as_deref(), Some("subject-1"));
    assert_eq!(target.session_id, None);
    assert!(target.session_indexes.is_empty());
}

#[test]
fn a_session_only_token_names_the_session() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.remove("sub");
    claims.insert("sid".to_owned(), json!("session-1"));
    let token = mint(&idp, Jwa::Rs256, &claims);
    let target = ok(
        validate(&idp, &token, NOW, &MemoryReplayStore::new()),
        "validate",
    );
    assert_eq!(target.subject, None);
    assert_eq!(target.session_id.as_deref(), Some("session-1"));
}

#[test]
fn a_token_with_both_names_both() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.insert("sid".to_owned(), json!("session-1"));
    let token = mint(&idp, Jwa::Rs256, &claims);
    let target = ok(
        validate(&idp, &token, NOW, &MemoryReplayStore::new()),
        "validate",
    );
    assert_eq!(target.subject.as_deref(), Some("subject-1"));
    assert_eq!(target.session_id.as_deref(), Some("session-1"));
}

#[test]
fn an_es256_token_verifies() {
    let idp = TestIdp::new();
    let token = mint(&idp, Jwa::Es256, &base_claims(NOW));
    let target = ok(
        validate(&idp, &token, NOW, &MemoryReplayStore::new()),
        "validate",
    );
    assert_eq!(target.subject.as_deref(), Some("subject-1"));
}

#[test]
fn a_wrong_issuer_is_refused() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.insert("iss".to_owned(), json!("https://evil.example.com"));
    let token = mint(&idp, Jwa::Rs256, &claims);
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::ISSUER_MISMATCH
    );
}

#[test]
fn a_wrong_audience_is_refused() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.insert("aud".to_owned(), json!("another-client"));
    let token = mint(&idp, Jwa::Rs256, &claims);
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::AUDIENCE_MISMATCH
    );
}

#[test]
fn a_replayed_jti_is_refused() {
    let idp = TestIdp::new();
    let token = mint(&idp, Jwa::Rs256, &base_claims(NOW));
    let replay = MemoryReplayStore::new();
    assert!(validate(&idp, &token, NOW, &replay).is_ok());
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &replay)),
        spot_sso::codes::LOGOUT_TOKEN_REPLAYED
    );
}

#[test]
fn a_token_without_the_events_member_is_refused() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.remove("events");
    let token = mint(&idp, Jwa::Rs256, &claims);
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::LOGOUT_TOKEN_EVENTS_MISSING
    );
}

#[test]
fn an_events_member_for_a_different_event_is_refused() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.insert(
        "events".to_owned(),
        json!({ "https://example.com/some-other-event": {} }),
    );
    let token = mint(&idp, Jwa::Rs256, &claims);
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::LOGOUT_TOKEN_EVENTS_MISSING
    );
}

#[test]
fn a_token_carrying_a_nonce_is_an_id_token_and_is_refused() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.insert("nonce".to_owned(), json!("nonce-from-a-sign-in"));
    let token = mint(&idp, Jwa::Rs256, &claims);
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::LOGOUT_TOKEN_NONCE_PRESENT
    );
}

#[test]
fn an_old_token_is_refused() {
    let idp = TestIdp::new();
    let token = mint(&idp, Jwa::Rs256, &base_claims(NOW - 61));
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::TOKEN_EXPIRED
    );
}

#[test]
fn a_future_issued_at_is_refused() {
    let idp = TestIdp::new();
    let token = mint(&idp, Jwa::Rs256, &base_claims(NOW + 61));
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::ISSUED_AT_INVALID
    );
}

#[test]
fn a_token_without_a_jti_is_refused() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.remove("jti");
    let token = mint(&idp, Jwa::Rs256, &claims);
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::LOGOUT_TOKEN_MISSING_JTI
    );
}

#[test]
fn a_token_naming_neither_subject_nor_session_is_refused() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.remove("sub");
    let token = mint(&idp, Jwa::Rs256, &claims);
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::LOGOUT_TOKEN_NO_TARGET
    );
}

#[test]
fn alg_none_is_refused() {
    let idp = TestIdp::new();
    let token = mint_with_header(&idp, "none", KID_RSA, &base_claims(NOW));
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::ALG_UNSUPPORTED
    );
}

#[test]
fn hs256_is_refused_even_when_it_names_the_rsa_key() {
    let idp = TestIdp::new();
    let token = mint_with_header(&idp, "HS256", KID_RSA, &base_claims(NOW));
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::ALG_UNSUPPORTED
    );
}

#[test]
fn a_tampered_token_fails_the_signature() {
    let idp = TestIdp::new();
    let token = mint(&idp, Jwa::Rs256, &base_claims(NOW));
    let parts: Vec<&str> = token.split('.').collect();
    // Change the first character only: a flipped last character can leave non-canonical
    // trailing bits, which is a malformed token rather than a bad signature.
    let original = parts.get(2).copied().unwrap_or_default();
    let mut signature = String::new();
    for (index, character) in original.chars().enumerate() {
        signature.push(if index == 0 {
            if character == 'A' { 'B' } else { 'A' }
        } else {
            character
        });
    }
    let tampered = format!(
        "{}.{}.{signature}",
        parts.first().unwrap_or(&""),
        parts.get(1).unwrap_or(&"")
    );
    assert_eq!(
        refusal(validate(&idp, &tampered, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::SIGNATURE_INVALID
    );
}

#[test]
fn an_unknown_key_is_reported_as_stale() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.insert("jti".to_owned(), json!("logout-2"));
    let token = idp.mint(Jwa::Rs256, "not-a-key", &Value::Object(claims));
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::JWKS_STALE
    );
}

/// Validates with a provider whose longest logout token lifetime is `max`.
fn validate_with_lifetime(
    idp: &TestIdp,
    token: &str,
    max: i64,
    replay: &MemoryReplayStore,
) -> Result<spot_sso::LogoutTarget> {
    let mut config = idp.config();
    config.max_logout_token_lifetime_seconds = max;
    block_on(validate_logout_token(
        &config,
        &idp.discovery(),
        &keys(idp),
        token,
        NOW,
        replay,
    ))
}

#[test]
fn a_token_without_exp_is_refused() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.remove("exp");
    let token = mint(&idp, Jwa::Rs256, &claims);
    let replay = MemoryReplayStore::new();
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &replay)),
        spot_sso::codes::LOGOUT_TOKEN_NO_EXP
    );
    assert!(replay.is_empty(), "a refused token leaves no replay record");
}

#[test]
fn a_token_whose_exp_is_not_an_integer_is_refused() {
    let idp = TestIdp::new();
    for bad in [
        json!("1700000060"),
        json!(1_700_000_060.5),
        json!(null),
        json!([1]),
    ] {
        let mut claims = base_claims(NOW);
        claims.insert("exp".to_owned(), bad);
        let token = mint(&idp, Jwa::Rs256, &claims);
        assert_eq!(
            refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
            spot_sso::codes::LOGOUT_TOKEN_NO_EXP
        );
    }
}

#[test]
fn a_token_whose_exp_is_not_after_its_iat_is_malformed_not_expired() {
    let idp = TestIdp::new();
    for exp in [NOW - 3600, NOW - 1, NOW] {
        let mut claims = base_claims(NOW);
        claims.insert("exp".to_owned(), json!(exp));
        let token = mint(&idp, Jwa::Rs256, &claims);
        assert_eq!(
            refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
            spot_sso::codes::LOGOUT_TOKEN_MALFORMED_TIMES,
            "exp {exp}"
        );
    }
    // One second after `iat` is well formed.
    let mut claims = base_claims(NOW);
    claims.insert("exp".to_owned(), json!(NOW + 1));
    let token = mint(&idp, Jwa::Rs256, &claims);
    assert!(validate(&idp, &token, NOW, &MemoryReplayStore::new()).is_ok());
}

#[test]
fn a_token_at_exactly_the_longest_lifetime_is_accepted_and_one_second_more_is_refused() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.insert("exp".to_owned(), json!(NOW + 120));
    let token = mint(&idp, Jwa::Rs256, &claims);
    assert!(validate(&idp, &token, NOW, &MemoryReplayStore::new()).is_ok());

    claims.insert("exp".to_owned(), json!(NOW + 121));
    let token = mint(&idp, Jwa::Rs256, &claims);
    assert_eq!(
        refusal(validate(&idp, &token, NOW, &MemoryReplayStore::new())),
        spot_sso::codes::LOGOUT_TOKEN_LIFETIME_EXCESSIVE
    );
}

#[test]
fn the_longest_lifetime_is_configurable_within_its_bounds() {
    let idp = TestIdp::new();
    let mut claims = base_claims(NOW);
    claims.insert("exp".to_owned(), json!(NOW + 300));
    let token = mint(&idp, Jwa::Rs256, &claims);
    assert_eq!(
        refusal(validate_with_lifetime(
            &idp,
            &token,
            299,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::LOGOUT_TOKEN_LIFETIME_EXCESSIVE
    );
    assert!(validate_with_lifetime(&idp, &token, 300, &MemoryReplayStore::new()).is_ok());

    for bad in [0, -5, 601] {
        let mut config = idp.config();
        config.max_logout_token_lifetime_seconds = bad;
        let errors = match config.validate() {
            Err(errors) => errors,
            Ok(()) => panic!("lifetime {bad} must be refused"),
        };
        assert!(
            errors
                .problems()
                .iter()
                .any(|p| p.code() == spot_sso::codes::LOGOUT_LIFETIME_INVALID),
            "lifetime {bad}"
        );
    }
    for good in [1, 120, 600] {
        let mut config = idp.config();
        config.max_logout_token_lifetime_seconds = good;
        assert!(config.validate().is_ok(), "lifetime {good}");
    }
}

#[test]
fn the_replay_record_lasts_until_exp_plus_skew_and_no_longer() {
    let idp = TestIdp::new();
    let replay = MemoryReplayStore::new();
    let mut claims = base_claims(NOW);
    claims.insert("exp".to_owned(), json!(NOW + 120));
    assert!(validate(&idp, &mint(&idp, Jwa::Rs256, &claims), NOW, &replay).is_ok());

    // The same `jti` on a fresh token while the record lives (skew is 60) is a replay ...
    let later = NOW + 180;
    let mut again = base_claims(later);
    again.insert("exp".to_owned(), json!(later + 60));
    let token = mint(&idp, Jwa::Rs256, &again);
    assert_eq!(
        refusal(validate(&idp, &token, later, &replay)),
        spot_sso::codes::LOGOUT_TOKEN_REPLAYED
    );
    // ... and is forgotten one second after `exp + skew`.
    let after = NOW + 181;
    let mut again = base_claims(after);
    again.insert("exp".to_owned(), json!(after + 60));
    let token = mint(&idp, Jwa::Rs256, &again);
    assert!(validate(&idp, &token, after, &replay).is_ok());
}

#[test]
fn a_failing_replay_store_refuses_the_logout_token() {
    let idp = TestIdp::new();
    let token = mint(&idp, Jwa::Rs256, &base_claims(NOW));
    let config = idp.config();
    let discovery = idp.discovery();
    let cache = keys(&idp);
    let store = spot_sso::contract::FailingStore;
    let future = validate_logout_token(&config, &discovery, &cache, &token, NOW, &store);
    assert_send(&future);
    assert_eq!(
        refusal(block_on(future)),
        spot_sso::codes::REPLAY_STORE_UNAVAILABLE
    );
    assert!(validate(&idp, &token, NOW, &MemoryReplayStore::new()).is_ok());
}

/// The same raw id under a different kind or provider is not a replay.
#[test]
fn a_jti_is_scoped_to_its_provider_and_kind() {
    let idp = TestIdp::new();
    let token = mint(&idp, Jwa::Rs256, &base_claims(NOW));
    let store = MemoryReplayStore::new();
    assert!(validate(&idp, &token, NOW, &store).is_ok());
    let mut other = idp.config();
    other.id = spot_sso::ProviderId::new("another-provider");
    assert!(
        block_on(validate_logout_token(
            &other,
            &idp.discovery(),
            &keys(&idp),
            &token,
            NOW,
            &store
        ))
        .is_ok(),
        "another provider's jti space is separate"
    );
    // The stored key is namespaced, so the bare `jti` was never recorded.
    assert_eq!(
        block_on(spot_sso::ReplayStore::claim(
            &store,
            "logout-1",
            NOW,
            NOW + 10
        )),
        Ok(spot_sso::Claim::Fresh)
    );
}

fn assert_send<T: Send>(_: &T) {}
