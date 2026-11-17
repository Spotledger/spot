//! Process governance and process-first entry, hosted for one site (WP-BPM-08.T3; ADR-49,
//! ADR-080, native API 0.15.0).
//!
//! `spot-bpm-governance` (the guard), `spot-bpm-entry` (the form step, the `process_only` guard,
//! draft ownership) and `spot-bpm-request-authority` (the signed grant) are libraries. This
//! module is the host that makes them real for one site:
//!
//! - **The guard** ([`GovernanceParticipant`]): a pre-write transaction participant. A write to
//!   a governed DocType is allowed only with a grant the authority verifies for the acting
//!   person and the current attempt of a step of an open Run that may write the type; the grant
//!   is **consumed in the same transaction** (a second use is a replay and is refused). An
//!   administrator is not exempt unless `admin_override = true`, and every override is audited.
//!   See `participant.rs` for the full rule and for why it is a participant and not a controller.
//! - **No trust in a command id**: the command id is an idempotency key the caller chooses. It
//!   is a request context only when [`KeyedAuthority`] (the production HMAC authority) verifies
//!   it as a grant it signed for this actor; a client header or field never asserts "this came
//!   from a process". The keys are derived from the site's key-encryption key by id and rotate
//!   by adding an id (see `grants.rs`).
//! - **Process-first entry**: [`DraftStore`] keeps the form step's draft records (not yet a
//!   business document) readable by the requester and, once submitted, by the Run's
//!   participants; [`PgDraftWriter`] writes a draft in the same transaction as the step's
//!   transition when `FormService` drives it ([`form_service`]). `process_only` mode refuses a
//!   direct create with `bpm_entry.start_process` naming the process to start, and
//!   [`GovernanceHost::route`] answers "start this process" for a client that asks first.
//! - **Drafts are reaped** after `draft_retention_days` (default 30) by the module's service,
//!   on the holder of the lease.
//! - **Grants for the engine**: [`GrantHost`] is offered as a port; an engine step asks it for the
//!   grant its document writes run under and closes the request when the Run ends.
//!
//! # Threads
//!
//! Everything here is asynchronous and runs on the host's runtime: the participant runs inside
//! the document engine's own transaction, the ledger and the drafts use the site's pool. No
//! blocking bridge is involved.
//!
//! # Fail closed
//!
//! A policy that cannot be read, a ledger that cannot be reached when a grant is consumed, an
//! unresolvable key and a principal that cannot be resolved each end in a refusal or in "holds
//! nothing", never in an allow. Every refusal is audited with who, what and which rule.
//!
//! # Configuration
//!
//! `[services.process_governance]`: `enabled`, `admin_override`, `admin_roles`, `key_ids`,
//! `grant_ttl_secs`, `draft_retention_days`, `tick_secs`, `reason_min_chars`, `companies` and
//! the lists `governed` and `entry`. Governance is **off by default**: a record type nobody
//! lists is free. See `docs/running-spot/process_governance.md`.
//!
//! # Audit and logs
//!
//! Refusals and overrides go to the ledger's audit table; an override also rides the outbox in
//! the write's transaction. A log line carries ids and stable rule tokens, never a document
//! value, a reason, a grant or a key; counters carry no label at all.

#[path = "process_governance/drafts.rs"]
mod drafts;
#[path = "process_governance/grants.rs"]
mod grants;
#[path = "process_governance/ledger.rs"]
mod ledger;
#[path = "process_governance/participant.rs"]
mod participant;
#[path = "process_governance/policy.rs"]
mod policy;
#[path = "process_governance/settings.rs"]
mod settings;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_app_api::BoxFuture;
use spot_app_api::context::Actor;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_entry::{CreateRoute, EntryProcesses, EntryResult, ProcessOnlyGuard, ScopeResolver};
use spot_bpm_governance::{
    GovernanceError, GovernanceMode, OutsideProcess, PolicyStore, RecordingAudit, RequestAuthority,
    RoleOutsideProcess,
};
use spot_bpm_request_authority::{GrantSecrets, RequestStore};
use spot_core::Clock;
use spot_document::engine::{PrincipalSource as DocPrincipals, TxParticipant};
use spot_permissions::Principal;
use spot_postgres::PgTransaction;
use spot_secrets::KeyProvider;
use tokio::sync::watch;

pub use drafts::{
    DraftError, DraftRecord, DraftState, DraftStore, LedgerParticipants, MAX_DRAFT_BYTES,
    PgDraftWriter, RunParticipants, class_of, form_service,
};
pub use grants::{DerivedSecrets, GrantHost, IssueError, KeyedAuthority, LedgerRequests, micros};
pub use ledger::{Attempt, AuditRow, ConsumeError, Ledger, LedgerError, SCHEMA, Slot};
pub use participant::{GovernanceParticipant, NAME as PARTICIPANT_NAME, Parts, PrincipalLookup};
pub use policy::{SitePolicy, SiteScopes, entry_processes};
pub use settings::{EntryProcessEntry, GovernedEntry, Settings};

use crate::live::SiteLive;
use crate::services::ports::{ConsumerLease, OutboxLease};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the process engine (1100), the Document node module (1110) and the task
/// inbox (1120), and before the modules that consume the guard or the grants.
pub const ORDER: u32 = 1140;

/// The consumer name the module's lease is held under (`outbox:<site>:process_governance`).
const LEASE_CONSUMER: &str = "process_governance";

/// How many drafts one reaping pass removes at most.
const REAP_BATCH: u32 = 500;

/// The hosts of the sites built so far, by site name: a reload builds the site again without
/// building the host again.
fn built() -> &'static Mutex<HashMap<String, Arc<GovernanceHost>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<GovernanceHost>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How a site's governance is joined from its parts: the production wiring fills every field
/// from the live site, a test fills them with fakes (the ledger is always real PostgreSQL).
pub struct Wiring {
    /// The site's name (also the tenant key of the ledger).
    pub site: String,
    /// The source of `now`.
    pub clock: Arc<dyn Clock>,
    /// The site's pool.
    pub pool: Pool,
    /// The settings.
    pub settings: Settings,
    /// The mode of each record type.
    pub policy: Arc<dyn PolicyStore>,
    /// The record types the guard looks at; `None` asks the policy for every write.
    pub covered: Option<BTreeSet<String>>,
    /// Who the acting principal is.
    pub principals: Arc<dyn PrincipalLookup>,
    /// Who holds the outside-process permission.
    pub outside: Arc<dyn OutsideProcess>,
    /// The derived grant keys.
    pub secrets: Arc<DerivedSecrets>,
    /// Who may read a submitted draft besides its requester; `None`: the Run's grant actors.
    pub participants: Option<Arc<dyn RunParticipants>>,
}

/// One site's hosted governance: the guard, the grants, the drafts and the service's work.
pub struct GovernanceHost {
    site: String,
    ledger: Ledger,
    grants: Arc<GrantHost>,
    drafts: Arc<DraftStore>,
    participant: Arc<GovernanceParticipant>,
    policy: Arc<dyn PolicyStore>,
    entries: Arc<EntryProcesses>,
    authority: Arc<dyn RequestAuthority>,
    secrets: Arc<DerivedSecrets>,
    key_ids: Vec<String>,
    retention_days: u32,
    clock: Arc<dyn Clock>,
}

impl Wiring {
    /// Joins the parts into one hosted governance.
    ///
    /// # Errors
    /// A setting that does not validate (the key is named, never its value).
    pub fn build(self) -> std::result::Result<GovernanceHost, String> {
        self.settings.validate().map_err(str::to_owned)?;
        let ledger = Ledger::new(self.pool.clone(), &self.site);
        let requests = Arc::new(LedgerRequests::new());
        let store: Arc<dyn RequestStore> = Arc::clone(&requests) as Arc<dyn RequestStore>;
        let secrets: Arc<dyn GrantSecrets> = Arc::clone(&self.secrets) as Arc<dyn GrantSecrets>;
        let authority = Arc::new(KeyedAuthority::new(
            &store,
            &secrets,
            &self.settings.key_ids,
            &self.clock,
        )?);
        let grants = Arc::new(GrantHost::new(
            ledger.clone(),
            Arc::clone(&authority),
            requests,
            Arc::clone(&self.clock),
            self.settings.grant_ttl_secs,
        ));
        let verifier = grants.authority();
        let entries = entry_processes(&self.settings, &self.site);
        let scopes: Arc<dyn ScopeResolver> =
            Arc::new(SiteScopes::new(&self.site, &self.settings.companies));
        let participants = self
            .participants
            .unwrap_or_else(|| Arc::new(LedgerParticipants::new(ledger.clone())));
        let drafts = Arc::new(DraftStore::new(ledger.clone(), participants));
        let participant = GovernanceParticipant::new(Parts {
            site: self.site.clone(),
            ledger: ledger.clone(),
            policy: Arc::clone(&self.policy),
            covered: self.covered,
            entries: Arc::clone(&entries),
            scopes,
            authority: Arc::clone(&verifier),
            outside: self.outside,
            principals: self.principals,
            admin_override: self.settings.admin_override,
            admin_roles: self.settings.admin_roles.clone(),
        })
        .shared();
        Ok(GovernanceHost {
            site: self.site,
            ledger,
            grants,
            drafts,
            participant,
            policy: self.policy,
            entries,
            authority: verifier,
            secrets: self.secrets,
            key_ids: self.settings.key_ids.clone(),
            retention_days: self.settings.draft_retention_days,
            clock: self.clock,
        })
    }
}

impl GovernanceHost {
    /// The site this host serves.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    /// The ledger (tables, audit rows).
    #[must_use]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// The issuing side of the grants.
    #[must_use]
    pub fn grants(&self) -> &Arc<GrantHost> {
        &self.grants
    }

    /// The draft records.
    #[must_use]
    pub fn drafts(&self) -> &Arc<DraftStore> {
        &self.drafts
    }

    /// The authority that verifies grants.
    #[must_use]
    pub fn authority(&self) -> Arc<dyn RequestAuthority> {
        Arc::clone(&self.authority)
    }

    /// The pre-write guard, ready for `Engine::set_participants` (see the module's report for
    /// the hook the host table needs to add it).
    #[must_use]
    pub fn participant(&self) -> Arc<dyn TxParticipant<PgTransaction>> {
        Arc::clone(&self.participant) as Arc<dyn TxParticipant<PgTransaction>>
    }

    /// The same guard by its own type, for a test that calls `check` directly.
    #[must_use]
    pub fn guard(&self) -> &Arc<GovernanceParticipant> {
        &self.participant
    }

    /// The writer `FormService` saves drafts through.
    #[must_use]
    pub fn draft_writer(&self) -> Arc<PgDraftWriter> {
        Arc::new(PgDraftWriter::new(&self.site))
    }

    /// The mode of `doctype` in `scope` at `now`.
    ///
    /// # Errors
    /// The policy could not be read.
    pub fn mode(
        &self,
        scope: &str,
        doctype: &str,
        now: OffsetDateTime,
    ) -> std::result::Result<GovernanceMode, GovernanceError> {
        self.policy.mode_at(scope, doctype, now)
    }

    /// Where a create of `doctype` in `scope` should go: straight to the record type, or by
    /// starting its entry process (`process_only`). The desk uses it to show "Start" instead of
    /// "New"; an API client or an import uses it to learn what to start.
    ///
    /// # Errors
    /// The policy or the entry setting could not be read.
    pub fn route(
        &self,
        scope: &str,
        doctype: &str,
        now: OffsetDateTime,
    ) -> EntryResult<CreateRoute> {
        ProcessOnlyGuard::new(
            Arc::clone(&self.policy),
            Arc::clone(&self.entries),
            Arc::clone(&self.authority),
            Arc::new(RecordingAudit::new()),
        )
        .route(scope, doctype, now)
    }

    /// Derives the grant keys from `provider` and reloads the grant cache from the ledger. Every
    /// node does this on every tick. Returns how many keys are usable and how many steps are
    /// open.
    pub async fn refresh(&self, provider: Option<&dyn KeyProvider>) -> (usize, usize) {
        let usable = match provider {
            Some(provider) => self.secrets.refresh(provider, &self.key_ids).await,
            None => self.secrets.len(),
        };
        let open = match self.grants.refresh().await {
            Ok(open) => open,
            Err(error) => {
                tracing::warn!(reason = %error, "the grant table could not be reloaded");
                0
            }
        };
        (usable, open)
    }

    /// Removes drafts nobody touched for the retention period, in batches (the holder of the
    /// lease calls it). Returns how many went.
    pub async fn reap(&self) -> u64 {
        let Some(cutoff) = self
            .clock
            .now_utc()
            .checked_sub(time::Duration::days(i64::from(self.retention_days)))
        else {
            return 0;
        };
        let mut total = 0;
        loop {
            match self.drafts.reap(cutoff, REAP_BATCH).await {
                Ok(0) => break,
                Ok(removed) => {
                    total += removed;
                    if removed < u64::from(REAP_BATCH) {
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(code = error.code(), "reaping the drafts failed");
                    break;
                }
            }
        }
        total
    }
}

/// The principal of the site's live identity: a user's role assignments, `system` for the
/// platform, nothing for a consumer. An unreadable one holds nothing.
pub struct LivePrincipalLookup {
    live: Arc<SiteLive>,
}

impl LivePrincipalLookup {
    /// The lookup over `live`.
    #[must_use]
    pub fn new(live: Arc<SiteLive>) -> Self {
        Self { live }
    }
}

impl PrincipalLookup for LivePrincipalLookup {
    fn principal<'a>(&'a self, actor: &'a Actor) -> BoxFuture<'a, Principal> {
        Box::pin(async move {
            match actor {
                Actor::System { .. } => Principal::system(),
                Actor::User { id, .. } => match self.live.principals() {
                    Some(principals) => principals.resolve(id).await,
                    None => Principal::user(&id.to_string(), Vec::new()),
                },
                _ => Principal::user(&actor.principal_text(), Vec::new()),
            }
        })
    }
}

/// The service: it keeps the keys and the grant cache fresh on every node, and, on the holder of
/// the lease, reaps drafts and dead grant rows.
struct GovernanceService {
    host: Arc<GovernanceHost>,
    provider: Option<Arc<dyn KeyProvider>>,
    outbox: Arc<dyn OutboxLease>,
    interval: Duration,
}

impl Service for GovernanceService {
    fn name(&self) -> &'static str {
        "process_governance"
    }

    fn level(&self) -> Level {
        // Governance that cannot start leaves its record types free of grants but never open:
        // a write to a governed type with no usable key is refused. The site still serves.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if self.host.ledger.prepare().await.is_err() {
                return Err(
                    "the governance tables are not usable; run `spot migrate` or check the \
                     database"
                        .to_owned(),
                );
            }
            // Keys are derived at start so the first grant needs no tick; a failure is not fatal:
            // the loop derives them again, and until then no grant is issued or accepted.
            let (usable, _) = self.host.refresh(self.provider.as_deref()).await;
            if usable == 0 {
                tracing::warn!("no request grant key could be derived yet");
            }
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut ticker = tokio::time::interval(self.interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut lease: Option<ConsumerLease> = None;
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        self.host.refresh(self.provider.as_deref()).await;
                        let held = match &lease {
                            Some(held) => matches!(self.outbox.still_held(held).await, Ok(true)),
                            None => false,
                        };
                        if !held {
                            lease = self.outbox.acquire(LEASE_CONSUMER).await.ok();
                        }
                        if lease.is_some() {
                            let removed = self.host.reap().await;
                            if removed > 0 {
                                tracing::debug!(removed, "governance drafts reaped");
                            }
                        }
                    }
                }
            }
            if let Some(held) = lease {
                let _ = self.outbox.release(&held).await;
            }
            Ok(())
        })
    }
}

/// Says in readiness that no grant key can be derived. It checks as failed on purpose: an
/// optional service that fails its check only degrades readiness with this sentence, and it does
/// nothing else, so the real service keeps running and picks the key up once it exists.
struct KeyNotice;

impl Service for KeyNotice {
    fn name(&self) -> &'static str {
        "process_governance_keys"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async {
            Err(
                "no request grant key: [services.secrets] is not configured, so no grant is \
                 issued or accepted and governed record types admit only the audited exceptions"
                    .to_owned(),
            )
        })
    }

    fn run(&self, _stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

/// Registers governance: the ledger, the guard, the grants, the drafts and the service. Inert
/// when `services.process_governance.enabled = false`.
///
/// # Errors
/// A setting that does not decode or is out of range; a table that cannot be prepared; a port
/// another module already provided.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_governance")?;
    settings
        .validate()
        .map_err(|key| Error(format!("services.process_governance.{key}: out of range")))?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let host = if ctx.assembling() {
        let secrets = ctx.port::<super::secrets::SecretsHost>();
        let provider: Option<Arc<dyn KeyProvider>> = secrets
            .as_ref()
            .map(|host| Arc::clone(host.keyring()) as Arc<dyn KeyProvider>);
        let outbox = ctx.require_port::<dyn OutboxLease>()?;
        let interval = Duration::from_secs(settings.tick_secs);
        let wiring = Wiring {
            site: site.clone(),
            clock: Arc::clone(ctx.clock()),
            pool: env.backend.pool(),
            policy: SitePolicy::from_settings(&settings, &site).shared(),
            covered: Some(settings.covered()),
            principals: Arc::new(LivePrincipalLookup::new(Arc::clone(env.live))),
            outside: Arc::new(RoleOutsideProcess) as Arc<dyn OutsideProcess>,
            secrets: Arc::new(DerivedSecrets::new()),
            participants: None,
            settings,
        };
        let host =
            Arc::new(wiring.build().map_err(|key| {
                Error(format!("services.process_governance.{key}: out of range"))
            })?);
        host.ledger
            .prepare()
            .await
            .map_err(|error| Error(format!("cli.process_governance_tables: {error}")))?;
        if provider.is_none() {
            ctx.add(Arc::new(KeyNotice));
        }
        ctx.add(Arc::new(GovernanceService {
            host: Arc::clone(&host),
            provider,
            outbox,
            interval,
        }));
        lock(built()).insert(site, Arc::clone(&host));
        host
    } else {
        match lock(built()).get(&site).cloned() {
            Some(host) => host,
            None => return Ok(()),
        }
    };
    ctx.provide::<GovernanceHost>(Arc::clone(&host))?;
    ctx.provide::<GrantHost>(Arc::clone(host.grants()))?;
    ctx.provide::<DraftStore>(Arc::clone(host.drafts()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_module_starts_after_the_task_inbox() {
        const { assert!(ORDER > super::super::process_tasks::ORDER) };
    }

    #[test]
    fn a_site_with_no_settings_governs_nothing() {
        let settings = Settings::default();
        assert!(settings.covered().is_empty());
        assert!(settings.enabled);
    }
}
