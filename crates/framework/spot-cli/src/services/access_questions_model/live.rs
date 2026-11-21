//! The live model: [`LiveModelSource`] builds the assembled core from one consistent read of the
//! stored directory and the installed apps' grants, caches it **per role-snapshot version** and
//! hands every question a [`LiveAccessModel`], the core plus the history of the asked window.
//!
//! # When the core is rebuilt
//!
//! The cached core is reused while all of these hold, and rebuilt (once, under a lock, so a burst
//! of questions reads the directory once) as soon as one does not:
//!
//! - the version of the role snapshot the roles service publishes is the one the core was built
//!   under (a role, duty or assignment change publishes a new one);
//! - the served metadata is the one the core was built from (a customisation swaps it);
//! - the core is younger than [`ModelSettings::max_age_seconds`] (the cross-node bound: another
//!   node's write is in the shared stored records and is seen within that interval).
//!
//! With a change feed attached ([`LiveModelSource::attach_feed`], WP-IAM-08.T15) the cached core
//! is instead kept for [`DEFAULT_REPAIR_SECONDS`] (see [`LiveModelSource::with_repair_interval`])
//! and a change is applied the
//! moment its event arrives: the records the event names are read again (their child rows; the
//! rest of the cached directory is reused) and the core is assembled from the result. A feed that
//! says its view is broken ([`LiveModelSource::request_rebuild`]) forces a full read, and so does
//! the repair interval, so a missed event is repaired within it.
//!
//! # Failure
//!
//! Fail closed. A site with a role-snapshot port but no snapshot yet refuses
//! ([`ModelError::SnapshotMissing`]); a directory that cannot be read, roles that do not form a
//! registry or grant files that do not parse refuse every question, and the previous core is never
//! served in their place. A history that cannot be read refuses only the history question.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_core::Clock;
use spot_iam_governance::ChangeEvent;
use spot_iam_questions::{AccessModel, DocumentRecord, ShareRecord};
use spot_iam_roles::{Assignment, Duty, Registry};
use spot_meta::EffectiveMeta;
use spot_permissions::{FieldFacts, ProtectionGroup};
use spot_sod::RuleSet;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use super::assemble::{AssembleInput, GrantInputs, ModelCore, RawDirectory, assemble};
use super::directory::DirectoryReader;
use super::events::{AccessChange, Touched, change_events};
use super::grants::StaticInputs;
use super::history::{ChangeJournal, ReadLimits, read_window};
use super::{
    CODE_HISTORY_SKIPPED, CODE_HISTORY_UNAVAILABLE, CODE_SNAPSHOT_UNVERSIONED, ModelError, Note,
};
use crate::live::SiteLive;
use crate::services::roles::RoleSnapshots;

/// With a change feed attached: how long the stored records are trusted without a full read.
pub const DEFAULT_REPAIR_SECONDS: i64 = 300;

/// How the source behaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelSettings {
    /// The longest a built core is reused without a re-read of the stored records, in seconds.
    pub max_age_seconds: i64,
    /// What one history read may do.
    pub limits: ReadLimits,
}

impl Default for ModelSettings {
    fn default() -> Self {
        Self {
            max_age_seconds: 5,
            limits: ReadLimits::default(),
        }
    }
}

/// What the site contributes to one build besides the stored directory.
#[derive(Clone)]
pub struct SiteView {
    /// The metadata the site serves now.
    pub meta: Arc<EffectiveMeta>,
    /// A token that differs whenever the served metadata does.
    pub token: u64,
    /// The apps' grants and the runtime default grants of customised DocTypes.
    pub grants: GrantInputs,
    /// The duty definitions the apps ship.
    pub duties: Vec<Duty>,
    /// The SoD rules.
    pub rules: RuleSet,
}

/// The site's side of a build: what is served and what the installed apps ship.
pub trait SiteInputs: Send + Sync {
    /// The inputs now.
    ///
    /// # Errors
    ///
    /// [`ModelError::DirectoryUnavailable`] while the site serves no metadata;
    /// [`ModelError::GrantsUnavailable`] when the apps' files could not be read at start.
    fn view(&self) -> Result<SiteView, ModelError>;
}

/// The production inputs: the live hub's served metadata and default grants over the apps' files.
pub struct LiveInputs {
    live: Arc<SiteLive>,
    statics: Result<Arc<StaticInputs>, ModelError>,
}

impl LiveInputs {
    /// Inputs over the site's live hub and the apps' shipped files (or why they could not be read:
    /// every question then refuses with that code, the site still starts).
    #[must_use]
    pub fn new(live: Arc<SiteLive>, statics: Result<Arc<StaticInputs>, ModelError>) -> Self {
        Self { live, statics }
    }
}

impl SiteInputs for LiveInputs {
    fn view(&self) -> Result<SiteView, ModelError> {
        let statics = self.statics.as_ref().map_err(|e| *e)?;
        let served = self
            .live
            .current()
            .map_err(|_| ModelError::DirectoryUnavailable)?;
        // The address of the served metadata: a swap builds a new one, so it differs after it.
        let token = Arc::as_ptr(&served.meta).cast::<()>() as usize as u64;
        Ok(SiteView {
            meta: Arc::clone(&served.meta),
            token,
            grants: statics.grant_inputs(self.live.grants()),
            duties: statics.duties.clone(),
            rules: statics.rules.clone(),
        })
    }
}

struct Cached {
    core: Arc<ModelCore>,
    /// The stored records the core was assembled from (reused by an incremental refresh).
    directory: Arc<RawDirectory>,
    snapshot: Option<u64>,
    token: u64,
    /// When the stored records were last read in full.
    built_at: OffsetDateTime,
}

/// What the feed has reported and the model has not applied yet.
#[derive(Default)]
struct Pending {
    touched: Touched,
    full: bool,
}

/// The core of one refresh and the directory it replaced.
struct Refreshed {
    core: Arc<ModelCore>,
    after: Arc<RawDirectory>,
}

/// What a question reads: the shared core, the history of its window and what was noticed.
pub struct LiveAccessModel {
    core: Arc<ModelCore>,
    history: Vec<ChangeEvent>,
    documents: Vec<DocumentRecord>,
    notes: Vec<Note>,
}

impl LiveAccessModel {
    /// A model over `core` with no history window.
    #[must_use]
    pub fn new(core: Arc<ModelCore>) -> Self {
        let notes = core.notes.clone();
        Self {
            core,
            history: Vec::new(),
            documents: Vec::new(),
            notes,
        }
    }

    /// The same model with the history of one window.
    #[must_use]
    pub fn with_history(mut self, history: Vec<ChangeEvent>) -> Self {
        self.history = history;
        self
    }

    /// The same model with the documents `who_can_see` may be asked about. Loading a document is
    /// the caller's job (it needs the asker's own read permission); the model only holds them.
    #[must_use]
    pub fn with_documents(mut self, documents: Vec<DocumentRecord>) -> Self {
        self.documents = documents;
        self
    }

    /// What the build and the history read noticed and did not fail on.
    #[must_use]
    pub fn notes(&self) -> &[Note] {
        &self.notes
    }

    /// The assembled core.
    #[must_use]
    pub fn core(&self) -> &Arc<ModelCore> {
        &self.core
    }

    fn note(&mut self, note: Note) {
        self.notes.push(note);
    }
}

impl AccessModel for LiveAccessModel {
    fn registry(&self) -> &Registry {
        &self.core.registry
    }
    fn assignments(&self) -> &[Assignment] {
        &self.core.assignments
    }
    fn portal_users(&self) -> &[String] {
        &self.core.portal
    }
    fn groups(&self) -> &[ProtectionGroup] {
        &self.core.groups
    }
    fn shares(&self) -> &[ShareRecord] {
        &self.core.shares
    }
    fn rules(&self) -> &RuleSet {
        &self.core.rules
    }
    fn history(&self) -> &[ChangeEvent] {
        &self.history
    }
    fn meta(&self) -> &EffectiveMeta {
        &self.core.meta
    }
    fn field_facts(&self, doctype: &str) -> Vec<FieldFacts> {
        self.core
            .field_facts
            .get(doctype)
            .cloned()
            .unwrap_or_default()
    }
    fn document(&self, doctype: &str, name: &str) -> Option<&DocumentRecord> {
        self.documents
            .iter()
            .find(|d| d.doctype == doctype && d.name == name)
    }
    fn documents(&self) -> &[DocumentRecord] {
        &self.documents
    }
}

/// How the model and its history are doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelStatus {
    /// Whether the core can be built now (the other three questions answer).
    pub model: Result<(), ModelError>,
    /// Whether the history can be read (the history question answers).
    pub history: Result<(), ModelError>,
    /// What the last build noticed.
    pub notes: Vec<Note>,
}

impl ModelStatus {
    /// Whether every part works.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.model.is_ok() && self.history.is_ok()
    }

    /// The readiness text: `Ok` when ready, else the code of the first failed part and which
    /// questions it affects. Never a record's content.
    ///
    /// # Errors
    ///
    /// The degraded reason.
    pub fn readiness(&self) -> Result<(), String> {
        if let Err(error) = self.model {
            return Err(format!(
                "{}: the access model cannot be built, so no question is answered",
                error.code()
            ));
        }
        if let Err(error) = self.history {
            return Err(format!(
                "{}: the permission-change history cannot be read, so only \"what changed\" is refused",
                error.code()
            ));
        }
        Ok(())
    }
}

/// The source of models for the questions of one site.
pub struct LiveModelSource {
    directory: Arc<dyn DirectoryReader>,
    inputs: Arc<dyn SiteInputs>,
    snapshots: Option<Arc<dyn RoleSnapshots>>,
    journal: Option<Arc<dyn ChangeJournal>>,
    clock: Arc<dyn Clock>,
    settings: ModelSettings,
    cache: Mutex<Option<Cached>>,
    pending: std::sync::Mutex<Pending>,
    feed: AtomicBool,
    repair_seconds: i64,
}

impl LiveModelSource {
    /// A source over the stored directory (`directory`), the site's inputs, the role snapshot port
    /// when the site has one, and the access-change journal when it has one.
    #[must_use]
    pub fn new(
        directory: Arc<dyn DirectoryReader>,
        inputs: Arc<dyn SiteInputs>,
        snapshots: Option<Arc<dyn RoleSnapshots>>,
        journal: Option<Arc<dyn ChangeJournal>>,
        clock: Arc<dyn Clock>,
        settings: ModelSettings,
    ) -> Self {
        Self {
            directory,
            inputs,
            snapshots,
            journal,
            clock,
            settings,
            cache: Mutex::new(None),
            pending: std::sync::Mutex::new(Pending::default()),
            feed: AtomicBool::new(false),
            repair_seconds: DEFAULT_REPAIR_SECONDS,
        }
    }

    /// The same source with another repair interval: with a feed attached, the longest the stored
    /// records are trusted without a full read, in seconds. A change the feed missed is seen
    /// within it.
    #[must_use]
    pub fn with_repair_interval(mut self, seconds: i64) -> Self {
        self.repair_seconds = seconds.max(1);
        self
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, Pending> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The access-change journal this source reads history from, when the site has one.
    #[must_use]
    pub fn journal(&self) -> Option<&Arc<dyn ChangeJournal>> {
        self.journal.as_ref()
    }

    /// Declares that a change feed keeps this source up to date: the cached core is then kept for
    /// [`DEFAULT_REPAIR_SECONDS`] instead of [`ModelSettings::max_age_seconds`].
    pub fn attach_feed(&self) {
        self.feed.store(true, Ordering::SeqCst);
        self.request_rebuild();
    }

    /// Whether a change feed is attached.
    #[must_use]
    pub fn feed_attached(&self) -> bool {
        self.feed.load(Ordering::SeqCst)
    }

    /// The feed's view is broken (it lost its lease, the stream restarted, a read failed): the
    /// next question reads the stored records in full.
    pub fn request_rebuild(&self) {
        self.pending().full = true;
    }

    /// Notes changes after commit; the next question applies them. Cheap and synchronous.
    pub fn note_changes(&self, changes: &[AccessChange]) {
        let mut pending = self.pending();
        for change in changes {
            pending.touched.add(change);
        }
    }

    /// Notes `changes`, applies them now (the records they name are read again) and returns the
    /// change events to journal: ids, names and tokens, no snapshot.
    ///
    /// # Errors
    ///
    /// [`ModelError`] as [`LiveModelSource::core`]; the changes stay noted and the next question
    /// applies them.
    pub async fn apply_changes(
        &self,
        changes: &[AccessChange],
    ) -> Result<Vec<spot_iam_governance::ChangeEvent>, ModelError> {
        let before = self
            .cache
            .lock()
            .await
            .as_ref()
            .map(|cached| Arc::clone(&cached.directory));
        self.note_changes(changes);
        let refreshed = self.refresh().await?;
        let at = self.clock.now_utc().unix_timestamp();
        let empty = RawDirectory::default();
        Ok(change_events(
            before.as_deref().unwrap_or(&empty),
            &refreshed.after,
            changes,
            at,
        ))
    }

    /// The assembled core now, rebuilt when the role snapshot, the served metadata or the age
    /// bound says so, and updated when the change feed reported changes.
    ///
    /// # Errors
    ///
    /// [`ModelError`]: the questions refuse; the previous core is not served in its place.
    pub async fn core(&self) -> Result<Arc<ModelCore>, ModelError> {
        self.refresh().await.map(|refreshed| refreshed.core)
    }

    async fn refresh(&self) -> Result<Refreshed, ModelError> {
        let snapshot = match &self.snapshots {
            Some(port) => Some(port.current().ok_or(ModelError::SnapshotMissing)?.version),
            None => None,
        };
        let view = self.inputs.view()?;
        let now = self.clock.now_utc();
        let bound = if self.feed_attached() {
            self.repair_seconds
        } else {
            self.settings.max_age_seconds
        };
        let max_age = time::Duration::seconds(bound.max(1));
        let mut slot = self.cache.lock().await;
        let pending = std::mem::take(&mut *self.pending());
        let current = slot.as_ref().filter(|cached| {
            cached.snapshot == snapshot
                && cached.token == view.token
                && now >= cached.built_at
                && now - cached.built_at < max_age
        });
        if let Some(cached) = current
            && pending.touched.is_empty()
            && !pending.full
        {
            return Ok(Refreshed {
                core: Arc::clone(&cached.core),
                after: Arc::clone(&cached.directory),
            });
        }
        // One transaction reads the records: a consistent point. Only the records the feed named
        // are read in full when the cached ones are still trustworthy.
        let read = match current {
            Some(cached) if !pending.full => {
                self.directory
                    .read_delta(&cached.directory, &pending.touched)
                    .await
            }
            _ => self.directory.read().await,
        };
        let full = !(current.is_some() && !pending.full);
        let built_at = if full {
            now
        } else {
            current.map_or(now, |cached| cached.built_at)
        };
        let outcome = match read {
            Ok(directory) => assemble(&AssembleInput {
                directory: &directory,
                grants: &view.grants,
                duties: &view.duties,
                rules: &view.rules,
                meta: Arc::clone(&view.meta),
            })
            .map(|core| (core, directory)),
            Err(error) => Err(error),
        };
        let (core, directory) = match outcome {
            Ok(built) => built,
            Err(error) => {
                // The changes stay noted: the next question tries again, never serving the old
                // core in their place.
                let mut again = self.pending();
                again.touched.merge(pending.touched);
                again.full |= pending.full;
                return Err(error);
            }
        };
        let core = Arc::new(core);
        let directory = Arc::new(directory);
        *slot = Some(Cached {
            core: Arc::clone(&core),
            directory: Arc::clone(&directory),
            snapshot,
            token: view.token,
            built_at,
        });
        Ok(Refreshed {
            core,
            after: directory,
        })
    }

    /// The model for one question: the core and, when `window` is given (epoch seconds, both ends
    /// inclusive), the permission-change history of that window.
    ///
    /// # Errors
    ///
    /// Any error of [`LiveModelSource::core`]; with a window, [`ModelError::HistoryUnavailable`],
    /// [`ModelError::HistoryTooLarge`] or [`ModelError::RangeInvalid`] (the other questions do not
    /// ask for a window and are not affected).
    pub async fn model(&self, window: Option<(i64, i64)>) -> Result<LiveAccessModel, ModelError> {
        let core = self.core().await?;
        let mut model = LiveAccessModel::new(core);
        if self.snapshots.is_none() {
            model.note(Note {
                code: CODE_SNAPSHOT_UNVERSIONED,
                count: 1,
            });
        }
        if let Some((from, to)) = window {
            let journal = self
                .journal
                .as_ref()
                .ok_or(ModelError::HistoryUnavailable)?;
            let read = read_window(journal.as_ref(), from, to, &self.settings.limits).await?;
            if read.skipped > 0 {
                model.note(Note {
                    code: CODE_HISTORY_SKIPPED,
                    count: read.skipped,
                });
                tracing::warn!(
                    code = CODE_HISTORY_SKIPPED,
                    count = read.skipped,
                    "journal entries could not be interpreted and were skipped"
                );
            }
            model.history = read.events;
        }
        Ok(model)
    }

    /// How the parts are doing, for readiness.
    pub async fn status(&self) -> ModelStatus {
        let (model, notes) = match self.core().await {
            Ok(core) => (Ok(()), core.notes.clone()),
            Err(error) => (Err(error), Vec::new()),
        };
        let history = match &self.journal {
            None => Err(ModelError::HistoryUnavailable),
            Some(journal) => journal
                .head()
                .await
                .map(|_| ())
                .map_err(|_| ModelError::HistoryUnavailable),
        };
        if history.is_err() {
            tracing::warn!(
                code = CODE_HISTORY_UNAVAILABLE,
                "the access-change journal cannot be read"
            );
        }
        ModelStatus {
            model,
            history,
            notes,
        }
    }
}

/// The source of `ctx`'s site, over its database, its live hub, the roles service's snapshot port
/// and the access-change journal in the site's audit store.
///
/// The apps' files are read here once; if they cannot be, the source is still returned and every
/// question refuses with [`ModelError::GrantsUnavailable`] (readiness says so) instead of the site
/// failing to start.
///
/// # Errors
///
/// The context carries no site environment (a harness without a database).
pub fn from_site(
    ctx: &crate::services::SiteCtx<'_>,
    settings: ModelSettings,
) -> crate::Result<LiveModelSource> {
    let env = ctx.env()?;
    let statics = StaticInputs::from_site(
        env.root,
        env.loaded,
        env.site,
        env.permission.apps.grants.clone(),
    )
    .map(Arc::new);
    let directory = Arc::new(super::directory::PgDirectory::new(
        env.backend.clone(),
        env.permission.catalog.clone(),
    ));
    let store = spot_audit_store::PgAuditStore::new(env.backend.pool());
    let journal = Arc::new(super::history::ChainJournal::new(store, &env.site.site));
    Ok(LiveModelSource::new(
        directory,
        Arc::new(LiveInputs::new(Arc::clone(env.live), statics)),
        ctx.port::<dyn RoleSnapshots>(),
        Some(journal),
        Arc::clone(ctx.clock()),
        settings,
    ))
}
