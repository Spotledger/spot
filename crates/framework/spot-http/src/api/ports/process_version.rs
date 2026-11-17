//! The process version seam of `process_version_list`, `process_version_publish`,
//! `process_version_get`, `process_version_retire`, `process_migration_plan`,
//! `process_migration_start` and `process_migration_get` (native API 0.12.0, WP-BPM-09.T3).
//!
//! `spot-http` does not depend on the versioning engine. It defines [`VersionPort`], the seven
//! operations in the shapes the router needs, and the host writes the adapter over
//! `spot-bpm-versioning` (WP-BPM-09.T4). **Everything that decides is the port's:** who the
//! caller is (from `ctx.credential` on `ctx.site`), whether the caller holds `process.publish`,
//! `process.read_versions`, `process.migrate` or `process.publish_service_principal`, the
//! validation of a definition with the model's own validator, and the audit of every write, a
//! refusal included. The routes pass **no user, no role and no tenant**: nothing in this trait
//! lets a caller name another actor or another site.
//!
//! # One answer for "not there" and "not yours"
//!
//! A process the caller may not read or change is [`VersionPortError::NotFound`], exactly as a
//! process or version that does not exist. Only `PublishDenied` may differ, and only because the
//! caller already sees the process.
//!
//! # What is never carried
//!
//! A [`Problem`] is a node id and a closed fault kind, never text taken from the document. A
//! migration plan names runs by id only. No error variant carries a message or a principal.
//!
//! [`MemoryVersionPort`] is the in-memory fake the route tests and the conformance walk use. It
//! reads no clock, keeps every site apart, and counts its writes so a test can prove a dry run
//! wrote nothing. Its definitions are a tiny line format of its own (see
//! `MemoryVersionPort::validate`); it stands for the model's validator and nothing more.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use serde_json::{Map, Value as Json};
use sha2::{Digest as _, Sha256};

use super::PortKey;
use crate::api::service::{BoxFuture, RequestCtx};

/// The port key of [`VersionPort`].
pub const KEY: PortKey = PortKey::of::<dyn VersionPort>();

/// The default page of `process_version_list`.
pub const DEFAULT_PAGE: u32 = 50;
/// The largest page of `process_version_list`.
pub const MAX_PAGE: u32 = 100;
/// The largest definition (the model's text form), in bytes: 256 KiB.
pub const MAX_DEFINITION_BYTES: usize = 256 * 1024;
/// The longest note, in characters.
pub const MAX_NOTE_CHARS: usize = 500;
/// The most problems one refused definition reports.
pub const MAX_PROBLEMS: usize = 100;
/// The most unmoved runs one plan lists.
pub const MAX_UNMOVED: usize = 100;
/// The most node pairs one plan counts.
pub const MAX_NODE_COUNTS: usize = 500;

/// What a port call answers.
pub type VersionResult<T> = Result<T, VersionPortError>;

/// Why a port call was refused. Every variant maps to a code the contract declares for the
/// operations; the router adds no detail to any of them except the problems of
/// [`VersionPortError::DefinitionInvalid`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VersionPortError {
    /// `404`: no such process, version or migration that the caller may see. The same answer for
    /// "does not exist" and "not allowed" (`process.not_found`, or `process.migration_not_found`
    /// on `process_migration_get`).
    NotFound,
    /// `403 process.publish_denied`: the definition acts as a service principal and the caller
    /// lacks `process.publish_service_principal`. Names no principal.
    PublishDenied,
    /// `409 process.version_conflict`: `expected_previous` is not the latest version.
    VersionConflict,
    /// `422 process.definition_invalid`: the model's validator refused the definition. Node ids
    /// and fault kinds only.
    DefinitionInvalid(Vec<Problem>),
    /// `409 process.version_not_published`: the version to retire, or the migration target, is
    /// not published.
    VersionNotPublished,
    /// `422 process.migration_invalid`: the target equals the source, or the mapping names a node
    /// the target does not have.
    MigrationInvalid,
    /// `400 api.invalid_request`: the cursor is not one this port issued.
    BadCursor,
    /// `503 server.unavailable`: the store cannot answer now.
    Unavailable,
    /// `500 http.internal`: anything else. The router answers it with no detail.
    Internal,
}

/// One fault of a refused definition: a node id when the fault belongs to a node, and a closed
/// `kind` token (`bpm_model.unknown_node`, ...). Never free text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// The node id, when the fault belongs to one.
    pub node: Option<String>,
    /// The fault kind: a stable dotted token (`^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`).
    pub kind: String,
}

/// The state of a version (the contract's `ProcessVersionState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionState {
    /// Saved, not published.
    Draft,
    /// New runs start on the newest published version.
    Published,
    /// No new run starts on it; running runs continue.
    Retired,
}

impl VersionState {
    /// The contract's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            VersionState::Draft => "draft",
            VersionState::Published => "published",
            VersionState::Retired => "retired",
        }
    }
}

/// One version without its definition (the contract's `ProcessVersion`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionSummary {
    /// The version number, 1 or more.
    pub version: u32,
    /// The state.
    pub state: VersionState,
    /// When it was published, RFC 3339 UTC; `None` for a draft.
    pub published_at: Option<String>,
    /// The publisher's user id; `None` for a draft.
    pub published_by: Option<String>,
    /// The publisher's note, at most [`MAX_NOTE_CHARS`] characters.
    pub note: String,
    /// The runs (running or waiting) pinned to this version.
    pub running_runs: u32,
}

/// The answer of a publish: the new version and the content hash the response's `ETag` carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Published {
    /// The new version.
    pub version: VersionSummary,
    /// The lower-case hex SHA-256 of the definition.
    pub hash: String,
}

/// One version with its definition (the contract's `ProcessVersionDetail`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionDetail {
    /// The version's metadata.
    pub version: VersionSummary,
    /// The definition, the model's text form, at most [`MAX_DEFINITION_BYTES`] bytes.
    pub definition: String,
    /// The lower-case hex SHA-256 of `definition`: the `ETag` without its quotes.
    pub hash: String,
}

/// One page of a list: the items and the opaque cursor of the next page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionPage {
    /// The versions, newest first, at most the requested limit.
    pub items: Vec<VersionSummary>,
    /// The cursor of the next page; `None` on the last.
    pub next_cursor: Option<String>,
}

/// The body of a publish, already shaped by the route. `definition` is **untouched**: the route
/// never reads, trims or rewrites it. The publisher is never in it: the port takes it from the
/// request's credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishRequest {
    /// The model's text form.
    pub definition: String,
    /// The note, empty when none was given.
    pub note: String,
    /// The version number the client believes is the latest (0 for none); `None` skips the check.
    pub expected_previous: Option<u32>,
}

/// The explicit node pairs and variable contract of one migration.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MigrationMapping {
    /// Node id in the source to node id in the target.
    pub nodes: BTreeMap<String, String>,
    /// Dotted variable paths the target requires.
    pub required_variables: Vec<String>,
    /// Defaults for required paths a run lacks.
    pub variable_defaults: Map<String, Json>,
}

/// The body of a migration plan or start, already shaped by the route.
#[derive(Clone, Debug, PartialEq)]
pub struct MigrationRequest {
    /// The target version.
    pub to: u32,
    /// The mapping; `None` when the request gave none.
    pub mapping: Option<MigrationMapping>,
}

/// Runs a node pair would move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeCount {
    /// Node id in the source version.
    pub from: String,
    /// Node id in the target version.
    pub to: String,
    /// The runs that would move along this pair.
    pub runs: u32,
}

/// A run that could not move, by id and a stable reason code only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unmoved {
    /// The run's opaque id.
    pub run: String,
    /// A stable `bpm_versioning.*` code.
    pub reason: String,
}

/// The dry run of a migration (the contract's `MigrationPlan`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationPlan {
    /// The source version.
    pub from: u32,
    /// The target version.
    pub to: u32,
    /// The runs that would move.
    pub movable: u32,
    /// How many runs could not move (the full count; `unmoved` is capped).
    pub unmoved_total: u32,
    /// Per node pair, the runs that would move.
    pub node_counts: Vec<NodeCount>,
    /// At most [`MAX_UNMOVED`] runs that could not move.
    pub unmoved: Vec<Unmoved>,
}

/// The state of a migration (the contract's `MigrationState`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationState {
    /// Planned, running, stopping or paused: resumable.
    Running,
    /// Every run has an outcome and none failed.
    Completed,
    /// A run failed and the batch stopped, or an operator abandoned it.
    Failed,
}

impl MigrationState {
    /// The contract's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            MigrationState::Running => "running",
            MigrationState::Completed => "completed",
            MigrationState::Failed => "failed",
        }
    }
}

/// Where a migration stands (the contract's `Migration`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationProgress {
    /// The state.
    pub state: MigrationState,
    /// Runs moved.
    pub moved: u32,
    /// Runs skipped with a reason code.
    pub skipped: u32,
    /// Runs that failed.
    pub failed: u32,
    /// When the batch started, RFC 3339 UTC.
    pub started_at: String,
    /// When it ended, RFC 3339 UTC; `None` while it runs.
    pub ended_at: Option<String>,
}

/// The process version and migration operations of one site, for the signed-in caller.
pub trait VersionPort: Send + Sync {
    /// The versions of `process`, newest first, for a caller holding `process.publish` or
    /// `process.read_versions`.
    fn list_versions<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, VersionResult<VersionPage>>;

    /// Validates and publishes a new version as the caller. `ctx.idempotency_key` is the key.
    /// Audited, a refusal included, and the audit never records the definition text.
    fn publish<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        request: PublishRequest,
    ) -> BoxFuture<'a, VersionResult<Published>>;

    /// One version with its definition; unknown and not readable are one `NotFound`.
    fn get_version<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        version: u32,
    ) -> BoxFuture<'a, VersionResult<VersionDetail>>;

    /// Retires a published version as the caller. Audited, a refusal included.
    fn retire<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        version: u32,
    ) -> BoxFuture<'a, VersionResult<VersionSummary>>;

    /// The dry run of a migration. **Changes nothing**: no run, no batch (audited as a read).
    fn plan_migration<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        from: u32,
        request: MigrationRequest,
    ) -> BoxFuture<'a, VersionResult<MigrationPlan>>;

    /// Starts the bounded, resumable batch and answers its id. Audited, a refusal included.
    fn start_migration<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        from: u32,
        request: MigrationRequest,
    ) -> BoxFuture<'a, VersionResult<String>>;

    /// Where a migration stands; another process's, unknown and not permitted are one `NotFound`.
    fn get_migration<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, VersionResult<MigrationProgress>>;
}

/// The lower-case hex SHA-256 of a definition: the content hash a version is known by.
pub fn content_hash(definition: &str) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(definition.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// ------------------------------------------------------------------------------------------
// The in-memory fake

/// A platform permission the fake can grant a user on a site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Grant {
    /// `process.publish`.
    Publish,
    /// `process.read_versions`.
    ReadVersions,
    /// `process.migrate`.
    Migrate,
    /// `process.publish_service_principal`.
    PublishServicePrincipal,
}

/// One call the fake audited: who, which operation, the refusing rule. **Never a value.**
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// The site the call was for.
    pub site: String,
    /// The acting user.
    pub actor: String,
    /// The operation id.
    pub operation: &'static str,
    /// The process name.
    pub subject: String,
    /// `ok` or the refusing code.
    pub outcome: &'static str,
}

struct StoredVersion {
    summary: VersionSummary,
    definition: String,
    hash: String,
}

#[derive(Default)]
struct ProcessState {
    versions: Vec<StoredVersion>,
    unmovable: BTreeMap<u32, Vec<Unmoved>>,
}

struct StoredMigration {
    process: String,
    progress: MigrationProgress,
}

#[derive(Default)]
struct SiteState {
    processes: BTreeMap<String, ProcessState>,
    grants: BTreeMap<String, BTreeSet<Grant>>,
    migrations: BTreeMap<String, StoredMigration>,
}

#[derive(Default)]
struct State {
    sites: BTreeMap<String, SiteState>,
    audit: Vec<AuditEntry>,
    clock: u64,
    next_migration: u64,
    writes: u64,
}

/// The in-memory [`VersionPort`]: per-site processes, versions and migrations, the contract's
/// visibility rules and an audit list. The user of a call is the credential's token itself.
#[derive(Default)]
pub struct MemoryVersionPort {
    state: Mutex<State>,
}

/// A deterministic RFC 3339 timestamp: the fake reads no clock.
fn stamp(tick: u64) -> String {
    let hour = (tick / 3600) % 24;
    let minute = (tick / 60) % 60;
    let second = tick % 60;
    format!("2026-11-01T{hour:02}:{minute:02}:{second:02}Z")
}

/// The caller's user id, or a refusal: a call without a credential sees nothing and does nothing
/// (fail closed).
fn caller(ctx: &RequestCtx) -> VersionResult<String> {
    match &ctx.credential {
        Some(credential) if !credential.token.is_empty() => Ok(credential.token.clone()),
        _ => Err(VersionPortError::NotFound),
    }
}

/// The offset a cursor stands for. The fake's cursor is `c<offset>`.
fn parse_cursor(cursor: Option<&str>) -> VersionResult<usize> {
    match cursor {
        None => Ok(0),
        Some(raw) => raw
            .strip_prefix('c')
            .and_then(|digits| digits.parse::<usize>().ok())
            .ok_or(VersionPortError::BadCursor),
    }
}

/// The node ids a fake definition declares: the lines `node <id>`.
fn nodes_of(definition: &str) -> BTreeSet<&str> {
    definition
        .lines()
        .filter_map(|line| line.strip_prefix("node "))
        .map(str::trim)
        .collect()
}

impl MemoryVersionPort {
    /// An empty fake.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The mutex guards plain maps; a panic elsewhere cannot leave them inconsistent.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Makes `process` known on `site`, with no version yet.
    pub fn define_process(&self, site: &str, process: &str) {
        self.lock()
            .sites
            .entry(site.to_owned())
            .or_default()
            .processes
            .entry(process.to_owned())
            .or_default();
    }

    /// Gives `user` a platform permission on `site`.
    pub fn grant(&self, site: &str, user: &str, grant: Grant) {
        self.lock()
            .sites
            .entry(site.to_owned())
            .or_default()
            .grants
            .entry(user.to_owned())
            .or_default()
            .insert(grant);
    }

    /// Sets the number of runs pinned to `version` (a test helper standing for the engine).
    pub fn set_running_runs(&self, site: &str, process: &str, version: u32, runs: u32) {
        let mut state = self.lock();
        if let Some(stored) = state
            .sites
            .entry(site.to_owned())
            .or_default()
            .processes
            .get_mut(process)
            .and_then(|p| p.versions.iter_mut().find(|v| v.summary.version == version))
        {
            stored.summary.running_runs = runs;
        }
    }

    /// Marks a run of `version` as one a migration cannot move, with a reason code.
    pub fn add_unmovable(&self, site: &str, process: &str, version: u32, run: &str, reason: &str) {
        let mut state = self.lock();
        if let Some(p) = state
            .sites
            .entry(site.to_owned())
            .or_default()
            .processes
            .get_mut(process)
        {
            p.unmovable.entry(version).or_default().push(Unmoved {
                run: run.to_owned(),
                reason: reason.to_owned(),
            });
        }
    }

    /// Finishes a migration with these counts (a test helper standing for the batch).
    pub fn finish_migration(&self, site: &str, id: &str, moved: u32, skipped: u32, failed: u32) {
        let mut guard = self.lock();
        let state = &mut *guard;
        state.clock += 1;
        let at = stamp(state.clock);
        if let Some(stored) = state
            .sites
            .entry(site.to_owned())
            .or_default()
            .migrations
            .get_mut(id)
        {
            stored.progress.moved = moved;
            stored.progress.skipped = skipped;
            stored.progress.failed = failed;
            stored.progress.ended_at = Some(at);
            stored.progress.state = if failed > 0 {
                MigrationState::Failed
            } else {
                MigrationState::Completed
            };
        }
    }

    /// The calls the fake audited, oldest first.
    pub fn audit(&self) -> Vec<AuditEntry> {
        self.lock().audit.clone()
    }

    /// How many writes the fake took (publish, retire, migration start): a dry run adds none.
    pub fn write_count(&self) -> u64 {
        self.lock().writes
    }

    /// How many versions `process` holds on `site`.
    pub fn version_count(&self, site: &str, process: &str) -> usize {
        self.lock()
            .sites
            .get(site)
            .and_then(|s| s.processes.get(process))
            .map_or(0, |p| p.versions.len())
    }

    /// How many migrations `site` holds.
    pub fn migration_count(&self, site: &str) -> usize {
        self.lock()
            .sites
            .get(site)
            .map_or(0, |s| s.migrations.len())
    }

    /// The fake's validator, standing for the model's: a definition must start with `process `;
    /// a line `bad <node>` is an unknown node; a line `acts_as service_principal` marks a
    /// definition whose Document node acts as a service principal.
    fn validate(definition: &str) -> Vec<Problem> {
        let mut problems = Vec::new();
        if !definition.starts_with("process ") {
            problems.push(Problem {
                node: None,
                kind: "bpm_model.syntax".to_owned(),
            });
        }
        for line in definition.lines() {
            if let Some(node) = line.strip_prefix("bad ") {
                problems.push(Problem {
                    node: Some(node.trim().to_owned()),
                    kind: "bpm_model.unknown_node".to_owned(),
                });
            }
        }
        problems
    }
}

fn audit_entry(
    state: &mut State,
    site: &str,
    actor: &str,
    operation: &'static str,
    subject: &str,
    outcome: &'static str,
) {
    state.audit.push(AuditEntry {
        site: site.to_owned(),
        actor: actor.to_owned(),
        operation,
        subject: subject.to_owned(),
        outcome,
    });
}

impl SiteState {
    fn holds(&self, user: &str, grant: Grant) -> bool {
        self.grants
            .get(user)
            .is_some_and(|set| set.contains(&grant))
    }

    fn may_read(&self, user: &str) -> bool {
        self.holds(user, Grant::Publish) || self.holds(user, Grant::ReadVersions)
    }
}

/// The checks of a plan and a start, in the contract's order: an equal target is `422`, one that
/// is not published (an unknown one counts) `409`, a mapping naming a missing node `422`.
fn check_migration(
    process: &ProcessState,
    from: u32,
    request: &MigrationRequest,
) -> VersionResult<()> {
    if request.to == from {
        return Err(VersionPortError::MigrationInvalid);
    }
    let target = process
        .versions
        .iter()
        .find(|v| v.summary.version == request.to)
        .filter(|v| v.summary.state == VersionState::Published)
        .ok_or(VersionPortError::VersionNotPublished)?;
    if let Some(mapping) = &request.mapping {
        let nodes = nodes_of(&target.definition);
        if mapping
            .nodes
            .values()
            .any(|node| !nodes.contains(node.as_str()))
        {
            return Err(VersionPortError::MigrationInvalid);
        }
    }
    Ok(())
}

impl VersionPort for MemoryVersionPort {
    fn list_versions<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, VersionResult<VersionPage>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let offset = parse_cursor(cursor)?;
            let state = self.lock();
            let site = state
                .sites
                .get(ctx.site.as_str())
                .filter(|site| site.may_read(&user))
                .ok_or(VersionPortError::NotFound)?;
            let stored = site
                .processes
                .get(process)
                .ok_or(VersionPortError::NotFound)?;
            let all: Vec<&StoredVersion> = stored.versions.iter().rev().collect();
            let items: Vec<VersionSummary> = all
                .iter()
                .skip(offset)
                .take(limit as usize)
                .map(|v| v.summary.clone())
                .collect();
            let next = offset.saturating_add(limit as usize);
            let next_cursor = (next < all.len()).then(|| format!("c{next}"));
            Ok(VersionPage { items, next_cursor })
        })
    }

    fn publish<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        request: PublishRequest,
    ) -> BoxFuture<'a, VersionResult<Published>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let site_name = ctx.site.as_str().to_owned();
            let mut guard = self.lock();
            let state = &mut *guard;
            let site = state.sites.entry(site_name.clone()).or_default();
            let refuse = |state: &mut State, outcome: &'static str, error: VersionPortError| {
                audit_entry(
                    state,
                    &site_name,
                    &user,
                    "process_version_publish",
                    process,
                    outcome,
                );
                Err(error)
            };
            if !site.holds(&user, Grant::Publish) || !site.processes.contains_key(process) {
                return refuse(state, "process.not_found", VersionPortError::NotFound);
            }
            let principal = request
                .definition
                .lines()
                .any(|line| line.trim() == "acts_as service_principal");
            if principal && !site.holds(&user, Grant::PublishServicePrincipal) {
                return refuse(
                    state,
                    "process.publish_denied",
                    VersionPortError::PublishDenied,
                );
            }
            let problems = Self::validate(&request.definition);
            if !problems.is_empty() {
                return refuse(
                    state,
                    "process.definition_invalid",
                    VersionPortError::DefinitionInvalid(problems),
                );
            }
            let Some(stored) = site.processes.get_mut(process) else {
                return refuse(state, "process.not_found", VersionPortError::NotFound);
            };
            let latest = u32::try_from(stored.versions.len()).unwrap_or(u32::MAX);
            if request.expected_previous.is_some_and(|e| e != latest) {
                return refuse(
                    state,
                    "process.version_conflict",
                    VersionPortError::VersionConflict,
                );
            }
            state.clock += 1;
            let hash = content_hash(&request.definition);
            let summary = VersionSummary {
                version: latest + 1,
                state: VersionState::Published,
                published_at: Some(stamp(state.clock)),
                published_by: Some(user.clone()),
                note: request.note,
                running_runs: 0,
            };
            stored.versions.push(StoredVersion {
                summary: summary.clone(),
                definition: request.definition,
                hash: hash.clone(),
            });
            state.writes += 1;
            audit_entry(
                state,
                &site_name,
                &user,
                "process_version_publish",
                process,
                "ok",
            );
            Ok(Published {
                version: summary,
                hash,
            })
        })
    }

    fn get_version<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        version: u32,
    ) -> BoxFuture<'a, VersionResult<VersionDetail>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let state = self.lock();
            let site = state
                .sites
                .get(ctx.site.as_str())
                .filter(|site| site.may_read(&user))
                .ok_or(VersionPortError::NotFound)?;
            site.processes
                .get(process)
                .and_then(|p| p.versions.iter().find(|v| v.summary.version == version))
                .map(|v| VersionDetail {
                    version: v.summary.clone(),
                    definition: v.definition.clone(),
                    hash: v.hash.clone(),
                })
                .ok_or(VersionPortError::NotFound)
        })
    }

    fn retire<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        version: u32,
    ) -> BoxFuture<'a, VersionResult<VersionSummary>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let site_name = ctx.site.as_str().to_owned();
            let mut guard = self.lock();
            let state = &mut *guard;
            let site = state.sites.entry(site_name.clone()).or_default();
            let allowed = site.holds(&user, Grant::Publish);
            let found = site
                .processes
                .get_mut(process)
                .filter(|_| allowed)
                .and_then(|p| p.versions.iter_mut().find(|v| v.summary.version == version));
            let outcome = match found {
                None => Err(("process.not_found", VersionPortError::NotFound)),
                Some(stored) if stored.summary.state != VersionState::Published => Err((
                    "process.version_not_published",
                    VersionPortError::VersionNotPublished,
                )),
                Some(stored) => {
                    stored.summary.state = VersionState::Retired;
                    Ok(stored.summary.clone())
                }
            };
            let label = outcome.as_ref().err().map_or("ok", |(code, _)| *code);
            if outcome.is_ok() {
                state.writes += 1;
            }
            audit_entry(
                state,
                &site_name,
                &user,
                "process_version_retire",
                process,
                label,
            );
            outcome.map_err(|(_, error)| error)
        })
    }

    fn plan_migration<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        from: u32,
        request: MigrationRequest,
    ) -> BoxFuture<'a, VersionResult<MigrationPlan>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let site_name = ctx.site.as_str().to_owned();
            let mut guard = self.lock();
            let state = &mut *guard;
            let site = state.sites.entry(site_name.clone()).or_default();
            let allowed = site.holds(&user, Grant::Migrate);
            let outcome = site
                .processes
                .get(process)
                .filter(|_| allowed)
                .and_then(|p| {
                    p.versions
                        .iter()
                        .find(|v| v.summary.version == from)
                        .map(|source| (p, source))
                })
                .ok_or(VersionPortError::NotFound)
                .and_then(|(p, source)| {
                    check_migration(p, from, &request)?;
                    let unmoved = p.unmovable.get(&from).cloned().unwrap_or_default();
                    let movable = source.summary.running_runs;
                    Ok(MigrationPlan {
                        from,
                        to: request.to,
                        movable,
                        unmoved_total: u32::try_from(unmoved.len()).unwrap_or(u32::MAX),
                        node_counts: vec![NodeCount {
                            from: "start".to_owned(),
                            to: "start".to_owned(),
                            runs: movable,
                        }],
                        unmoved: unmoved.into_iter().take(MAX_UNMOVED).collect(),
                    })
                });
            let label = match &outcome {
                Ok(_) => "ok",
                Err(VersionPortError::NotFound) => "process.not_found",
                Err(VersionPortError::VersionNotPublished) => "process.version_not_published",
                Err(_) => "process.migration_invalid",
            };
            audit_entry(
                state,
                &site_name,
                &user,
                "process_migration_plan",
                process,
                label,
            );
            outcome
        })
    }

    fn start_migration<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        from: u32,
        request: MigrationRequest,
    ) -> BoxFuture<'a, VersionResult<String>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let site_name = ctx.site.as_str().to_owned();
            let mut guard = self.lock();
            let state = &mut *guard;
            state.clock += 1;
            let at = stamp(state.clock);
            let next = state.next_migration + 1;
            let site = state.sites.entry(site_name.clone()).or_default();
            let allowed = site.holds(&user, Grant::Migrate);
            let checked = site
                .processes
                .get(process)
                .filter(|_| allowed)
                .filter(|p| p.versions.iter().any(|v| v.summary.version == from))
                .ok_or(VersionPortError::NotFound)
                .and_then(|p| check_migration(p, from, &request));
            let outcome = checked.map(|()| {
                let id = format!("mig_{next:016}");
                site.migrations.insert(
                    id.clone(),
                    StoredMigration {
                        process: process.to_owned(),
                        progress: MigrationProgress {
                            state: MigrationState::Running,
                            moved: 0,
                            skipped: 0,
                            failed: 0,
                            started_at: at,
                            ended_at: None,
                        },
                    },
                );
                id
            });
            let label = match &outcome {
                Ok(_) => "ok",
                Err(VersionPortError::NotFound) => "process.not_found",
                Err(VersionPortError::VersionNotPublished) => "process.version_not_published",
                Err(_) => "process.migration_invalid",
            };
            if outcome.is_ok() {
                state.next_migration = next;
                state.writes += 1;
            }
            audit_entry(
                state,
                &site_name,
                &user,
                "process_migration_start",
                process,
                label,
            );
            outcome
        })
    }

    fn get_migration<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        process: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, VersionResult<MigrationProgress>> {
        Box::pin(async move {
            let user = caller(ctx)?;
            let state = self.lock();
            state
                .sites
                .get(ctx.site.as_str())
                .filter(|site| site.holds(&user, Grant::Migrate))
                .and_then(|site| site.migrations.get(id))
                .filter(|stored| stored.process == process)
                .map(|stored| stored.progress.clone())
                .ok_or(VersionPortError::NotFound)
        })
    }
}
