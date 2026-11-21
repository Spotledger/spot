//! The gateway: one call from a prompt to a model under the tenant's policy.
//!
//! The flow of [`LlmGateway::complete`], in the order that matters:
//!
//! 1. **The tenant's configuration** (provider, endpoint, model, allow-list, price, budget), from
//!    the store; a tenant with none, or a task it does not list, is refused.
//! 2. **The tenant's own allow-list**: an endpoint host it does not cover is refused here, before
//!    any lookup, connection or byte.
//! 3. **The key**: opened from the sealed column by the secrets vault only now, only for a
//!    provider that needs one, and held in a zeroising [`ApiKey`] inside the provider. A missing
//!    key is refused.
//! 4. **The egress policy** (`purpose = llm`) for the tenant: the host is resolved and every
//!    answer checked, so a name that resolves to a private or metadata address is refused with no
//!    connection. The transport authorises every request and redirect hop again and dials only the
//!    pinned addresses.
//! 5. **The budget period** rolls over when a new day or month began; the tenant's spend of the
//!    period is read back from the database, so a restart does not reset it.
//! 6. **The send record** is written (`sending`) before the provider is called: endpoint, model,
//!    a digest of the request, its size, the DocTypes and field classes, the user and the purpose.
//!    Never the content, never the key.
//! 7. **The router** reserves the worst case against the budget (a call over the cap is refused
//!    before the provider is touched), calls the provider with retry and breaker, and settles.
//! 8. **Usage** is written per tenant, task, day, provider and model, and the send record is
//!    closed with the outcome and the tokens.
//!
//! Every refusal writes a send record with its reason (`refused_*`), so an operator can see what
//! was blocked as well as what was sent.
//!
//! [`LlmGateway::embed`] (module `embed`, WP-AI-03.T7) passes the same controls for an embedding
//! call: the tenant's embedding model, the allow-list, the egress policy, the key, the budget
//! (a worst case reserved for the whole call, the exact tokens settled), a send record written
//! first, and usage counted under the task `embed`.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use spot_core::Clock;
use spot_doc_audit::sha256_hex;
use spot_document::secrets::SecretVault;
use spot_egress::{Guard, Purpose, Request as EgressRequest};
use spot_llm::transport::HttpTransport;
use spot_llm::transport::egress::EgressTransport;
use spot_llm::{ApiKey, Embedder as ProviderEmbedder, LlmError, Provider, Request, Response};
use spot_llm_anthropic::{AnthropicProvider, ModelInfo as AnthropicModel};
use spot_llm_openai::{ModelInfo as OpenAiModel, OpenAiProvider, Profile};
use spot_llm_router::{
    Budget, BudgetContext, BudgetScope, DayKey, Price, PriceTable, Route, Router, RouterConfig,
    RouterError, RoutingTable,
};
use spot_redact::Redactor;
use spot_secrets::SecretString;
use tokio::sync::Mutex;
use url::Url;

use super::config::{ConfigProblem, Kind, TenantConfig, check_task, check_tenant, origin_of};
use super::prompt::{DocumentReader, Manifest, PromptBuilder, PromptPolicy};
use super::store::{SendRecord, Store, StoreError, UsageRow};
use super::usage::{CapturingLedger, RouterClock, now_ms, period_start_day, record_cost};
use crate::services::egress::SharedGuard;

/// How long a tenant's built provider stack is reused before the configuration is read again, so
/// a change made by another node takes effect.
pub const RUNTIME_TTL_MS: u64 = 60_000;

/// The largest key accepted, in bytes.
pub const MAX_KEY_BYTES: usize = 4_096;

/// Builds the provider for a tenant's configuration over a transport. The production
/// implementation is [`BuiltinProviders`]; a test supplies a scripted provider.
pub trait ProviderFactory: Send + Sync {
    /// The provider for `config`, using `key` (when the kind needs one) over `transport`.
    fn build(
        &self,
        config: &TenantConfig,
        key: Option<ApiKey>,
        transport: Arc<dyn HttpTransport>,
    ) -> Arc<dyn Provider>;

    /// The embedder for `config` (WP-AI-03.T7), built only for a tenant that sets an embedding
    /// model. The default answers `None`: a factory that builds no embedder leaves the tenant's
    /// embedding calls refused with the provider's fixed `llm.embeddings_unsupported`.
    fn build_embedder(
        &self,
        _config: &TenantConfig,
        _key: Option<ApiKey>,
        _transport: Arc<dyn HttpTransport>,
    ) -> Option<Arc<dyn ProviderEmbedder>> {
        None
    }
}

/// The providers that ship: Anthropic and the OpenAI-compatible family.
#[derive(Clone, Copy, Debug, Default)]
pub struct BuiltinProviders;

impl ProviderFactory for BuiltinProviders {
    fn build(
        &self,
        config: &TenantConfig,
        key: Option<ApiKey>,
        transport: Arc<dyn HttpTransport>,
    ) -> Arc<dyn Provider> {
        match config.kind {
            Kind::Anthropic => Arc::new(AnthropicProvider::new(
                config.endpoint.clone(),
                key.unwrap_or_else(|| ApiKey::new("")),
                transport,
                vec![AnthropicModel {
                    id: config.model.clone(),
                    context_window: config.context_window,
                    max_output: config.max_output,
                    vision: false,
                }],
            )),
            Kind::Openrouter | Kind::Openai | Kind::Ollama => {
                let profile = match config.kind {
                    Kind::Openrouter => Profile::openrouter(),
                    Kind::Ollama => Profile::ollama(),
                    _ => Profile::generic(),
                };
                Arc::new(OpenAiProvider::new(
                    profile,
                    config.endpoint.clone(),
                    key,
                    transport,
                    vec![OpenAiModel {
                        id: config.model.clone(),
                        context_window: config.context_window,
                        max_output: config.max_output,
                        vision: false,
                    }],
                ))
            }
        }
    }

    fn build_embedder(
        &self,
        config: &TenantConfig,
        key: Option<ApiKey>,
        transport: Arc<dyn HttpTransport>,
    ) -> Option<Arc<dyn ProviderEmbedder>> {
        match config.kind {
            // Anthropic has no embeddings endpoint; the provider says so with its fixed refusal.
            Kind::Anthropic => Some(Arc::new(AnthropicProvider::new(
                config.endpoint.clone(),
                key.unwrap_or_else(|| ApiKey::new("")),
                transport,
                Vec::new(),
            ))),
            Kind::Openrouter => Some(Arc::new(OpenAiProvider::new(
                Profile::openrouter(),
                config.endpoint.clone(),
                key,
                transport,
                Vec::new(),
            ))),
            Kind::Ollama => Some(Arc::new(OpenAiProvider::new(
                Profile::ollama(),
                config.endpoint.clone(),
                key,
                transport,
                Vec::new(),
            ))),
            Kind::Openai => Some(Arc::new(OpenAiProvider::new(
                Profile::generic(),
                config.endpoint.clone(),
                key,
                transport,
                Vec::new(),
            ))),
        }
    }
}

/// Builds the HTTP transport a tenant's provider sends through. Production uses
/// [`EgressTransports`]; a test supplies one over a scripted sender.
pub trait TransportFactory: Send + Sync {
    /// A transport that authorises with `guard` for `tenant` (purpose `llm`).
    fn transport(&self, guard: Arc<Guard>, tenant: &str) -> Arc<dyn HttpTransport>;
}

/// The production transport: the egress-guarded HTTP client.
#[derive(Clone, Copy, Debug)]
pub struct EgressTransports {
    /// The total time one request may take.
    pub total_timeout: Duration,
}

impl TransportFactory for EgressTransports {
    fn transport(&self, guard: Arc<Guard>, tenant: &str) -> Arc<dyn HttpTransport> {
        Arc::new(EgressTransport::for_tenant(
            guard,
            tenant,
            self.total_timeout,
        ))
    }
}

/// Who a call is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The invoking user (never a system principal where the user's own identity is available).
    pub user: String,
    /// Why the call is made, in the caller's words (`summarise ticket`): stored in the send
    /// record, so keep it free of data.
    pub purpose: String,
}

/// Why a call or an operation was refused or failed. [`CallError::code`] is the stable code; the
/// text never carries a key, an endpoint path, a prompt or a model answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The configuration is not acceptable.
    Config(ConfigProblem),
    /// The tenant has no provider configuration.
    NotConfigured,
    /// The task is not one of the tenant's.
    TaskUnknown,
    /// The provider needs a key and none was stored.
    KeyMissing,
    /// The endpoint host is not on the tenant's allow-list.
    EndpointNotAllowed,
    /// The egress policy refused the endpoint (the egress code, stable).
    EndpointDenied(&'static str),
    /// The budget would be exceeded; nothing was sent.
    BudgetExceeded {
        /// What the period had spent, in micro-units.
        spent_micro: u64,
        /// The cap, in micro-units.
        limit_micro: u64,
    },
    /// The provider (or the router in front of it) failed.
    Provider(LlmError),
    /// The store failed.
    Store(StoreError),
    /// The key could not be sealed or opened.
    Vault,
    /// The tenant has no embedding model configured (WP-AI-03.T7).
    EmbeddingsNotConfigured,
    /// An embedding call is over the per-call text count or the per-text size (WP-AI-03.T7).
    EmbedTooLarge,
    /// A text holds a secret or a financial identifier and was not sent; the classes are the
    /// redactor's labels (`card_number`, `iban`, `token`...), never a value.
    EmbedRefused {
        /// The labels of what the redactor found, sorted, without repeats.
        classes: Vec<String>,
    },
}

impl CallError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            CallError::Config(_) => "llm_host.config_invalid",
            CallError::NotConfigured => "llm_host.not_configured",
            CallError::TaskUnknown => "llm_host.task_unknown",
            CallError::KeyMissing => "llm_host.key_missing",
            CallError::EndpointNotAllowed => "llm_host.endpoint_not_allowed",
            CallError::EndpointDenied(_) => "llm_host.endpoint_denied",
            CallError::BudgetExceeded { .. } => "llm_host.budget_exceeded",
            CallError::Provider(_) => "llm_host.provider_failed",
            CallError::Store(_) => "llm_host.store_unavailable",
            CallError::Vault => "llm_host.key_unusable",
            CallError::EmbeddingsNotConfigured => "llm_host.embeddings_not_configured",
            CallError::EmbedTooLarge => "llm_host.embed_too_large",
            CallError::EmbedRefused { .. } => "llm_host.embed_refused",
        }
    }

    /// The short reason the send record keeps (`refused_budget`, `failed_llm_timeout`).
    fn outcome(&self) -> String {
        match self {
            CallError::Config(_)
            | CallError::NotConfigured
            | CallError::TaskUnknown
            | CallError::EmbeddingsNotConfigured => "refused_config".to_owned(),
            CallError::EmbedTooLarge => "refused_too_large".to_owned(),
            CallError::EmbedRefused { .. } => "refused_redaction".to_owned(),
            CallError::KeyMissing | CallError::Vault => "refused_key".to_owned(),
            CallError::EndpointNotAllowed => "refused_allow_list".to_owned(),
            CallError::EndpointDenied(_) => "refused_egress".to_owned(),
            CallError::BudgetExceeded { .. } => "refused_budget".to_owned(),
            CallError::Provider(error) => format!("failed_{}", error.code().replace('.', "_")),
            CallError::Store(_) => "failed_store".to_owned(),
        }
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: ", self.code())?;
        match self {
            CallError::Config(problem) => write!(f, "{problem}"),
            CallError::NotConfigured => f.write_str("the tenant has no model provider configured"),
            CallError::TaskUnknown => f.write_str("the task is not one of the tenant's tasks"),
            CallError::KeyMissing => f.write_str("the provider needs a key and none is stored"),
            CallError::EndpointNotAllowed => {
                f.write_str("the endpoint host is not on the tenant's allow-list")
            }
            CallError::EndpointDenied(code) => {
                write!(f, "the egress policy refused the endpoint ({code})")
            }
            CallError::BudgetExceeded {
                spent_micro,
                limit_micro,
            } => write!(
                f,
                "the budget is spent: {spent_micro} of {limit_micro} micro-units used"
            ),
            CallError::Provider(error) => write!(f, "the provider failed ({})", error.code()),
            CallError::Store(error) => write!(f, "{error}"),
            CallError::Vault => f.write_str("the stored key could not be opened"),
            CallError::EmbeddingsNotConfigured => {
                f.write_str("the tenant has no embedding model configured")
            }
            CallError::EmbedTooLarge => f.write_str(
                "the embedding call has more texts, or a longer text, than the service allows",
            ),
            CallError::EmbedRefused { classes } => write!(
                f,
                "a text holds a secret or a financial identifier ({}) and was not sent",
                classes.join(", ")
            ),
        }
    }
}

impl std::error::Error for CallError {}

impl From<StoreError> for CallError {
    fn from(error: StoreError) -> Self {
        CallError::Store(error)
    }
}

impl From<ConfigProblem> for CallError {
    fn from(problem: ConfigProblem) -> Self {
        CallError::Config(problem)
    }
}

/// What the gateway is built from.
pub struct Deps {
    /// The site name: part of every stored row and of the key's associated data.
    pub site: String,
    /// The durable store.
    pub store: Store,
    /// The secrets vault that seals and opens keys.
    pub vault: Arc<dyn SecretVault>,
    /// The site's egress guard (follows policy reloads).
    pub guard: Arc<SharedGuard>,
    /// The injected clock.
    pub clock: Arc<dyn Clock>,
    /// Builds providers.
    pub providers: Arc<dyn ProviderFactory>,
    /// Builds transports.
    pub transports: Arc<dyn TransportFactory>,
    /// Redacts text before it enters a prompt.
    pub redactor: Arc<Redactor>,
    /// The permission-checked document reader.
    pub reader: Arc<dyn DocumentReader>,
    /// Retries of a retryable provider failure on the same route.
    pub max_retries: u32,
    /// How many days send records are kept.
    pub retention_days: u32,
}

/// One tenant's built stack.
struct TenantRuntime {
    config: TenantConfig,
    url: Url,
    origin: String,
    price: Price,
    router: Router,
    ledger: Arc<CapturingLedger>,
    /// The embedder, built only for a tenant that sets an embedding model.
    embedder: Option<Arc<dyn ProviderEmbedder>>,
    guard: Arc<Guard>,
    built_at_ms: u64,
    /// The first day of the budget period the router's book is in; 0 until installed.
    period: Mutex<u64>,
}

/// The LLM gateway of one site.
pub struct LlmGateway {
    deps: Deps,
    runtimes: StdMutex<HashMap<String, Arc<TenantRuntime>>>,
    embed_limits: EmbedLimits,
    /// Makes the request id of each embedding call unique within one millisecond.
    embed_seq: AtomicU64,
}

/// The bounds of one embedding call (`[services.llm] max_texts_per_call` and `max_text_bytes`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EmbedLimits {
    /// The most texts one call may carry.
    pub max_texts: usize,
    /// The longest single text, in bytes.
    pub max_text_bytes: usize,
}

impl Default for EmbedLimits {
    fn default() -> Self {
        Self {
            max_texts: 512,
            max_text_bytes: 32_768,
        }
    }
}

#[path = "embed.rs"]
mod embed;

fn key_aad(site: &str, tenant: &str) -> Vec<u8> {
    format!("spot-llm/{site}/{tenant}/key").into_bytes()
}

impl LlmGateway {
    /// A gateway over `deps`.
    #[must_use]
    pub fn new(deps: Deps) -> Self {
        Self {
            deps,
            runtimes: StdMutex::new(HashMap::new()),
            embed_limits: EmbedLimits::default(),
            embed_seq: AtomicU64::new(0),
        }
    }

    /// Sets the bounds of an embedding call (the defaults are 512 texts of 32,768 bytes).
    #[must_use]
    pub fn with_embed_limits(mut self, limits: EmbedLimits) -> Self {
        self.embed_limits = limits;
        self
    }

    fn now_ms(&self) -> u64 {
        now_ms(self.deps.clock.as_ref())
    }

    fn cached(&self, tenant: &str) -> Option<Arc<TenantRuntime>> {
        self.runtimes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(tenant)
            .cloned()
    }

    fn forget(&self, tenant: &str) -> Option<Arc<TenantRuntime>> {
        self.runtimes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(tenant)
    }

    /// Stores a tenant's configuration (the stored key is kept). The next call builds the new
    /// stack.
    ///
    /// # Errors
    ///
    /// [`CallError::Config`] when the configuration is not acceptable, [`CallError::Store`] when
    /// the write fails.
    pub async fn put_config(&self, config: &TenantConfig) -> Result<(), CallError> {
        config.validate()?;
        self.deps.store.put_config(config, self.now_ms()).await?;
        if let Some(old) = self.forget(&config.tenant) {
            self.flush(&old).await;
        }
        Ok(())
    }

    /// A tenant's configuration.
    ///
    /// # Errors
    ///
    /// [`CallError::Store`] when the read fails.
    pub async fn config(&self, tenant: &str) -> Result<Option<TenantConfig>, CallError> {
        Ok(self.deps.store.config(tenant).await?)
    }

    /// Seals and stores a tenant's key. **Write-only**: there is no call that returns it; only the
    /// gateway's own call path opens it, and only into a zeroising [`ApiKey`].
    ///
    /// # Errors
    ///
    /// [`CallError::Config`] for an empty or oversized key, [`CallError::Vault`] when sealing
    /// fails, [`CallError::Store`] when the tenant has no configuration or the write fails.
    pub async fn set_key(&self, tenant: &str, key: &SecretString) -> Result<(), CallError> {
        check_tenant(tenant)?;
        if key.expose().is_empty() || key.expose().len() > MAX_KEY_BYTES {
            return Err(CallError::Config(ConfigProblem(
                "the key must be 1 to 4096 bytes",
            )));
        }
        let aad = key_aad(&self.deps.site, tenant);
        let sealed = self
            .deps
            .vault
            .seal(key, &aad)
            .await
            .map_err(|_| CallError::Vault)?;
        self.deps
            .store
            .put_key(tenant, &sealed, self.now_ms())
            .await?;
        if let Some(old) = self.forget(tenant) {
            self.flush(&old).await;
        }
        Ok(())
    }

    /// Deletes the tenant's stored key (the configuration is kept, and so are usage and records).
    /// Returns whether a key was there. The next call of the tenant is refused as
    /// [`CallError::KeyMissing`] (a provider that needs none, Ollama, is unaffected).
    ///
    /// # Errors
    ///
    /// [`CallError::Config`] for a bad tenant id, [`CallError::Store`] when the write fails.
    pub async fn remove_key(&self, tenant: &str) -> Result<bool, CallError> {
        check_tenant(tenant)?;
        let existed = self.deps.store.delete_key(tenant, self.now_ms()).await?;
        if let Some(old) = self.forget(tenant) {
            self.flush(&old).await;
        }
        Ok(existed)
    }

    /// Whether a key is stored for the tenant (never the key).
    ///
    /// # Errors
    ///
    /// [`CallError::Store`] when the read fails.
    pub async fn has_key(&self, tenant: &str) -> Result<bool, CallError> {
        Ok(self.deps.store.key(tenant).await?.is_some())
    }

    /// A prompt builder for `user`: documents enter only through the permission-checked reader,
    /// protected and sealed fields are stripped and the text is redacted.
    #[must_use]
    pub fn prompt(&self, user: &str, policy: PromptPolicy) -> PromptBuilder {
        PromptBuilder::new(
            Arc::clone(&self.deps.redactor),
            Arc::clone(&self.deps.reader),
            user,
            policy,
        )
    }

    /// The tenant's settled usage rows of the current budget period (the whole history when the
    /// tenant has no budget).
    ///
    /// # Errors
    ///
    /// [`CallError::NotConfigured`] and [`CallError::Store`].
    pub async fn usage(&self, tenant: &str) -> Result<Vec<UsageRow>, CallError> {
        let config = self
            .deps
            .store
            .config(tenant)
            .await?
            .ok_or(CallError::NotConfigured)?;
        let first = config
            .budget
            .map_or(0, |b| period_start_day(b.period, self.now_ms()));
        Ok(self.deps.store.usage_since(tenant, first).await?)
    }

    /// The tenant's send records, newest first.
    ///
    /// # Errors
    ///
    /// [`CallError::Store`] when the read fails.
    pub async fn sends(&self, tenant: &str, limit: u32) -> Result<Vec<SendRecord>, CallError> {
        Ok(self.deps.store.sends(tenant, limit).await?)
    }

    /// Runs one model call for `caller` on `tenant`'s `task`. `request` carries the conversation;
    /// its model and request id are replaced (the id becomes `tenant:task:<caller's id>`) and an
    /// output cap is added when it has none. `manifest` is what the prompt builder reported.
    ///
    /// # Errors
    ///
    /// A [`CallError`]; every refusal wrote a send record first.
    pub async fn complete(
        &self,
        tenant: &str,
        task: &str,
        caller: &Caller,
        mut request: Request,
        manifest: &Manifest,
    ) -> Result<Response, CallError> {
        check_tenant(tenant)?;
        check_task(task)?;
        let at_ms = self.now_ms();
        let caller_id = if request.request_id.is_empty() {
            format!("{at_ms}")
        } else {
            request.request_id.clone()
        };
        let request_id = format!("{tenant}:{task}:{caller_id}");
        let mut record = SendRecord {
            request_id: request_id.clone(),
            tenant: tenant.to_owned(),
            task: task.to_owned(),
            at_ms,
            principal: caller.user.clone(),
            purpose: caller.purpose.clone(),
            doctypes: manifest.doctypes.iter().cloned().collect(),
            classes: manifest.classes.clone(),
            ..SendRecord::default()
        };

        let runtime = match self.runtime_for(tenant).await {
            Ok(runtime) => runtime,
            Err(error) => {
                record.outcome = error.outcome();
                self.keep(&record).await;
                return Err(error);
            }
        };
        record.endpoint = runtime.origin.clone();
        record.provider = runtime.config.kind.provider_id().to_owned();
        record.model = runtime.config.model.clone();

        request.model = runtime.config.model.clone();
        request.request_id = request_id;
        if request.max_output_tokens.is_none() {
            request.max_output_tokens = Some(runtime.config.default_max_output_tokens);
        }
        match serde_json::to_vec(&request) {
            Ok(bytes) => {
                record.digest = sha256_hex(&bytes);
                record.size_bytes = bytes.len() as u64;
            }
            Err(_) => {
                record.outcome = "failed_encode".to_owned();
                self.keep(&record).await;
                return Err(CallError::Provider(LlmError::BadRequest {
                    detail: "the request could not be encoded".to_owned(),
                }));
            }
        }

        if let Err(error) = self.admit(&runtime, tenant, task, caller).await {
            record.outcome = error.outcome();
            self.keep(&record).await;
            return Err(error);
        }

        // From here bytes may leave: the record exists before they do.
        record.outcome = "sending".to_owned();
        self.deps.store.put_send(&record).await?;

        let result = runtime.router.complete_detail(request).await;
        let outcome = match result {
            Ok(response) => {
                record.outcome = "sent".to_owned();
                record.input_tokens = response.usage.input_total();
                record.output_tokens = response.usage.output_tokens;
                Ok(response)
            }
            Err(RouterError::BudgetExceeded { spent, limit, .. }) => {
                Err(CallError::BudgetExceeded {
                    spent_micro: spent,
                    limit_micro: limit,
                })
            }
            Err(RouterError::TaskUnrouted { .. }) => Err(CallError::TaskUnknown),
            Err(RouterError::RoutesExhausted { detail, .. }) => {
                // The detail starts with the last provider failure's `llm.*` code.
                let code = detail.split(':').next().unwrap_or_default();
                Err(CallError::Provider(LlmError::from_code(code).unwrap_or(
                    LlmError::Protocol {
                        detail: "no route answered".to_owned(),
                    },
                )))
            }
            Err(other) => Err(CallError::Provider(LlmError::Protocol {
                detail: other.code().to_owned(),
            })),
        };
        if let Err(error) = &outcome {
            record.outcome = error.outcome();
        }
        self.keep(&record).await;
        self.flush(&runtime).await;
        outcome
    }

    /// The checks that must pass before a byte can leave: task, allow-list, key, egress policy
    /// (after DNS) and the budget period.
    async fn admit(
        &self,
        runtime: &TenantRuntime,
        tenant: &str,
        task: &str,
        caller: &Caller,
    ) -> Result<(), CallError> {
        if !runtime.config.tasks.iter().any(|t| t == task) {
            return Err(CallError::TaskUnknown);
        }
        self.admit_send(runtime, tenant, caller).await
    }

    /// The checks every call that can send bytes passes, a completion or an embedding: the
    /// allow-list, the key, the egress policy (after DNS) and the budget period.
    async fn admit_send(
        &self,
        runtime: &TenantRuntime,
        tenant: &str,
        caller: &Caller,
    ) -> Result<(), CallError> {
        if !runtime.config.endpoint_allowed() {
            return Err(CallError::EndpointNotAllowed);
        }
        if runtime.config.kind.needs_key() && !self.has_key(tenant).await? {
            return Err(CallError::KeyMissing);
        }
        let guard = self.deps.guard.current();
        guard
            .authorize(&EgressRequest {
                url: runtime.url.clone(),
                purpose: Purpose::Llm,
                tenant: tenant.to_owned(),
                actor: Some(caller.user.clone()),
            })
            .await
            .map_err(|error| CallError::EndpointDenied(error.code()))?;
        self.roll_period(runtime).await
    }

    /// Installs the tenant's budget on the router's book for the current period and reads the
    /// period's spend back from the database; a new period starts from zero.
    async fn roll_period(&self, runtime: &TenantRuntime) -> Result<(), CallError> {
        let Some(budget) = runtime.config.budget else {
            return Ok(());
        };
        let start = period_start_day(budget.period, self.now_ms());
        let mut installed = runtime.period.lock().await;
        if *installed == start {
            return Ok(());
        }
        let tenant = runtime.config.tenant.clone();
        runtime.router.budgets().set_budget(Budget::new(
            BudgetScope::Tenant {
                tenant: tenant.clone(),
            },
            budget.limit_micro,
            budget.warn_at_percent,
        ));
        // Records still waiting in the outbox are in the database before it is read.
        self.flush(runtime).await;
        for row in self.deps.store.usage_since(&tenant, start).await? {
            runtime.router.budgets().settle(
                &BudgetContext::new(tenant.clone(), row.task.clone(), DayKey(row.day)),
                0,
                u128::from(row.cost_micro),
            );
        }
        *installed = start;
        Ok(())
    }

    /// The tenant's built stack, rebuilt when the egress policy was reloaded or the stack is older
    /// than [`RUNTIME_TTL_MS`].
    async fn runtime_for(&self, tenant: &str) -> Result<Arc<TenantRuntime>, CallError> {
        let now = self.now_ms();
        let guard = self.deps.guard.current();
        if let Some(runtime) = self.cached(tenant) {
            if Arc::ptr_eq(&runtime.guard, &guard)
                && now.saturating_sub(runtime.built_at_ms) < RUNTIME_TTL_MS
            {
                return Ok(runtime);
            }
            self.flush(&runtime).await;
        }
        let config = self
            .deps
            .store
            .config(tenant)
            .await?
            .ok_or(CallError::NotConfigured)?;
        let url = config.endpoint_url()?;
        if !config.endpoint_allowed() {
            return Err(CallError::EndpointNotAllowed);
        }
        config.validate()?;
        let key = if config.kind.needs_key() {
            let sealed = self
                .deps
                .store
                .key(tenant)
                .await?
                .ok_or(CallError::KeyMissing)?;
            let secret = self
                .deps
                .vault
                .open(&sealed, &key_aad(&self.deps.site, tenant))
                .await
                .map_err(|_| CallError::Vault)?;
            Some(ApiKey::from_secret(&secret))
        } else {
            None
        };
        let transport = self.deps.transports.transport(Arc::clone(&guard), tenant);
        let embedder = if config.embedding_model.is_some() {
            self.deps
                .providers
                .build_embedder(&config, key.clone(), Arc::clone(&transport))
        } else {
            None
        };
        let provider = self.deps.providers.build(&config, key, transport);
        let provider_id = config.kind.provider_id();
        let price = Price::new(
            config.price.input_micro_per_million,
            config.price.output_micro_per_million,
        );
        let chain = vec![Route::new(provider_id, config.model.clone())];
        let mut routes = BTreeMap::new();
        for task in &config.tasks {
            routes.insert(task.clone(), chain.clone());
        }
        let mut providers: BTreeMap<String, Arc<dyn Provider>> = BTreeMap::new();
        providers.insert(provider_id.to_owned(), provider);
        let ledger = Arc::new(CapturingLedger::new());
        let warn_tenant = tenant.to_owned();
        let router = Router::new(
            providers,
            RoutingTable::new(routes),
            Arc::clone(&ledger) as Arc<dyn spot_llm_router::UsageLedger>,
            PriceTable::new().with_price(provider_id, config.model.clone(), price),
            spot_llm_router::BudgetBook::new(),
        )
        .with_config(RouterConfig::default().with_max_retries(self.deps.max_retries))
        .with_clock(Arc::new(RouterClock(Arc::clone(&self.deps.clock))))
        .with_warning_callback(Arc::new(
            move |warning: &spot_llm_router::BudgetWarning| {
                tracing::warn!(
                    tenant = %warn_tenant,
                    percent = warning.percent,
                    "llm: a tenant's budget crossed its warning level"
                );
            },
        ));
        let runtime = Arc::new(TenantRuntime {
            origin: origin_of(&url),
            url,
            price,
            router,
            ledger,
            embedder,
            guard,
            built_at_ms: now,
            period: Mutex::new(0),
            config,
        });
        self.runtimes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(tenant.to_owned(), Arc::clone(&runtime));
        Ok(runtime)
    }

    /// Writes the send record; a failure is logged (code only) and never hides the call's own
    /// outcome.
    async fn keep(&self, record: &SendRecord) {
        if self.deps.store.put_send(record).await.is_err() {
            tracing::error!(
                tenant = %record.tenant,
                outcome = %record.outcome,
                "llm: a send record could not be written"
            );
        }
    }

    /// Writes the usage the router settled and not yet stored; what fails is kept for the next
    /// flush.
    async fn flush(&self, runtime: &TenantRuntime) {
        let records = runtime.ledger.drain();
        if records.is_empty() {
            return;
        }
        for (index, record) in records.iter().enumerate() {
            let row = UsageRow {
                tenant: record.tenant.clone(),
                task: record.task.clone(),
                day: record.at / 86_400_000,
                provider: record.provider.clone(),
                model: record.model.clone(),
                input: record.input,
                output: record.output,
                cache_read: record.cache_read,
                cache_write: record.cache_write,
                requests: 1,
                cost_micro: record_cost(record, runtime.price),
            };
            if self.deps.store.add_usage(&row).await.is_err() {
                tracing::error!(
                    tenant = %record.tenant,
                    "llm: usage could not be written; it is kept for the next flush"
                );
                runtime
                    .ledger
                    .requeue(records.get(index..).unwrap_or_default().to_vec());
                return;
            }
        }
    }

    /// Writes every tenant's pending usage; the service loop calls it, and shutdown does.
    pub async fn flush_all(&self) {
        let runtimes: Vec<Arc<TenantRuntime>> = self
            .runtimes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for runtime in runtimes {
            self.flush(&runtime).await;
        }
    }

    /// Deletes the send records past the retention period; returns how many went.
    ///
    /// # Errors
    ///
    /// [`CallError::Store`] when the delete fails.
    pub async fn purge_old(&self) -> Result<u64, CallError> {
        let keep_ms = u64::from(self.deps.retention_days).saturating_mul(86_400_000);
        let cutoff = self.now_ms().saturating_sub(keep_ms);
        Ok(self.deps.store.purge_sends_before(cutoff).await?)
    }

    /// The readiness probe.
    ///
    /// # Errors
    ///
    /// [`CallError::Store`] when the tables do not answer.
    pub async fn probe(&self) -> Result<(), CallError> {
        Ok(self.deps.store.probe().await?)
    }
}
