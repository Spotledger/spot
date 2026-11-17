//! External peers, hosted (WP-CTL-11.T4, ADR-058 rule 12; ADR-080): the optional tenant-host
//! service that runs the external carrier of `spot-link-external` and mounts the peers' inbound
//! webhook, `POST /api/v1/link/inbound/{peer}`. **Off by default**: disabled, there is no task, no route
//! and no network call.
//!
//! # Outbound
//!
//! A periodic pass hands what each peer's outbox releases to the host's [`PeerChannel`], which
//! delivers through the **egress policy** (allow-listed hosts only: a connector call or a signed
//! webhook `POST` through `spot-webhooks`' `Egress`). The credential is a reference
//! (`CredentialRef`) resolved from the vault for one call and dropped; this module never holds a
//! secret. A refused or failed delivery is audited and the message waits, as the exchange does.
//! While the side-effect hold is on (or unknown) nothing leaves.
//!
//! # Inbound
//!
//! [`ExternalInbound`] answers the route's [`LinkInboundPort`], in this order:
//!
//! 1. per-peer **rate limit**, charged to the name in the path before the peer is looked up, then
//!    the peer's **source-address allow-list** (when configured), then the **body cap**;
//! 2. the HMAC signature over the raw body and a timestamp window of at most five minutes
//!    ([`MAX_WINDOW_SECS`]), checked before anything is parsed;
//! 3. the **replay check**: the signature of an accepted request is claimed in a durable
//!    [`NonceStore`] ([`FileNonceStore`] survives a restart); a second use is refused;
//! 4. the tenant link's own receive path (`ExternalReceiver`): grant limits, the field list, schema
//!    range, deduplication, ordering and audit are the exchange's. A message is created as the
//!    **peer's link principal**: the receiver stores through the inbox under the link's grant and
//!    never as a system actor.
//!
//! Every refusal is the one `401` of the route; an unknown peer and a bad signature are
//! indistinguishable. Secrets are never logged or echoed.
//!
//! Nothing here reads the wall clock (the host's clock supplies every instant), draws randomness,
//! or uses floats.

use std::collections::HashMap;
use std::io::Write as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use serde::Deserialize;
use spot_connectors::CredentialVault;
use spot_http::api::ports::link_inbound::{
    DEFAULT_MAX_BODY_BYTES, InboundCall, LinkInboundPort, LinkInboundRefusal,
};
use spot_http::api::service::BoxFuture;
use spot_link_external::{
    ExternalPeer, ExternalReceiver, ExternalSender, PeerChannel, ReceiverPorts, RefusalCounter,
    SignatureScheme, verify_inbound,
};
use spot_link_transport::{LinkHost, RateConfig, RateLimiter};
use spot_tenant_link::{IdentitySigner, LinkId, LinkState};
use tokio::sync::watch;

use crate::services::link_exchange::SendHold;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the links (1400) and the exchange runner (1500).
pub const ORDER: u32 = 1600;

/// The widest timestamp window this service accepts, whatever the peer is configured with.
pub const MAX_WINDOW_SECS: i64 = 300;

/// `[services.link_external]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the service runs.
    pub enabled: bool,
    /// How often the outbound pass runs.
    pub tick_ms: u64,
    /// The most messages one pass hands to one peer's channel. At least 1.
    pub pump_batch: usize,
    /// Inbound requests per peer name and minute. At least 1.
    pub per_peer_per_minute: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            tick_ms: 1_000,
            pump_batch: 16,
            per_peer_per_minute: 120,
        }
    }
}

impl Settings {
    /// Checks the ranges.
    ///
    /// # Errors
    /// A message naming the setting, never a value.
    pub fn validate(&self) -> Result<()> {
        let bad = |name: &str| Error(format!("services.link_external.{name} must be at least 1"));
        if self.tick_ms == 0 {
            return Err(bad("tick_ms"));
        }
        if self.pump_batch == 0 {
            return Err(bad("pump_batch"));
        }
        if self.per_peer_per_minute == 0 {
            return Err(bad("per_peer_per_minute"));
        }
        Ok(())
    }

    fn rate(&self) -> RateConfig {
        RateConfig {
            burst: self.per_peer_per_minute,
            per_second: (self.per_peer_per_minute / 60).max(1),
            max_keys: 1_024,
        }
    }
}

/// One external peer as the host configured it.
pub struct ConfiguredPeer {
    /// The peer: credential reference, outbound target, inbound spec, format and caps.
    pub peer: ExternalPeer,
    /// The link to this peer.
    pub link: LinkId,
    /// The bridge identity: the key this platform holds for the external party, whose public half
    /// the link pins for it.
    pub bridge: Arc<dyn IdentitySigner + Send + Sync>,
    /// Source addresses allowed to post inbound; empty means any address.
    pub allowed_sources: Vec<IpAddr>,
}

/// The configured external peers.
pub trait ExternalPeers: Send + Sync {
    /// The peers, each with its link and bridge.
    fn peers(&self) -> Vec<Arc<ConfiguredPeer>>;
}

/// A nonce store could not answer: the request is refused (fail closed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonceUnavailable;

/// Remembers the signatures of accepted requests until their window ends. It must be **durable**:
/// a restart that forgot them would accept a replay inside the window.
pub trait NonceStore: Send + Sync {
    /// Claims `nonce` for `peer` until `expires_ms`. `Ok(true)` the first time, `Ok(false)` when it
    /// was already claimed and has not expired.
    ///
    /// # Errors
    /// [`NonceUnavailable`] when the claim could not be made durable.
    fn claim(
        &self,
        peer: &str,
        nonce: &str,
        now_ms: u64,
        expires_ms: u64,
    ) -> std::result::Result<bool, NonceUnavailable>;
}

/// A [`NonceStore`] over an append-only file: each claim is written and synced before it is
/// answered, and the file is read back on open (expired entries dropped), so claims survive a
/// restart. Bounded: a full table refuses new claims instead of growing.
pub struct FileNonceStore {
    path: PathBuf,
    max_entries: usize,
    claimed: Mutex<HashMap<String, u64>>,
}

impl FileNonceStore {
    /// Opens (creating, mode 0600) the store at `path`, keeping claims that are still live at
    /// `now_ms`.
    ///
    /// # Errors
    /// The file cannot be read or written.
    pub fn open(path: &Path, max_entries: usize, now_ms: u64) -> std::io::Result<Self> {
        let mut claimed = HashMap::new();
        match std::fs::read_to_string(path) {
            Ok(text) => {
                for line in text.lines() {
                    let mut parts = line.splitn(3, '\t');
                    if let (Some(expires), Some(peer), Some(nonce)) =
                        (parts.next(), parts.next(), parts.next())
                        && let Ok(expires) = expires.parse::<u64>()
                        && expires > now_ms
                    {
                        claimed.insert(format!("{peer}\t{nonce}"), expires);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        // Rewrite with the live claims only, so the file does not grow without bound.
        let mut live = String::new();
        for (key, expires) in &claimed {
            live.push_str(&format!("{expires}\t{key}\n"));
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, live)?;
        std::fs::rename(&tmp, path)?;
        set_private(path);
        Ok(Self {
            path: path.to_path_buf(),
            max_entries: max_entries.max(1),
            claimed: Mutex::new(claimed),
        })
    }
}

#[cfg(unix)]
fn set_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_private(_path: &Path) {}

impl NonceStore for FileNonceStore {
    fn claim(
        &self,
        peer: &str,
        nonce: &str,
        now_ms: u64,
        expires_ms: u64,
    ) -> std::result::Result<bool, NonceUnavailable> {
        if peer.contains(['\t', '\n']) || nonce.contains(['\t', '\n']) {
            return Err(NonceUnavailable);
        }
        let key = format!("{peer}\t{nonce}");
        let mut claimed = self
            .claimed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if claimed.get(&key).is_some_and(|expires| *expires > now_ms) {
            return Ok(false);
        }
        if claimed.len() >= self.max_entries {
            claimed.retain(|_, expires| *expires > now_ms);
            if claimed.len() >= self.max_entries {
                return Err(NonceUnavailable);
            }
        }
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&self.path)
            .map_err(|_| NonceUnavailable)?;
        file.write_all(format!("{expires_ms}\t{key}\n").as_bytes())
            .map_err(|_| NonceUnavailable)?;
        file.sync_data().map_err(|_| NonceUnavailable)?;
        claimed.insert(key, expires_ms);
        Ok(true)
    }
}

/// What the service needs from the rest of the host. Provide one with
/// `SiteCtx::provide::<LinkExternalPorts>`; enabled with no provider the site refuses to start.
pub struct LinkExternalPorts {
    /// The stores, clock, ids and audit of this side (the exchange's own).
    pub host: LinkHost,
    /// The configured peers.
    pub peers: Arc<dyn ExternalPeers>,
    /// How bytes reach a peer: a connector or a signed webhook, **through the egress policy**.
    pub channel: Arc<dyn PeerChannel>,
    /// Where credential references are resolved, per call.
    pub vault: Arc<dyn CredentialVault>,
    /// The durable replay store.
    pub nonces: Arc<dyn NonceStore>,
    /// The side-effect hold.
    pub hold: Arc<dyn SendHold>,
}

fn micros(now_ms: u64) -> i64 {
    i64::try_from(now_ms)
        .unwrap_or(i64::MAX / 1_000)
        .saturating_mul(1_000)
}

/// The header value that identifies one signed request: what the nonce store claims.
fn signature_material(scheme: &SignatureScheme, headers: &[(String, String)]) -> Option<String> {
    let name = match scheme {
        SignatureScheme::SpotHmacSha256 => "x-spot-signature",
        SignatureScheme::HeaderHmacSha256 {
            signature_header, ..
        } => signature_header.as_str(),
    };
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The inbound side: the port the route calls.
pub struct ExternalInbound {
    ports: Arc<LinkExternalPorts>,
    rate: RateLimiter,
    refusals: RefusalCounter,
}

impl ExternalInbound {
    /// The inbound port over `ports`.
    #[must_use]
    pub fn new(ports: Arc<LinkExternalPorts>, settings: &Settings) -> Self {
        Self {
            ports,
            rate: RateLimiter::new(settings.rate()),
            refusals: RefusalCounter::new(),
        }
    }

    fn find(&self, name: &str) -> Option<Arc<ConfiguredPeer>> {
        self.ports
            .peers
            .peers()
            .into_iter()
            .find(|candidate| candidate.peer.id.as_str() == name)
    }

    async fn handle(
        &self,
        call: InboundCall,
        body: Bytes,
    ) -> std::result::Result<(), LinkInboundRefusal> {
        let reject = LinkInboundRefusal::Rejected;
        let configured = self.find(&call.peer).ok_or(reject)?;
        let host = &self.ports.host;
        let now = host.now_ms().map_err(|_| reject)?;
        let link = host.links.get(&configured.link).map_err(|_| reject)?;
        if link.state_at(now) != LinkState::Active {
            return Err(reject);
        }
        let peer = &configured.peer;
        peer.validate_for(&link).map_err(|_| reject)?;
        let window = peer.inbound.tolerance_secs.min(MAX_WINDOW_SECS);
        // The signature first, with a secret that lives for this call only.
        let secret = self
            .ports
            .vault
            .resolve(&peer.auth, micros(now))
            .await
            .map_err(|_| reject)?;
        let verified = verify_inbound(
            &peer.inbound.signature_scheme,
            secret.expose().as_bytes(),
            &call.headers,
            &body,
            now,
            window,
        );
        drop(secret);
        verified.map_err(|_| reject)?;
        // Replay: a signature that verified is accepted once inside its window.
        let nonce =
            signature_material(&peer.inbound.signature_scheme, &call.headers).ok_or(reject)?;
        let window_ms = u64::try_from(window).unwrap_or(0).saturating_mul(2_000);
        let first = self
            .ports
            .nonces
            .claim(peer.id.as_str(), &nonce, now, now.saturating_add(window_ms))
            .map_err(|_| reject)?;
        if !first {
            return Err(reject);
        }
        let inbox = host.inbox();
        let outbox = host.outbox();
        let receiver = ExternalReceiver::new(
            peer,
            ReceiverPorts {
                inbox: &inbox,
                inbox_store: host.inbox.as_ref(),
                outbox: &outbox,
                outbox_store: host.outbox.as_ref(),
                bridge: configured.bridge.as_ref(),
                vault: self.ports.vault.as_ref(),
                lifetimes: &host.lifetimes,
                audit: host.audit.as_ref(),
                refusals: &self.refusals,
            },
        );
        let request = receiver
            .read(
                peer.inbound.path.clone(),
                call.headers.clone(),
                std::iter::once(Ok::<Vec<u8>, std::io::Error>(body.to_vec())),
            )
            .map_err(|_| reject)?;
        let response = receiver.handle(&link, &request, now).await;
        if (200..300).contains(&response.status) {
            Ok(())
        } else {
            Err(reject)
        }
    }
}

impl LinkInboundPort for ExternalInbound {
    fn admit(&self, call: &InboundCall) -> std::result::Result<usize, LinkInboundRefusal> {
        // Charged to the name in the path, known or not, before the peer is looked up.
        if self
            .rate
            .take(&call.peer, self.ports.host.clock.monotonic())
            .is_err()
        {
            return Err(LinkInboundRefusal::RateLimited);
        }
        match self.find(&call.peer) {
            Some(configured) => {
                if !configured.allowed_sources.is_empty()
                    && !call
                        .source
                        .is_some_and(|ip| configured.allowed_sources.contains(&ip))
                {
                    return Err(LinkInboundRefusal::Rejected);
                }
                Ok(usize::try_from(configured.peer.max_body_bytes)
                    .unwrap_or(DEFAULT_MAX_BODY_BYTES)
                    .max(1))
            }
            None => Ok(DEFAULT_MAX_BODY_BYTES),
        }
    }

    fn receive<'a>(
        &'a self,
        call: InboundCall,
        body: Bytes,
    ) -> BoxFuture<'a, std::result::Result<(), LinkInboundRefusal>> {
        Box::pin(async move { self.handle(call, body).await })
    }
}

/// The outbound side: a periodic pass over the configured peers.
pub struct ExternalService {
    ports: Arc<LinkExternalPorts>,
    settings: Settings,
}

impl ExternalService {
    /// A service over `ports`.
    #[must_use]
    pub fn new(ports: Arc<LinkExternalPorts>, settings: Settings) -> Self {
        Self { ports, settings }
    }

    /// One outbound pass: hands each peer's released messages to the channel. Returns the number
    /// of messages attempted. Nothing leaves while the hold is on, nor for a link that is not
    /// active (the outbox applies the grant, direction, expiry and revocation as well).
    ///
    /// # Errors
    /// A message when the clock cannot be trusted.
    pub async fn pass(&self) -> std::result::Result<usize, String> {
        if self.ports.hold.held() {
            return Ok(0);
        }
        let host = &self.ports.host;
        let now = host
            .now_ms()
            .map_err(|_| "the clock cannot be trusted".to_owned())?;
        let mut attempted = 0;
        for configured in self.ports.peers.peers() {
            let Ok(link) = host.links.get(&configured.link) else {
                continue;
            };
            if link.state_at(now) != LinkState::Active {
                continue;
            }
            let outbox = host.outbox();
            let sender = ExternalSender::new(
                &outbox,
                &configured.peer,
                self.ports.channel.as_ref(),
                configured.bridge.as_ref(),
                host.audit.as_ref(),
            );
            match sender
                .pump(&link, now, self.settings.pump_batch, "system", None)
                .await
            {
                Ok(report) => attempted += report.attempted,
                Err(error) => tracing::warn!(code = error.code(), "external link pass failed"),
            }
        }
        Ok(attempted)
    }
}

impl Service for ExternalService {
    fn name(&self) -> &'static str {
        "link_external"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.ports
                .host
                .links
                .list_for(&self.ports.host_party())
                .map(|_| ())
                .map_err(|_| "the link store could not be read".to_owned())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let tick = Duration::from_millis(self.settings.tick_ms);
            loop {
                if let Err(reason) = self.pass().await {
                    tracing::warn!(reason = %reason, "external link pass failed");
                }
                tokio::select! {
                    () = tokio::time::sleep(tick) => {}
                    _ = stop.changed() => return Ok(()),
                }
                if *stop.borrow() {
                    return Ok(());
                }
            }
        })
    }
}

impl LinkExternalPorts {
    fn host_party(&self) -> spot_tenant_link::Party {
        self.host.signer.party()
    }
}

/// Registers the service when `[services.link_external]` is enabled: provides the inbound port of
/// `POST /api/v1/link/inbound/{peer}` and adds the outbound pass.
///
/// # Errors
/// A setting out of range, or a missing [`LinkExternalPorts`].
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("link_external")?;
    if !settings.enabled {
        return Ok(());
    }
    settings.validate()?;
    let ports = ctx.require_port::<LinkExternalPorts>()?;
    let inbound: Arc<dyn LinkInboundPort> =
        Arc::new(ExternalInbound::new(Arc::clone(&ports), &settings));
    // The route is the registry group `link_inbound`: it is mounted when this port is provided,
    // so it exists only while the service is enabled.
    ctx.provide::<dyn LinkInboundPort>(inbound)?;
    ctx.add(Arc::new(ExternalService::new(ports, settings)));
    Ok(())
}
