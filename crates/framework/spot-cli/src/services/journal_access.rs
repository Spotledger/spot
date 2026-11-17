//! The journal access surface hosted (WP-JRN-09.T4, ADR-061 section 7 surface 2): the
//! `journal_reader` grant model, `journal_info_get`, `journal_manifest_get` and
//! `journal_segment_get` over `spot-journal-access`. Operator guide:
//! `docs/running-spot/journal_access.md`.
//!
//! # What is hosted
//!
//! - **Grants**: [`PgGrantStore`] over the site's pool (table `spot_journal_grant`, created by
//!   `spot migrate`; the service refuses to start without it) and the `sys_journal_grant` events
//!   written in the grant's own transaction.
//! - **Who is an administrator**: the site's permission snapshot, now ([`LiveAdmins`]): a caller
//!   holding the `Tenant Administrator` role. A grant is void the moment its grantor loses it.
//! - **The routes**: mounted here, over [`Adapter`], the production
//!   [`JournalAccessPort`]. The adapter authenticates the caller with the site's sign-in service
//!   (a caller who must change their password first may do nothing here), then asks
//!   [`JournalReader`], which gates on the grant, the signed manifest and the limits and writes
//!   the `sys_journal_access` record of every call ([`PgAccessAudit`]).
//! - **Streaming**: a segment body is the library's bounded-chunk stream; its access record is
//!   written when the stream ends (a stream the client abandons is recorded as `failed`).
//!
//! # What it needs from the journal service
//!
//! Two ports the journal service (WP-RTB-01.T3) provides: the site's `dyn JournalStore` and a
//! [`ManifestKeys`] (the tenant's public manifest keys). Without either the journal is not
//! hosted on this site and the module registers nothing: the three operations answer `501
//! service.not_configured`. See the task report: the journal service had not landed when this
//! was written, so the providers of these two ports are the wiring still owed.
//!
//! # Level and settings
//!
//! Required once it registers: the grant table must exist. Settings are `services.journal_access`
//! (`enabled`, `max_grant_days`, `max_concurrent_streams`, `daily_byte_budget`, `policy`,
//! `region`); unknown keys are refused.
//!
//! # Audit
//!
//! Every call is an outbox event of class Audit (`sys_journal_access`), denied and failed calls
//! included; if it cannot be written the call is refused with `503`. Never a token, a key or a
//! segment byte.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use axum::body::Bytes;
use serde::Deserialize;
use serde_json::Value as Json;
use spot_core::Clock;
use spot_document::engine::PrincipalSource as DocPrincipals;
use spot_http::api::codec::parse_record_id;
use spot_http::api::files::{BoxByteStream, ByteStream};
use spot_http::api::ports::journal_access::{
    JournalAccessError, JournalAccessPort, JournalGeneration, JournalInfo, SegmentDownload,
    SegmentStatus as WireStatus,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::api::{Ports, RouteState};
use spot_journal_access::pg::{PgAccessAudit, PgGrantStore};
use spot_journal_access::{
    Access, AccessError, AdminDirectory, Fut, IdSource, JournalMeta, JournalReader, Limits,
    ReadError, ReaderParts, SegmentStatus, SegmentStream, Settings as AccessSettings, TimeSource,
    rfc3339,
};
use spot_journal_manifest::VerifyingKeys;
use spot_journal_store::JournalStore;
use spot_permissions::TENANT_ADMINISTRATOR;
use spot_storage_contract::RecordId;

use crate::audit::PathClass;
use crate::live::SiteLive;
use crate::services::{Idle, Level, SiteCtx};
use crate::{Error, Result};

/// Start order: after the framework's own services and the journal service they read from.
pub const ORDER: u32 = 1210;

/// Runs on a hub only (the row of `SERVICE_SETS`).
pub const ROLES: &[&str] = &["hub"];

/// The tenant's public manifest keys, provided as a port by the journal service. The reader
/// verifies every manifest it serves from against these.
pub trait ManifestKeys: Send + Sync + 'static {
    /// The keys with their generation windows, or why they cannot be read.
    ///
    /// # Errors
    /// A sentence without key material.
    fn keys(&self) -> std::result::Result<VerifyingKeys, String>;
}

/// `[services.journal_access]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `false` leaves the surface off.
    pub enabled: bool,
    /// The longest a grant may live, in days (1 to 365; default 30).
    pub max_grant_days: u32,
    /// Simultaneous segment streams per principal.
    pub max_concurrent_streams: u32,
    /// Bytes one principal may read per UTC day.
    pub daily_byte_budget: u64,
    /// The durability policy name `journal_info_get` shows.
    pub policy: String,
    /// The journal's region `journal_info_get` shows.
    pub region: String,
}

impl Default for Settings {
    fn default() -> Self {
        let base = AccessSettings::default();
        Self {
            enabled: true,
            max_grant_days: base.max_grant_days,
            max_concurrent_streams: base.max_concurrent_streams,
            daily_byte_budget: base.daily_byte_budget,
            policy: base.policy,
            region: base.region,
        }
    }
}

impl Settings {
    fn access(&self) -> AccessSettings {
        AccessSettings {
            max_grant_days: self.max_grant_days,
            max_concurrent_streams: self.max_concurrent_streams,
            daily_byte_budget: self.daily_byte_budget,
            policy: self.policy.clone(),
            region: self.region.clone(),
        }
    }
}

/// Whether a user holds the Tenant Administrator role in the permission snapshot served now.
struct LiveAdmins {
    live: Arc<SiteLive>,
}

impl AdminDirectory for LiveAdmins {
    fn is_tenant_administrator<'a>(&'a self, principal: &'a str) -> Fut<'a, bool> {
        Box::pin(async move {
            // A principal that is not a record id (the literal `system`, say) is no user.
            let Some(user) = parse_record_id(principal) else {
                return Ok(false);
            };
            let principals = self.live.principals().ok_or(AccessError::Store)?;
            let resolved = DocPrincipals::resolve(&*principals, &user).await;
            Ok(!resolved.is_system
                && resolved
                    .assignments
                    .iter()
                    .flat_map(|a| a.roles.iter())
                    .any(|role| role.as_str() == TENANT_ADMINISTRATOR))
        })
    }
}

struct NewIds;

impl IdSource for NewIds {
    fn new_id(&self) -> String {
        RecordId::new_v7().to_string()
    }
}

struct SiteClock(Arc<dyn Clock>);

impl TimeSource for SiteClock {
    fn now_millis(&self) -> Option<i64> {
        i64::try_from(self.0.now_utc().unix_timestamp_nanos() / 1_000_000).ok()
    }
}

/// Who the caller of a request is, as the site's user id (a record id's text).
pub trait Callers: Send + Sync + 'static {
    /// The caller's user id: `Unauthenticated` for a credential that does not authenticate,
    /// `Denied` for one that may not act yet, `Unavailable` when sign-in cannot answer.
    fn caller<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, std::result::Result<String, JournalAccessError>>;
}

/// The production [`Callers`]: the site's sign-in service.
struct LiveCallers {
    site: String,
    live: Arc<SiteLive>,
}

impl Callers for LiveCallers {
    /// A credential that does not authenticate is `Unauthenticated`; one that may only change
    /// its password is denied.
    fn caller<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, std::result::Result<String, JournalAccessError>> {
        Box::pin(async move {
            if ctx.site.as_str() != self.site {
                return Err(JournalAccessError::Denied { expired: false });
            }
            let auth = self.live.auth().ok_or(JournalAccessError::Unavailable)?;
            match auth.authenticate_on(ctx, PathClass::Api).await {
                Ok(who) => {
                    if auth.is_restricted(&who) {
                        Err(JournalAccessError::Denied { expired: false })
                    } else {
                        Ok(who.user_id.to_string())
                    }
                }
                Err(error) if error.status().as_u16() == 503 => {
                    Err(JournalAccessError::Unavailable)
                }
                Err(_) => Err(JournalAccessError::Unauthenticated),
            }
        })
    }
}

/// The [`JournalAccessPort`] of one site over `callers` and `reader`.
#[must_use]
pub fn port(callers: Arc<dyn Callers>, reader: JournalReader) -> Arc<dyn JournalAccessPort> {
    Arc::new(Adapter { callers, reader })
}

struct Adapter {
    callers: Arc<dyn Callers>,
    reader: JournalReader,
}

fn wire_error(error: ReadError) -> JournalAccessError {
    match error {
        ReadError::Denied { expired } => JournalAccessError::Denied { expired },
        ReadError::NotFound => JournalAccessError::NotFound,
        ReadError::RateLimited { retry_after_secs } => {
            JournalAccessError::RateLimited { retry_after_secs }
        }
        ReadError::AuditUnavailable | ReadError::Unavailable => JournalAccessError::Unavailable,
    }
}

impl Adapter {
    async fn principal(&self, ctx: &RequestCtx) -> std::result::Result<String, JournalAccessError> {
        self.callers.caller(ctx).await
    }
}

impl JournalAccessPort for Adapter {
    fn info<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, std::result::Result<JournalInfo, JournalAccessError>> {
        Box::pin(async move {
            let principal = self.principal(ctx).await?;
            let info = self.reader.info(&principal).await.map_err(wire_error)?;
            Ok(JournalInfo {
                generations: info
                    .generations
                    .iter()
                    .map(|g| JournalGeneration {
                        generation: g.generation,
                        first_seq: g.first_seq,
                        last_seq: g.last_seq,
                        sealed_until_seq: g.sealed_until_seq,
                        opened_at: rfc3339(g.opened_at_millis),
                    })
                    .collect(),
                policy: info.policy,
                region: info.region,
            })
        })
    }

    fn manifest<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        generation: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Json, JournalAccessError>> {
        Box::pin(async move {
            let principal = self.principal(ctx).await?;
            self.reader
                .manifest(&principal, generation)
                .await
                .map_err(wire_error)
        })
    }

    fn segment<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        generation: &'a str,
        range: &'a str,
        range_header: Option<&'a str>,
    ) -> BoxFuture<'a, std::result::Result<SegmentDownload, JournalAccessError>> {
        Box::pin(async move {
            let principal = self.principal(ctx).await?;
            let read = self
                .reader
                .segment(&principal, generation, range, range_header)
                .await
                .map_err(wire_error)?;
            let status = match read.status {
                SegmentStatus::Full => WireStatus::Full,
                SegmentStatus::Partial => WireStatus::Partial,
                SegmentStatus::RangeNotSatisfiable => WireStatus::RangeNotSatisfiable,
            };
            let body: Option<BoxByteStream> = read
                .stream
                .map(|stream| Box::pin(AuditedStream::new(stream)) as BoxByteStream);
            Ok(SegmentDownload {
                status,
                etag: read.etag,
                content_length: read.content_length,
                content_range: read.content_range,
                body,
            })
        })
    }
}

type Finishing = Pin<Box<dyn Future<Output = std::result::Result<(), ReadError>> + Send>>;

/// A segment body that writes the access record when it ends. If the record cannot be written
/// the stream ends in an error (the connection closes short: no unaudited complete read). A
/// stream dropped before its end is recorded as `failed`.
struct AuditedStream {
    inner: Option<SegmentStream>,
    finishing: Option<Finishing>,
}

impl AuditedStream {
    fn new(stream: SegmentStream) -> Self {
        Self {
            inner: Some(stream),
            finishing: None,
        }
    }
}

impl ByteStream for AuditedStream {
    fn poll_chunk(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Bytes, ApiError>>> {
        let this = &mut *self;
        loop {
            if let Some(finishing) = this.finishing.as_mut() {
                return match finishing.as_mut().poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Ok(())) => {
                        this.finishing = None;
                        Poll::Ready(None)
                    }
                    Poll::Ready(Err(_)) => {
                        this.finishing = None;
                        Poll::Ready(Some(Err(ApiError::from_code(Code::ServerUnavailable))))
                    }
                };
            }
            let Some(inner) = this.inner.as_mut() else {
                return Poll::Ready(None);
            };
            if let Some(chunk) = inner.next_chunk() {
                return Poll::Ready(Some(Ok(Bytes::from(chunk))));
            }
            match this.inner.take() {
                Some(stream) => this.finishing = Some(Box::pin(stream.finish())),
                None => return Poll::Ready(None),
            }
        }
    }
}

impl Drop for AuditedStream {
    fn drop(&mut self) {
        // Abandoned mid-stream (a disconnect): record it as failed, off the dropping thread.
        if let Some(stream) = self.inner.take()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            handle.spawn(async move {
                let _ = stream.finish().await;
            });
        }
    }
}

/// The adapters built per site, kept across a reload (a reload must not build a second one).
fn built() -> &'static Mutex<HashMap<String, Arc<dyn JournalAccessPort>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<dyn JournalAccessPort>>>> = OnceLock::new();
    BUILT.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Mounts the three routes over a state with no port: each answers `501 service.not_configured`
/// (never a `200` with an empty history), as the other optional services do.
fn not_configured(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let stub = routes::journal_access::routes().with_state(RouteState::new(Ports::new()));
    ctx.router_layer(move |base| base.merge(stub));
    Ok(())
}

/// Registers the journal access surface for the site.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("journal_access")?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site_name = env.site.site.clone();
    let adapter = if ctx.assembling() {
        let (Some(store), Some(keys)) = (
            ctx.port::<dyn JournalStore>(),
            ctx.port::<dyn ManifestKeys>(),
        ) else {
            tracing::debug!(
                "journal_access: the journal is not hosted on this site; the routes answer 501"
            );
            return not_configured(ctx);
        };
        let keys = keys
            .keys()
            .map_err(|why| Error(format!("journal_access: the manifest keys: {why}")))?;
        let catalog = Arc::clone(&env.permission.catalog);
        let grants = Arc::new(PgGrantStore::new(env.backend.clone(), Arc::clone(&catalog)));
        let audit = Arc::new(PgAccessAudit::new(env.backend.clone(), catalog));
        let access_settings = settings.access();
        let access = Arc::new(
            Access::new(
                grants.clone(),
                Arc::new(LiveAdmins {
                    live: Arc::clone(env.live),
                }),
                Arc::new(NewIds),
                access_settings.clone(),
            )
            .map_err(|e| Error(format!("services.journal_access: {e}")))?,
        );
        let reader = JournalReader::new(ReaderParts {
            tenant: site_name.clone(),
            store,
            keys: Arc::new(keys),
            access,
            audit,
            limits: Arc::new(Limits::new(
                access_settings.max_concurrent_streams,
                access_settings.daily_byte_budget,
            )),
            clock: Arc::new(SiteClock(Arc::clone(ctx.clock()))),
            meta: JournalMeta {
                policy: access_settings.policy,
                region: access_settings.region,
            },
        })
        .map_err(|e| Error(format!("services.journal_access: {e}")))?;
        let check_store = Arc::clone(&grants);
        ctx.add(Idle::new("journal_access", Level::Required, move || {
            let store = Arc::clone(&check_store);
            Box::pin(async move {
                store.check().await.map_err(|_| {
                    "the table spot_journal_grant is missing or unreachable: run `spot migrate`"
                        .to_owned()
                })
            })
        }));
        let adapter = port(
            Arc::new(LiveCallers {
                site: site_name.clone(),
                live: Arc::clone(env.live),
            }),
            reader,
        );
        built()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(site_name, Arc::clone(&adapter));
        adapter
    } else {
        let built = built().lock().unwrap_or_else(|p| p.into_inner());
        let Some(adapter) = built.get(&site_name).cloned() else {
            drop(built);
            return not_configured(ctx);
        };
        adapter
    };
    ctx.provide::<dyn JournalAccessPort>(Arc::clone(&adapter))?;
    let mut ports = Ports::new();
    ports.provide::<dyn JournalAccessPort>(adapter);
    // Mounted here like the other hosted groups: the host authenticates every request before
    // the router sees it, and the port authenticates again.
    let mounted = routes::mount(&[routes::journal_access::registration()], &ports, &[]);
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the journal access routes were not mounted"
        );
    }
    let router = mounted.authenticated;
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}
