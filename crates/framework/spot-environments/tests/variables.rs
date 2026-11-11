//! Environment-specific values: one template digest, different resolved values, and the fold
//! back that proves the applied result is the template instantiated for the environment.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_custom_bundle::Limits;
use spot_environments::{SecretRef, Variables};

#[test]
fn one_template_has_one_digest_and_each_environment_resolves_differently() {
    let bundle = source_bundle();
    let source_digest = bundle.digest().to_hex();
    let limits = Limits::default();

    let mut digests = Vec::new();
    for environment in ["dev", "test", "prod"] {
        let resolved = variables(environment).resolve(&bundle, &limits).unwrap();
        assert_ne!(resolved.digest().to_hex(), source_digest);
        digests.push(resolved.digest().to_hex());
        // The value is resolved and the secret stayed a reference.
        let files = resolved.section("fixtures").unwrap();
        let text = String::from_utf8(files.files["invoice.json"].clone()).unwrap();
        assert!(text.contains(&format!("https://{environment}.acme.test")));
        assert!(text.contains(&format!("secret:{environment}-token")));
        assert!(!text.contains("${env.API_URL}"));
    }
    assert_ne!(digests[0], digests[1]);
    assert_ne!(digests[1], digests[2]);

    // Folding a resolved capture back gives the source digest again.
    let resolved = variables("test").resolve(&bundle, &limits).unwrap();
    let folded = variables("test").normalise(&resolved, &limits).unwrap();
    assert_eq!(folded.digest().to_hex(), source_digest);
}

#[test]
fn an_unbound_placeholder_is_named_before_resolution() {
    let bundle = source_bundle();
    let mut variables = Variables::new();
    variables
        .set_text("API_URL", "https://test.acme.test")
        .unwrap();
    let error = variables.validate(&bundle).unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::UNRESOLVED_VARIABLE);
    assert_eq!(error.item(), "TOKEN");
    assert_eq!(variables.unresolved(&bundle), vec!["TOKEN".to_owned()]);
}

#[test]
fn a_malformed_placeholder_is_refused() {
    let bundle = source_bundle();
    let limits = Limits::default();
    let variables = variables("test");
    let mut sections = bundle.sections().clone();
    sections.get_mut("fixtures").unwrap().files.insert(
        "invoice.json".into(),
        b"{\"x\":\"${env.BAD-NAME}\"}".to_vec(),
    );
    let broken =
        spot_custom_bundle::Bundle::pack(bundle.header().clone(), sections, &limits).unwrap();
    let error = variables.validate(&broken).unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::INVALID_VARIABLE);
}

#[test]
fn two_names_with_one_value_cannot_be_folded_back() {
    let bundle = source_bundle();
    let limits = Limits::default();
    let mut variables = variables("test");
    variables
        .set_text("OTHER", "https://test.acme.test")
        .unwrap();
    let resolved = variables.resolve(&bundle, &limits).unwrap();
    let error = variables.normalise(&resolved, &limits).unwrap_err();
    assert_eq!(error.code(), spot_environments::codes::INVALID_VARIABLE);
}

#[test]
fn a_secret_never_carries_its_value() {
    let secret = SecretRef::new("payments-api-key").unwrap();
    assert_eq!(secret.reference(), "secret:payments-api-key");
    let mut variables = Variables::new();
    variables.set_secret("KEY", secret).unwrap();
    assert_eq!(
        variables.get("KEY").unwrap().resolved(),
        "secret:payments-api-key"
    );
    assert!(SecretRef::new("a-value-that-looks-like-a-key").is_ok());
    assert!(SecretRef::new("API_KEY").is_err());
}
