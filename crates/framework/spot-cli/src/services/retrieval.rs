//! The retrieval service (WP-AI-10.T3; blueprint WS13, ADR-070 sections 6 and 12): the outbox-fed
//! indexer, the index policy per DocType, the embedding seam behind the LLM service, and the
//! permission-aware `retrieve` tool. Operator guide: `docs/running-spot/retrieval.md`.
//!
//! # What it does
//!
//! `spot-retrieval` is the logic (chunking, the indexer, the retriever that re-checks every hit)
//! and `spot-retrieval-pg` is the durable pgvector store. This module hosts them on a site:
//!
//! - **The indexer** ([`pump`]) runs on the node that holds the `retrieval_indexer` coordinator
//!   lease. It reads the site's change feed from a durable cursor, loads each changed document
//!   through the platform's `SystemReader` port, chunks the policy's fields, embeds the chunks
//!   through the [`ports::EmbedPort`] and replaces the document's chunks in the store. A delete
//!   removes them. The cursor moves only after the store accepted the batch.
//! - **The index policy** (`index_policy`, inline TOML) names the DocTypes and fields that are
//!   indexed. **Default deny:** a DocType without an entry is never indexed, and a field the
//!   permission rules protect is left out (so it never reaches an embedding request) unless the
//!   policy lists it under `restricted_ok` **and** `index_protected = true`.
//! - **The `retrieve` tool** ([`tool`]) answers the calling user. Every hit is re-checked at read
//!   time against the live permission core as that user and against the field guard on the
//!   `Search` channel before it leaves; a permission revoked a moment ago applies to the next call.
//! - **The backfill job** `platform:retrieval.backfill` resets the cursor and replays the feed.
//!
//! # Off by default, per tenant
//!
//! Nothing is created, opened or started unless `enabled = true` **and** the site is listed in
//! `tenants`: no table, no connection, no thread, no cursor. A tenant's chunks live in its own
//! rows (the tenant is part of every key) of the retrieval table of that site's database.
//!
//! # Settings (`[services.retrieval]`)
//!
//! `enabled` (false), `tenants` (empty), `provider` (the embedding provider the LLM service
//! resolves), `dimensions` (the model's vector width, required), `batch_size` (32), `page` (200),
//! `interval_ms` (1000), `index` (`"hnsw"` or `"ivfflat"`), `ivfflat_lists`, `index_policy`,
//! `index_protected` (false), `max_document_chars`, `max_query_chars`, `max_hits`, `snippet_chars`.
//! Unknown keys and out-of-range values stop the site from starting, naming the key.
//!
//! # Level
//!
//! **Optional.** If pgvector is missing on the server, the retrieval table cannot be created, or
//! the embedding port (the LLM service) or the platform ports are absent, the site serves and
//! readiness says `degraded: retrieval (<code>: <fix>)`; the `retrieve` tool answers
//! `mcp.not_supported` and nothing is indexed. A running indexer that is failing reports
//! `degraded` too and retries; it never loses a change.
//!
//! # What this module does not do
//!
//! It calls no model, URL or socket itself: embeddings go through the LLM service. It has no
//! fake embedder: without the port it degrades. It never runs a query as the system: the tool
//! needs the caller's own identity.

#[path = "retrieval/access.rs"]
pub mod access;
#[path = "retrieval/ports.rs"]
pub mod ports;
#[path = "retrieval/pump.rs"]
pub mod pump;
#[path = "retrieval/tool.rs"]
pub mod tool;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;
use serde_json::Value as Json;
use spot_core::SiteId;
use spot_mcp::principal::McpPrincipal;
use spot_mcp::{BoxFuture, ExtensionError, ExtensionSpec, ExtensionTool};
use spot_postgres::PgConfig;
use spot_retrieval::VectorStore;
use spot_retrieval_pg::{IndexKind, PgVectorStore, PrepareError};
use tokio::sync::watch;

use self::access::{LiveGuards, LivePolicy, LiveProtection, LoaderAccess};
use self::ports::{EmbedPort, FilePositions};
use self::pump::{Pump, PumpDeps, Shared};
use self::tool::{Limits, RetrieveTool, ToolDeps};
use crate::services::mcp::McpHandle;
use crate::services::ports::{LiveLoader, OutboxLease, SystemReader};
use crate::services::{CheckFuture, Idle, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the LLM service (1260), whose port it takes, and the platform ports.
pub const ORDER: u32 = 1270;

/// The job that asks the indexer to replay the feed from the start.
pub const BACKFILL_JOB: &str = "platform:retrieval.backfill";

fn default_tenants() -> Vec<String> {
    Vec::new()
}
fn default_batch() -> usize {
    32
}
fn default_page() -> u32 {
    200
}
fn default_interval() -> u64 {
    1_000
}
fn default_max_document() -> usize {
    spot_retrieval::DEFAULT_MAX_DOCUMENT_CHARS
}
fn default_max_query() -> usize {
    1_000
}
fn default_max_hits() -> usize {
    20
}
fn default_snippet() -> usize {
    spot_retrieval::DEFAULT_SNIPPET_CHARS
}

/// Which approximate index the table carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexMethod {
    /// Hierarchical navigable small world (the default).
    #[default]
    Hnsw,
    /// Inverted file with flat lists (`ivfflat_lists`).
    Ivfflat,
}

/// `[services.retrieval]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The master switch. Off by default: nothing is created.
    pub enabled: bool,
    /// The sites (tenants) retrieval is on for. A site not listed runs none of it.
    #[serde(default = "default_tenants")]
    pub tenants: Vec<String>,
    /// The embedding provider name the LLM service resolves for this tenant.
    pub provider: String,
    /// The width of the provider's vectors; fixed for the life of the index table.
    pub dimensions: usize,
    /// The most texts one embedding call carries.
    pub batch_size: usize,
    /// The most changes one pump pass reads.
    pub page: u32,
    /// The pause between passes, in milliseconds.
    pub interval_ms: u64,
    /// The index method.
    pub index: IndexMethod,
    /// The `lists` of an IVFFlat index.
    pub ivfflat_lists: u32,
    /// The index policy, inline TOML (see `spot_retrieval::IndexPolicy`). Empty indexes nothing.
    pub index_policy: String,
    /// Whether a protected field the policy lists under `restricted_ok` is indexed.
    pub index_protected: bool,
    /// The most characters one document may hold before it is skipped as too large.
    #[serde(default = "default_max_document")]
    pub max_document_chars: usize,
    /// The longest query text, in characters.
    #[serde(default = "default_max_query")]
    pub max_query_chars: usize,
    /// The most hits one call returns.
    #[serde(default = "default_max_hits")]
    pub max_hits: usize,
    /// The snippet length, in characters.
    #[serde(default = "default_snippet")]
    pub snippet_chars: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            tenants: default_tenants(),
            provider: String::new(),
            dimensions: 0,
            batch_size: default_batch(),
            page: default_page(),
            interval_ms: default_interval(),
            index: IndexMethod::Hnsw,
            ivfflat_lists: 100,
            index_policy: String::new(),
            index_protected: false,
            max_document_chars: default_max_document(),
            max_query_chars: default_max_query(),
            max_hits: default_max_hits(),
            snippet_chars: default_snippet(),
        }
    }
}

impl Settings {
    /// `Err(key)` for a value out of range. Only checked when the service is enabled.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if !(1..=spot_retrieval_pg::MAX_DIM).contains(&self.dimensions) {
            return Err("dimensions");
        }
        if self.provider.trim().is_empty() || self.provider.len() > 64 {
            return Err("provider");
        }
        if !(1..=256).contains(&self.batch_size) {
            return Err("batch_size");
        }
        if !(1..=500).contains(&self.page) {
            return Err("page");
        }
        if !(100..=3_600_000).contains(&self.interval_ms) {
            return Err("interval_ms");
        }
        if self.index == IndexMethod::Ivfflat
            && !(1..=spot_retrieval_pg::MAX_LISTS).contains(&self.ivfflat_lists)
        {
            return Err("ivfflat_lists");
        }
        if !(1_000..=100_000_000).contains(&self.max_document_chars) {
            return Err("max_document_chars");
        }
        if !(1..=8_192).contains(&self.max_query_chars) {
            return Err("max_query_chars");
        }
        if !(1..=100).contains(&self.max_hits) {
            return Err("max_hits");
        }
        if !(20..=2_000).contains(&self.snippet_chars) {
            return Err("snippet_chars");
        }
        if self.tenants.iter().any(|tenant| tenant.trim().is_empty()) {
            return Err("tenants");
        }
        Ok(())
    }

    /// Whether `site` is one of the tenants retrieval is on for.
    #[must_use]
    pub fn covers(&self, site: &str) -> bool {
        self.enabled && self.tenants.iter().any(|tenant| tenant == site)
    }

    /// The limits the tool applies.
    #[must_use]
    pub fn limits(&self) -> Limits {
        Limits {
            provider: self.provider.clone(),
            dimensions: self.dimensions,
            max_query_chars: self.max_query_chars,
            max_hits: self.max_hits,
            snippet_chars: self.snippet_chars,
        }
    }

    fn index_kind(&self) -> IndexKind {
        match self.index {
            IndexMethod::Hnsw => IndexKind::Hnsw,
            IndexMethod::Ivfflat => IndexKind::Ivfflat {
                lists: self.ivfflat_lists,
            },
        }
    }
}

/// Runtimes the stores block on, kept alive for the life of the process: a `PgVectorStore` owns an
/// `Arc<Runtime>`, and dropping the last reference inside an async context panics. A runtime is
/// two idle threads per enabled site and is reclaimed when the process exits.
fn keep_alive(runtime: &Arc<tokio::runtime::Runtime>) {
    static KEPT: OnceLock<Mutex<Vec<Arc<tokio::runtime::Runtime>>>> = OnceLock::new();
    KEPT.get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(Arc::clone(runtime));
}

/// Builds the pgvector store for `site` on the site's database.
///
/// # Errors
///
/// A short, fixed message when the runtime or the pool cannot be built (for example the site's
/// database requires TLS, which this store does not open).
pub fn build_store(
    config: &PgConfig,
    site: &SiteId,
    settings: &Settings,
) -> std::result::Result<Arc<PgVectorStore>, String> {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("spot-retrieval")
            .enable_all()
            .build()
            .map_err(|_| {
                "retrieval.runtime_unavailable: the store runtime could not start".to_owned()
            })?,
    );
    keep_alive(&runtime);
    PgVectorStore::connect(
        config,
        site.clone(),
        settings.dimensions,
        settings.index_kind(),
        runtime,
    )
    .map(Arc::new)
    .map_err(|error| format!("{}: the vector store could not be opened", error.code()))
}

/// The service: the start-up check and the indexer loop.
struct RetrievalService {
    store: Arc<PgVectorStore>,
    pump: Pump<spot_postgres::PgBackend>,
    shared: Arc<Shared>,
    /// The MCP service's handle, when it runs: the `retrieve` tool is taken off it at shutdown.
    mcp: Option<Arc<McpHandle>>,
}

/// `retrieve` as the MCP service lists it: only while the service is ready and its last indexing
/// pass did not fail (contract `mcp.md`: an extension tool is absent while its service is off or
/// degraded), and a call that arrives after that is a tool error.
struct Offered {
    tool: Arc<RetrieveTool>,
    shared: Arc<Shared>,
}

impl Offered {
    fn available(&self) -> bool {
        self.shared.degraded().is_none()
    }
}

impl ExtensionTool for Offered {
    fn name(&self) -> &str {
        tool::NAME
    }

    fn spec(&self, principal: &McpPrincipal) -> Option<ExtensionSpec> {
        if !self.available() {
            return None;
        }
        ExtensionTool::spec(self.tool.as_ref(), principal)
    }

    fn call<'a>(
        &'a self,
        principal: &'a McpPrincipal,
        arguments: &'a Json,
    ) -> BoxFuture<'a, std::result::Result<Json, ExtensionError>> {
        if !self.available() {
            return Box::pin(async {
                Err(ExtensionError::failed(
                    "server.unavailable",
                    "retrieval is not available on this site",
                ))
            });
        }
        ExtensionTool::call(self.tool.as_ref(), principal, arguments)
    }
}

impl Service for RetrievalService {
    fn name(&self) -> &'static str {
        "retrieval"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let store = Arc::clone(&self.store);
            let outcome = tokio::task::block_in_place(|| store.prepare());
            match outcome {
                Ok(()) => {
                    self.shared.ready.store(true, Ordering::SeqCst);
                    Ok(())
                }
                Err(PrepareError::ExtensionMissing) => Err(format!(
                    "retrieval.pgvector_missing: {}",
                    PrepareError::ExtensionMissing
                )),
                Err(error) => Err(format!("retrieval.store_unavailable: {error}")),
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move { self.pump.run(stop).await })
    }

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            self.shared.ready.store(false, Ordering::SeqCst);
            if let Some(mcp) = &self.mcp {
                mcp.uninstall_tool(tool::NAME);
            }
            Ok(())
        })
    }

    fn degraded(&self) -> Option<String> {
        self.shared.degraded()
    }
}

/// A degraded entry: the module is on for this site but cannot serve, for `reason`.
fn degraded(ctx: &mut SiteCtx<'_>, reason: &'static str) {
    ctx.add(Idle::new("retrieval", Level::Optional, move || {
        Box::pin(async move { Err(reason.to_owned()) })
    }));
}

/// A degraded entry with a sentence built at run time.
fn degraded_with(ctx: &mut SiteCtx<'_>, reason: String) {
    ctx.add(Idle::new("retrieval", Level::Optional, move || {
        let reason = reason.clone();
        Box::pin(async move { Err(reason) })
    }));
}

/// Registers retrieval for a site that turned it on: the service, the backfill job and the tool.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("retrieval")?;
    if !settings.enabled {
        return Ok(());
    }
    settings
        .validate()
        .map_err(|key| Error(format!("services.retrieval.{key}: out of range")))?;
    let env = *ctx.env()?;
    let site = SiteId::new(&env.site.site).map_err(crate::error)?;
    if !settings.covers(site.as_str()) || !ctx.assembling() {
        return Ok(());
    }
    let Some(embed) = ctx.port::<dyn EmbedPort>() else {
        degraded(
            ctx,
            "retrieval.embedder_missing: no embedding port is provided (turn the LLM service on and configure an embedding provider for this tenant), so nothing is indexed and nothing is retrieved",
        );
        return Ok(());
    };
    let (Some(lease), Some(reader)) = (
        ctx.port::<dyn OutboxLease>(),
        ctx.port::<dyn SystemReader>(),
    ) else {
        degraded(
            ctx,
            "retrieval.ports_missing: the site's platform ports (coordinator lease, document reader) are not available",
        );
        return Ok(());
    };
    let store = match build_store(env.backend.config(), &site, &settings) {
        Ok(store) => store,
        Err(reason) => {
            tracing::error!(target: "spot::retrieval", site = %site.as_str(), %reason, "retrieval stays off");
            degraded(
                ctx,
                "retrieval.store_unavailable: the vector store could not be opened (see the log)",
            );
            return Ok(());
        }
    };
    let live = Arc::clone(env.live);
    let shared = Shared::new();
    let cursor = Arc::new(FilePositions::new(
        env.root
            .join("sites")
            .join(site.as_str())
            .join("retrieval.cursor"),
    ));
    let pump = Pump::new(PumpDeps {
        site: site.clone(),
        feed: env.backend.clone(),
        lease,
        reader,
        embed: Arc::clone(&embed),
        store: Arc::clone(&store) as Arc<dyn VectorStore>,
        cursor,
        protection: Arc::new(LiveProtection::new(Arc::clone(&live))),
        policy: Arc::new(LivePolicy::new(
            Arc::clone(&live),
            settings.index_policy.clone(),
        )),
        clock: Arc::clone(ctx.clock()),
        settings: settings.clone(),
        shared: Arc::clone(&shared),
    });
    let job_shared = Arc::clone(&shared);
    ctx.job_handlers().register(BACKFILL_JOB, move |_job| {
        let shared = Arc::clone(&job_shared);
        Box::pin(async move {
            // Only the lease holder resets the cursor; another node asks the job store to retry,
            // which reaches a worker on the holder.
            Ok(if shared.leader.load(Ordering::SeqCst) {
                shared.backfill.store(true, Ordering::SeqCst);
                spot_jobs::worker::HandlerResult::Ok
            } else {
                spot_jobs::worker::HandlerResult::Retry(
                    "this node does not hold the retrieval indexer lease".to_owned(),
                )
            })
        })
    });
    let tool = Arc::new(RetrieveTool::new(ToolDeps {
        site: site.clone(),
        store: Arc::clone(&store) as Arc<dyn VectorStore>,
        embed,
        access: Arc::new(LoaderAccess::new(
            Arc::new(LiveLoader(Arc::clone(&live))),
            site,
            Arc::clone(ctx.clock()),
        )),
        guards: Arc::new(LiveGuards::new(live, Arc::clone(ctx.clock()))),
        ready: Arc::clone(&shared.ready),
        limits: settings.limits(),
    }));
    // The MCP service is absent when it is off: nothing to install, not an error. A reload builds
    // this module again, so the tool is taken off first; a name another service holds is a
    // refusal that degrades this service and names the tool.
    let mcp = ctx.port::<McpHandle>();
    if let Some(mcp) = &mcp {
        mcp.uninstall_tool(tool::NAME);
        let offered = Offered {
            tool: Arc::clone(&tool),
            shared: Arc::clone(&shared),
        };
        if let Err(error) = mcp.install_tool(Arc::new(offered)) {
            degraded_with(
                ctx,
                format!(
                    "retrieval.tool_refused: the tool `{}` could not be added to the MCP service ({error})",
                    tool::NAME
                ),
            );
            return Ok(());
        }
    }
    ctx.add(Arc::new(RetrievalService {
        store,
        pump,
        shared,
        mcp,
    }));
    ctx.provide(tool)
        .map_err(|e| Error(format!("services.retrieval: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn on() -> Settings {
        Settings {
            enabled: true,
            tenants: vec!["acme".to_owned()],
            provider: "p".to_owned(),
            dimensions: 8,
            ..Settings::default()
        }
    }

    #[test]
    fn retrieval_is_off_by_default_and_per_tenant() {
        let parsed: Settings = toml::from_str("").unwrap();
        assert!(!parsed.enabled && parsed.tenants.is_empty());
        assert!(!parsed.covers("acme"));
        let enabled: Settings = toml::from_str(
            "enabled = true\ntenants = [\"acme\"]\nprovider = \"p\"\ndimensions = 8\n",
        )
        .unwrap();
        assert!(enabled.validate().is_ok());
        assert!(enabled.covers("acme"));
        assert!(
            !enabled.covers("globex"),
            "a site not listed runs none of it"
        );
        assert!(toml::from_str::<Settings>("enabled = true\nverify = 1\n").is_err());
    }

    #[test]
    fn out_of_range_settings_name_their_key() {
        assert_eq!(on().validate(), Ok(()));
        let cases = [
            (
                Settings {
                    dimensions: 0,
                    ..on()
                },
                "dimensions",
            ),
            (
                Settings {
                    dimensions: 5_000,
                    ..on()
                },
                "dimensions",
            ),
            (
                Settings {
                    provider: String::new(),
                    ..on()
                },
                "provider",
            ),
            (
                Settings {
                    batch_size: 0,
                    ..on()
                },
                "batch_size",
            ),
            (Settings { page: 501, ..on() }, "page"),
            (
                Settings {
                    interval_ms: 10,
                    ..on()
                },
                "interval_ms",
            ),
            (
                Settings {
                    max_hits: 0,
                    ..on()
                },
                "max_hits",
            ),
            (
                Settings {
                    index: IndexMethod::Ivfflat,
                    ivfflat_lists: 0,
                    ..on()
                },
                "ivfflat_lists",
            ),
            (
                Settings {
                    tenants: vec![" ".to_owned()],
                    ..on()
                },
                "tenants",
            ),
        ];
        for (settings, key) in cases {
            assert_eq!(settings.validate(), Err(key));
        }
    }

    #[test]
    fn the_index_kind_follows_the_setting() {
        assert_eq!(on().index_kind(), IndexKind::Hnsw);
        let ivfflat = Settings {
            index: IndexMethod::Ivfflat,
            ivfflat_lists: 64,
            ..on()
        };
        assert_eq!(ivfflat.index_kind(), IndexKind::Ivfflat { lists: 64 });
        let parsed: Settings = toml::from_str("index = \"ivfflat\"\n").unwrap();
        assert_eq!(parsed.index, IndexMethod::Ivfflat);
    }
}
