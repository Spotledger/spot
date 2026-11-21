//! Arbitrary inputs never panic and never verify (the brief's property test).

mod support;

use proptest::prelude::*;
use spot_sso::{CallbackNext, CallbackParams, begin, callback, complete};

use support::*;

/// A fixed instant.
const NOW: i64 = 1_700_000_000;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// No arbitrary string is ever accepted as an ID token, and none panics the validator.
    #[test]
    fn arbitrary_tokens_never_verify(token in ".{0,512}") {
        let idp = support::TestIdp::new();
        let (_, pending) = ok(
            begin(&idp.config(), &idp.discovery(), &rng_bytes(5), NOW, "/app"),
            "begin",
        );
        let outcome = complete(
            &idp.config(),
            &idp.discovery(),
            &keys(&idp),
            &pending,
            &token_response(&token),
            NOW,
        );
        prop_assert!(outcome.is_err(), "an arbitrary token verified: {token:?}");
    }

    /// No arbitrary `state` is ever accepted, and none panics the callback.
    #[test]
    fn arbitrary_states_are_never_accepted(state in ".{0,256}") {
        let idp = support::TestIdp::new();
        let (_, pending) = ok(
            begin(&idp.config(), &idp.discovery(), &rng_bytes(6), NOW, "/app"),
            "begin",
        );
        let params = CallbackParams {
            state: Some(state),
            code: Some("code".to_owned()),
            error: None,
            error_description: None,
        };
        let accepted = matches!(
            callback(&idp.config(), &idp.discovery(), &pending, &params, NOW),
            Ok(CallbackNext::ExchangeCode { .. })
        );
        prop_assert!(!accepted, "an arbitrary state was accepted");
    }
}
