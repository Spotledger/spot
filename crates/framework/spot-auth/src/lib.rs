//! Credential primitives for the Spotledger Platform: Argon2id password hashing, the
//! password policy, TOTP, recovery codes, opaque tokens and API keys.
//!
//! No HTTP, no storage, no user model: every function is pure, or takes a
//! [`spot_core::Clock`](spot_core::clock::Clock). Later work packages build session
//! persistence and login flows on top of this crate; this crate is where the cryptography
//! itself lives, so those later tasks cannot get it wrong.
//!
//! # Secrets never leak
//!
//! Every type holding secret material — [`password::Password`], [`password::PasswordHash`],
//! [`totp::TotpSecret`], [`token::OpaqueToken`], [`recovery::RecoveryCode`],
//! [`api_key::ApiKey`] and every digest type — follows three rules: no `Display`
//! implementation, so a stray `{}` can never print the secret; `Debug` output is the
//! redacted literal `Secret(***)` (an API key also shows its public key id, which is
//! designed to be quotable in logs and support tickets); and the secret is zeroized when
//! the value is dropped. Comparisons of secret material are constant time (`subtle`).
//!
//! # Errors
//!
//! Every check returns [`error::AuthError`] with a stable machine-readable code, a
//! plain-language message, and no echo of user input.

pub mod api_key;
pub mod error;
pub mod password;
pub mod policy;
pub mod recovery;
pub mod token;
pub mod totp;

mod util;

pub use api_key::{ApiKey, StoredApiKey};
pub use error::AuthError;
pub use password::{HashParams, Password, PasswordHash, Verified, hash_password, verify_password};
pub use policy::{PasswordPolicy, PolicyViolation};
pub use recovery::{RecoveryCode, StoredDigest, generate_recovery_codes, verify_recovery_code};
pub use token::{OpaqueToken, TokenDigest};
pub use totp::{TotpAlgorithm, TotpConfig, TotpDigits, TotpSecret, base32_decode, base32_encode};

#[cfg(test)]
mod tests {
    use crate::api_key::ApiKey;
    use crate::password::{HashParams, Password, hash_password};
    use crate::recovery::generate_recovery_codes;
    use crate::token::OpaqueToken;
    use crate::totp::TotpSecret;

    #[test]
    fn debug_output_redacts_every_secret_type() {
        let password = Password::new("correct horse battery staple");
        let hash = hash_password(&password, &HashParams::for_tests());
        let phc = hash.phc_string().to_owned();
        let totp_secret = TotpSecret::generate();
        let base32_secret = totp_secret.to_base32();
        let token = OpaqueToken::generate();
        let (api_key, _stored) = ApiKey::generate();
        let codes = generate_recovery_codes(3);

        let rendered = format!("{password:?}{hash:?}{totp_secret:?}{token:?}{api_key:?}{codes:?}");

        assert!(rendered.contains("***"));
        assert!(!rendered.contains(password.as_str()));
        assert!(!rendered.contains(&phc));
        assert!(!rendered.contains(&base32_secret));
        assert!(!rendered.contains(token.as_str()));
        // The API key's Debug output deliberately shows its public key id; the secret stays hidden.
        let api_key_debug = format!("{api_key:?}");
        assert!(api_key_debug.contains(api_key.key_id()));
        assert!(api_key_debug.contains("***"));
        assert!(!rendered.contains(api_key.text().as_str()));
        for code in &codes {
            assert!(!rendered.contains(code.display().as_str()));
        }
    }
}
