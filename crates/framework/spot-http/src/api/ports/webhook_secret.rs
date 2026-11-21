//! The signing-secret seam of `PUT|GET /api/v1/webhooks/{name}/signing-secret` (WP-INT-01.T5):
//! write-only by design. A webhook's signing secret can be set (a supplied value, or one the
//! server generates and shows once) and its **fingerprint** read back; the value itself never
//! travels out of the store except to the delivery handler that signs with it.
//!
//! `spot-http` does not depend on the document engine or the vault. It defines
//! [`WebhookSecretPort`] in the shapes the router needs, and the host writes the adapter.
//! Everything that decides is the port's: who the caller is (from `ctx.credential` on
//! `ctx.site`), whether that caller may write the `webhook` document **and** the `secret`
//! protection group, and whether the webhook exists. The routes pass **no user and no role**.
//!
//! # No enumeration
//!
//! A caller who may not write the secret group is answered [`WebhookSecretError::Refused`]
//! whether or not the webhook exists: the port checks the right before it looks the webhook up.
//!
//! # Secret text
//!
//! A value on its way in or out is a [`SigningSecretText`]: its `Debug` is redacted, it has no
//! `Display`, `Clone` or serde, and its bytes are overwritten when it drops. A type that holds
//! one derives `Debug` safely because of that.

use std::fmt;

use sha2::{Digest, Sha256};

use super::PortKey;
use crate::api::service::{ApiError, BoxFuture, RequestCtx};

/// The port key of [`WebhookSecretPort`].
pub const KEY: PortKey = PortKey::of::<dyn WebhookSecretPort>();

/// The shortest supplied signing secret, in bytes.
pub const MIN_SECRET_BYTES: usize = 16;

/// The longest supplied signing secret, in bytes.
pub const MAX_SECRET_BYTES: usize = 256;

/// The error codes of the signing-secret path (see `catalogue/webhook_secret.toml`). They ride
/// in `details.reason` of the registry error the route answers with, and in the audit line.
pub mod codes {
    /// The caller may not write the webhook's signing secret (the same answer whether or not
    /// the webhook exists).
    pub const REFUSED: &str = "webhook_secret.refused";
    /// The site has no secrets vault, so no signing secret can be written or read.
    pub const NOT_CONFIGURED: &str = "webhook_secret.not_configured";
    /// The webhook does not exist (answered only to a caller who may write secrets).
    pub const NOT_FOUND: &str = "webhook_secret.not_found";
    /// A supplied secret is shorter than [`MIN_SECRET_BYTES`](super::MIN_SECRET_BYTES).
    pub const TOO_SHORT: &str = "webhook_secret.too_short";
    /// A supplied secret is longer than [`MAX_SECRET_BYTES`](super::MAX_SECRET_BYTES).
    pub const TOO_LONG: &str = "webhook_secret.too_long";
    /// The body names both `secret` and `generate`, or neither, or something else.
    pub const BAD_REQUEST: &str = "webhook_secret.bad_request";
    /// The store could not be reached; nothing was written.
    pub const UNAVAILABLE: &str = "webhook_secret.unavailable";
}

/// A signing secret in transit. Redacted `Debug`, no `Display`, bytes overwritten on drop.
pub struct SigningSecretText(String);

impl SigningSecretText {
    /// Wraps `text`.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The text, for the store that seals it and for the one response that shows a generated
    /// value. Never for a log line.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The length in bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the text is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SigningSecretText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningSecretText(<redacted>)")
    }
}

impl Drop for SigningSecretText {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.fill(0);
        std::hint::black_box(&bytes);
    }
}

/// What a `PUT` asks for.
#[derive(Debug)]
pub enum SigningSecretIn {
    /// Set this value (already checked by the route for shape; the port checks its length).
    Supplied(SigningSecretText),
    /// Generate a random value and show it once.
    Generate,
}

/// What a `PUT` answers. `generated` is `Some` only for [`SigningSecretIn::Generate`]; a
/// supplied secret is never in the answer.
#[derive(Debug)]
pub struct SigningSecretOut {
    /// The vault's key version the value was sealed under.
    pub key_version: u32,
    /// The short non-reversible fingerprint of the stored value.
    pub fingerprint: String,
    /// The generated value, shown once.
    pub generated: Option<SigningSecretText>,
}

/// What a `GET` answers: the fingerprint and key version, never the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigningSecretInfo {
    /// Whether the webhook has a signing secret at all.
    pub set: bool,
    /// The vault's key version, 0 when the secret is not set.
    pub key_version: u32,
    /// The fingerprint, empty when the secret is not set.
    pub fingerprint: String,
}

/// Why the port did not answer.
#[derive(Debug)]
pub enum WebhookSecretError {
    /// The caller may not do this (also the answer for a webhook that does not exist, to a
    /// caller who may not write secrets): `403 permission.denied`.
    Refused,
    /// The webhook does not exist, to a caller who may write secrets: `404 document.not_found`.
    NotFound,
    /// The site has no vault: `501 service.not_configured`.
    NotConfigured,
    /// An error to answer with as it is: a registry error such as a malformed request
    /// (`400 api.invalid_request`) or the sign-in layer's `401`. It carries no secret text.
    Api(ApiError),
    /// Storage was unreachable; nothing was written: `503 server.unavailable`.
    Unavailable,
    /// Anything else: `500 http.internal`, with no detail.
    Failed,
}

/// The signing-secret store of one site, for the signed-in caller.
pub trait WebhookSecretPort: Send + Sync {
    /// Sets (creates or rotates) the signing secret of `webhook`.
    fn put<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        webhook: &'a str,
        input: SigningSecretIn,
    ) -> BoxFuture<'a, Result<SigningSecretOut, WebhookSecretError>>;

    /// The fingerprint and key version of the signing secret of `webhook`.
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        webhook: &'a str,
    ) -> BoxFuture<'a, Result<SigningSecretInfo, WebhookSecretError>>;
}

/// The short non-reversible fingerprint of a signing secret: twelve hex characters of
/// SHA-256 over a domain tag, the site, the webhook name and the value.
///
/// It is for display ("is this the secret I think it is?"). The site and the webhook are part
/// of the input, so two webhooks holding one value show different fingerprints and a
/// fingerprint tells nothing across tenants. Only a caller who may already rotate the secret
/// ever sees it.
pub fn secret_fingerprint(site: &str, webhook: &str, secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"spot/webhook-signing-secret-fingerprint/v1");
    for part in [site, webhook] {
        hasher.update([0u8]);
        hasher.update(part.as_bytes());
    }
    hasher.update([0u8]);
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(12);
    for byte in digest.iter().take(6) {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_debug_of_a_secret_text_shows_no_value() {
        let secret = SigningSecretText::new("whsec-the-canary-value-0123456789");
        let shown = format!("{secret:?}");
        assert!(!shown.contains("canary"), "{shown}");
        let held = SigningSecretOut {
            key_version: 1,
            fingerprint: "abc".to_owned(),
            generated: Some(secret),
        };
        assert!(!format!("{held:?}").contains("canary"));
        let input = SigningSecretIn::Supplied(SigningSecretText::new("whsec-another-canary-value"));
        assert!(!format!("{input:?}").contains("canary"));
    }

    #[test]
    fn a_fingerprint_is_short_stable_and_scoped() {
        let a = secret_fingerprint("acme.test", "orders", "value-value-value-1");
        assert_eq!(a.len(), 12);
        assert!(a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            a,
            secret_fingerprint("acme.test", "orders", "value-value-value-1")
        );
        assert_ne!(
            a,
            secret_fingerprint("acme.test", "orders", "value-value-value-2")
        );
        assert_ne!(
            a,
            secret_fingerprint("other.test", "orders", "value-value-value-1")
        );
        assert_ne!(
            a,
            secret_fingerprint("acme.test", "invoices", "value-value-value-1")
        );
        assert!(!a.contains("value"));
    }
}
