//! Shared test fixtures: a fake identity provider that signs RS256 and ES256 tokens with
//! `ring`, and an in-memory directory.
//!
//! The RSA key below is a throwaway 2048-bit key generated for these tests only; it signs
//! nothing real and protects nothing. `ring` can generate EC keys but not RSA keys, so the RSA
//! key is embedded. ES256 keys are generated fresh in each test run.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use base64::engine::general_purpose::STANDARD;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{
    ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair, RSA_PKCS1_SHA256, RsaKeyPair,
};
use serde_json::json;
use spot_sso::{
    ClaimsMap, ClientAuth, DirectoryUser, DirectoryView, Discovery, Jwa, ProviderConfig,
    ProviderId, SecondFactor,
};
use spot_storage_contract::RecordId;
use time::{Duration, OffsetDateTime};

/// The RSA test key, PKCS#8 v1, Base64 (standard, padded).
pub(crate) const RSA_PKCS8_B64: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCkNztn3ca3IMzcC5PqCeYYYAB46zow4Cpae2VEvWvRhBlHnwSmCEkWcpOJz9pOeDvqYlKmywlyd8ll5PV0+t69G7VFCsFCJOnxpfSmfSzPaC6xIWFaHkKLk5FhM7fqOxAiEZF6YeRYtX2R/V+SRbWkMWBIGI+g7EfYGBhQPvQLrfTkTBBHnwfNvIxGObInZeKzhmVl2dVD9VHlaSHhdGJnqnvbVHJhnCqPGfph9R3gD9PwMUfeJ5sOvlM/GfWekK6o21E4dTiFOCiOb6ZSoYdiv+l7qkLcFwm8oRoFKWi8yP6hYZLOsQmuWeF4SaYisNarSXHKDTKILIt0o19D3uS/AgMBAAECggEAEsCr7ASLeMYqHsbDsWpH+GieJTDXiFVRANwuiFcJRPfM2zzxjvRLRGD5MDeoOmXIkc+AmMN+5K+BND2dOiqb3QSZArVjtGX6wTmuYoTQ5S6Ehn3FXEEwYDgN1gfN7NWp1fojntIm/WpcWJuSmmWThJiO4izL2hiha6/0ttU5kUuxYXYZ57i2d/r+yY4XX69XlczEKI0M1s0F5EUyKzB6EH5RfDqBYTQSCyCwyMfIMZLpHIrzflzEqmw/svTpFqEv7v6IZCqmzOl0bF+nS+aeCutYjG/AqjgqgdEw18PiAZ5d+yrh3a/dKp1YerpM/2pLnOfDOGOOD5UtjMBKqrF4YQKBgQDQy8vh4MnT6O15Lu5tvwad2mCT32kFjFjpRKecXy0WiVKMc8UtTN3HzNErXNrlMFhyjRDyemiqtLvgqNIyPHPF18T0mSDiCpLmn1Pv48lywxpDpoVVJtU/u49gbZzNVnlsCch8SznCEtWL2y3s34NBNDd7DgOzsl98ybA0H8ep4wKBgQDJV1I6ylofNZ9dXka4X+7S1XEBXnJE2D+eFRauIf/3sBVe1HkMlQlBQ6CnSlEOFl2rBJPzGqwXGc6ns9BQMTBAPep2QNDiVFNj8OJk5oAsGuEMUndCeON93IwXse6DZ2SFRIP7RA6CxJ2KkAPJ5XoaNoKREXU63M4c9+tjrzzAdQKBgQC0SdURhHUhurYa9pBASTsgYGkWJknb1ZUMTxDfcDtRg1+5EVoHxKCYHl3asIH+dmFZAD/K8DGifQ7hbUSaxiI9ihNyFSsUo4a3BoqG0EypAJiUI5xrq21la8HgxKUplOqhhE0OLjKARp0H90jPiSwfyEPDyJUfu/zBlrHEQFC6ewKBgA82oef/Uj/5dUlAzmcwGIOlzRwlXs0E6gmv3j7aItU8YOEBfQQOXtynl4QMgYNm/Tzm46cC5O357TuOwrbMgLDZ/NVioGX5qTNsCC9Hm3e5sDeWI5zKNaZU++DueO9E6LuJKc/anJ4U7TKDH0YqKMfCRqtfflQsSwa1VPv7ScPhAoGAKThRfpLw5GtUzQN2ZwO6xzINY+ezRNIruijmHxCNrJQJ70wZA7I6qsv9WbNwjOa13uPLfA4XUVzm/Rc+uUW+Y2KiwXWqFlt3DpeXFnhb6OMit+6FHeGa+TNlf1sHFOwmTXG57mqttsPpClEIxC2OsX4XnnWEBtOyJWUyVemC6Q0=";

/// The RSA test key's modulus, Base64url.
pub(crate) const RSA_N_B64: &str = "pDc7Z93GtyDM3AuT6gnmGGAAeOs6MOAqWntlRL1r0YQZR58EpghJFnKTic_aTng76mJSpssJcnfJZeT1dPrevRu1RQrBQiTp8aX0pn0sz2gusSFhWh5Ci5ORYTO36jsQIhGRemHkWLV9kf1fkkW1pDFgSBiPoOxH2BgYUD70C6305EwQR58HzbyMRjmyJ2Xis4ZlZdnVQ_VR5Wkh4XRiZ6p721RyYZwqjxn6YfUd4A_T8DFH3iebDr5TPxn1npCuqNtROHU4hTgojm-mUqGHYr_pe6pC3BcJvKEaBSlovMj-oWGSzrEJrlnheEmmIrDWq0lxyg0yiCyLdKNfQ97kvw";
/// The RSA test key's public exponent, Base64url (`AQAB` = 65537).
pub(crate) const RSA_E_B64: &str = "AQAB";

/// The RSA key id the fake provider publishes.
pub(crate) const KID_RSA: &str = "rsa-1";
/// The EC key id the fake provider publishes.
pub(crate) const KID_EC: &str = "ec-1";

/// The fake provider's issuer.
pub(crate) const ISSUER: &str = "https://idp.example.com";
/// The redirect URI the fake client registers.
pub(crate) const REDIRECT_URI: &str = "https://ledger.example.com/sso/callback";

/// Encodes bytes as unpadded Base64url.
fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A fake identity provider holding one RSA and one EC signing key.
pub(crate) struct TestIdp {
    rng: SystemRandom,
    rsa: RsaKeyPair,
    ec: EcdsaKeyPair,
    ec_public: Vec<u8>,
}

impl TestIdp {
    /// Builds the fake provider with a fresh EC key.
    pub(crate) fn new() -> Self {
        let rng = SystemRandom::new();
        let rsa = RsaKeyPair::from_pkcs8(
            &STANDARD
                .decode(RSA_PKCS8_B64)
                .unwrap_or_else(|_| panic!("the embedded RSA test key is Base64")),
        )
        .unwrap_or_else(|_| panic!("the embedded RSA test key parses"));
        let ec_pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .unwrap_or_else(|_| panic!("EC key generation works"));
        let ec =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, ec_pkcs8.as_ref(), &rng)
                .unwrap_or_else(|_| panic!("the generated EC key parses"));
        let ec_public = ec.public_key().as_ref().to_vec();
        Self {
            rng,
            rsa,
            ec,
            ec_public,
        }
    }

    /// The provider's configuration.
    pub(crate) fn config(&self) -> ProviderConfig {
        ProviderConfig {
            id: ProviderId::new("acme"),
            issuer: ISSUER.to_owned(),
            client_id: "spotledger".to_owned(),
            auth: ClientAuth::ClientSecretRef {
                secret_ref: "providers/acme/client-secret".to_owned(),
            },
            scopes: vec![
                "openid".to_owned(),
                "email".to_owned(),
                "profile".to_owned(),
            ],
            redirect_uri: REDIRECT_URI.to_owned(),
            discovery_url: format!("{ISSUER}/.well-known/openid-configuration"),
            allowed_algs: vec![Jwa::Rs256, Jwa::Es256],
            clock_skew_seconds: 60,
            max_logout_token_lifetime_seconds: 120,
            claims_map: ClaimsMap::default(),
            require_email_verified: true,
            acr_values: Vec::new(),
        }
    }

    /// The discovery document, as bytes.
    pub(crate) fn discovery_bytes(&self) -> Vec<u8> {
        let document = json!({
            "issuer": ISSUER,
            "authorization_endpoint": format!("{ISSUER}/authorize"),
            "token_endpoint": format!("{ISSUER}/token"),
            "jwks_uri": format!("{ISSUER}/jwks"),
            "end_session_endpoint": format!("{ISSUER}/logout"),
            "id_token_signing_alg_values_supported": ["RS256", "ES256"],
            "code_challenge_methods_supported": ["S256"],
            "response_types_supported": ["code"],
        });
        serde_json::to_vec(&document).unwrap_or_default()
    }

    /// The parsed discovery document for the configuration.
    pub(crate) fn discovery(&self) -> Discovery {
        Discovery::parse(&self.discovery_bytes(), &self.config())
            .unwrap_or_else(|error| panic!("discovery parses: {error:?}"))
    }

    /// The JWKS document, as bytes.
    pub(crate) fn jwks_bytes(&self) -> Vec<u8> {
        let (x, y) = self.ec_coordinates();
        let document = json!({
            "keys": [
                {
                    "kty": "RSA",
                    "kid": KID_RSA,
                    "alg": "RS256",
                    "use": "sig",
                    "n": RSA_N_B64,
                    "e": RSA_E_B64,
                },
                {
                    "kty": "EC",
                    "crv": "P-256",
                    "kid": KID_EC,
                    "alg": "ES256",
                    "use": "sig",
                    "x": b64(&x),
                    "y": b64(&y),
                }
            ]
        });
        serde_json::to_vec(&document).unwrap_or_default()
    }

    /// The EC public key's affine coordinates.
    pub(crate) fn ec_coordinates(&self) -> ([u8; 32], [u8; 32]) {
        let mut x = [0u8; 32];
        let mut y = [0u8; 32];
        x.copy_from_slice(&self.ec_public[1..33]);
        y.copy_from_slice(&self.ec_public[33..65]);
        (x, y)
    }

    /// Signs `message` with RS256.
    pub(crate) fn sign_rs256(&self, message: &[u8]) -> Vec<u8> {
        let mut signature = vec![0u8; self.rsa.public().modulus_len()];
        self.rsa
            .sign(&RSA_PKCS1_SHA256, &self.rng, message, &mut signature)
            .unwrap_or_else(|_| panic!("RS256 signing works"));
        signature
    }

    /// Signs `message` with ES256.
    pub(crate) fn sign_es256(&self, message: &[u8]) -> Vec<u8> {
        let signature = self
            .ec
            .sign(&self.rng, message)
            .unwrap_or_else(|_| panic!("ES256 signing works"));
        signature.as_ref().to_vec()
    }

    /// Mints a compact token with the given header algorithm, key id and claims.
    pub(crate) fn mint(&self, alg: Jwa, kid: &str, claims: &serde_json::Value) -> String {
        let header = json!({ "alg": alg.as_str(), "kid": kid, "typ": "JWT" });
        let header = b64(&serde_json::to_vec(&header).unwrap_or_default());
        let payload = b64(&serde_json::to_vec(claims).unwrap_or_default());
        let signing_input = format!("{header}.{payload}");
        let signature = match alg {
            Jwa::Rs256 => self.sign_rs256(signing_input.as_bytes()),
            Jwa::Es256 => self.sign_es256(signing_input.as_bytes()),
        };
        format!("{signing_input}.{}", b64(&signature))
    }

    /// A minted token with the standard claims for a fresh sign-in.
    pub(crate) fn id_token(
        &self,
        alg: Jwa,
        nonce: &str,
        now: i64,
        mutate: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
    ) -> String {
        let mut claims = serde_json::Map::new();
        claims.insert("iss".to_owned(), json!(ISSUER));
        claims.insert("sub".to_owned(), json!("subject-1"));
        claims.insert("aud".to_owned(), json!("spotledger"));
        claims.insert("exp".to_owned(), json!(now + 3600));
        claims.insert("iat".to_owned(), json!(now));
        claims.insert("nbf".to_owned(), json!(now));
        claims.insert("nonce".to_owned(), json!(nonce));
        claims.insert("email".to_owned(), json!("ada@example.com"));
        claims.insert("email_verified".to_owned(), json!(true));
        claims.insert("name".to_owned(), json!("Ada Lovelace"));
        claims.insert("groups".to_owned(), json!(["engineering"]));
        claims.insert("acr".to_owned(), json!("urn:mace:incommon:iap:silver"));
        claims.insert("amr".to_owned(), json!(["pwd", "otp"]));
        mutate(&mut claims);
        let kid = match alg {
            Jwa::Rs256 => KID_RSA,
            Jwa::Es256 => KID_EC,
        };
        self.mint(alg, kid, &serde_json::Value::Object(claims))
    }
}

/// An in-memory tenant directory for policy and resolution tests.
#[derive(Default)]
pub(crate) struct FakeDirectory {
    users: BTreeMap<RecordId, DirectoryUser>,
    emails: BTreeMap<String, RecordId>,
    linked: BTreeMap<(String, String), RecordId>,
    invitations: BTreeMap<String, RecordId>,
    break_glass: BTreeSet<RecordId>,
    healthy: BTreeSet<ProviderId>,
}

impl FakeDirectory {
    /// An empty directory.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) a user.
    pub(crate) fn add(&mut self, user: DirectoryUser) -> RecordId {
        self.emails
            .insert(user.sign_in_name.to_lowercase(), user.id);
        let id = user.id;
        self.users.insert(id, user);
        id
    }

    /// Marks a user as a break-glass administrator.
    pub(crate) fn make_break_glass(&mut self, id: RecordId) {
        self.break_glass.insert(id);
    }

    /// Links an identity to a user.
    pub(crate) fn link(&mut self, issuer: &str, subject: &str, user: RecordId) {
        self.linked
            .insert((issuer.to_owned(), subject.to_owned()), user);
    }

    /// Records a pending invitation for an e-mail.
    pub(crate) fn invite(&mut self, email: &str, invitation: RecordId) {
        self.invitations.insert(email.to_lowercase(), invitation);
    }

    /// Marks a provider healthy.
    pub(crate) fn healthy_provider(&mut self, provider: ProviderId) {
        self.healthy.insert(provider);
    }
}

impl DirectoryView for FakeDirectory {
    fn user(&self, id: RecordId) -> Option<DirectoryUser> {
        self.users.get(&id).cloned()
    }

    fn enabled_tenant_admins(&self) -> usize {
        self.users
            .values()
            .filter(|u| u.enabled && u.is_tenant_admin)
            .count()
    }

    fn enabled_break_glass_admins(&self) -> usize {
        self.break_glass
            .iter()
            .filter(|id| self.users.get(*id).is_some_and(|u| u.enabled))
            .count()
    }

    fn provider_healthy(&self, provider: &ProviderId) -> bool {
        self.healthy.contains(provider)
    }

    fn user_for_email(&self, email: &str) -> Option<RecordId> {
        self.emails.get(&email.to_lowercase()).copied()
    }

    fn linked_user(&self, issuer: &str, subject: &str) -> Option<RecordId> {
        self.linked
            .get(&(issuer.to_owned(), subject.to_owned()))
            .copied()
    }

    fn open_invitation(&self, email: &str, _now: OffsetDateTime) -> Option<RecordId> {
        self.invitations.get(&email.to_lowercase()).copied()
    }
}

/// A user as the directory returns it: an enabled, administrator, local-password, TOTP user by
/// default.
pub(crate) fn user(id: RecordId, sign_in_name: &str) -> DirectoryUser {
    DirectoryUser {
        id,
        enabled: true,
        user_type: spot_iam_directory::UserType::Internal,
        sign_in_name: sign_in_name.to_owned(),
        is_tenant_admin: true,
        has_local_password: true,
        second_factors: vec![SecondFactor::Totp],
        allows_sso: true,
    }
}

/// A fresh record id.
pub(crate) fn new_id() -> RecordId {
    RecordId::new_v7()
}

/// A fixed instant.
pub(crate) fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::seconds(seconds)
}

/// A key cache built from the fake provider's JWKS.
pub(crate) fn keys(idp: &TestIdp) -> spot_sso::KeyCache {
    let jwks = spot_sso::parse_jwks(&idp.jwks_bytes())
        .unwrap_or_else(|error| panic!("the fake JWKS parses: {error:?}"));
    spot_sso::KeyCache::new(jwks, 300)
}

/// A token response carrying an ID token.
pub(crate) fn token_response(id_token: &str) -> spot_sso::TokenResponse {
    spot_sso::TokenResponse {
        id_token: Some(id_token.to_owned()),
        ..spot_sso::TokenResponse::default()
    }
}

/// A token response carrying an ID token and an access token.
pub(crate) fn token_response_with_access(
    id_token: &str,
    access_token: &str,
) -> spot_sso::TokenResponse {
    spot_sso::TokenResponse {
        id_token: Some(id_token.to_owned()),
        access_token: Some(access_token.to_owned()),
        ..spot_sso::TokenResponse::default()
    }
}

/// 32 deterministic "random" bytes for [`spot_sso::begin`], distinct per `seed`.
pub(crate) fn rng_bytes(seed: u8) -> Vec<u8> {
    (0..32u8).map(|i| i.wrapping_add(seed)).collect()
}

/// Unwraps an `Ok`, panicking with context (tests may not use `unwrap`/`expect`).
pub(crate) fn ok<T, E: std::fmt::Debug>(outcome: Result<T, E>, what: &str) -> T {
    match outcome {
        Ok(value) => value,
        Err(error) => panic!("{what}: {error:?}"),
    }
}

/// Unwraps an `Err`, panicking with context (tests may not use `unwrap`/`expect`).
pub(crate) fn err<T: std::fmt::Debug, E>(outcome: Result<T, E>) -> E {
    match outcome {
        Ok(value) => panic!("expected an error, got {value:?}"),
        Err(error) => error,
    }
}
