//! A fake SAML 2.0 identity provider for the SAML tests: it generates throwaway certificates
//! and signs assertions and responses with `ring`, in test code only.
//!
//! Nothing here ships: the crate never holds a private key, and every key and certificate is
//! generated when the test runs (the RSA key is the same throwaway 2048-bit key the OIDC tests
//! already embed, because `ring` cannot generate RSA keys).

#![allow(dead_code)]

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, RSA_PKCS1_SHA256, RsaKeyPair,
};
use spot_sso::{IdpConfig, Jwa, ProviderId, SamlClaimsMap, SignaturePolicy, SpConfig};

pub const ISSUER: &str = "https://idp.example.com";
pub const OTHER_ISSUER: &str = "https://evil.example.com";
pub const SP_ENTITY_ID: &str = "https://ledger.example.com/saml";
pub const ACS_URL: &str = "https://ledger.example.com/saml/acs";
pub const SLO_URL: &str = "https://ledger.example.com/saml/slo";

pub const NS_PROTOCOL: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
pub const NS_ASSERTION: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
pub const NS_DS: &str = "http://www.w3.org/2000/09/xmldsig#";
pub const NS_METADATA: &str = "urn:oasis:names:tc:SAML:2.0:metadata";

pub const ENVELOPED: &str = "http://www.w3.org/2000/09/xmldsig#enveloped-signature";
pub const C14N_EXC: &str = "http://www.w3.org/2001/10/xml-exc-c14n#";
pub const SIG_RSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
pub const SIG_ECDSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha256";
pub const DIGEST_SHA256: &str = "http://www.w3.org/2001/04/xmlenc#sha256";
pub const DIGEST_SHA1: &str = "http://www.w3.org/2000/09/xmldsig#sha1";
pub const SIG_RSA_SHA1: &str = "http://www.w3.org/2000/09/xmldsig#rsa-sha1";
pub const TRANSFORM_XPATH: &str = "http://www.w3.org/TR/1999/REC-xpath-19991116";
pub const STATUS_SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";
pub const BEARER: &str = "urn:oasis:names:tc:SAML:2.0:cm:bearer";
pub const NAMEID_PERSISTENT: &str = "urn:oasis:names:tc:SAML:1.1:nameid-format:persistent";
pub const NAMEID_TRANSIENT: &str = "urn:oasis:names:tc:SAML:2.0:nameid-format:transient";
pub const ACR_SILVER: &str = "urn:oasis:names:tc:SAML:2.0:ac:classes:PasswordProtectedTransport";

/// The throwaway 2048-bit RSA key the OIDC tests also embed; signs nothing real.
const RSA_PKCS8_B64: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCkNztn3ca3IMzcC5PqCeYYYAB46zow4Cpae2VEvWvRhBlHnwSmCEkWcpOJz9pOeDvqYlKmywlyd8ll5PV0+t69G7VFCsFCJOnxpfSmfSzPaC6xIWFaHkKLk5FhM7fqOxAiEZF6YeRYtX2R/V+SRbWkMWBIGI+g7EfYGBhQPvQLrfTkTBBHnwfNvIxGObInZeKzhmVl2dVD9VHlaSHhdGJnqnvbVHJhnCqPGfph9R3gD9PwMUfeJ5sOvlM/GfWekK6o21E4dTiFOCiOb6ZSoYdiv+l7qkLcFwm8oRoFKWi8yP6hYZLOsQmuWeF4SaYisNarSXHKDTKILIt0o19D3uS/AgMBAAECggEAEsCr7ASLeMYqHsbDsWpH+GieJTDXiFVRANwuiFcJRPfM2zzxjvRLRGD5MDeoOmXIkc+AmMN+5K+BND2dOiqb3QSZArVjtGX6wTmuYoTQ5S6Ehn3FXEEwYDgN1gfN7NWp1fojntIm/WpcWJuSmmWThJiO4izL2hiha6/0ttU5kUuxYXYZ57i2d/r+yY4XX69XlczEKI0M1s0F5EUyKzB6EH5RfDqBYTQSCyCwyMfIMZLpHIrzflzEqmw/svTpFqEv7v6IZCqmzOl0bF+nS+aeCutYjG/AqjgqgdEw18PiAZ5d+yrh3a/dKp1YerpM/2pLnOfDOGOOD5UtjMBKqrF4YQKBgQDQy8vh4MnT6O15Lu5tvwad2mCT32kFjFjpRKecXy0WiVKMc8UtTN3HzNErXNrlMFhyjRDyemiqtLvgqNIyPHPF18T0mSDiCpLmn1Pv48lywxpDpoVVJtU/u49gbZzNVnlsCch8SznCEtWL2y3s34NBNDd7DgOzsl98ybA0H8ep4wKBgQDJV1I6ylofNZ9dXka4X+7S1XEBXnJE2D+eFRauIf/3sBVe1HkMlQlBQ6CnSlEOFl2rBJPzGqwXGc6ns9BQMTBAPep2QNDiVFNj8OJk5oAsGuEMUndCeON93IwXse6DZ2SFRIP7RA6CxJ2KkAPJ5XoaNoKREXU63M4c9+tjrzzAdQKBgQC0SdURhHUhurYa9pBASTsgYGkWJknb1ZUMTxDfcDtRg1+5EVoHxKCYHl3asIH+dmFZAD/K8DGifQ7hbUSaxiI9ihNyFSsUo4a3BoqG0EypAJiUI5xrq21la8HgxKUplOqhhE0OLjKARp0H90jPiSwfyEPDyJUfu/zBlrHEQFC6ewKBgA82oef/Uj/5dUlAzmcwGIOlzRwlXs0E6gmv3j7aItU8YOEBfQQOXtynl4QMgYNm/Tzm46cC5O357TuOwrbMgLDZ/NVioGX5qTNsCC9Hm3e5sDeWI5zKNaZU++DueO9E6LuJKc/anJ4U7TKDH0YqKMfCRqtfflQsSwa1VPv7ScPhAoGAKThRfpLw5GtUzQN2ZwO6xzINY+ezRNIruijmHxCNrJQJ70wZA7I6qsv9WbNwjOa13uPLfA4XUVzm/Rc+uUW+Y2KiwXWqFlt3DpeXFnhb6OMit+6FHeGa+TNlf1sHFOwmTXG57mqttsPpClEIxC2OsX4XnnWEBtOyJWUyVemC6Q0=";

/// Unwraps a `Result`, panicking with context (tests may not use `unwrap`/`expect`).
pub fn must<T, E: std::fmt::Debug>(outcome: Result<T, E>, what: &str) -> T {
    match outcome {
        Ok(value) => value,
        Err(error) => panic!("{what}: {error:?}"),
    }
}

/// Unwraps an `Err`, panicking if the outcome is `Ok`.
pub fn err<T: std::fmt::Debug, E>(outcome: Result<T, E>) -> E {
    match outcome {
        Ok(value) => panic!("expected an error, got {value:?}"),
        Err(error) => error,
    }
}

/// 32 deterministic "random" bytes, distinct per `seed`.
pub fn rng_bytes(seed: u8) -> Vec<u8> {
    (0..32u8).map(|i| i.wrapping_add(seed)).collect()
}

/// Standard Base64 of `bytes`.
pub fn encode_base64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// A fake IdP holding an RSA and a P-256 signing key, each with a matching self-signed
/// certificate.
pub struct TestIdp {
    rng: SystemRandom,
    rsa: RsaKeyPair,
    ec: EcdsaKeyPair,
    rsa_cert: Vec<u8>,
    ec_cert: Vec<u8>,
}

impl TestIdp {
    /// Builds the fake provider with fresh keys and certificates.
    pub fn new() -> Self {
        let rng = SystemRandom::new();
        let rsa_der = must(B64.decode(RSA_PKCS8_B64), "the RSA test key is Base64");
        let rsa = must(RsaKeyPair::from_pkcs8(&rsa_der), "the RSA test key parses");

        let ec_kp = must(rcgen::KeyPair::generate(), "EC key generation works");
        let ec_der = ec_kp.serialized_der().to_vec();
        let ec = must(
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &ec_der, &rng),
            "the generated EC key parses",
        );

        let rsa_cert = self_signed_certificate(
            &must(
                rcgen::KeyPair::try_from(rsa_der.as_slice()),
                "the RSA test key loads into rcgen",
            ),
            "rsa.idp.example.com",
        );
        let ec_cert = self_signed_certificate(&ec_kp, "ec.idp.example.com");

        Self {
            rng,
            rsa,
            ec,
            rsa_cert,
            ec_cert,
        }
    }

    /// The DER certificate matching the key for `alg`.
    pub fn certificate(&self, alg: Jwa) -> Vec<u8> {
        match alg {
            Jwa::Rs256 => self.rsa_cert.clone(),
            Jwa::Es256 => self.ec_cert.clone(),
        }
    }

    /// Both pinned certificates.
    pub fn signing_certificates(&self) -> Vec<Vec<u8>> {
        vec![self.rsa_cert.clone(), self.ec_cert.clone()]
    }

    /// Signs `message` with the key for `alg`.
    pub fn sign(&self, alg: Jwa, message: &[u8]) -> Vec<u8> {
        match alg {
            Jwa::Rs256 => {
                let mut signature = vec![0u8; self.rsa.public().modulus_len()];
                must(
                    self.rsa
                        .sign(&RSA_PKCS1_SHA256, &self.rng, message, &mut signature),
                    "RS256 signing works",
                );
                signature
            }
            Jwa::Es256 => {
                let signature = must(self.ec.sign(&self.rng, message), "ES256 signing works");
                signature.as_ref().to_vec()
            }
        }
    }

    /// An IdP configuration that pins this provider's certificates.
    pub fn idp_config(&self, provider: &str) -> IdpConfig {
        let mut config = IdpConfig::new(
            ProviderId::new(provider),
            ISSUER,
            format!("{ISSUER}/sso"),
            self.signing_certificates(),
        );
        config.slo_url = Some(format!("{ISSUER}/slo"));
        config.claims_map = SamlClaimsMap {
            subject_attribute: None,
            email: "email".to_owned(),
            name: "displayName".to_owned(),
            groups: "groups".to_owned(),
        };
        config
    }
}

/// A self-signed certificate for `key`, DER-encoded.
fn self_signed_certificate(key: &rcgen::KeyPair, name: &str) -> Vec<u8> {
    let params = must(
        rcgen::CertificateParams::new(vec![name.to_owned()]),
        "certificate parameters build",
    );
    let certificate = must(
        params.self_signed(key),
        "the self-signed certificate builds",
    );
    certificate.der().to_vec()
}

/// A service-provider configuration for the fake IdP.
pub fn sp_config() -> SpConfig {
    SpConfig {
        entity_id: SP_ENTITY_ID.to_owned(),
        acs_url: ACS_URL.to_owned(),
        slo_url: Some(SLO_URL.to_owned()),
        signature_policy: SignaturePolicy::Assertion,
        ..SpConfig::default()
    }
}

/// The configurable content of one assertion.
#[derive(Clone, Debug)]
pub struct Assertion {
    pub prefix: String,
    pub id: String,
    pub issuer: String,
    pub audience: String,
    pub name_id: String,
    pub name_id_format: String,
    pub acr: String,
    pub conditions_not_before: Option<i64>,
    pub conditions_not_on_or_after: Option<i64>,
    pub confirmation_not_before: Option<i64>,
    pub confirmation_not_on_or_after: Option<i64>,
    pub confirmation_in_response_to: Option<String>,
    pub recipient: String,
    pub session_not_on_or_after: Option<i64>,
    pub authn_instant: i64,
    pub email: Option<String>,
    pub display_name: Option<String>,
    pub groups: Vec<String>,
    pub email_attribute: String,
    pub name_attribute: String,
    pub groups_attribute: String,
    pub include_conditions: bool,
    pub include_subject: bool,
}

impl Assertion {
    /// A valid assertion for the fake IdP at `now`.
    pub fn valid(id: &str, now: i64, in_response_to: Option<&str>) -> Self {
        Self {
            prefix: "saml".to_owned(),
            id: id.to_owned(),
            issuer: ISSUER.to_owned(),
            audience: SP_ENTITY_ID.to_owned(),
            name_id: "ada@example.com".to_owned(),
            name_id_format: NAMEID_PERSISTENT.to_owned(),
            acr: ACR_SILVER.to_owned(),
            conditions_not_before: Some(now - 60),
            conditions_not_on_or_after: Some(now + 3600),
            confirmation_not_before: Some(now - 60),
            confirmation_not_on_or_after: Some(now + 300),
            confirmation_in_response_to: in_response_to.map(str::to_owned),
            recipient: ACS_URL.to_owned(),
            session_not_on_or_after: Some(now + 7200),
            authn_instant: now,
            email: Some("ada@example.com".to_owned()),
            display_name: Some("Ada Lovelace".to_owned()),
            groups: vec!["engineering".to_owned()],
            email_attribute: "email".to_owned(),
            name_attribute: "displayName".to_owned(),
            groups_attribute: "groups".to_owned(),
            include_conditions: true,
            include_subject: true,
        }
    }
}

/// A qualified name with the assertion's prefix (empty prefix means the default namespace).
fn qname(prefix: &str, local: &str) -> String {
    if prefix.is_empty() {
        local.to_owned()
    } else {
        format!("{prefix}:{local}")
    }
}

/// Renders the unsigned assertion element.
pub fn render_assertion(a: &Assertion, now: i64) -> String {
    let p = &a.prefix;
    let ns = if p.is_empty() {
        format!(" xmlns=\"{NS_ASSERTION}\"")
    } else {
        format!(" xmlns:{p}=\"{NS_ASSERTION}\"")
    };
    let mut out = format!(
        "<{}{} ID=\"{}\" Version=\"2.0\" IssueInstant=\"{}\">",
        qname(p, "Assertion"),
        ns,
        a.id,
        format_instant(now)
    );
    out.push_str(&format!(
        "<{}>{}</{}>",
        qname(p, "Issuer"),
        a.issuer,
        qname(p, "Issuer")
    ));
    if a.include_subject {
        out.push_str(&format!("<{}>", qname(p, "Subject")));
        out.push_str(&format!(
            "<{} Format=\"{}\">{}</{}>",
            qname(p, "NameID"),
            a.name_id_format,
            a.name_id,
            qname(p, "NameID")
        ));
        out.push_str(&format!(
            "<{} Method=\"{}\">",
            qname(p, "SubjectConfirmation"),
            BEARER
        ));
        out.push_str(&format!(
            "<{} Recipient=\"{}\"",
            qname(p, "SubjectConfirmationData"),
            a.recipient
        ));
        if let Some(not_before) = a.confirmation_not_before {
            out.push_str(&format!(" NotBefore=\"{}\"", format_instant(not_before)));
        }
        if let Some(not_on_or_after) = a.confirmation_not_on_or_after {
            out.push_str(&format!(
                " NotOnOrAfter=\"{}\"",
                format_instant(not_on_or_after)
            ));
        }
        if let Some(in_response_to) = &a.confirmation_in_response_to {
            out.push_str(&format!(" InResponseTo=\"{in_response_to}\""));
        }
        out.push_str("/>");
        out.push_str(&format!("</{}>", qname(p, "SubjectConfirmation")));
        out.push_str(&format!("</{}>", qname(p, "Subject")));
    }
    if a.include_conditions {
        out.push_str(&format!("<{}", qname(p, "Conditions")));
        if let Some(not_before) = a.conditions_not_before {
            out.push_str(&format!(" NotBefore=\"{}\"", format_instant(not_before)));
        }
        if let Some(not_on_or_after) = a.conditions_not_on_or_after {
            out.push_str(&format!(
                " NotOnOrAfter=\"{}\"",
                format_instant(not_on_or_after)
            ));
        }
        out.push('>');
        out.push_str(&format!(
            "<{}><{}>{}</{}></{}>",
            qname(p, "AudienceRestriction"),
            qname(p, "Audience"),
            a.audience,
            qname(p, "Audience"),
            qname(p, "AudienceRestriction")
        ));
        out.push_str(&format!("</{}>", qname(p, "Conditions")));
    }
    out.push_str(&format!(
        "<{} AuthnInstant=\"{}\"",
        qname(p, "AuthnStatement"),
        format_instant(a.authn_instant)
    ));
    if let Some(expiry) = a.session_not_on_or_after {
        out.push_str(&format!(
            " SessionNotOnOrAfter=\"{}\"",
            format_instant(expiry)
        ));
    }
    out.push('>');
    out.push_str(&format!(
        "<{}><{}>{}</{}></{}>",
        qname(p, "AuthnContext"),
        qname(p, "AuthnContextClassRef"),
        a.acr,
        qname(p, "AuthnContextClassRef"),
        qname(p, "AuthnContext")
    ));
    out.push_str(&format!("</{}>", qname(p, "AuthnStatement")));

    out.push_str(&format!("<{}>", qname(p, "AttributeStatement")));
    if let Some(email) = &a.email {
        out.push_str(&format!(
            "<{} Name=\"{}\"><{}>{}</{}></{}>",
            qname(p, "Attribute"),
            a.email_attribute,
            qname(p, "AttributeValue"),
            email,
            qname(p, "AttributeValue"),
            qname(p, "Attribute")
        ));
    }
    if let Some(name) = &a.display_name {
        out.push_str(&format!(
            "<{} Name=\"{}\"><{}>{}</{}></{}>",
            qname(p, "Attribute"),
            a.name_attribute,
            qname(p, "AttributeValue"),
            name,
            qname(p, "AttributeValue"),
            qname(p, "Attribute")
        ));
    }
    if !a.groups.is_empty() {
        out.push_str(&format!(
            "<{} Name=\"{}\">",
            qname(p, "Attribute"),
            a.groups_attribute
        ));
        for group in &a.groups {
            out.push_str(&format!(
                "<{}>{}</{}>",
                qname(p, "AttributeValue"),
                group,
                qname(p, "AttributeValue")
            ));
        }
        out.push_str(&format!("</{}>", qname(p, "Attribute")));
    }
    out.push_str(&format!("</{}>", qname(p, "AttributeStatement")));

    out.push_str(&format!("</{}>", qname(p, "Assertion")));
    out
}

/// A rendered, signed artifact: the XML and the ids a test may reference.
#[derive(Clone, Debug)]
pub struct Signed {
    pub xml: String,
    pub assertion_id: String,
    pub response_id: String,
}

/// Builds a `Response` whose `Assertion` is signed by `idp`.
pub fn signed_assertion_response(
    idp: &TestIdp,
    alg: Jwa,
    now: i64,
    request_id: &str,
    assertion: &Assertion,
) -> Signed {
    let response_id = "_response-1".to_owned();
    let unsigned_assertion = render_assertion(assertion, now);
    let signed_assertion = sign_assertion_xml(idp, alg, &unsigned_assertion, &assertion.id);
    let xml = render_response(
        "samlp",
        &response_id,
        ACS_URL,
        request_id,
        None,
        STATUS_SUCCESS,
        &[signed_assertion],
        None,
    );
    Signed {
        xml,
        assertion_id: assertion.id.clone(),
        response_id,
    }
}

/// Signs an unsigned assertion element: digest, `SignedInfo`, signature, enveloped insertion.
pub fn sign_assertion_xml(idp: &TestIdp, alg: Jwa, assertion_xml: &str, id: &str) -> String {
    let digest = digest_of(assertion_xml);
    let signed_info = render_signed_info(
        &format!("#{id}"),
        &digest,
        DIGEST_SHA256,
        &[ENVELOPED, C14N_EXC],
    );
    let signature = render_signature(idp, alg, &signed_info, signed_info_algorithm(alg));
    insert_after_issuer(assertion_xml, &signature, "")
}

/// Signs an unsigned `Response` element that already carries its assertion.
pub fn sign_response_xml(idp: &TestIdp, alg: Jwa, response_xml: &str, id: &str) -> String {
    let digest = digest_of(response_xml);
    let signed_info = render_signed_info(
        &format!("#{id}"),
        &digest,
        DIGEST_SHA256,
        &[ENVELOPED, C14N_EXC],
    );
    let signature = render_signature(idp, alg, &signed_info, signed_info_algorithm(alg));
    insert_after_issuer(response_xml, &signature, "")
}

/// Builds a `Response` that is itself signed (the assertion inside is not).
pub fn signed_response(
    idp: &TestIdp,
    alg: Jwa,
    now: i64,
    request_id: &str,
    assertion: &Assertion,
) -> Signed {
    let response_id = "_response-1".to_owned();
    let unsigned_assertion = render_assertion(assertion, now);
    let unsigned = render_response(
        "samlp",
        &response_id,
        ACS_URL,
        request_id,
        None,
        STATUS_SUCCESS,
        &[unsigned_assertion],
        None,
    );
    let digest = digest_of(&unsigned);
    let signed_info = render_signed_info(
        &format!("#{response_id}"),
        &digest,
        DIGEST_SHA256,
        &[ENVELOPED, C14N_EXC],
    );
    let signature = render_signature(idp, alg, &signed_info, signed_info_algorithm(alg));
    let xml = insert_after_issuer(&unsigned, &signature, "samlp");
    Signed {
        xml,
        assertion_id: assertion.id.clone(),
        response_id,
    }
}

/// Renders a `Response` element around `assertions`.
#[allow(clippy::too_many_arguments)]
pub fn render_response(
    prefix: &str,
    id: &str,
    destination: &str,
    in_response_to: &str,
    issuer: Option<&str>,
    status: &str,
    assertions: &[String],
    extensions: Option<&str>,
) -> String {
    let mut out = format!(
        "<{} xmlns:{}=\"{}\" ID=\"{}\" Version=\"2.0\" IssueInstant=\"{}\" Destination=\"{}\" InResponseTo=\"{}\">",
        qname(prefix, "Response"),
        prefix,
        NS_PROTOCOL,
        id,
        format_instant(1_700_000_000),
        destination,
        in_response_to
    );
    if let Some(issuer) = issuer {
        out.push_str(&format!(
            "<saml2:Issuer xmlns:saml2=\"{NS_ASSERTION}\">{issuer}</saml2:Issuer>"
        ));
    }
    if let Some(extensions) = extensions {
        out.push_str(extensions);
    }
    out.push_str(&format!(
        "<{}><{} Value=\"{}\"/></{}>",
        qname(prefix, "Status"),
        qname(prefix, "StatusCode"),
        status,
        qname(prefix, "Status")
    ));
    for assertion in assertions {
        out.push_str(assertion);
    }
    out.push_str(&format!("</{}>", qname(prefix, "Response")));
    out
}

/// Renders the `ds:SignedInfo` element with the given reference, digest and transforms.
pub fn render_signed_info(
    uri: &str,
    digest: &str,
    digest_algorithm: &str,
    transforms: &[&str],
) -> String {
    let mut out = format!(
        "<ds:SignedInfo xmlns:ds=\"{NS_DS}\"><ds:CanonicalizationMethod Algorithm=\"{C14N_EXC}\"/><ds:SignatureMethod Algorithm=\"{{SIGALG}}\"/><ds:Reference URI=\"{uri}\"><ds:Transforms>"
    );
    for transform in transforms {
        out.push_str(&format!("<ds:Transform Algorithm=\"{transform}\"/>"));
    }
    out.push_str(&format!(
        "</ds:Transforms><ds:DigestMethod Algorithm=\"{digest_algorithm}\"/><ds:DigestValue>{digest}</ds:DigestValue></ds:Reference></ds:SignedInfo>"
    ));
    out
}

/// The signature-method URI for an algorithm.
pub fn signed_info_algorithm(alg: Jwa) -> &'static str {
    match alg {
        Jwa::Rs256 => SIG_RSA_SHA256,
        Jwa::Es256 => SIG_ECDSA_SHA256,
    }
}

/// Renders the `ds:Signature` element, substituting the signature algorithm and value.
pub fn render_signature(
    idp: &TestIdp,
    alg: Jwa,
    signed_info: &str,
    signature_algorithm: &str,
) -> String {
    let signed_info = signed_info.replace("{SIGALG}", signature_algorithm);
    let canonical = must(
        spot_sso::saml::canonicalize(&signed_info, false),
        "SignedInfo canonicalises",
    );
    let signature = idp.sign(alg, canonical.as_bytes());
    let cert = B64.encode(idp.certificate(alg));
    format!(
        "<ds:Signature xmlns:ds=\"{NS_DS}\">{signed_info}<ds:SignatureValue>{}</ds:SignatureValue><ds:KeyInfo><ds:X509Data><ds:X509Certificate>{cert}</ds:X509Certificate></ds:X509Data></ds:KeyInfo></ds:Signature>",
        B64.encode(signature)
    )
}

/// The Base64 SHA-256 digest of a fragment's exclusive-canonical form.
pub fn digest_of(xml: &str) -> String {
    let canonical = must(
        spot_sso::saml::canonicalize(xml, false),
        "fragment canonicalises",
    );
    // SHA-256 via ring, re-exported through the crate's public surface by canonicalising only.
    B64.encode(sha256(canonical.as_bytes()))
}

/// Inserts `signature` right after the first `Issuer` element of the `prefix`-namespaced element.
pub fn insert_after_issuer(element: &str, signature: &str, prefix: &str) -> String {
    let marker = format!("</{}>", qname(prefix, "Issuer"));
    match element.find(&marker) {
        Some(position) => {
            let at = position + marker.len();
            format!("{}{}{}", &element[..at], signature, &element[at..])
        }
        None => {
            // No Issuer: insert right after the element's opening tag.
            match element.find('>') {
                Some(at) => format!("{}{}{}", &element[..=at], signature, &element[at + 1..]),
                None => element.to_owned(),
            }
        }
    }
}

/// Signs an unsigned `EntityDescriptor` in place, returning the signed document.
pub fn sign_metadata(idp: &TestIdp, alg: Jwa, entity_xml: &str, id: &str) -> String {
    let digest = digest_of(entity_xml);
    let signed_info = render_signed_info(
        &format!("#{id}"),
        &digest,
        DIGEST_SHA256,
        &[ENVELOPED, C14N_EXC],
    );
    let signature = render_signature(idp, alg, &signed_info, signed_info_algorithm(alg));
    insert_after_issuer(entity_xml, &signature, "md")
}

/// Extracts the first assertion element (start tag through end tag) from a document.
pub fn assertion_block<'a>(document: &'a str, prefix: &str) -> &'a str {
    let open = format!("<{} ", qname(prefix, "Assertion"));
    let close = format!("</{}>", qname(prefix, "Assertion"));
    let Some(start) = document.find(&open) else {
        return "";
    };
    match document[start..].find(&close) {
        Some(offset) => &document[start..start + offset + close.len()],
        None => &document[start..],
    }
}

/// Extracts the first `ds:Signature` element from a document.
pub fn signature_block(document: &str) -> &str {
    let close = "</ds:Signature>";
    let Some(start) = document.find("<ds:Signature") else {
        return "";
    };
    match document[start..].find(close) {
        Some(offset) => &document[start..start + offset + close.len()],
        None => &document[start..],
    }
}

/// Base64-encodes a SAML message for the HTTP-POST binding.
pub fn post(xml: &str) -> String {
    B64.encode(xml.as_bytes())
}

/// A pending sign-in for a request id.
pub fn pending(request_id: &str, relay_state: &str, now: i64) -> spot_sso::SamlPendingAuth {
    spot_sso::SamlPendingAuth {
        provider: ProviderId::new("acme"),
        request_id: request_id.to_owned(),
        relay_state: relay_state.to_owned(),
        return_to: "/app".to_owned(),
        created_at: now,
        expires_at: now + 600,
        used: false,
    }
}

/// Formats a unix instant as `YYYY-MM-DDThh:mm:ssZ`.
pub fn format_instant(seconds: i64) -> String {
    use time::OffsetDateTime;
    let instant = must(
        OffsetDateTime::from_unix_timestamp(seconds),
        "a valid unix instant",
    );
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        instant.year(),
        u8::from(instant.month()),
        instant.day(),
        instant.hour(),
        instant.minute(),
        instant.second()
    )
}

/// The SHA-256 digest of `data`, via `ring`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}
