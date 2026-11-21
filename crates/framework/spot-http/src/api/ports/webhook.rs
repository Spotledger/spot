//! The incoming-webhook seam of `POST /api/v1/process/hook/{trigger}` (`process_webhook`,
//! native API 0.12.0, WP-BPM-02.T5).
//!
//! The route is the one **public** route of the process group: no bearer, no cookie. The caller
//! proves itself by the `X-Spot-Signature` header over the **raw body**, so the port gets the
//! bytes exactly as they arrived and does two things, in this order:
//!
//! 1. verifies the signature (the scheme of `spot_webhooks::signing`) and the delivery, **before
//!    the body is parsed**: a body whose signature has not verified is never parsed, and no JSON
//!    error is ever produced for it;
//! 2. parses the body (a JSON object) and hands it to the trigger.
//!
//! The route does not parse the body and never sees the secret. Every cause of refusal (a bad or
//! missing signature, a stale timestamp, an unknown or disabled trigger, a body that is not a JSON
//! object, a replayed delivery whose first delivery was rejected) is the **one**
//! [`WebhookRejected`]: the endpoint reveals neither which triggers exist nor why a delivery
//! failed, and the port must take the same path for every cause so no timing tells them apart.
//! An accepted delivery that matches no condition, or replays one already accepted, is `Ok(())`
//! too: the answer never says whether a run started.
//!
//! The port is handed the request context of the resolved site (no credential: the route has
//! none) so the trigger is looked up in the right tenant and the client address
//! (`ctx.peer`) reaches the audit. The port audits every delivery, a rejection included (the
//! trigger name as received, the client address, the delivery id and the rule that refused) and
//! never stores the body of a rejected one.

use std::collections::HashSet;
use std::sync::Mutex;

use axum::body::Bytes;

use super::PortKey;
use crate::api::service::{BoxFuture, RequestCtx};

/// The port key of [`WebhookPort`].
pub const KEY: PortKey = PortKey::of::<dyn WebhookPort>();

/// The one refusal of a delivery. It carries nothing: not the cause, not the trigger, not a
/// byte of the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WebhookRejected;

impl std::fmt::Display for WebhookRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the webhook delivery was rejected")
    }
}

impl std::error::Error for WebhookRejected {}

/// The receiver of incoming webhook deliveries of one site.
pub trait WebhookPort: Send + Sync {
    /// Receives one delivery for `trigger` (the path segment as received, already checked to be
    /// `^[a-z0-9][a-z0-9_-]{0,63}$`), with the sender's `delivery_id` when it sent one, the
    /// `signature` header value (empty when the header was missing or unusable) and the raw
    /// `body` (at most 256 KiB).
    ///
    /// `Ok(())` is an accepted delivery; it does not say whether a run started. Every refusal is
    /// the one [`WebhookRejected`].
    fn receive<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        trigger: &'a str,
        delivery_id: Option<&'a str>,
        signature: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<(), WebhookRejected>>;
}

/// One delivery the fake accepted: what a test can check, never the signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivery {
    /// The site it was received for.
    pub site: String,
    /// The trigger.
    pub trigger: String,
    /// The delivery id: the sender's, or `sha256:<hex>` of the body when it sent none.
    pub id: String,
    /// The accepted body, as the route handed it over.
    pub body: Vec<u8>,
}

#[derive(Default)]
struct State {
    /// `(site, trigger)` -> the signature that verifies.
    triggers: Vec<(String, String, String)>,
    seen: HashSet<(String, String, String)>,
    accepted: Vec<Delivery>,
    refused: usize,
    calls: usize,
}

/// The in-memory [`WebhookPort`]. A trigger is registered with the exact signature string that
/// verifies (the fake does not sign anything: the real scheme belongs to `spot_webhooks`). A
/// delivery is accepted when the trigger exists on the call's site, the signature equals the
/// registered one, the body is a JSON object, and the `(site, trigger, delivery id)` was not
/// accepted before (a replay is accepted again and starts nothing).
#[derive(Default)]
pub struct MemoryWebhookPort {
    state: Mutex<State>,
}

impl MemoryWebhookPort {
    /// An empty fake: every delivery is rejected until a trigger is registered.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The mutex guards plain collections; a panic elsewhere cannot leave them inconsistent.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Registers `trigger` on `site`; a delivery must carry exactly `signature`.
    pub fn register(&self, site: &str, trigger: &str, signature: &str) {
        self.lock()
            .triggers
            .push((site.to_owned(), trigger.to_owned(), signature.to_owned()));
    }

    /// The deliveries accepted so far, oldest first. A replay appears once only.
    pub fn accepted(&self) -> Vec<Delivery> {
        self.lock().accepted.clone()
    }

    /// How many deliveries were refused.
    pub fn refused(&self) -> usize {
        self.lock().refused
    }

    /// How many times the port was called at all (a route that refuses before the port leaves
    /// this at zero).
    pub fn calls(&self) -> usize {
        self.lock().calls
    }
}

/// The delivery id of a body when the sender named none: SHA-256 of the body, hex.
fn body_digest(body: &[u8]) -> String {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    let digest = sha2::Sha256::digest(body);
    let mut out = String::from("sha256:");
    for byte in digest {
        // Writing a hex pair into a String cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl WebhookPort for MemoryWebhookPort {
    fn receive<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        trigger: &'a str,
        delivery_id: Option<&'a str>,
        signature: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, Result<(), WebhookRejected>> {
        Box::pin(async move {
            let mut state = self.lock();
            state.calls += 1;
            let site = ctx.site.as_str().to_owned();
            // Signature first, parse second: a body is parsed only for a verified delivery.
            let verified = state
                .triggers
                .iter()
                .any(|(s, t, sig)| *s == site && t == trigger && sig == signature);
            let is_object = verified
                && matches!(
                    serde_json::from_slice::<serde_json::Value>(&body),
                    Ok(serde_json::Value::Object(_))
                );
            if !is_object {
                state.refused += 1;
                return Err(WebhookRejected);
            }
            let id = match delivery_id {
                Some(id) => id.to_owned(),
                None => body_digest(&body),
            };
            let key = (site.clone(), trigger.to_owned(), id.clone());
            if state.seen.insert(key) {
                state.accepted.push(Delivery {
                    site,
                    trigger: trigger.to_owned(),
                    id,
                    body: body.to_vec(),
                });
            }
            Ok(())
        })
    }
}
