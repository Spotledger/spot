//! Conversations across organisations, hosted (WP-BPM-12.T7; ADR-080 section 5, ADR-058).
//!
//! `spot-bpm-conversations` lets a process send a message to another organisation and wait for the
//! reply over a tenant link, and `spot-bpm-conversations-pg` keeps the conversation records. This
//! module connects them to one site: it builds the runtime of the site's conversations, registers
//! the send and wait node handlers with the process engine, registers the `message_arrived`
//! trigger kind, takes the messages the link host delivers and drives the waits, the timeouts and
//! the requests an arrival starts.
//!
//! ```text
//!   link host (carriers, link stores)             this module
//!   ---------------------------------             -----------
//!   inbound message --> LinkMessageSink::receive --> rate/size bound
//!                                                    inbox (grant, direction, expiry, region,
//!                                                           revocation, signature) per message
//!                                                    contract gate (order, role, schema)
//!                                                    conversation record + notice
//!   notice feed <------------------------------------ driver pass: arrival -> start / wake
//!   outbox <--- LinkSender <--- send node             driver pass: timeout, delivery wait
//! ```
//!
//! # The seams
//!
//! The link host itself belongs to another hosting task. This module consumes it through
//! [`LinkResources`] (a port the link host provides) and offers [`LinkMessageSink`] (a port the
//! link host calls); both are described in [`seams`]. Without a [`LinkResources`] port the module
//! stays inert and says so in the log. [`memory`] implements the seam over in-memory stores and
//! carries messages between two sites of one process, which is what the tests run on.
//!
//! # Rules the module keeps
//!
//! - **Per-message checks.** The link is read from the host for every message. Grants per topic
//!   and direction, expiry, the peer-region requirement (`Link::allows_with_region`) and
//!   revocation are judged by the inbox before any Run is touched. A message from a revoked,
//!   expired, wrong-direction or wrong-region link is refused or parked with an audit row and
//!   starts or wakes nothing.
//! - **Identity and ids.** A reply wakes a waiting Run only if it names the cryptographic Run id
//!   **and** arrives on the link the conversation was opened on; a message from another link
//!   that names the id is dropped without an answer that confirms the id exists (the library
//!   gate, WP-BPM-12.T3; this module never answers from its own state).
//! - **Limits.** Per link: open conversations, quarantined inputs, messages per minute and
//!   payload size, each from `[services.process_conversations]`. Over a bound the message is not
//!   taken (the sender keeps it) or refused, an audit row is written and the health check warns;
//!   no table and no map grows without bound.
//! - **Timeouts and late replies.** A waiting node's time limit is taken by the driver pass; a
//!   reply after it is recorded and parked, not applied.
//! - **Principals.** A request started by a message, a wait woken by one and a timeout run as the
//!   service principal [`SERVICE_PRINCIPAL`], recorded under the log target
//!   `spot::audit::process_conversations`; no input selects it. A site's own send is the Run's.
//! - **Logs and audit.** Ids, rule codes and counts only: never a message body, a payload field,
//!   a token, a signature or a secret, and no id is a metric label.
//! - **Fail closed.** A conversation store, audit trail or link store that cannot answer is a
//!   failed receive: nothing is acknowledged and the sender keeps the message.
//!
//! # Threads
//!
//! The conversation store is synchronous and refuses a call from a runtime thread. Every store
//! call and every node handler of this module runs on a short-lived plain thread ([`offload`]).
//! The link stores belong to the link host, which must offer handles that are safe to call from
//! where [`LinkMessageSink::receive`] and the driver pass run (a runtime worker): the in-memory
//! ones are.
//!
//! # Configuration
//!
//! See the operator guide `docs/running-spot/process_conversations.md`. The table is
//! `[services.process_conversations]`; unknown keys are refused. The module is `Level::Optional`:
//! a missing identity key or a store that cannot answer degrades readiness, the site still
//! serves documents.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_bpm_contract::from_text as contract_from_text;
use spot_bpm_conversations::{
    ContractBook, ConversationHandlers, ConversationStore, Runtime, SendHandler, SharedSigner,
    WaitHandler,
};
use spot_bpm_conversations_pg::PgConversationStore;
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_engine::NodeHandler;
use spot_core::SiteId;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_tenant_link::{Party, SeedSigner};
use tokio::sync::watch;

use crate::services::ports::OutboxLease;
use crate::services::process::{HandlerBuilder, ProcessHost};
use crate::services::process_triggers::{SealedSecrets, SecretSource};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx, secrets::SecretsHost};
use crate::{Error, Result};

#[path = "process_conversations/host.rs"]
mod host;
#[path = "process_conversations/index.rs"]
mod index;
#[path = "process_conversations/memory.rs"]
pub mod memory;
#[path = "process_conversations/offload.rs"]
mod offload;
#[path = "process_conversations/seams.rs"]
mod seams;
#[path = "process_conversations/settings.rs"]
mod settings;

pub use host::{
    ConversationCore, CoreParts, CycleReport, Driver, SERVICE_PRINCIPAL, Stats, refusals,
};
pub use memory::{InProcessSite, SharedLinkStore, carry};
pub use offload::{CODE_THREAD, HostHandler, OffRuntime};
pub use seams::{ConfigPeers, LinkMessageSink, LinkResources};
pub use settings::{ContractSetting, PeerSetting, Settings};

/// Start order: after the process engine (1100), the node-kind modules (1110 to 1115) and the
/// trigger intake (1130), which this module's trigger kind sits beside.
pub const ORDER: u32 = 1180;

const CONSUMER: &str = "process_conversations";
/// The longest a contract file may be.
const MAX_CONTRACT_BYTES: u64 = 1024 * 1024;

/// The node handlers of the conversation steps, hosted: the engine runs a handler on a runtime
/// worker and the handlers call the conversation store, so each runs off the runtime thread.
#[must_use]
pub fn handlers(runtime: &Arc<Runtime>) -> Vec<Arc<dyn NodeHandler>> {
    vec![
        Arc::new(HostHandler::new(ConversationHandlers::new(Arc::clone(
            runtime,
        )))) as Arc<dyn NodeHandler>,
        Arc::new(HostHandler::new(SendHandler::new(Arc::clone(runtime)))),
        Arc::new(HostHandler::new(WaitHandler::new(Arc::clone(runtime)))),
    ]
}

/// Reads and checks the contracts of the settings into a book.
///
/// # Errors
/// A contract file that cannot be read, is larger than 1 MiB, does not parse or has a problem;
/// a role the contract does not declare. The error names the setting, never the file's text.
fn load_book(root: &std::path::Path, settings: &Settings) -> Result<ContractBook> {
    let mut book = ContractBook::new();
    for (name, setting) in &settings.contracts {
        let relative = settings::relative_path(&setting.path).ok_or_else(|| {
            Error(format!(
                "services.process_conversations.contracts.{name}.path: not a plain relative path"
            ))
        })?;
        let path = root.join(relative);
        let unusable = |why: &str| {
            Error(format!(
                "services.process_conversations.contracts.{name}: {why}"
            ))
        };
        let size = std::fs::metadata(&path)
            .map_err(|_| unusable("the file cannot be read"))?
            .len();
        if size > MAX_CONTRACT_BYTES {
            return Err(unusable("the file is larger than 1 MiB"));
        }
        let text =
            std::fs::read_to_string(&path).map_err(|_| unusable("the file cannot be read"))?;
        let contract =
            contract_from_text(&text).map_err(|_| unusable("the contract does not parse"))?;
        book.add(contract, setting.role.clone())
            .map_err(|problems| {
                Error(format!(
                    "services.process_conversations.contracts.{name}: the contract has {} problem(s)",
                    problems.len()
                ))
            })?;
    }
    Ok(book)
}

/// Runs `work` on a fresh plain thread and waits for it: the store and the bridge refuse to be
/// built or prepared on a runtime thread.
fn on_plain_thread<T, F>(name: &str, work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(work)
        .map_err(|_| Error(format!("the {name} thread did not start")))?
        .join()
        .map_err(|_| Error(format!("the {name} thread failed")))?
}

/// The site's identity key: a 32-byte seed opened from the sealed secret the settings name.
async fn open_identity(
    settings: &Settings,
    secrets: &dyn SecretSource,
    party: Party,
) -> Option<SharedSigner> {
    let reference = settings.identity.as_deref()?;
    let seed = secrets.secret(reference).await?;
    let seed: [u8; 32] = seed.as_slice().try_into().ok()?;
    Some(Arc::new(SeedSigner::from_seed(seed, party)) as SharedSigner)
}

struct ConversationService {
    core: Arc<ConversationCore>,
    process: Arc<ProcessHost>,
    definitions: Arc<PgDefinitionStore>,
    outbox: Arc<dyn OutboxLease>,
    fingerprint: Mutex<Option<String>>,
    broken: AtomicUsize,
}

impl ConversationService {
    /// Rebuilds the trigger index when the published set changed.
    async fn reload_index(&self) -> Result<()> {
        let loaded = index::load(&self.definitions).await?;
        {
            let mut known = self
                .fingerprint
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if known.as_deref() == Some(loaded.fingerprint.as_str()) {
                return Ok(());
            }
            *known = Some(loaded.fingerprint);
        }
        self.broken.store(loaded.broken, Ordering::Relaxed);
        self.core.set_index(loaded.index);
        Ok(())
    }

    async fn cycle(&self) -> Result<CycleReport> {
        self.reload_index().await?;
        let engine = self.process.engine()?;
        Driver::new(Arc::clone(&self.core), engine)
            .cycle()
            .await
            .map_err(|e| Error(format!("the conversation pass failed: {}", e.code())))
    }

    /// Runs the driver as the lease holder; `true` means the node was stopped.
    async fn lead(
        &self,
        stop: &mut watch::Receiver<bool>,
        mut lease: crate::services::ports::ConsumerLease,
    ) -> bool {
        let tick = Duration::from_secs(self.core.settings().tick_secs);
        let mut ticker = tokio::time::interval(tick);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let stopped = loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break true;
                    }
                }
                _ = ticker.tick() => {
                    match self.outbox.renew(&lease).await {
                        Ok(renewed) => lease = renewed,
                        Err(_) => break false,
                    }
                    if !matches!(self.outbox.still_held(&lease).await, Ok(true)) {
                        break false;
                    }
                    if let Err(e) = self.cycle().await {
                        tracing::warn!(site = %self.core.site(), %e, "the conversation pass failed");
                    }
                }
            }
        };
        let _ = self.outbox.release(&lease).await;
        stopped
    }
}

impl Service for ConversationService {
    fn name(&self) -> &'static str {
        "process_conversations"
    }

    fn level(&self) -> Level {
        // A broken conversation service degrades the processes that talk to other
        // organisations; documents still serve.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.reload_index().await.map_err(|e| e.to_string())?;
            let mut warnings = self.core.warnings();
            let broken = self.broken.load(Ordering::Relaxed);
            if broken > 0 {
                warnings.push(format!(
                    "{broken} published process version(s) have triggers that do not build"
                ));
            }
            if warnings.is_empty() {
                Ok(())
            } else {
                Err(warnings.join("; "))
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let wait = Duration::from_secs(self.core.settings().tick_secs);
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                if let Ok(lease) = self.outbox.acquire(CONSUMER).await
                    && self.lead(&mut stop, lease).await
                {
                    return Ok(());
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(wait) => {}
                }
            }
        })
    }
}

/// Registers the conversation module: the runtime, the node handlers, the sink the link host
/// delivers to and the driver service. Inert when `services.process_conversations.enabled =
/// false`, when the process engine is off or when no link host provides [`LinkResources`].
///
/// # Errors
/// A setting that does not decode or is out of range; a contract that is not usable; a store that
/// cannot be built or prepared; a port another module already provided.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_conversations")?;
    settings.validate().map_err(Error)?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site_name = env.site.site.clone();
    let Some(process) = ctx.port::<ProcessHost>() else {
        tracing::info!(site = %site_name, "the process engine is off; no conversations");
        return Ok(());
    };
    let Some(links) = ctx.port::<dyn LinkResources>() else {
        tracing::info!(site = %site_name, "no link host; no conversations");
        return Ok(());
    };
    let site = SiteId::new(&site_name).map_err(|e| Error(e.to_string()))?;
    let book = load_book(env.root, &settings)?;

    let secrets: Arc<dyn SecretSource> = match ctx.port::<SecretsHost>() {
        Some(host) => Arc::new(SealedSecrets::new(
            Arc::clone(host.keyring()),
            settings.secrets.clone(),
        )),
        None => Arc::new(crate::services::process_triggers::NoSecrets),
    };
    let Some(signer) = open_identity(&settings, &*secrets, links.party()).await else {
        // The key is a precondition of every send and every receipt: without it the module is a
        // degraded service, not a half-wired one.
        tracing::error!(site = %site_name, "the link identity key is not available; conversations are off");
        ctx.add(crate::services::Idle::new(
            "process_conversations",
            Level::Optional,
            || {
                Box::pin(async {
                    Err(
                        "services.process_conversations.identity: no usable identity key"
                            .to_owned(),
                    )
                })
            },
        ));
        return Ok(());
    };

    let pool = env.backend.pool();
    let pool_size = pool.status().max_size;
    let largest = BridgeConfig::largest_in_flight(pool_size);
    if largest < 1 {
        return Err(Error(format!(
            "cli.process_conversations_pool: conversations need a PostgreSQL pool of at least 3 \
             connections (postgres.pool_max_size is {pool_size})"
        )));
    }
    let config = BridgeConfig {
        name: "process_conversations".to_owned(),
        max_in_flight: largest.min(4),
        workers: 2,
        queue_capacity: 16,
        default_deadline: Duration::from_secs(10),
        max_deadline: Duration::from_secs(30),
    };
    let backend = env.backend.clone();
    let bridge = Arc::new(on_plain_thread("conversation bridge", move || {
        Bridge::for_backend(config, &backend).map_err(|e| Error(e.to_string()))
    })?);
    let pg = Arc::new(
        PgConversationStore::new(Arc::clone(&bridge), &pool, site.as_str())
            .map_err(|e| Error(e.to_string()))?,
    );
    {
        let pg = Arc::clone(&pg);
        on_plain_thread("conversation tables", move || {
            pg.prepare().map_err(|e| Error(e.to_string()))
        })?;
    }
    let store: Arc<dyn ConversationStore> = Arc::new(OffRuntime(pg));

    let peers = Arc::new(ConfigPeers::new(
        &settings.peers,
        Arc::clone(&links),
        Arc::clone(&signer),
    ));
    let core = ConversationCore::new(CoreParts {
        site: site_name.clone(),
        clock: Arc::clone(ctx.clock()),
        settings: settings.clone(),
        book,
        store,
        peers,
        links,
        signer,
        gate_audit: None,
    });

    let builder = ctx.require_port::<HandlerBuilder>()?;
    for handler in handlers(core.runtime()) {
        builder.register(handler);
    }
    ctx.provide::<dyn LinkMessageSink>(Arc::clone(&core) as Arc<dyn LinkMessageSink>)?;

    let definitions = Arc::new(PgDefinitionStore::new(pool).with_tenant(site.as_str()));
    definitions
        .prepare()
        .await
        .map_err(|e| Error(format!("the definition store: {}", e.code())))?;
    let outbox = ctx.require_port::<dyn OutboxLease>()?;
    ctx.add(Arc::new(ConversationService {
        core,
        process,
        definitions,
        outbox,
        fingerprint: Mutex::new(None),
        broken: AtomicUsize::new(0),
    }));
    Ok(())
}
