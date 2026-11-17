//! The SAML provider administration (WP-IAM-05.T17): a provider is created and replaced from
//! validated IdP metadata. No database and no network: the fetcher is a double and the functions are
//! the ones the administration port calls.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::sync::Mutex;

use serde_json::{Map, Value as Json};
use spot_cli::services::sso::{FetchError, Fetcher};
use spot_cli::services::sso_admin::{
    fetch_metadata, metadata_trust, saml_from_metadata, saml_metadata_url,
};
use spot_core::Audience;
use spot_http::api::service::BoxFuture;
use spot_sso::ProviderId;

const NOW: i64 = 1_700_000_000;
const BASE: &str = "https://erp.acme.example";
const URL: &str = "https://idp.example/metadata.xml";

/// An IdP metadata document: an entity id, one redirect endpoint and one signing certificate (the
/// parser decodes the certificate's Base64, it does not inspect the DER).
fn metadata(entity: &str, valid_until: Option<&str>) -> String {
    let until = valid_until
        .map(|u| format!(" validUntil=\"{u}\""))
        .unwrap_or_default();
    format!(
        "<md:EntityDescriptor xmlns:md=\"urn:oasis:names:tc:SAML:2.0:metadata\" \
         xmlns:ds=\"http://www.w3.org/2000/09/xmldsig#\" entityID=\"{entity}\"{until}>\
         <md:IDPSSODescriptor protocolSupportEnumeration=\"urn:oasis:names:tc:SAML:2.0:protocol\">\
         <md:KeyDescriptor use=\"signing\"><ds:KeyInfo><ds:X509Data>\
         <ds:X509Certificate>MIIBCgKCAQEA</ds:X509Certificate></ds:X509Data></ds:KeyInfo>\
         </md:KeyDescriptor>\
         <md:SingleSignOnService Binding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect\" \
         Location=\"https://idp.example/sso\"/>\
         </md:IDPSSODescriptor></md:EntityDescriptor>"
    )
}

/// Answers the scripted document (or error) for any address and records what was asked.
struct Script {
    answer: Result<Vec<u8>, FetchError>,
    asked: Mutex<Vec<(String, String)>>,
}

impl Script {
    fn serving(text: impl Into<String>) -> Self {
        Self {
            answer: Ok(text.into().into_bytes()),
            asked: Mutex::default(),
        }
    }

    fn failing(error: FetchError) -> Self {
        Self {
            answer: Err(error),
            asked: Mutex::default(),
        }
    }
}

impl Fetcher for Script {
    fn get<'a>(
        &'a self,
        tenant: &'a str,
        url: &'a str,
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        self.asked
            .lock()
            .unwrap()
            .push((tenant.to_owned(), url.to_owned()));
        let answer = self.answer.clone();
        Box::pin(async move { answer })
    }

    fn post_form<'a>(
        &'a self,
        _tenant: &'a str,
        _url: &'a str,
        _form: &'a [(String, String)],
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        Box::pin(async { Err(FetchError::Unavailable) })
    }
}

fn body(text: &str) -> Map<String, Json> {
    serde_json::from_str::<Json>(text)
        .unwrap()
        .as_object()
        .unwrap()
        .clone()
}

const INPUT: &str = r#"{"kind":"saml","name":"Acme IdP","audience":["staff"],
    "saml":{"metadata_url":"https://idp.example/metadata.xml"}}"#;

fn code_of(error: &spot_http::api::service::ApiError) -> String {
    error
        .details
        .get("code")
        .and_then(Json::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_document_becomes_a_provider_with_the_derived_service_provider_side() {
    let fetcher = Script::serving(metadata("https://idp.example/entity", None));
    let found = fetch_metadata(&fetcher, "acme", URL, NOW).await.unwrap();
    assert_eq!(found.entity_id, "https://idp.example/entity");
    // The tenant and the address reached the fetcher as given.
    assert_eq!(
        *fetcher.asked.lock().unwrap(),
        [("acme".to_owned(), URL.to_owned())]
    );
    let id = ProviderId::new("acme-idp");
    let provider = saml_from_metadata(&id, &body(INPUT), BASE, &found).unwrap();
    assert_eq!(provider.audience, Audience::Staff);
    assert_eq!(provider.name, "Acme IdP");
    assert_eq!(
        provider.sp.entity_id,
        "https://erp.acme.example/api/v1/auth/sso/acme-idp/saml-metadata"
    );
    assert_eq!(
        provider.sp.acs_url,
        "https://erp.acme.example/api/v1/auth/sso/acme-idp/acs"
    );
    assert_eq!(provider.idp.entity_id, "https://idp.example/entity");
    assert_eq!(provider.idp.sso_url, "https://idp.example/sso");
    assert_eq!(provider.idp.signing_certificates.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_or_expired_or_unusable_document_is_rejected_by_code_and_never_quoted() {
    for (what, text, code) in [
        ("not xml", "this is not metadata <<<".to_owned(), ""),
        (
            "wrong root",
            "<html xmlns=\"http://www.w3.org/1999/xhtml\"/>".to_owned(),
            "sso.saml.metadata_invalid",
        ),
        (
            "expired",
            metadata("https://idp.example/entity", Some("2001-01-01T00:00:00Z")),
            "sso.saml.metadata_expired",
        ),
        (
            "no signing certificate",
            metadata("https://idp.example/entity", None)
                .replace("MIIBCgKCAQEA", "")
                .replace("use=\"signing\"", "use=\"encryption\""),
            "sso.saml.no_signing_certificate",
        ),
    ] {
        let fetcher = Script::serving(text.clone());
        let error = fetch_metadata(&fetcher, "acme", URL, NOW)
            .await
            .unwrap_err();
        assert_eq!(error.code.as_str(), "sso.provider_invalid", "{what}");
        // The document is not even XML: only that a metadata code is named.
        if code.is_empty() {
            assert!(code_of(&error).starts_with("sso."), "{what}");
        } else {
            assert_eq!(code_of(&error), code, "{what}");
        }
        // Nothing of the document is in the error.
        assert!(!format!("{error:?}").contains("not metadata"), "{what}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fetch_that_fails_is_unavailable_or_invalid_and_the_trust_is_the_explicit_one() {
    let slow = Script::failing(FetchError::Timeout);
    let error = fetch_metadata(&slow, "acme", URL, NOW).await.unwrap_err();
    assert_eq!(error.code.as_str(), "server.unavailable");
    let refused = Script::failing(FetchError::Refused);
    let error = fetch_metadata(&refused, "acme", URL, NOW)
        .await
        .unwrap_err();
    assert_eq!(error.code.as_str(), "sso.provider_invalid");
    // The trust the administration applies: no pinned certificate, unsigned accepted.
    let trust = metadata_trust();
    assert!(trust.pinned_certificate.is_none() && trust.trust_unsigned);
}

#[test]
fn the_input_must_be_a_saml_provider_with_an_https_metadata_address() {
    assert_eq!(saml_metadata_url(&body(INPUT)).unwrap(), URL);
    for (what, text) in [
        (
            "oidc kind",
            r#"{"kind":"oidc","name":"n","audience":["staff"],"saml":{"metadata_url":"https://i/m"}}"#,
        ),
        (
            "plain http",
            r#"{"kind":"saml","name":"n","audience":["staff"],"saml":{"metadata_url":"http://idp.example/m"}}"#,
        ),
        (
            "credentials in the address",
            r#"{"kind":"saml","name":"n","audience":["staff"],"saml":{"metadata_url":"https://u:p@idp.example/m"}}"#,
        ),
        (
            "no address",
            r#"{"kind":"saml","name":"n","audience":["staff"],"saml":{}}"#,
        ),
        (
            "no saml object",
            r#"{"kind":"saml","name":"n","audience":["staff"]}"#,
        ),
        (
            "both audiences",
            r#"{"kind":"saml","name":"n","audience":["staff","partner"],"saml":{"metadata_url":"https://i/m"}}"#,
        ),
        (
            "jit create",
            r#"{"kind":"saml","name":"n","audience":["staff"],"jit":"create","saml":{"metadata_url":"https://i/m"}}"#,
        ),
    ] {
        assert!(saml_metadata_url(&body(text)).is_err(), "{what}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entity_id_that_disagrees_with_the_document_is_refused_and_no_origin_is_never_guessed() {
    let fetcher = Script::serving(metadata("https://idp.example/entity", None));
    let found = fetch_metadata(&fetcher, "acme", URL, NOW).await.unwrap();
    let id = ProviderId::new("acme-idp");
    let wrong = r#"{"kind":"saml","name":"n","audience":["staff"],
        "saml":{"metadata_url":"https://idp.example/metadata.xml","entity_id":"https://other/entity"}}"#;
    assert_eq!(
        saml_from_metadata(&id, &body(wrong), BASE, &found)
            .unwrap_err()
            .code
            .as_str(),
        "sso.provider_invalid"
    );
    let right = r#"{"kind":"saml","name":"n","audience":["partner"],
        "saml":{"metadata_url":"https://idp.example/metadata.xml","entity_id":"https://idp.example/entity"}}"#;
    let provider = saml_from_metadata(&id, &body(right), BASE, &found).unwrap();
    assert_eq!(provider.audience, Audience::Partner);
    // No public origin configured: unavailable, never a guess.
    assert_eq!(
        saml_from_metadata(&id, &body(INPUT), "", &found)
            .unwrap_err()
            .code
            .as_str(),
        "server.unavailable"
    );
}
