//! What a validated ID token tells us about the person who signed in.
//!
//! Only the mapped fields and a hash of the raw claim set are kept; the raw claims are never
//! stored (ADR-070 section 7). A hash lets an operator confirm two events describe the same
//! token without keeping the token.

use serde::{Deserialize, Serialize};

use crate::provider::ProviderId;

/// A person as an identity provider describes them, after full validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SsoIdentity {
    /// The tenant provider this came from.
    pub provider: ProviderId,
    /// The issuer (`iss`), equal to the configured and discovered issuer.
    pub issuer: String,
    /// The provider's stable subject (`sub`).
    pub subject: String,
    /// The e-mail, when the provider supplied one.
    pub email: Option<String>,
    /// Whether the provider marked the e-mail verified.
    pub email_verified: bool,
    /// The display name, when supplied.
    pub display_name: Option<String>,
    /// The group memberships, when supplied.
    pub groups: Vec<String>,
    /// The authentication context class, when supplied.
    pub acr: Option<String>,
    /// The authentication methods, when supplied.
    pub amr: Vec<String>,
    /// When the authentication happened, in unix seconds, when supplied.
    pub auth_time: Option<i64>,
    /// A Base64url SHA-256 hash of the raw claim set; the claims themselves are not kept.
    pub raw_claims_hash: String,
}

impl SsoIdentity {
    /// The `(issuer, subject)` key that linked identities are stored under.
    #[must_use]
    pub fn key(&self) -> spot_iam_directory::IdentityKey {
        spot_iam_directory::IdentityKey {
            issuer: self.issuer.clone(),
            subject: self.subject.clone(),
        }
    }
}
