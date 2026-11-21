//! SAML 2.0 IdP-initiated single logout: a signed `LogoutRequest` names a `NameID` and any
//! number of `SessionIndex` values, every check in the profile refuses its own defect, and the
//! service provider can answer with a `LogoutResponse`.

#[path = "saml_support/mod.rs"]
mod support;

use roxmltree::Document;
use spot_sso::contract::block_on;
use spot_sso::saml::slo::{STATUS_PARTIAL, STATUS_SUCCESS};
use spot_sso::saml::{LogoutStatus, build_logout_response, validate_logout_request};
use spot_sso::{Jwa, MemoryReplayStore, Result};

use support::*;

const NOW: i64 = 1_700_000_000;
const REQUEST_ID: &str = "_logout-1";

/// The configurable content of one `LogoutRequest`.
struct Request {
    id: String,
    issuer: String,
    destination: Option<String>,
    not_on_or_after: Option<i64>,
    name_id: Option<String>,
    name_id_format: String,
    session_indexes: Vec<String>,
    issue_instant: i64,
}

impl Request {
    /// A valid request from the fake IdP.
    fn valid(id: &str) -> Self {
        Self {
            id: id.to_owned(),
            issuer: ISSUER.to_owned(),
            destination: Some(SLO_URL.to_owned()),
            not_on_or_after: Some(NOW + 300),
            name_id: Some("ada@example.com".to_owned()),
            name_id_format: NAMEID_PERSISTENT.to_owned(),
            session_indexes: vec!["_session-1".to_owned(), "_session-2".to_owned()],
            issue_instant: NOW,
        }
    }

    /// The unsigned `samlp:LogoutRequest`.
    fn render(&self) -> String {
        let mut out = format!(
            "<samlp:LogoutRequest xmlns:samlp=\"{NS_PROTOCOL}\" xmlns:saml=\"{NS_ASSERTION}\" ID=\"{}\" Version=\"2.0\" IssueInstant=\"{}\"",
            self.id,
            format_instant(self.issue_instant)
        );
        if let Some(destination) = &self.destination {
            out.push_str(&format!(" Destination=\"{destination}\""));
        }
        if let Some(expiry) = self.not_on_or_after {
            out.push_str(&format!(" NotOnOrAfter=\"{}\"", format_instant(expiry)));
        }
        out.push('>');
        out.push_str(&format!("<saml:Issuer>{}</saml:Issuer>", self.issuer));
        if let Some(name_id) = &self.name_id {
            out.push_str(&format!(
                "<saml:NameID Format=\"{}\">{name_id}</saml:NameID>",
                self.name_id_format
            ));
        }
        for index in &self.session_indexes {
            out.push_str(&format!("<samlp:SessionIndex>{index}</samlp:SessionIndex>"));
        }
        out.push_str("</samlp:LogoutRequest>");
        out
    }

    /// The request signed by the fake IdP with `alg`.
    fn signed(&self, idp: &TestIdp, alg: Jwa) -> String {
        sign(idp, alg, &self.render(), &self.id)
    }
}

/// Signs an unsigned `LogoutRequest`: digest, `SignedInfo`, signature, enveloped insertion.
fn sign(idp: &TestIdp, alg: Jwa, request_xml: &str, id: &str) -> String {
    let digest = digest_of(request_xml);
    let signed_info = render_signed_info(
        &format!("#{id}"),
        &digest,
        DIGEST_SHA256,
        &[ENVELOPED, C14N_EXC],
    );
    let signature = render_signature(idp, alg, &signed_info, signed_info_algorithm(alg));
    insert_after_issuer(request_xml, &signature, "saml")
}

/// Validates `xml` against the fake IdP.
fn validate(
    idp: &TestIdp,
    sp: &spot_sso::SpConfig,
    xml: &str,
    now: i64,
    replay: &MemoryReplayStore,
) -> Result<spot_sso::LogoutTarget> {
    block_on(validate_logout_request(
        sp,
        &idp.idp_config("acme"),
        xml,
        now,
        replay,
    ))
}

/// The refusal code of a failed validation.
fn refusal(outcome: Result<spot_sso::LogoutTarget>) -> &'static str {
    err(outcome).code()
}

#[test]
fn a_valid_request_names_the_subject_and_sessions() {
    let idp = TestIdp::new();
    let request = Request::valid(REQUEST_ID);
    let target = must(
        validate(
            &idp,
            &sp_config(),
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new(),
        ),
        "validate",
    );
    assert_eq!(target.provider.as_str(), "acme");
    assert_eq!(target.issuer, ISSUER);
    assert_eq!(target.subject.as_deref(), Some("ada@example.com"));
    assert_eq!(target.session_id, None);
    assert_eq!(
        target.session_indexes,
        vec!["_session-1".to_owned(), "_session-2".to_owned()]
    );
}

#[test]
fn a_valid_es256_request_verifies() {
    let idp = TestIdp::new();
    let request = Request::valid(REQUEST_ID);
    let target = must(
        validate(
            &idp,
            &sp_config(),
            &request.signed(&idp, Jwa::Es256),
            NOW,
            &MemoryReplayStore::new(),
        ),
        "validate",
    );
    assert_eq!(target.subject.as_deref(), Some("ada@example.com"));
}

#[test]
fn a_request_without_session_indexes_ends_every_session_of_the_subject() {
    let idp = TestIdp::new();
    let mut request = Request::valid(REQUEST_ID);
    request.session_indexes.clear();
    let target = must(
        validate(
            &idp,
            &sp_config(),
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new(),
        ),
        "validate",
    );
    assert!(target.session_indexes.is_empty());
}

#[test]
fn a_request_without_not_on_or_after_is_accepted() {
    let idp = TestIdp::new();
    let mut request = Request::valid(REQUEST_ID);
    request.not_on_or_after = None;
    assert!(
        validate(
            &idp,
            &sp_config(),
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new(),
        )
        .is_ok()
    );
}

#[test]
fn a_replayed_request_id_is_refused() {
    let idp = TestIdp::new();
    let signed = Request::valid(REQUEST_ID).signed(&idp, Jwa::Rs256);
    let replay = MemoryReplayStore::new();
    let sp = sp_config();
    assert!(validate(&idp, &sp, &signed, NOW, &replay).is_ok());
    assert_eq!(
        refusal(validate(&idp, &sp, &signed, NOW, &replay)),
        spot_sso::codes::SAML_SLO_REPLAY
    );
}

#[test]
fn a_wrong_destination_is_refused() {
    let idp = TestIdp::new();
    let mut request = Request::valid(REQUEST_ID);
    request.destination = Some(ACS_URL.to_owned());
    assert_eq!(
        refusal(validate(
            &idp,
            &sp_config(),
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_SLO_DESTINATION_MISMATCH
    );
}

#[test]
fn a_missing_destination_is_refused() {
    let idp = TestIdp::new();
    let mut request = Request::valid(REQUEST_ID);
    request.destination = None;
    assert_eq!(
        refusal(validate(
            &idp,
            &sp_config(),
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_SLO_DESTINATION_MISMATCH
    );
}

#[test]
fn an_unsigned_request_is_refused() {
    let idp = TestIdp::new();
    let request = Request::valid(REQUEST_ID);
    assert_eq!(
        refusal(validate(
            &idp,
            &sp_config(),
            &request.render(),
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_SIGNATURE_MISSING
    );
}

#[test]
fn a_signature_moved_off_the_request_is_refused() {
    let idp = TestIdp::new();
    let signed = Request::valid(REQUEST_ID).signed(&idp, Jwa::Rs256);
    let block = signature_block(&signed);
    let without_signature = signed.replacen(block, "", 1);
    let moved = without_signature.replace("</saml:NameID>", &format!("{block}</saml:NameID>"));
    assert_eq!(
        refusal(validate(
            &idp,
            &sp_config(),
            &moved,
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_SIGNATURE_NOT_ENVELOPED
    );
}

#[test]
fn a_wrapped_signed_request_is_refused() {
    let idp = TestIdp::new();
    let signed_inner = Request::valid("_inner").signed(&idp, Jwa::Rs256);
    let outer = Request::valid("_outer").render();
    let closing = "</samlp:LogoutRequest>";
    let at = outer.rfind(closing).unwrap_or(outer.len());
    let wrapped = format!("{}{}{}", &outer[..at], signed_inner, &outer[at..]);
    assert_eq!(
        refusal(validate(
            &idp,
            &sp_config(),
            &wrapped,
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_WRAPPER_FORBIDDEN
    );
}

#[test]
fn an_expired_request_is_refused() {
    let idp = TestIdp::new();
    let mut request = Request::valid(REQUEST_ID);
    request.not_on_or_after = Some(NOW - 121);
    assert_eq!(
        refusal(validate(
            &idp,
            &sp_config(),
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_SLO_EXPIRED
    );
}

#[test]
fn a_request_from_another_issuer_is_refused() {
    let idp = TestIdp::new();
    let mut request = Request::valid(REQUEST_ID);
    request.issuer = OTHER_ISSUER.to_owned();
    assert_eq!(
        refusal(validate(
            &idp,
            &sp_config(),
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_ISSUER_MISMATCH
    );
}

#[test]
fn a_request_without_a_name_id_is_refused() {
    let idp = TestIdp::new();
    let mut request = Request::valid(REQUEST_ID);
    request.name_id = None;
    assert_eq!(
        refusal(validate(
            &idp,
            &sp_config(),
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_SLO_NAME_ID_MISSING
    );
}

#[test]
fn a_transient_name_id_is_refused() {
    let idp = TestIdp::new();
    let mut request = Request::valid(REQUEST_ID);
    request.name_id_format = NAMEID_TRANSIENT.to_owned();
    assert_eq!(
        refusal(validate(
            &idp,
            &sp_config(),
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_NAMEID_INVALID
    );
}

#[test]
fn a_tampered_name_id_fails_the_digest() {
    let idp = TestIdp::new();
    let signed = Request::valid(REQUEST_ID).signed(&idp, Jwa::Rs256);
    let tampered = signed.replace("ada@example.com", "eve@example.com");
    assert_eq!(
        refusal(validate(
            &idp,
            &sp_config(),
            &tampered,
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_DIGEST_MISMATCH
    );
}

#[test]
fn a_non_request_root_is_refused() {
    let idp = TestIdp::new();
    let response =
        format!("<samlp:Response xmlns:samlp=\"{NS_PROTOCOL}\" ID=\"_r\" Version=\"2.0\"/>");
    assert_eq!(
        refusal(validate(
            &idp,
            &sp_config(),
            &response,
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_SLO_NOT_A_REQUEST
    );
}

#[test]
fn a_service_provider_without_a_logout_url_refuses_the_request() {
    let idp = TestIdp::new();
    let mut sp = sp_config();
    sp.slo_url = None;
    assert_eq!(
        refusal(validate(
            &idp,
            &sp,
            &Request::valid(REQUEST_ID).signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_SLO_ENDPOINT_MISSING
    );
}

#[test]
fn the_logout_response_answers_the_request() {
    let idp = TestIdp::new();
    let sp = sp_config();
    let idp_config = idp.idp_config("acme");
    let response = must(
        build_logout_response(&sp, &idp_config, REQUEST_ID, LogoutStatus::Success, NOW),
        "build_logout_response",
    );
    let again = must(
        build_logout_response(&sp, &idp_config, REQUEST_ID, LogoutStatus::Success, NOW),
        "build_logout_response",
    );
    assert_eq!(response, again, "the same request yields the same document");

    let document = must(Document::parse(&response), "the response is well-formed");
    let root = document.root_element();
    assert_eq!(root.tag_name().name(), "LogoutResponse");
    assert_eq!(root.tag_name().namespace(), Some(NS_PROTOCOL));
    assert_eq!(
        root.attribute("Destination"),
        idp_config.slo_url.as_deref(),
        "the response is delivered to the IdP's single-logout endpoint"
    );
    assert_eq!(root.attribute("InResponseTo"), Some(REQUEST_ID));
    assert_eq!(root.attribute("Version"), Some("2.0"));
    let status = root
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == "StatusCode");
    assert_eq!(
        status.and_then(|node| node.attribute("Value")),
        Some(STATUS_SUCCESS)
    );
}

#[test]
fn a_partial_logout_is_reported_as_partial() {
    let idp = TestIdp::new();
    let response = must(
        build_logout_response(
            &sp_config(),
            &idp.idp_config("acme"),
            REQUEST_ID,
            LogoutStatus::Partial,
            NOW,
        ),
        "build_logout_response",
    );
    let document = must(Document::parse(&response), "the response is well-formed");
    let status = document
        .root_element()
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == "StatusCode");
    assert_eq!(
        status.and_then(|node| node.attribute("Value")),
        Some(STATUS_PARTIAL)
    );
    assert_eq!(LogoutStatus::Partial.code(), STATUS_PARTIAL);
}

#[test]
fn a_response_without_an_idp_logout_url_cannot_be_built() {
    let idp = TestIdp::new();
    let mut idp_config = idp.idp_config("acme");
    idp_config.slo_url = None;
    assert_eq!(
        err(build_logout_response(
            &sp_config(),
            &idp_config,
            REQUEST_ID,
            LogoutStatus::Success,
            NOW
        ))
        .code(),
        spot_sso::codes::SAML_SLO_ENDPOINT_MISSING
    );
}

#[test]
fn a_not_on_or_after_beyond_the_longest_lifetime_is_refused() {
    let idp = TestIdp::new();
    let sp = sp_config();
    assert_eq!(sp.max_logout_lifetime_seconds, 600);

    let mut request = Request::valid(REQUEST_ID);
    request.not_on_or_after = Some(NOW + 600);
    assert!(
        validate(
            &idp,
            &sp,
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new()
        )
        .is_ok(),
        "exactly the limit"
    );

    let mut request = Request::valid(REQUEST_ID);
    request.not_on_or_after = Some(NOW + 601);
    let replay = MemoryReplayStore::new();
    assert_eq!(
        refusal(validate(
            &idp,
            &sp,
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &replay
        )),
        spot_sso::codes::SAML_SLO_LIFETIME_EXCESSIVE
    );
    assert!(
        replay.is_empty(),
        "a refused request leaves no replay record"
    );

    // A far-future IssueInstant does not stretch the window either.
    let mut request = Request::valid(REQUEST_ID);
    request.issue_instant = NOW + 86_400;
    request.not_on_or_after = Some(NOW + 86_400 + 300);
    assert_eq!(
        refusal(validate(
            &idp,
            &sp,
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_SLO_LIFETIME_EXCESSIVE
    );
}

/// A configuration with the given logout lifetime and skew.
fn sp_with(max: i64, skew: i64) -> spot_sso::SpConfig {
    let mut sp = sp_config();
    sp.max_logout_lifetime_seconds = max;
    sp.clock_skew_seconds = skew;
    sp
}

#[test]
fn a_request_without_not_on_or_after_is_bounded_to_the_configured_lifetime() {
    let idp = TestIdp::new();
    // max 120, skew 30: an IssueInstant is good until `issued + 150`, exclusive.
    let sp = sp_with(120, 30);
    for (age, accepted) in [(0, true), (149, true), (150, false), (3_600, false)] {
        let mut request = Request::valid(REQUEST_ID);
        request.not_on_or_after = None;
        request.issue_instant = NOW - age;
        let outcome = validate(
            &idp,
            &sp,
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new(),
        );
        if accepted {
            assert!(outcome.is_ok(), "age {age}");
        } else {
            assert_eq!(
                refusal(outcome),
                spot_sso::codes::SAML_SLO_EXPIRED,
                "age {age}"
            );
        }
    }
}

#[test]
fn a_request_without_not_on_or_after_has_no_issue_instant_slack_past_the_lifetime() {
    let idp = TestIdp::new();
    let sp = sp_config();
    // The default lifetime is the ceiling shared with the OIDC logout token; an hour old is gone.
    let mut request = Request::valid(REQUEST_ID);
    request.not_on_or_after = None;
    request.issue_instant = NOW - 3_000;
    assert_eq!(
        refusal(validate(
            &idp,
            &sp,
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_SLO_EXPIRED
    );
    // An IssueInstant beyond the skew in the future is refused too.
    let mut request = Request::valid(REQUEST_ID);
    request.not_on_or_after = None;
    request.issue_instant = NOW + 121;
    assert_eq!(
        refusal(validate(
            &idp,
            &sp,
            &request.signed(&idp, Jwa::Rs256),
            NOW,
            &MemoryReplayStore::new()
        )),
        spot_sso::codes::SAML_SLO_EXPIRED
    );
}

#[test]
fn the_replay_record_of_a_request_without_not_on_or_after_lasts_exactly_the_window() {
    let idp = TestIdp::new();
    let sp = sp_with(120, 30);
    let mut request = Request::valid(REQUEST_ID);
    request.not_on_or_after = None;
    let signed = request.signed(&idp, Jwa::Rs256);
    let replay = MemoryReplayStore::new();
    assert!(validate(&idp, &sp, &signed, NOW, &replay).is_ok());
    // Still recorded one second before the window closes: a replay, not an expiry.
    assert_eq!(
        refusal(validate(&idp, &sp, &signed, NOW + 149, &replay)),
        spot_sso::codes::SAML_SLO_REPLAY
    );
    // At the close the request is refused as expired, and the record is forgotten with it.
    assert_eq!(
        refusal(validate(&idp, &sp, &signed, NOW + 150, &replay)),
        spot_sso::codes::SAML_SLO_EXPIRED
    );
}

#[test]
fn the_logout_lifetime_is_validated_like_the_oidc_one() {
    let sp = sp_config();
    assert!(sp.validate().is_ok());
    for good in [1, 120, 600] {
        assert!(sp_with(good, 120).validate().is_ok(), "lifetime {good}");
    }
    for bad in [i64::MIN, -5, 0, 601, i64::MAX] {
        let errors = match sp_with(bad, 120).validate() {
            Err(errors) => errors,
            Ok(()) => panic!("lifetime {bad} must be refused"),
        };
        assert!(
            errors.has_code(spot_sso::codes::LOGOUT_LIFETIME_INVALID),
            "lifetime {bad}"
        );
    }
    assert_eq!(
        spot_sso::SpConfig::MAX_LOGOUT_LIFETIME_SECONDS,
        spot_sso::ProviderConfig::MAX_LOGOUT_TOKEN_LIFETIME_SECONDS
    );
}

#[test]
fn a_bad_logout_lifetime_refuses_every_request_with_its_own_code() {
    let idp = TestIdp::new();
    let signed = Request::valid(REQUEST_ID).signed(&idp, Jwa::Rs256);
    for bad in [-1, 0, 601, i64::MAX] {
        let replay = MemoryReplayStore::new();
        assert_eq!(
            refusal(validate(&idp, &sp_with(bad, 120), &signed, NOW, &replay)),
            spot_sso::codes::LOGOUT_LIFETIME_INVALID,
            "lifetime {bad}"
        );
        assert!(replay.is_empty(), "lifetime {bad}");
    }
}

#[test]
fn a_failing_replay_store_refuses_the_logout_request() {
    let idp = TestIdp::new();
    let signed = Request::valid(REQUEST_ID).signed(&idp, Jwa::Rs256);
    let sp = sp_config();
    let idp_config = idp.idp_config("acme");
    let store = spot_sso::contract::FailingStore;
    let future = validate_logout_request(&sp, &idp_config, &signed, NOW, &store);
    assert_send(&future);
    assert_eq!(
        refusal(block_on(future)),
        spot_sso::codes::REPLAY_STORE_UNAVAILABLE
    );
    assert!(validate(&idp, &sp, &signed, NOW, &MemoryReplayStore::new()).is_ok());
}

fn assert_send<T: Send>(_: &T) {}
