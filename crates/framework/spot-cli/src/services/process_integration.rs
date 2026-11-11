//! Integration-node hosting for the process engine (WP-BPM-06.T3): HTTP, e-mail and outgoing
//! webhook handlers over `spot-bpm-integration`, each reaching the outside world only through
//! the hosted egress client built here.
//!
//! The node logic (the literal-host rule for a credentialed request, data mapping, the retry
//! policy, the [`SecretRegistry`] that scrubs a credential from everything observable) is
//! `spot-bpm-integration`'s. This module builds the production ports (an egress [`Guard`] and
//! [`Sender`] for HTTP and webhooks, a guarded SMTP transport for e-mail) and the credential
//! vault, and registers one handler with the process engine's
//! [`HandlerBuilder`](super::process::HandlerBuilder) port ([`ORDER`] is larger than
//! `services::process`'s).
//!
//! # The node's config
//!
//! The Process model's `IntegrationConfig` (`connector`, `action`, `endpoint`) has no room for a
//! kind-specific shape and `spot-bpm-model` is not an owned path, so `connector` selects the kind
//! (`"http"`, `"mail"`, `"webhook"`) and `endpoint` carries that kind's config as JSON text.
//!
//! # Credentials
//!
//! `[services.process_integration.credentials]` names sealed entries opened through the site's
//! `[services.secrets]` key provider at call time only. Without a secrets service, or with an
//! entry that does not open, a credentialed node fails closed; nothing is sent.
//!
//! # Mail
//!
//! The mail port trait does not carry the engine's key, so [`EngineIntegrationHandler`] scopes
//! `(case, node, attempt)` in a task-local value and [`QueueMailPort`] keys its job
//! `bpm_integration_mail:<case>:<node>:<attempt>` (never a hash of the content). The node is
//! done once the job is queued; the job handler sends through a guarded SMTP transport that
//! resolves the relay on first use, so a DNS blip at site start never fails the site.
//!
//! # Egress policy
//!
//! HTTP and webhook calls go wherever the node's config and data say, so the guard uses the
//! egress crate's address and port classification (private, loopback, link-local and metadata
//! addresses are refused; dangerous ports need an allow entry) without a per-host allow-list.
//! Mail reaches only the site's configured SMTP host and port.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use http::HeaderMap;
use serde::Deserialize;
use serde_json::Value as Json;
use spot_bpm_engine::{IdempotencyKey, NodeCtx, NodeHandler, NodeKind, NodeOutcome};
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::OnceCell;
use spot_bpm_integration::{
    CredentialRef, HttpNodeHandler, IntegrationCtx, IntegrationError, IntegrationKind,
    IntegrationRegistry, InMemoryVault, MailNodeHandler, MailPort, RetryClass, RetryPolicy,
    SealedEntry, SealedResolver, SecretRegistry, SecretResolver, SpotEgressPort, WebhookNodeHandler,
    WebhookPort,
};
use spot_core::SiteId;
use spot_egress::{
    EgressConnector, EgressPolicy, Guard, HttpRequest, InMemorySink, Method as EgressMethod,
    Purpose, Resolver, Sender, SystemResolver, TenantPolicy,
};
use spot_jobs::worker::HandlerResult;
use spot_mail::{Credentials as SmtpCredentials, Mailbox, OutgoingMessage, Secret as SmtpSecret};
use spot_mail_queue::TransportFactory as _;
use url::Url;

use crate::services::ports::{JobSpec, JobSubmit, MAX_KEY_BYTES};
use crate::services::{Idle, Level, SiteCtx};
use crate::{Error, Result, error};

/// Start order: after `services::process` (1100), which this module's port comes from, and
/// before the other node-kind modules of this series (1100 to 1230, ties by name).
pub const ORDER: u32 = 1115;

/// The job name a mail node's send is queued under.
const SEND_MAIL_JOB: &str = "bpm_integration:send_mail";
/// The queue every site's worker claims by default.
const QUEUE: &str = "default";
/// The default cap on a response body, bytes (1 MiB).
const DEFAULT_MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
/// The default call timeout, seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// The default cap on a node's retry attempts (including the first try).
const DEFAULT_MAX_RETRY_ATTEMPTS: u32 = 5;
/// The prefix of a mail job's idempotency key.
const MAIL_KEY_PREFIX: &str = "bpm_integration_mail:";
/// The code a node fails with when the blocking bridge cannot run (no runtime, or a
/// current-thread one): an existing catalogue code, the catalogue being outside the owned paths.
const CODE_NO_RUNTIME: &str = "bpm_integration.transport_failed";

tokio::task_local! {
    /// The engine's `(case, node, attempt)` key of the node being run, so [`QueueMailPort`] can
    /// key its job without the [`MailPort`] trait carrying it.
    static MAIL_KEY: IdempotencyKey;
}
/// Ports an outbound call never opens without an explicit allow entry (none of this module's
/// own policy carries one).
const DANGEROUS_PORTS: [u16; 10] = [22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250];

/// One credential, sealed under the site's `[services.secrets]` key provider.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedCredential {
    /// The `spsec:v1:` sealed text.
    sealed: String,
    /// The associated data the value was sealed under.
    aad: String,
}

/// `[services.process_integration]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    /// Whether the module does anything at all.
    enabled: bool,
    /// The most a response body may be, bytes.
    max_response_bytes: u64,
    /// The call timeout, seconds; a node's own `timeout_secs` is capped to it.
    timeout_secs: u64,
    /// The most attempts (the first try included) any node's retry policy may ask for.
    max_retry_attempts: u32,
    /// Credential references a node's config may name, each sealed under the site's key.
    credentials: BTreeMap<String, SealedCredential>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
            max_retry_attempts: DEFAULT_MAX_RETRY_ATTEMPTS,
            credentials: BTreeMap::new(),
        }
    }
}

impl Settings {
    fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.max_response_bytes == 0 {
            return Err("max_response_bytes");
        }
        if !(1..=600).contains(&self.timeout_secs) {
            return Err("timeout_secs");
        }
        if !(1..=20).contains(&self.max_retry_attempts) {
            return Err("max_retry_attempts");
        }
        Ok(())
    }
}

/// Fails every resolution closed: no secrets service, or no credential configured.
struct NoVaultResolver;

#[async_trait]
impl SecretResolver for NoVaultResolver {
    async fn resolve(
        &self,
        reference: &CredentialRef,
    ) -> std::result::Result<spot_secrets::SecretString, IntegrationError> {
        Err(IntegrationError::CredentialMissing {
            reference: reference.name.clone(),
        })
    }
}

/// Builds the resolver this site's nodes resolve a credential reference through: a sealed
/// vault over the secrets service's key provider when both are configured, otherwise a
/// resolver that fails closed.
fn build_resolver(
    settings: &Settings,
    secrets_host: Option<Arc<super::secrets::SecretsHost>>,
) -> Arc<dyn SecretResolver> {
    if settings.credentials.is_empty() {
        return Arc::new(NoVaultResolver);
    }
    let Some(host) = secrets_host else {
        return Arc::new(NoVaultResolver);
    };
    sealed_resolver(
        settings
            .credentials
            .iter()
            .map(|(name, c)| (name.clone(), c.sealed.clone(), c.aad.clone())),
        Arc::clone(host.keyring()),
    )
}

/// A resolver over `provider` that opens the given `(name, sealed text, associated data)`
/// entries at call time through `spot-secrets`' envelope encryption. A name no entry carries, a
/// sealed text that does not parse and a value whose associated data does not match each fail
/// the call closed with a stable code (`credential_missing` / `secret_resolve_failed`); no
/// request is made without the credential.
pub fn sealed_resolver<P: spot_secrets::KeyProvider + 'static>(
    entries: impl IntoIterator<Item = (String, String, String)>,
    provider: P,
) -> Arc<dyn SecretResolver> {
    let mut vault = InMemoryVault::new();
    for (name, sealed, aad) in entries {
        vault.insert(name, SealedEntry { sealed, aad });
    }
    Arc::new(SealedResolver::new(provider, Arc::new(vault)))
}

/// The policy an HTTP or webhook call is checked against: public addresses on ordinary ports
/// pass (the destination is whatever the node's config and data name), a private, loopback,
/// link-local or cloud-metadata address never does, and a dangerous port needs an allow entry
/// this policy carries none of.
pub fn integration_egress_policy() -> EgressPolicy {
    EgressPolicy {
        default: "allow".to_owned(),
        dangerous_ports: DANGEROUS_PORTS.to_vec(),
        tenants: HashMap::new(),
    }
}

/// Posts a signed webhook body through the hosted egress guard and sender directly (the node's
/// own signing and headers are `spot-bpm-integration`'s; this is only the wire).
pub struct RawWebhookPort {
    guard: Arc<Guard>,
    sender: Arc<dyn Sender>,
    tenant: String,
    max_response_bytes: u64,
}

impl RawWebhookPort {
    /// A webhook port over `guard` and `sender` for `tenant`, bounding a response to
    /// `max_response_bytes`.
    #[must_use]
    pub fn new(
        guard: Arc<Guard>,
        sender: Arc<dyn Sender>,
        tenant: impl Into<String>,
        max_response_bytes: u64,
    ) -> Self {
        Self {
            guard,
            sender,
            tenant: tenant.into(),
            max_response_bytes,
        }
    }
}

#[async_trait]
impl WebhookPort for RawWebhookPort {
    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> std::result::Result<spot_bpm_integration::HttpResponse, IntegrationError> {
        let parsed = Url::parse(url).map_err(|e| IntegrationError::UrlInvalid {
            detail: e.to_string(),
        })?;
        let request = spot_egress::Request {
            url: parsed.clone(),
            purpose: Purpose::Webhook,
            tenant: self.tenant.clone(),
            actor: None,
        };
        let authorized = self
            .guard
            .authorize(&request)
            .await
            .map_err(spot_bpm_integration::ports::from_egress)?;
        let http_request =
            HttpRequest::new(EgressMethod::Post, parsed, headers, Some(body.into()));
        let response = self
            .sender
            .send_request(&http_request, &authorized, self.max_response_bytes)
            .await
            .map_err(spot_bpm_integration::ports::from_egress)?;
        Ok(spot_bpm_integration::HttpResponse {
            status: response.status.as_u16(),
            headers: response.headers,
            body: response.body,
        })
    }
}

/// The egress policy of the site's one SMTP relay: nothing else is reachable for `Purpose::Mail`.
fn mail_egress_policy(tenant: &str, host: &str, port: u16) -> EgressPolicy {
    let allow_private = host
        .parse::<std::net::IpAddr>()
        .ok()
        .map(|ip| vec![ip.to_string()]);
    let entry = spot_egress::AllowEntry {
        scheme: None,
        host: host.to_owned(),
        ports: Some(vec![port]),
        purpose: Some(Purpose::Mail.as_str().to_owned()),
        allow_private,
    };
    let mut policy = EgressPolicy {
        default: "deny_non_public".to_owned(),
        dangerous_ports: DANGEROUS_PORTS.to_vec(),
        tenants: HashMap::new(),
    };
    policy.tenants.insert(
        tenant.to_owned(),
        TenantPolicy {
            allow: vec![entry],
            ..TenantPolicy::default()
        },
    );
    policy
}

/// A transport already built (the memory capture one when `sink = "memory"`) as a [`MailPort`].
struct DirectMailPort(Arc<dyn spot_mail::MailTransport>);

#[async_trait]
impl MailPort for DirectMailPort {
    async fn send(
        &self,
        message: &OutgoingMessage,
    ) -> std::result::Result<spot_mail::SendReceipt, IntegrationError> {
        self.0
            .send(message)
            .await
            .map_err(spot_bpm_integration::ports::from_mail)
    }
}

/// Always refuses: no `[services.mail]` for this site. Fails closed, never drops silently.
struct NoMailPort;

#[async_trait]
impl MailPort for NoMailPort {
    async fn send(
        &self,
        _message: &OutgoingMessage,
    ) -> std::result::Result<spot_mail::SendReceipt, IntegrationError> {
        Err(IntegrationError::MailPermanent {
            detail: "services.mail is not configured for this site".to_owned(),
        })
    }
}

/// What the SMTP relay is built from.
#[derive(Clone)]
struct SmtpSpec {
    host: String,
    port: u16,
    security: spot_mail::TransportSecurity,
    credentials: Option<SmtpCredentials>,
}

/// The guarded SMTP transport, built on first use: the relay's name is resolved and vetted by
/// the egress guard when the first mail is sent, never when the site starts, so a DNS blip at
/// start degrades nothing (the queued job retries), and a failure to build is retried by the
/// next send rather than remembered.
pub struct SmtpMailPort {
    spec: SmtpSpec,
    guard: Arc<Guard>,
    tenant: String,
    transport: OnceCell<Arc<dyn spot_mail::MailTransport>>,
}

impl SmtpMailPort {
    /// A lazily built STARTTLS transport to `host`:`port` for `tenant`, vetted by `guard`
    /// (`Purpose::Mail`) on first use, with no credentials.
    #[must_use]
    pub fn new(host: &str, port: u16, guard: Arc<Guard>, tenant: &str) -> Self {
        Self {
            spec: SmtpSpec {
                host: host.to_owned(),
                port,
                security: spot_mail::TransportSecurity::StartTls,
                credentials: None,
            },
            guard,
            tenant: tenant.to_owned(),
            transport: OnceCell::new(),
        }
    }

    async fn transport(
        &self,
    ) -> std::result::Result<&Arc<dyn spot_mail::MailTransport>, IntegrationError> {
        self.transport
            .get_or_try_init(|| async {
                let authorized = self
                    .guard
                    .authorize_addr(
                        &self.spec.host,
                        self.spec.port,
                        Purpose::Mail,
                        &self.tenant,
                        None,
                    )
                    .await
                    .map_err(spot_bpm_integration::ports::from_egress)?;
                let config = spot_mail::SmtpConfig {
                    host: self.spec.host.clone(),
                    port: self.spec.port,
                    security: self.spec.security,
                    credentials: self.spec.credentials.clone(),
                    timeout: Duration::from_secs(30),
                    pool_size: 2,
                    allow_insecure: false,
                    root_certificate_pem: None,
                    pinned: Some(authorized.socket_addrs().to_vec()),
                };
                spot_mail_queue::SmtpTransportFactory
                    .build(config)
                    .map_err(spot_bpm_integration::ports::from_mail)
            })
            .await
    }
}

#[async_trait]
impl MailPort for SmtpMailPort {
    async fn send(
        &self,
        message: &OutgoingMessage,
    ) -> std::result::Result<spot_mail::SendReceipt, IntegrationError> {
        self.transport()
            .await?
            .send(message)
            .await
            .map_err(spot_bpm_integration::ports::from_mail)
    }
}

/// The mail port the job handler sends through: `Some` when `services.mail` is configured and
/// enabled, `None` otherwise (the node fails closed). Resolves nothing: see [`SmtpMailPort`].
fn build_mail_transport(
    mail: &crate::config::Mail,
    guard: Arc<Guard>,
    tenant: &str,
) -> Option<Arc<dyn MailPort>> {
    if !mail.enabled {
        return None;
    }
    if mail.sink == "memory" {
        let capture = Arc::new(spot_mail::CaptureTransport::new());
        return Some(Arc::new(DirectMailPort(
            capture as Arc<dyn spot_mail::MailTransport>,
        )));
    }
    let smtp = mail.smtp.as_ref()?;
    let credentials = match (&smtp.username, &smtp.password) {
        (Some(user), Some(password)) => Some(SmtpCredentials {
            user: user.clone(),
            secret: SmtpSecret::new(password.clone()),
        }),
        _ => None,
    };
    let security = match smtp.security.as_str() {
        "plain" => spot_mail::TransportSecurity::Plain,
        "tls" => spot_mail::TransportSecurity::Tls,
        _ => spot_mail::TransportSecurity::StartTls,
    };
    Some(Arc::new(SmtpMailPort {
        spec: SmtpSpec {
            host: smtp.host.clone(),
            port: smtp.port,
            security,
            credentials,
        },
        guard,
        tenant: tenant.to_owned(),
        transport: OnceCell::new(),
    }))
}

/// A message as a durable job's arguments: plain strings, never the lettre types directly, so
/// the job store's JSON carries no more than what SMTP itself would send.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct QueuedMailbox {
    name: Option<String>,
    address: String,
}

impl QueuedMailbox {
    fn from_mailbox(mailbox: &Mailbox) -> Self {
        Self {
            name: mailbox.name.clone(),
            address: mailbox.address.to_string(),
        }
    }

    fn into_mailbox(self) -> std::result::Result<Mailbox, IntegrationError> {
        Mailbox::new(self.name, self.address).map_err(|e| IntegrationError::MailPermanent {
            detail: e.to_string(),
        })
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct QueuedEmail {
    from: QueuedMailbox,
    to: Vec<QueuedMailbox>,
    cc: Vec<QueuedMailbox>,
    subject: String,
    text: Option<String>,
    html: Option<String>,
}

impl QueuedEmail {
    fn from_message(message: &OutgoingMessage) -> Self {
        Self {
            from: QueuedMailbox::from_mailbox(&message.from),
            to: message.to.iter().map(QueuedMailbox::from_mailbox).collect(),
            cc: message.cc.iter().map(QueuedMailbox::from_mailbox).collect(),
            subject: message.subject.clone(),
            text: message.text.clone(),
            html: message.html.clone(),
        }
    }

    fn into_message(self) -> std::result::Result<OutgoingMessage, IntegrationError> {
        let from = self.from.into_mailbox()?;
        let mut message = OutgoingMessage::new(from, self.subject);
        for mailbox in self.to {
            message.to.push(mailbox.into_mailbox()?);
        }
        for mailbox in self.cc {
            message.cc.push(mailbox.into_mailbox()?);
        }
        message.text = self.text;
        message.html = self.html;
        Ok(message)
    }
}

/// Queues a mail node's send as a durable, idempotent job instead of dialing SMTP inline: the
/// node completes once the job store accepted it, exactly as a document hook's mail reaches
/// the outbox before it is actually sent. The job's idempotency key is the engine's
/// `(case, node, attempt)` key (read from the run's task-local scope), so a replay of one attempt
/// is one job and nothing about the message content can collide two cases' mails.
pub struct QueueMailPort {
    submit: Arc<dyn JobSubmit>,
}

impl QueueMailPort {
    /// A mail port that queues every send through `submit`.
    #[must_use]
    pub fn new(submit: Arc<dyn JobSubmit>) -> Self {
        Self { submit }
    }
}

#[async_trait]
impl MailPort for QueueMailPort {
    async fn send(
        &self,
        message: &OutgoingMessage,
    ) -> std::result::Result<spot_mail::SendReceipt, IntegrationError> {
        // Fails closed: with no engine key in scope there is nothing stable to dedupe on.
        let engine_key = MAIL_KEY
            .try_with(Clone::clone)
            .map_err(|_| IntegrationError::MailPermanent {
                detail: "no engine idempotency key is in scope for this send".to_owned(),
            })?;
        let key = format!("{MAIL_KEY_PREFIX}{engine_key}");
        if key.len() > MAX_KEY_BYTES {
            return Err(IntegrationError::MailPermanent {
                detail: "the engine idempotency key is too long for the job store".to_owned(),
            });
        }
        let queued = QueuedEmail::from_message(message);
        let args = serde_json::to_value(&queued).map_err(|e| IntegrationError::MailPermanent {
            detail: e.to_string(),
        })?;
        let spec = JobSpec {
            name: SEND_MAIL_JOB.to_owned(),
            queue: QUEUE.to_owned(),
            args,
            idempotency_key: key.clone(),
            not_before: None,
        };
        self.submit
            .submit(spec)
            .await
            .map_err(|e| IntegrationError::MailTransient {
                detail: e.to_string(),
            })?;
        Ok(spot_mail::SendReceipt {
            message_id: key,
            accepted: message.to.iter().map(|m| m.address.clone()).collect(),
            rejected: Vec::new(),
        })
    }
}

/// `connector` names which hosted handler a node runs, since the Process model's
/// `IntegrationConfig` carries no kind of its own (see the module docs).
fn select_kind(connector: &str) -> Option<IntegrationKind> {
    match connector {
        "http" => Some(IntegrationKind::Http),
        "mail" => Some(IntegrationKind::Mail),
        "webhook" => Some(IntegrationKind::Webhook),
        _ => None,
    }
}

/// The node's full, kind-specific config: JSON text carried in `endpoint` (see the module
/// docs). An empty or malformed value is `bpm_integration.config_invalid`.
fn parse_endpoint(node: &str, endpoint: &str) -> std::result::Result<Json, IntegrationError> {
    if endpoint.trim().is_empty() {
        return Err(IntegrationError::ConfigInvalid {
            node: node.to_owned(),
            detail: "the integration node's `endpoint` carries no config".to_owned(),
        });
    }
    serde_json::from_str(endpoint).map_err(|e| IntegrationError::ConfigInvalid {
        node: node.to_owned(),
        detail: format!("the integration node's `endpoint` is not valid JSON: {e}"),
    })
}

/// The site-settings ceilings a node's own config is capped to before it runs.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// The most attempts (the first try included) a node's retry policy may ask for.
    pub max_retry_attempts: u32,
    /// The longest a node's `timeout_secs` may be, seconds.
    pub timeout_secs: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_retry_attempts: DEFAULT_MAX_RETRY_ATTEMPTS,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

/// Caps the node's own retry and timeout to the site's settings: the node may ask for less,
/// never more. A config that is not an object, or a field of the wrong type, is left for the
/// handler's own validation to refuse.
fn cap_config(config: &mut Json, caps: Caps) {
    let Some(object) = config.as_object_mut() else {
        return;
    };
    let cap = u64::from(caps.max_retry_attempts);
    let default_attempts = u64::from(RetryPolicy::default().max_attempts);
    match object.get_mut("retry") {
        Some(Json::Object(retry)) => {
            let asked = retry
                .get("max_attempts")
                .and_then(Json::as_u64)
                .unwrap_or(default_attempts);
            retry.insert("max_attempts".to_owned(), Json::from(asked.min(cap)));
        }
        Some(_) => {}
        None => {
            if default_attempts > cap {
                object.insert("retry".to_owned(), serde_json::json!({ "max_attempts": cap }));
            }
        }
    }
    if let Some(timeout) = object.get_mut("timeout_secs")
        && let Some(asked) = timeout.as_u64()
    {
        *timeout = Json::from(asked.min(caps.timeout_secs));
    }
}

/// Bridges the engine's synchronous [`NodeHandler`] onto `spot-bpm-integration`'s asynchronous
/// [`IntegrationHandler`]s: the engine's own contract says a handler never blocks, but calling
/// the outside world cannot be anything else here, so this runs the call with
/// [`tokio::task::block_in_place`] on the multi-thread runtime this binary always builds, and
/// blocks only the one case-step job that is working. Called outside a runtime, or on a
/// current-thread one (where `block_in_place` panics), the node fails closed with a stable code.
pub struct EngineIntegrationHandler {
    handlers: IntegrationRegistry,
    secrets: Arc<dyn SecretResolver>,
    caps: Caps,
}

impl EngineIntegrationHandler {
    /// A handler dispatching to `handlers` by the node's `connector` field, resolving a
    /// credential reference through `secrets`, with the default [`Caps`].
    #[must_use]
    pub fn new(handlers: IntegrationRegistry, secrets: Arc<dyn SecretResolver>) -> Self {
        Self {
            handlers,
            secrets,
            caps: Caps::default(),
        }
    }

    /// The same handler with the site's ceilings.
    #[must_use]
    pub fn with_caps(mut self, caps: Caps) -> Self {
        self.caps = caps;
        self
    }
}

impl NodeHandler for EngineIntegrationHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Integration
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        let node = ctx.node.to_string();
        let Some(connector) = ctx.config.get("connector").and_then(Json::as_str) else {
            return NodeOutcome::fail(
                "bpm_integration.config_invalid",
                format!("node `{node}` names no `connector`"),
            );
        };
        let Some(kind) = select_kind(connector) else {
            return NodeOutcome::fail(
                "bpm_integration.config_invalid",
                format!("node `{node}` names an unsupported integration connector"),
            );
        };
        let Some(handler) = self.handlers.get(kind) else {
            return NodeOutcome::fail(
                "bpm_integration.config_invalid",
                format!("node `{node}`: no `{connector}` integration handler is hosted"),
            );
        };
        let endpoint = ctx.config.get("endpoint").and_then(Json::as_str).unwrap_or("");
        let mut config = match parse_endpoint(&node, endpoint) {
            Ok(config) => config,
            Err(e) => return NodeOutcome::fail(e.code(), e.to_string()),
        };
        cap_config(&mut config, self.caps);
        // `block_in_place` panics on a current-thread runtime and `Handle::current` outside
        // any runtime: check both first and fail the node closed instead.
        let runtime = match Handle::try_current() {
            Ok(runtime) if runtime.runtime_flavor() != RuntimeFlavor::CurrentThread => runtime,
            _ => {
                return NodeOutcome::fail(
                    CODE_NO_RUNTIME,
                    format!(
                        "node `{node}`: the integration bridge needs a multi-thread async runtime"
                    ),
                );
            }
        };
        let mut registry = match SecretRegistry::new() {
            Ok(registry) => registry,
            Err(e) => return NodeOutcome::fail(e.code(), e.to_string()),
        };
        // The engine's `NodeCtx` resolves no separate per-node input or prior-node outputs (see
        // the module docs); the Case's variables stand in for both so `doc.input.*` and
        // `doc.case.*` both read the same data a Document or Decision node would.
        let input_snapshot = ctx.variables.clone();
        let node_outputs = Json::Object(serde_json::Map::new());
        let key = IdempotencyKey::new(ctx.case.clone(), ctx.node.clone(), ctx.attempt);
        let mut integration_ctx = IntegrationCtx {
            case: ctx.case,
            node: ctx.node,
            attempt: ctx.attempt,
            key: key.clone(),
            is_retry: ctx.is_retry,
            now: ctx.now,
            principal: ctx.principal,
            input: &input_snapshot,
            node_outputs: &node_outputs,
            variables: ctx.variables,
            config: &config,
            secrets: &*self.secrets,
            registry: &mut registry,
        };
        tokio::task::block_in_place(|| {
            runtime.block_on(MAIL_KEY.scope(key, handler.run(&mut integration_ctx)))
        })
    }
}

/// What the module's egress guards are built over; production is [`Wiring::production`], and a
/// test supplies its own resolver and policy to reach a loopback server it controls.
pub struct Wiring {
    /// How names are resolved (the guard resolves once; a connection never re-resolves).
    pub resolver: Arc<dyn Resolver>,
    /// The HTTP and webhook egress policy.
    pub policy: EgressPolicy,
}

impl Wiring {
    /// The system resolver and [`integration_egress_policy`].
    #[must_use]
    pub fn production() -> Self {
        Self {
            resolver: Arc::new(SystemResolver),
            policy: integration_egress_policy(),
        }
    }
}

/// Registers the Integration node handlers (HTTP, e-mail, outgoing webhook) with the process
/// engine's [`HandlerBuilder`](super::process::HandlerBuilder) port. Inert when
/// `services.process_integration.enabled = false`.
///
/// # Errors
///
/// A setting that does not decode or is out of range; the `HandlerBuilder` or `JobSubmit` port
/// is missing (the process and jobs modules must register first; both have a smaller `ORDER`
/// than this module's).
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_integration")?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let site = {
        let env = *ctx.env()?;
        SiteId::new(&env.site.site).map_err(error)?
    };
    let mail_settings = ctx.env()?.loaded.common.services.mail.clone();
    register_with(ctx, &site, &mail_settings, Wiring::production())
        .await
        .map(|_| ())
}

/// [`register`] with its site, mail settings and egress wiring given: the whole production
/// assembly (settings, vault, guards, handlers, the mail job handler), which `register` calls
/// with what the site environment holds. Returns the handler it registered with the
/// `HandlerBuilder` (`None` when the module is disabled or this is a reload).
///
/// # Errors
///
/// As [`register`].
pub async fn register_with(
    ctx: &mut SiteCtx<'_>,
    site: &SiteId,
    mail_settings: &crate::config::Mail,
    wiring: Wiring,
) -> Result<Option<Arc<EngineIntegrationHandler>>> {
    let settings: Settings = ctx.settings_or_default("process_integration")?;
    settings
        .validate()
        .map_err(|key| Error(format!("services.process_integration.{key}: out of range")))?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(None);
    }
    let tenant = site.as_str().to_owned();
    let clock = Arc::clone(ctx.clock());

    let builder = ctx.require_port::<super::process::HandlerBuilder>()?;
    let submit = ctx.require_port::<dyn JobSubmit>()?;
    let secrets_host = ctx.port::<super::secrets::SecretsHost>();

    let resolver = build_resolver(&settings, secrets_host);

    let guard = Arc::new(Guard::new(
        wiring.policy,
        Arc::clone(&wiring.resolver),
        Arc::new(InMemorySink::new()),
        Arc::clone(&clock),
    ));

    let http_sender: Arc<dyn Sender> = Arc::new(spot_egress::HttpSender::new(
        EgressConnector::new(Arc::clone(&guard), tenant.clone(), Purpose::Connector),
        Duration::from_secs(settings.timeout_secs),
    ));
    let http_port = Arc::new(SpotEgressPort::new(
        Arc::clone(&guard),
        http_sender,
        Purpose::Connector,
        None,
        settings.max_response_bytes,
    ));

    let webhook_sender: Arc<dyn Sender> = Arc::new(spot_egress::HttpSender::new(
        EgressConnector::new(Arc::clone(&guard), tenant.clone(), Purpose::Webhook),
        Duration::from_secs(settings.timeout_secs),
    ));
    let webhook_port = Arc::new(RawWebhookPort {
        guard: Arc::clone(&guard),
        sender: webhook_sender,
        tenant: tenant.clone(),
        max_response_bytes: settings.max_response_bytes,
    });

    let mail_guard = Arc::new(Guard::new(
        match &mail_settings.smtp {
            Some(smtp) => mail_egress_policy(&tenant, &smtp.host, smtp.port),
            None => EgressPolicy {
                default: "deny_non_public".to_owned(),
                dangerous_ports: DANGEROUS_PORTS.to_vec(),
                tenants: HashMap::new(),
            },
        },
        wiring.resolver,
        Arc::new(InMemorySink::new()),
        Arc::clone(&clock),
    ));
    let mail_transport = build_mail_transport(mail_settings, mail_guard, &tenant);

    let mut handlers = IntegrationRegistry::new();
    handlers.register(HttpNodeHandler::new(http_port, tenant.clone()));
    handlers.register(WebhookNodeHandler::new(webhook_port));
    let mail_port: Arc<dyn MailPort> = match &mail_transport {
        Some(_) => Arc::new(QueueMailPort {
            submit: Arc::clone(&submit),
        }),
        None => Arc::new(NoMailPort),
    };
    handlers.register(MailNodeHandler::new(mail_port));

    if let Some(transport) = mail_transport {
        ctx.job_handlers().register(SEND_MAIL_JOB, move |job| {
            let transport = Arc::clone(&transport);
            Box::pin(async move { Ok(run_send_mail_job(&transport, &job.job.args).await) })
        });
    }

    let handler = Arc::new(
        EngineIntegrationHandler {
            handlers,
            secrets: resolver,
            caps: Caps::default(),
        }
        .with_caps(Caps {
            max_retry_attempts: settings.max_retry_attempts,
            timeout_secs: settings.timeout_secs,
        }),
    );
    builder.register(handler.clone());

    ctx.add(Idle::new("process_integration", Level::Optional, || {
        Box::pin(async { Ok(()) })
    }));
    Ok(Some(handler))
}

/// Runs one queued mail job: rebuilds the message and sends it through the real transport,
/// classifying the result the same way the node's own retry policy would.
pub async fn run_send_mail_job(transport: &Arc<dyn MailPort>, args: &Json) -> HandlerResult {
    let queued: QueuedEmail = match serde_json::from_value(args.clone()) {
        Ok(queued) => queued,
        Err(_) => return HandlerResult::Fail("bpm_integration.config_invalid".to_owned()),
    };
    let message = match queued.into_message() {
        Ok(message) => message,
        Err(e) => return HandlerResult::Fail(e.code().to_owned()),
    };
    match transport.send(&message).await {
        Ok(_) => HandlerResult::Ok,
        Err(e) => match e.retry_class() {
            RetryClass::Transient => HandlerResult::Retry(e.code().to_owned()),
            RetryClass::Permanent => HandlerResult::Fail(e.code().to_owned()),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_site_caps_a_nodes_retry_and_timeout() {
        let caps = Caps {
            max_retry_attempts: 2,
            timeout_secs: 10,
        };
        // Asks for more: capped.
        let mut config = serde_json::json!({ "retry": { "max_attempts": 9, "backoff_secs": 1 }, "timeout_secs": 600 });
        cap_config(&mut config, caps);
        assert_eq!(config["retry"]["max_attempts"], 2);
        assert_eq!(config["retry"]["backoff_secs"], 1);
        assert_eq!(config["timeout_secs"], 10);
        // Asks for less: kept.
        let mut config = serde_json::json!({ "retry": { "max_attempts": 1 }, "timeout_secs": 3 });
        cap_config(&mut config, caps);
        assert_eq!(config["retry"]["max_attempts"], 1);
        assert_eq!(config["timeout_secs"], 3);
        // Silent (the handler's default of 3 applies): the cap is written in; a mail config
        // (no `timeout_secs` field) gains none.
        let mut config = serde_json::json!({ "from": "a@b.test" });
        cap_config(&mut config, caps);
        assert_eq!(config["retry"]["max_attempts"], 2);
        assert!(config.get("timeout_secs").is_none());
        // A cap at or above the default leaves a silent node alone.
        let mut config = serde_json::json!({});
        cap_config(&mut config, Caps::default());
        assert!(config.get("retry").is_none());
        // Not an object: untouched, for the handler to refuse.
        let mut config = serde_json::json!("text");
        cap_config(&mut config, caps);
        assert_eq!(config, "text");
    }

    #[test]
    fn the_connector_field_selects_the_hosted_kind() {
        assert_eq!(select_kind("http"), Some(IntegrationKind::Http));
        assert_eq!(select_kind("mail"), Some(IntegrationKind::Mail));
        assert_eq!(select_kind("webhook"), Some(IntegrationKind::Webhook));
        assert_eq!(select_kind("wasm:as2"), None);
        assert_eq!(select_kind(""), None);
    }

}
