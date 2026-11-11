//! The tamper-evident identity audit sink (WP-PRD-01.T16).
//!
//! [`ChainAudit`] is an [`IdentityAudit`] over the platform's hash-chained audit store
//! (`spot-audit-store`, the chain `spot-doc-audit` writes to). One identity record is one chained
//! entry, so an altered, removed or reordered record makes [`IdentityChain::verify`] fail and name
//! the `seq` at fault. The file log can still be written beside it, as an optional mirror that is
//! never relied on.
//!
//! # Where the chain lives
//!
//! * The chain scope of a site is [`scope_of`]: the fixed prefix [`SCOPE_PREFIX`] and the site
//!   name. It is a chain of its own, so the identity trail verifies independently of the
//!   document trail, and no caller chooses the scope.
//! * An entry's body is the record's existing serialisation (the JSON the file log writes),
//!   carried as one text value. The entry's `kind` is the record's kind under the `identity.`
//!   prefix and its actor the user's id (or `system`); nothing is added to the record, and nothing
//!   secret is in it by construction.
//! * Appending is the store's compare-and-set on the chain head, so two nodes (or two sinks)
//!   appending at once keep one chain, and a restart resumes from the stored head: the sink holds
//!   no sequence number of its own.
//!
//! # Sync seam, async store
//!
//! [`IdentityAudit::append`] is synchronous and is called from request tasks, where the blocking
//! PostgreSQL bridge is refused. The sink therefore owns one plain thread with a small runtime of
//! its own; `append` hands the entry over and waits, bounded by [`APPEND_DEADLINE`], for the
//! entry to be durable in the database. A caller on a multi-threaded runtime yields its worker
//! while it waits. An append that times out is reported as failed, so a successful sign-in is
//! refused (fail closed) even though the entry may land a moment later: a duplicate record is
//! possible, a lost one is not.
//!
//! # Checkpoints
//!
//! The chain is not signed here: the audit chain's checkpoints need a signing key the host does
//! not hold yet (WP-SEC-03). Hash links still make any alteration, removal or reordering *inside*
//! the chain detectable; dropping the newest entries cannot be told from a shorter chain until a
//! signed checkpoint exists. The database layer ([`spot_audit_store::install_guard`]) refuses the
//! rewrite in the first place.
use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use spot_audit_chain::{
    AuditChain, AuditError as ChainError, AuditRecord as ChainRecord, CheckpointPolicy,
    CheckpointSigner, KeyRing, PayloadValue, Verifier,
};
use spot_audit_store::PgAuditStore;
use spot_http::health::{CheckResult, ReadinessCheck};
use spot_postgres::{PgBackend, PgConfig};

use super::{AuditError, AuditRecord, IdentityAudit};
use crate::{Error, Result};

/// The fixed prefix of an identity chain's scope; the site name follows. Site names are
/// lowercase letters, digits, `-` and `_`, so no site's identity scope can equal another
/// chain's scope.
pub const SCOPE_PREFIX: &str = "identity:";
/// How long `append` waits for the entry to be durable.
pub const APPEND_DEADLINE: Duration = Duration::from_secs(5);
/// Appends waiting for the worker; a full queue fails the append (fail closed) instead of
/// growing.
const QUEUE: usize = 256;
/// The entries one readiness check verifies at most; the scan resumes where it stopped.
const READY_PAGE: u64 = 10_000;
/// How often readiness starts the scan over from the first entry.
const FULL_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// The text of every failed append: fixed, so a failure never carries a record's content.
const APPEND_FAILED: &str = "the identity audit chain append failed";
/// The key of the entry body's one value.
const BODY_KEY: &str = "record";

/// The chain scope of `site`'s identity events.
#[must_use]
pub fn scope_of(site: &str) -> String {
    format!("{SCOPE_PREFIX}{site}")
}

/// A signer that never signs. The chain's automatic checkpoints are off ([`policy`]), so the
/// signer is never asked; if it ever were, the refusal keeps an unsigned checkpoint from being
/// written.
struct NoSigner;

impl CheckpointSigner for NoSigner {
    fn key_id(&self) -> String {
        String::new()
    }
    fn sign(&self, _message: &[u8]) -> std::result::Result<Vec<u8>, ChainError> {
        Err(ChainError::Signer("no checkpoint key is configured".into()))
    }
}

fn policy() -> CheckpointPolicy {
    CheckpointPolicy { every: 0 }
}

/// What an audit record becomes in the chain: its kind under `identity.`, the user as actor (or
/// `system`), the time in microseconds, and its JSON serialisation as the one payload value.
///
/// # Errors
///
/// [`AuditError`] when the record cannot be serialised or does not fit the chain's limits.
pub fn chain_record_of(record: &AuditRecord) -> std::result::Result<ChainRecord, AuditError> {
    let body = serde_json::to_string(record)
        .map_err(|_| AuditError("the audit record could not be encoded".into()))?;
    let mut kind = String::from("identity.");
    let mut start = true;
    for c in record.kind.chars() {
        if start && !c.is_ascii_lowercase() {
            kind.push('k');
        }
        start = false;
        kind.push(
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            },
        );
    }
    let actor = match record.user.as_deref() {
        Some(user) if !user.is_empty() && user.len() <= 200 => format!("user:{user}"),
        _ => "system".to_owned(),
    };
    let mut payload = BTreeMap::new();
    payload.insert(BODY_KEY.to_owned(), PayloadValue::Text(body));
    let chained = ChainRecord {
        kind,
        actor,
        on_behalf_of: None,
        subject: None,
        at: record.at.saturating_mul(1_000_000),
        payload,
        // The record carries a redacted attempted name, a salted address digest and opaque ids:
        // no protected value.
        masked: true,
    };
    chained
        .validate()
        .map_err(|_| AuditError("the audit record does not fit the audit chain".into()))?;
    Ok(chained)
}

struct Job {
    record: ChainRecord,
    reply: SyncSender<std::result::Result<(), ChainError>>,
}

/// Opens the sink's backend on the worker's own runtime and reports how it went.
enum Opened {
    /// The store is prepared and protected.
    Ready(PgBackend),
    /// The database answers but the audit store is not (fully) there; the names.
    Unprepared(Vec<&'static str>),
}

/// The identity audit sink over the hash chain.
pub struct ChainAudit {
    scope: String,
    jobs: SyncSender<Job>,
    mirror: Option<Arc<dyn IdentityAudit>>,
    deadline: Duration,
}

impl ChainAudit {
    /// Opens the chain sink of `site` on the site's database.
    ///
    /// Returns `Ok(Err(missing))`, the names of the missing parts, when the database is reachable but the audit store is not prepared
    /// (its tables or guard are missing): the caller falls back to the file log and says so. An
    /// error means the database could not be reached at all.
    ///
    /// `mirror` is written after every successful chain append; its failure never fails one.
    ///
    /// # Errors
    ///
    /// [`Error`] when the worker cannot start or the database cannot be reached.
    pub async fn open(
        pg: PgConfig,
        site: &str,
        mirror: Option<Arc<dyn IdentityAudit>>,
    ) -> Result<std::result::Result<(ChainAudit, IdentityChain), Vec<&'static str>>> {
        let scope = scope_of(site);
        let (jobs, queue) = sync_channel::<Job>(QUEUE);
        let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
        let worker_scope = scope.clone();
        std::thread::Builder::new()
            .name("identity-audit".into())
            .spawn(move || worker(pg, worker_scope, queue, opened_tx))
            .map_err(|e| {
                Error(format!(
                    "cannot start the identity audit worker: {}",
                    e.kind()
                ))
            })?;
        let opened = opened_rx
            .await
            .map_err(|_| Error("the identity audit worker stopped while starting".into()))??;
        match opened {
            Opened::Unprepared(missing) => Ok(Err(missing)),
            Opened::Ready(backend) => Ok(Ok((
                ChainAudit {
                    scope: scope.clone(),
                    jobs,
                    mirror,
                    deadline: APPEND_DEADLINE,
                },
                IdentityChain::new(&backend, &scope),
            ))),
        }
    }

    /// Like [`ChainAudit::open`] with another append deadline (for tests).
    #[must_use]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    /// The chain scope this sink writes.
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    fn wait(&self, record: ChainRecord) -> std::result::Result<(), AuditError> {
        let (reply, answer) = sync_channel(1);
        self.jobs
            .try_send(Job { record, reply })
            .map_err(|_| AuditError(APPEND_FAILED.into()))?;
        let wait = || answer.recv_timeout(self.deadline);
        // A request task must not hold its worker while the entry becomes durable.
        let waited = match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(wait)
            }
            _ => wait(),
        };
        match waited {
            Ok(Ok(())) => Ok(()),
            _ => Err(AuditError(APPEND_FAILED.into())),
        }
    }
}

impl IdentityAudit for ChainAudit {
    fn append(&self, record: &AuditRecord) -> std::result::Result<(), AuditError> {
        self.wait(chain_record_of(record)?)?;
        if let Some(mirror) = &self.mirror
            && mirror.append(record).is_err()
        {
            // The mirror is a convenience: the entry is already durable in the chain.
            tracing::warn!(kind = %record.kind, "the identity audit file mirror failed");
        }
        Ok(())
    }
}

/// The worker: one plain thread, one runtime, the backend opened on it so every connection's
/// driver lives as long as the sink does.
fn worker(
    pg: PgConfig,
    scope: String,
    queue: Receiver<Job>,
    opened: tokio::sync::oneshot::Sender<Result<Opened>>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            let _ = opened.send(Err(Error(format!(
                "cannot start the identity audit runtime: {}",
                e.kind()
            ))));
            return;
        }
    };
    let start = runtime.block_on(async {
        let backend = PgBackend::connect(pg).await.map_err(crate::error)?;
        let protection = spot_audit_store::verify(&backend.pool())
            .await
            .map_err(crate::error)?;
        Ok::<_, Error>(if protection.is_complete() {
            Opened::Ready(backend)
        } else {
            Opened::Unprepared(protection.missing())
        })
    });
    let chain = match &start {
        Ok(Opened::Ready(backend)) => Some(AuditChain::new(
            PgAuditStore::new(backend.pool()),
            scope,
            Arc::new(NoSigner),
            policy(),
        )),
        _ => None,
    };
    if opened.send(start).is_err() {
        return;
    }
    let Some(chain) = chain else { return };
    // `recv` ends when the sink (the only sender) is dropped; the runtime then drops here, on a
    // plain thread, where dropping a runtime is allowed.
    while let Ok(job) = queue.recv() {
        let result = runtime.block_on(chain.append(job.record)).map(|_| ());
        let _ = job.reply.send(result);
    }
    drop(runtime);
}

// ---- verification --------------------------------------------------------------------------

/// Read access to one site's identity chain: its head and its verification.
#[derive(Clone)]
pub struct IdentityChain {
    store: Arc<PgAuditStore>,
    scope: String,
}

/// What a verification found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainReport {
    /// Entries whose hash and link were checked.
    pub checked: u64,
    /// The newest entry's sequence number (0 for an empty chain).
    pub head: u64,
    /// Each break: the entry's `seq` and the failure's stable code. Never a record's content.
    pub failures: Vec<(u64, &'static str)>,
}

impl ChainReport {
    /// Whether the chain verified.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }
}

impl IdentityChain {
    /// The identity chain of `scope` on `backend`'s database.
    #[must_use]
    pub fn new(backend: &PgBackend, scope: &str) -> Self {
        Self {
            store: Arc::new(PgAuditStore::new(backend.pool())),
            scope: scope.to_owned(),
        }
    }

    /// The chain scope.
    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// The sequence number of the newest entry (0 for an empty chain).
    ///
    /// # Errors
    ///
    /// [`Error`] when the store cannot be read.
    pub async fn head(&self) -> Result<u64> {
        let head = spot_audit_chain::AuditStore::head(&*self.store, &self.scope)
            .await
            .map_err(crate::error)?;
        Ok(head.map_or(0, |entry| entry.seq))
    }

    /// Verifies the entries from `from` (1 for the whole chain): every hash, every link, every
    /// sequence number. A range after the first entry is anchored on the entry before it.
    ///
    /// # Errors
    ///
    /// [`Error`] when the store cannot be read (a verification that did not run is not a pass).
    pub async fn verify_from(&self, from: u64) -> Result<ChainReport> {
        self.verify_range(from, u64::MAX).await
    }

    async fn verify_range(&self, from: u64, to: u64) -> Result<ChainReport> {
        let head = self.head().await?;
        let keys = KeyRing::new();
        let report = Verifier::new(&keys)
            .verify_range(&*self.store, &self.scope, from, to)
            .await
            .map_err(crate::error)?;
        Ok(ChainReport {
            checked: report.checked,
            head,
            failures: report
                .failures
                .iter()
                .map(|f| (f.seq, f.kind.code()))
                .collect(),
        })
    }

    /// Verifies the whole chain.
    ///
    /// # Errors
    ///
    /// As [`IdentityChain::verify_from`].
    pub async fn verify(&self) -> Result<ChainReport> {
        self.verify_from(1).await
    }
}

// ---- readiness -----------------------------------------------------------------------------

#[derive(Default)]
struct Progress {
    /// The newest entry known to verify.
    through: u64,
    /// When the scan last started from the first entry.
    full_at: Option<Instant>,
}

/// `/api/health/ready`'s view of one site's identity chain. It verifies a page of entries per
/// call and resumes where the last call stopped, restarting from the first entry once a day, so
/// the cost of a check stays bounded however long the chain grows. A break is reported as a
/// warning by default (the check stays ready); `strict` makes it fail readiness.
pub struct IdentityChainReady {
    name: String,
    chain: IdentityChain,
    strict: bool,
    progress: Mutex<Progress>,
}

impl IdentityChainReady {
    /// The check of `site`'s chain.
    #[must_use]
    pub fn new(site: &str, chain: IdentityChain, strict: bool) -> Self {
        Self {
            name: format!("identity-audit:{site}"),
            chain,
            strict,
            progress: Mutex::new(Progress::default()),
        }
    }

    async fn run(&self) -> CheckResult {
        let (from, restart) = {
            let mut progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
            if progress.full_at.is_none_or(|at| at.elapsed() >= FULL_EVERY) {
                *progress = Progress {
                    through: 0,
                    full_at: Some(Instant::now()),
                };
            }
            (progress.through.max(1), progress.through == 0)
        };
        let head = match self.chain.head().await {
            Ok(head) => head,
            Err(_) => return self.problem("identity audit chain: cannot read the chain".into()),
        };
        // A long tail is verified a page at a time; the last page runs to the end so an entry
        // whose sequence number was moved beyond the page is still seen.
        let to = if head.saturating_sub(from) > READY_PAGE {
            from.saturating_add(READY_PAGE)
        } else {
            u64::MAX
        };
        match self.chain.verify_range(from, to).await {
            Err(_) => self.problem("identity audit chain: verification did not run".into()),
            Ok(report) if !report.is_ok() => {
                let (seq, code) = report.failures[0];
                // A break is never "verified through": the scan starts over next time.
                *self.progress.lock().unwrap_or_else(|e| e.into_inner()) = Progress::default();
                self.problem(format!(
                    "identity audit chain: BROKEN at seq {seq} ({code}), {} break(s)",
                    report.failures.len()
                ))
            }
            Ok(report) => {
                let through = if to == u64::MAX { report.head } else { to };
                self.progress
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .through = through;
                let mut result = CheckResult::ok();
                result.detail = format!(
                    "identity audit chain: verified through seq {through} of {head}{}",
                    if restart { " (full scan)" } else { "" }
                );
                result
            }
        }
    }

    fn problem(&self, detail: String) -> CheckResult {
        if self.strict {
            CheckResult::failed(detail)
        } else {
            let mut result = CheckResult::ok();
            result.detail = format!("WARNING {detail}");
            result
        }
    }
}

impl ReadinessCheck for IdentityChainReady {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CheckResult> + Send + '_>> {
        Box::pin(self.run())
    }
}

/// The readiness line of a site whose identity trail is the file log only.
#[must_use]
pub fn file_only_detail() -> &'static str {
    "identity audit: file log only, NOT tamper-evident (no audit chain)"
}

/// A check that reports [`file_only_detail`].
pub struct FileOnlyReady {
    name: String,
}

impl FileOnlyReady {
    /// The check of `site`.
    #[must_use]
    pub fn new(site: &str) -> Self {
        Self {
            name: format!("identity-audit:{site}"),
        }
    }
}

impl ReadinessCheck for FileOnlyReady {
    fn name(&self) -> &str {
        &self.name
    }

    fn check(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CheckResult> + Send + '_>> {
        Box::pin(async {
            let mut result = CheckResult::ok();
            result.detail = file_only_detail().to_owned();
            result
        })
    }
}
