//! The event history service (WP-JRN-09.T3, ADR-061 sections 5 to 7,
//! `docs/running-spot/event_history.md`): the journal behind the event stream.
//!
//! # What it does
//!
//! A consumer whose checkpoint is older than the outbox horizon would get `410
//! eventstream.resync_required`. With a journal the history is still there: this module builds
//! the site's [`JournalReader`] (sealed segments verified against the signed manifest before
//! anything is served, then the unsealed tail chunks) over the same folder store, layout and key
//! provider the journal service ships to, and provides it as two ports:
//!
//! - `dyn JournalHistory`: the reader, for the stream engine's adapter;
//! - `dyn EventHistoryPort`: the question the routes ask ("does this tenant have history?").
//!
//! **Absent a journal** (`[services.journal]` without a target) nothing is provided, and older
//! checkpoints answer `410` exactly as before.
//!
//! # Per site, never shared
//!
//! The reader is built for the site's own tenant over a store that refuses any other prefix
//! ([`TenantScoped`]); a consumer of site A cannot read site B's journal.
//!
//! # Personal fields
//!
//! History is served as stored: personal fields are still sealed (or the erased placeholder). The
//! stream never reveals them; the read filter and the field guard run on journal-served events
//! exactly as on live ones, in the engine, not here.
//!
//! # What is not wired by this task
//!
//! `services/events.rs` (WP-INT-02.T4) builds the stream engine and must read the
//! `dyn JournalHistory` port, and `routes/events.rs` must call the selector and `served_from`
//! helpers of `spot-http`'s `event_history` port file: neither file is in this task's paths (see
//! the task report).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use serde::Deserialize;
// Public: the host-level surface `harness/rust/l3-journal-sync/tests/history.rs` needs (that crate
// owns no dependency on the event-stream crates of its own).
pub use spot_event_stream_journal::{JournalHistory, JournalReader, Selector};
pub use spot_events_contract::envelope::Position;
use spot_http::api::ports::event_history::EventHistoryPort;
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_journal_rebuild_plan::unsealed::{ChunkFault, TailChunkReader, TailEntry, TailPosition};
use spot_journal_relay::TailKeys;
use spot_journal_ship::JournalKeys;
use spot_journal_store::{FolderStore, TailKey};
use spot_secrets::KeyProvider;

use crate::services::SiteCtx;
use crate::services::journal::{Settings as JournalSettings, TenantScoped};
use crate::services::secrets::SecretsHost;
use crate::{Error, Result};

/// After the journal module (1100).
pub const ORDER: u32 = 1140;

/// `[services.event_history]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// `false` turns history off while the journal stays on (older checkpoints are `410`).
    pub enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Decodes tail chunks of version 1 and 2 with the tenant journal key (the relay's own decoder).
///
/// The decoder trait is synchronous and the relay's decoder is not (it asks the key provider), so
/// each call blocks on the runtime it runs in: it needs a multi-thread runtime, which `spot serve`
/// is.
pub struct RelayTailReader {
    tenant: String,
    keys: TailKeys,
}

impl RelayTailReader {
    /// A decoder for `tenant`'s chunks.
    #[must_use]
    pub fn new(tenant: &str, keys: TailKeys) -> Self {
        Self {
            tenant: tenant.to_owned(),
            keys,
        }
    }
}

impl TailChunkReader for RelayTailReader {
    fn decode(
        &self,
        key: &TailKey,
        object_key: &str,
        bytes: &[u8],
    ) -> std::result::Result<Vec<TailEntry>, ChunkFault> {
        let handle =
            tokio::runtime::Handle::try_current().map_err(|_| ChunkFault { letter: 'c' })?;
        let decoded = tokio::task::block_in_place(|| {
            handle.block_on(spot_journal_relay::tail::decode(
                &self.tenant,
                key.generation,
                Some(&self.keys),
                object_key,
                bytes,
            ))
        });
        decoded
            .map(|chunk| {
                chunk
                    .entries
                    .into_iter()
                    .map(|(position, line)| TailEntry {
                        position: TailPosition {
                            generation: position.generation,
                            seq: position.seq,
                        },
                        line,
                    })
                    .collect()
            })
            .map_err(|error| ChunkFault {
                letter: error.rule().letter(),
            })
    }
}

/// Builds the site's reader over `store` (a store limited to the site's prefix).
///
/// # Errors
/// An unusable tenant id.
pub fn build_reader(
    store: Arc<TenantScoped<FolderStore>>,
    site: &str,
    keys: &JournalKeys,
) -> Result<Arc<dyn JournalHistory>> {
    let chunks: Arc<dyn TailChunkReader> = Arc::new(RelayTailReader::new(site, keys.tail_keys()));
    let reader = JournalReader::new(store, site, Arc::new(keys.verifying()), chunks)
        .map_err(|e| Error(format!("services.event_history: {}", e.code())))?;
    Ok(Arc::new(reader))
}

/// The routes' question, answered `true`: this site has a journal.
struct Present;

impl EventHistoryPort for Present {
    fn has_history<'a>(&'a self, _ctx: &'a RequestCtx) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
}

#[derive(Clone)]
struct Provided {
    history: Arc<dyn JournalHistory>,
    port: Arc<dyn EventHistoryPort>,
}

fn provided() -> &'static Mutex<HashMap<String, Provided>> {
    static PROVIDED: OnceLock<Mutex<HashMap<String, Provided>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// The history reader of a running site (for a harness).
#[must_use]
pub fn hosted(site: &str) -> Option<Arc<dyn JournalHistory>> {
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(site)
        .map(|p| Arc::clone(&p.history))
}

/// Registers the history ports. Off without a journal target or with `enabled = false`.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let site = ctx.label().to_owned();
    if !ctx.assembling() {
        let existing = provided()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&site)
            .cloned();
        if let Some(p) = existing {
            ctx.provide::<dyn JournalHistory>(p.history)?;
            ctx.provide::<dyn EventHistoryPort>(p.port)?;
        }
        return Ok(());
    }
    let own: Settings = ctx.settings_or_default("event_history")?;
    let journal: JournalSettings = ctx.settings_or_default("journal")?;
    let Some(resolved) = journal.resolve(&site)? else {
        return Ok(());
    };
    if !own.enabled {
        return Ok(());
    }
    let secrets = ctx.port::<SecretsHost>().ok_or_else(|| {
        Error("services.event_history needs [services.secrets]: the journal keys come from the site's key provider".to_owned())
    })?;
    let provider: Arc<dyn KeyProvider> = secrets.keyring().clone();
    let keys = JournalKeys::derive(provider)
        .await
        .map_err(|e| Error(format!("services.event_history: {}", e.code())))?;
    let folder = FolderStore::new(&resolved.target).map_err(|_| {
        Error("services.event_history: the journal folder cannot be opened".to_owned())
    })?;
    let store = TenantScoped::new(folder, &site).map_err(|_| {
        Error("services.event_history: the site is not a usable journal tenant".to_owned())
    })?;
    let history = build_reader(Arc::new(store), &site, &keys)?;
    let port: Arc<dyn EventHistoryPort> = Arc::new(Present);
    ctx.provide::<dyn JournalHistory>(Arc::clone(&history))?;
    ctx.provide::<dyn EventHistoryPort>(Arc::clone(&port))?;
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(site, Provided { history, port });
    Ok(())
}
