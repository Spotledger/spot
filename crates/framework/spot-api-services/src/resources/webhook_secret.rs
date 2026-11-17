//! The engine-side implementation of the signing-secret port (WP-INT-01.T5): what stands between
//! the `spot-http` routes and the hosting module that holds the vault.
//!
//! `spot-http` defines [`WebhookSecretPort`]. This file implements it over [`WebhookSecretOps`],
//! the two operations the hosting module (`spot-cli`, which holds the engine, the vault and the
//! capability) performs, so that this crate needs no secrets dependency and holds no key:
//!
//! - **generation.** A `{"generate": true}` request is answered here: 32 bytes from the injected
//!   [`SecretEntropy`] source, base64url without padding (43 characters). The generated text is
//!   returned to the route once, in `SigningSecretOut::generated`, and nowhere else; this file
//!   never logs it.
//! - **length.** A supplied secret is checked again (16 to 256 bytes), because the port is a
//!   trait and may be called by something other than the route.
//! - **no memory of values.** Nothing here stores, caches or formats a secret. The buffers the
//!   entropy is drawn into are overwritten before they are dropped.
//!
//! Who may write, whether the webhook exists and the audit of the write are the operations': they
//! run as the caller the request names and answer a refusal the same way whether or not the webhook
//! exists.

use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use spot_http::api::ports::webhook_secret::{
    MAX_SECRET_BYTES, MIN_SECRET_BYTES, SigningSecretIn, SigningSecretInfo, SigningSecretOut,
    SigningSecretText, WebhookSecretError, WebhookSecretPort,
};
use spot_http::api::service::{BoxFuture, RequestCtx};

/// The raw bytes of a generated secret (256 bits).
pub const GENERATED_BYTES: usize = 32;

/// Where the random bytes of a generated secret come from. The host injects the operating
/// system's source; a test injects a fixed one.
pub trait SecretEntropy: Send + Sync {
    /// Fills `buf` with random bytes; `false` when the source failed (nothing is then used).
    fn fill(&self, buf: &mut [u8]) -> bool;
}

/// What a store knows about a signing secret: never the value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSigningSecret {
    /// The vault key version.
    pub key_version: u32,
    /// The short non-reversible fingerprint.
    pub fingerprint: String,
}

/// The two operations the hosting module performs, as the caller the request names.
pub trait WebhookSecretOps: Send + Sync {
    /// Seals `secret` into the webhook's secret field (create or rotate), in the same transaction
    /// as the record, after checking the caller's right. Returns the new key version and
    /// fingerprint.
    fn set<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        webhook: &'a str,
        secret: &'a SigningSecretText,
    ) -> BoxFuture<'a, Result<StoredSigningSecret, WebhookSecretError>>;

    /// The fingerprint and key version of the webhook's secret, or `None` when it has none.
    fn describe<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        webhook: &'a str,
    ) -> BoxFuture<'a, Result<Option<StoredSigningSecret>, WebhookSecretError>>;
}

/// The signing-secret port over a hosting module's [`WebhookSecretOps`].
pub struct EngineWebhookSecrets {
    ops: Arc<dyn WebhookSecretOps>,
    entropy: Arc<dyn SecretEntropy>,
}

impl EngineWebhookSecrets {
    /// The port over `ops`, generating from `entropy`.
    pub fn new(ops: Arc<dyn WebhookSecretOps>, entropy: Arc<dyn SecretEntropy>) -> Self {
        Self { ops, entropy }
    }

    /// A fresh secret: [`GENERATED_BYTES`] of entropy as base64url. `None` when the source fails.
    fn generate(&self) -> Option<SigningSecretText> {
        let mut raw = [0u8; GENERATED_BYTES];
        let ok = self.entropy.fill(&mut raw);
        // A source that did not fill the buffer must not yield a secret made of zeros.
        let text = (ok && raw.iter().any(|b| *b != 0)).then(|| URL_SAFE_NO_PAD.encode(raw));
        raw.fill(0);
        std::hint::black_box(&raw);
        text.map(SigningSecretText::new)
    }
}

fn length_error(len: usize) -> Option<WebhookSecretError> {
    use spot_http::api::ports::webhook_secret::codes;
    use spot_http::api::registry::Code;
    use spot_http::api::service::ApiError;
    let reason = if len < MIN_SECRET_BYTES {
        codes::TOO_SHORT
    } else if len > MAX_SECRET_BYTES {
        codes::TOO_LONG
    } else {
        return None;
    };
    Some(WebhookSecretError::Api(
        ApiError::new(
            Code::ApiInvalidRequest,
            "A signing secret must be between 16 and 256 bytes.",
        )
        .detail("reason", serde_json::Value::from(reason)),
    ))
}

impl WebhookSecretPort for EngineWebhookSecrets {
    fn put<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        webhook: &'a str,
        input: SigningSecretIn,
    ) -> BoxFuture<'a, Result<SigningSecretOut, WebhookSecretError>> {
        Box::pin(async move {
            let (secret, shown) = match input {
                SigningSecretIn::Supplied(text) => {
                    if let Some(error) = length_error(text.len()) {
                        return Err(error);
                    }
                    (text, false)
                }
                SigningSecretIn::Generate => match self.generate() {
                    Some(text) => (text, true),
                    None => return Err(WebhookSecretError::Failed),
                },
            };
            let stored = self.ops.set(ctx, webhook, &secret).await?;
            Ok(SigningSecretOut {
                key_version: stored.key_version,
                fingerprint: stored.fingerprint,
                // The generated value leaves here, once; a supplied one is dropped (zeroed).
                generated: shown.then_some(secret),
            })
        })
    }

    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        webhook: &'a str,
    ) -> BoxFuture<'a, Result<SigningSecretInfo, WebhookSecretError>> {
        Box::pin(async move {
            Ok(match self.ops.describe(ctx, webhook).await? {
                Some(stored) => SigningSecretInfo {
                    set: true,
                    key_version: stored.key_version,
                    fingerprint: stored.fingerprint,
                },
                None => SigningSecretInfo {
                    set: false,
                    key_version: 0,
                    fingerprint: String::new(),
                },
            })
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Mutex;

    use spot_core::SiteId;
    use spot_http::api::service::Credential;
    use spot_http::request::RequestId;

    use super::*;

    struct Fixed(u8);

    impl SecretEntropy for Fixed {
        fn fill(&self, buf: &mut [u8]) -> bool {
            buf.fill(self.0);
            true
        }
    }

    struct Broken;

    impl SecretEntropy for Broken {
        fn fill(&self, _buf: &mut [u8]) -> bool {
            false
        }
    }

    /// An ops double that records the length of what it was asked to seal (never the value).
    #[derive(Default)]
    struct Ops {
        lengths: Mutex<Vec<usize>>,
        refuse: bool,
    }

    impl WebhookSecretOps for Ops {
        fn set<'a>(
            &'a self,
            _ctx: &'a RequestCtx,
            _webhook: &'a str,
            secret: &'a SigningSecretText,
        ) -> BoxFuture<'a, Result<StoredSigningSecret, WebhookSecretError>> {
            Box::pin(async move {
                if self.refuse {
                    return Err(WebhookSecretError::Refused);
                }
                self.lengths.lock().unwrap().push(secret.len());
                Ok(StoredSigningSecret {
                    key_version: 1,
                    fingerprint: "0123456789ab".to_owned(),
                })
            })
        }

        fn describe<'a>(
            &'a self,
            _ctx: &'a RequestCtx,
            _webhook: &'a str,
        ) -> BoxFuture<'a, Result<Option<StoredSigningSecret>, WebhookSecretError>> {
            Box::pin(async { Ok(None) })
        }
    }

    fn ctx() -> RequestCtx {
        RequestCtx {
            site: SiteId::new("acme.test").unwrap(),
            credential: Some(Credential::new("token")),
            idempotency_key: None,
            language: None,
            request_id: RequestId::generate(),
            peer: None,
        }
    }

    fn port(ops: Arc<Ops>, entropy: Arc<dyn SecretEntropy>) -> EngineWebhookSecrets {
        EngineWebhookSecrets::new(ops, entropy)
    }

    #[tokio::test]
    async fn a_generated_secret_is_43_base64url_characters_and_shown_once() {
        let ops = Arc::new(Ops::default());
        let port = port(ops.clone(), Arc::new(Fixed(7)));
        let out = port
            .put(&ctx(), "orders", SigningSecretIn::Generate)
            .await
            .unwrap();
        let generated = out.generated.as_ref().expect("a generated value");
        assert_eq!(generated.len(), 43);
        assert!(
            generated
                .expose()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert_eq!(ops.lengths.lock().unwrap().as_slice(), &[43]);
        assert!(!format!("{out:?}").contains(generated.expose()));
    }

    #[tokio::test]
    async fn a_supplied_secret_is_never_in_the_answer() {
        let ops = Arc::new(Ops::default());
        let port = port(ops.clone(), Arc::new(Fixed(7)));
        let out = port
            .put(
                &ctx(),
                "orders",
                SigningSecretIn::Supplied(SigningSecretText::new("a-supplied-secret-0123456789")),
            )
            .await
            .unwrap();
        assert!(out.generated.is_none());
        assert_eq!(out.key_version, 1);
    }

    #[tokio::test]
    async fn the_length_bounds_are_enforced_here_too() {
        let ops = Arc::new(Ops::default());
        let port = port(ops.clone(), Arc::new(Fixed(7)));
        for text in [
            "x".repeat(MIN_SECRET_BYTES - 1),
            "x".repeat(MAX_SECRET_BYTES + 1),
        ] {
            let refused = port
                .put(
                    &ctx(),
                    "orders",
                    SigningSecretIn::Supplied(SigningSecretText::new(text)),
                )
                .await;
            assert!(matches!(refused, Err(WebhookSecretError::Api(_))));
        }
        for text in ["x".repeat(MIN_SECRET_BYTES), "x".repeat(MAX_SECRET_BYTES)] {
            let accepted = port
                .put(
                    &ctx(),
                    "orders",
                    SigningSecretIn::Supplied(SigningSecretText::new(text)),
                )
                .await;
            assert!(accepted.is_ok());
        }
        assert_eq!(
            ops.lengths.lock().unwrap().len(),
            2,
            "a refused one reached no store"
        );
    }

    #[tokio::test]
    async fn a_failed_entropy_source_yields_no_secret_and_no_write() {
        let ops = Arc::new(Ops::default());
        let port = port(ops.clone(), Arc::new(Broken));
        let out = port.put(&ctx(), "orders", SigningSecretIn::Generate).await;
        assert!(matches!(out, Err(WebhookSecretError::Failed)));
        let zeros = EngineWebhookSecrets::new(ops.clone(), Arc::new(Fixed(0)));
        let out = zeros.put(&ctx(), "orders", SigningSecretIn::Generate).await;
        assert!(
            matches!(out, Err(WebhookSecretError::Failed)),
            "an all-zero draw is no secret"
        );
        assert!(ops.lengths.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_refusal_of_the_store_is_passed_on_unchanged() {
        let ops = Arc::new(Ops {
            refuse: true,
            ..Ops::default()
        });
        let port = port(ops, Arc::new(Fixed(7)));
        let out = port.put(&ctx(), "orders", SigningSecretIn::Generate).await;
        assert!(matches!(out, Err(WebhookSecretError::Refused)));
    }

    #[tokio::test]
    async fn an_unset_secret_reads_back_as_not_set() {
        let port = port(Arc::new(Ops::default()), Arc::new(Fixed(7)));
        let info = port.get(&ctx(), "orders").await.unwrap();
        assert_eq!(
            info,
            SigningSecretInfo {
                set: false,
                key_version: 0,
                fingerprint: String::new()
            }
        );
    }
}
