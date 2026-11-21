//! API keys: one printable string carrying a public key id and a secret.
//!
//! Format: `spk_<key_id>_<secret>` — a fixed prefix so secret scanners recognise these
//! strings, a public 12-character lower-case Base32 key id for lookup and logs, and a
//! 43-character Base64url secret for 32 bytes of CSPRNG entropy. Only the secret's SHA-256
//! digest is stored.

use std::fmt;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::AuthError;
use crate::token::TokenDigest;
use crate::totp::{base32_decode, base32_encode};

/// The fixed prefix of every API key.
pub const API_KEY_PREFIX: &str = "spk_";

/// Number of characters in the public key id.
pub const API_KEY_ID_LENGTH: usize = 12;

/// Number of characters in the secret part of an API key (32 bytes, Base64url, no padding).
pub const API_KEY_SECRET_LENGTH: usize = 43;

// 7 bytes = 56 bits = exactly 12 Base32 characters.
const API_KEY_ID_BYTES: usize = 7;

const API_KEY_SECRET_BYTES: usize = 32;

/// An API key the client presents: `spk_<key_id>_<secret>`.
///
/// The key id is public by design and stays visible in [`Debug`]; the secret part is
/// redacted, the struct is zeroized on drop, and there is no `Display`.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct ApiKey {
    key_id: String,
    secret: String,
}

impl ApiKey {
    /// A fresh random key. The caller shows the textual key to the user once and stores
    /// only the returned [`StoredApiKey`]; the platform never needs the key again.
    #[must_use]
    pub fn generate() -> (Self, StoredApiKey) {
        let mut id_bytes = [0u8; API_KEY_ID_BYTES];
        crate::util::fill_os_random(&mut id_bytes);
        let key_id = base32_encode(&id_bytes).to_ascii_lowercase();

        let mut secret_bytes = [0u8; API_KEY_SECRET_BYTES];
        crate::util::fill_os_random(&mut secret_bytes);
        let secret = URL_SAFE_NO_PAD.encode(secret_bytes);

        let key = Self {
            key_id: key_id.clone(),
            secret,
        };
        let stored = StoredApiKey {
            key_id,
            digest: key.digest(),
        };
        (key, stored)
    }

    /// Wraps a presented key after checking the exact `spk_<key_id>_<secret>` shape, so a
    /// tampered or truncated key is rejected before its digest is ever computed. Errors
    /// never echo the input.
    pub fn parse(text: &str) -> Result<Self, AuthError> {
        let Some((key_id, secret)) = text
            .strip_prefix(API_KEY_PREFIX)
            .and_then(|rest| rest.split_once('_'))
        else {
            return Err(AuthError::ApiKeyMalformed);
        };

        let well_formed = key_id.len() == API_KEY_ID_LENGTH
            && key_id.chars().all(|c| matches!(c, 'a'..='z' | '2'..='7'))
            && secret.len() == API_KEY_SECRET_LENGTH
            && secret
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !well_formed {
            return Err(AuthError::ApiKeyMalformed);
        }
        // The key id must decode as Base32. That also drops strings whose first 12
        // characters share the alphabet shape but are not valid Base32.
        match base32_decode(&key_id.to_ascii_uppercase()) {
            Ok(bytes) if bytes.len() == API_KEY_ID_BYTES => {}
            _ => return Err(AuthError::ApiKeyMalformed),
        }

        Ok(Self {
            key_id: key_id.to_owned(),
            secret: secret.to_owned(),
        })
    }
}

impl ApiKey {
    /// The public key id; safe to log and to use as a lookup key.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// The SHA-256 digest of the secret part.
    #[must_use]
    pub fn digest(&self) -> TokenDigest {
        TokenDigest::of(self.secret.as_bytes())
    }

    /// The full textual form, exactly as the client presents it. Treat the returned string
    /// as secret material: this copy is not zeroized when dropped.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{API_KEY_PREFIX}{}_{}", self.key_id, self.secret)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The key id is public by design; only the secret part is redacted.
        f.debug_struct("ApiKey")
            .field("key_id", &self.key_id)
            .field("secret", &"***")
            .finish()
    }
}

/// The at-rest form of an API key: the public key id plus the digest of the secret.
#[derive(Debug)]
pub struct StoredApiKey {
    /// The public key id: safe to log, safe to use as a database lookup key.
    pub key_id: String,
    /// The verifier for the secret part; [`TokenDigest`]'s `Debug` redacts it and the
    /// digest zeroizes when dropped.
    pub digest: TokenDigest,
}

impl StoredApiKey {
    /// Verification of a presented key against the stored id and digest; the digest
    /// comparison is constant time.
    #[must_use]
    pub fn verify(&self, presented: &ApiKey) -> bool {
        self.key_id == presented.key_id() && self.digest.matches(&presented.digest())
    }

    /// Captures the public key id and the secret digest of an already generated key.
    #[must_use]
    pub fn from_key(key: &ApiKey) -> Self {
        Self {
            key_id: key.key_id().to_owned(),
            digest: key.digest(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::expect_ok;

    fn id_and_secret_of(key: &ApiKey) -> (String, String) {
        let text = key.text();
        match text
            .strip_prefix(API_KEY_PREFIX)
            .and_then(|rest| rest.split_once('_'))
        {
            Some((id, secret)) => (id.to_owned(), secret.to_owned()),
            None => panic!("generated keys must contain one separator"),
        }
    }

    #[test]
    fn generated_keys_round_trip() {
        let (key, stored) = ApiKey::generate();
        assert_eq!(key.key_id().len(), API_KEY_ID_LENGTH);
        assert!(
            key.key_id()
                .chars()
                .all(|c| matches!(c, 'a'..='z' | '2'..='7'))
        );
        assert_eq!(stored.key_id, key.key_id());
        let reparsed = expect_ok(ApiKey::parse(&key.text()), "round trip");
        assert!(stored.verify(&reparsed));
        assert!(stored.verify(&key));
    }

    #[test]
    fn from_key_during_generation_matches_the_pair() {
        let (key, stored) = ApiKey::generate();
        let captured = StoredApiKey::from_key(&key);
        assert_eq!(captured.key_id, stored.key_id);
    }

    #[test]
    fn verification_rejects_other_keys_and_tampering() {
        let (first, stored) = ApiKey::generate();
        let (second, _) = ApiKey::generate();
        assert!(stored.verify(&first));
        assert!(!stored.verify(&second));
        // A one-character change in the secret part keeps the shape but breaks the match.
        let (id, secret) = id_and_secret_of(&first);
        let replacement = if secret.ends_with('A') { 'B' } else { 'A' };
        let tampered = format!(
            "{API_KEY_PREFIX}{id}_{}{replacement}",
            &secret[..secret.len() - 1]
        );
        let altered = match ApiKey::parse(&tampered) {
            Ok(altered) => altered,
            Err(_) => panic!("tampered text still has a valid shape"),
        };
        assert!(!stored.verify(&altered));
    }

    #[test]
    fn parse_rejects_wrong_shapes() {
        let (key, _) = ApiKey::generate();
        let (id, secret) = id_and_secret_of(&key);
        let cases = vec![
            String::new(),
            API_KEY_PREFIX.to_owned(),
            format!("{API_KEY_PREFIX}{id}{secret}"),
            format!("xpk_{id}_{secret}"),
            format!("{API_KEY_PREFIX}ABCDEFGHIJKL_{secret}"),
            format!("{API_KEY_PREFIX}{id}_short"),
            format!("{API_KEY_PREFIX}{id}_"),
        ];
        for text in cases {
            match ApiKey::parse(&text) {
                Err(AuthError::ApiKeyMalformed) => {}
                other => panic!("expected ApiKeyMalformed, got {other:?}"),
            }
        }
    }

    #[test]
    fn debug_shows_the_key_id_only() {
        let (key, stored) = ApiKey::generate();
        let text = key.text();
        let rendered = format!("{key:?}");
        assert!(rendered.contains(key.key_id()));
        assert!(rendered.contains("***"));
        assert!(!rendered.contains(&text));
        assert!(format!("{stored:?}").contains("***"));
    }
}
