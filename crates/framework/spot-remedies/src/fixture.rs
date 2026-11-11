//! The in-memory fixture the self-test harness runs on: a fake over every typed port, plus a
//! read-back surface the scenario invariants assert against.
//!
//! The fixture stands in for the real subsystems — `spot-jobs`, `spot-cache`, `spot-retrieval`,
//! `spot-edge-sync`, `spot-quarantine`, the BPM engine and the document engine — behind the same
//! ports, so a remedy's scenario proves it end to end without a database.

use std::collections::BTreeMap;

use crate::audit::MemoryAudit;
use crate::ctx::{RemedyCtx, RemedySettings};
use crate::ports::{
    CacheChange, CacheNs, CacheOps, CaseChange, CaseLife, CaseOps, CaseRef, CaseView, DocOp,
    DocRef, DocState, DocumentChange, DocumentOps, DocumentView, EdgeChange, EdgeOps, EdgeRef,
    EdgeView, IndexChange, IndexOps, IndexRef, IndexView, JobChange, JobLife, JobOps, JobRef,
    JobView, OpError, QuarantineChange, QuarantineItem, QuarantineOps, QuarantineRef,
    QuarantineState,
};
use crate::scenario::WorldSpec;

/// The fake job store.
#[derive(Clone, Debug, Default)]
pub struct FakeJobs {
    jobs: BTreeMap<String, FakeJob>,
}

#[derive(Clone, Debug)]
struct FakeJob {
    life: JobLife,
    attempts: u32,
    lease_expires_at: Option<i64>,
}

impl FakeJobs {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds a job.
    pub fn seed(&mut self, id: &str, life: JobLife, attempts: u32, lease: Option<i64>) {
        self.jobs.insert(
            id.to_owned(),
            FakeJob {
                life,
                attempts,
                lease_expires_at: lease,
            },
        );
    }
}

impl JobOps for FakeJobs {
    fn failed(&self) -> Vec<JobRef> {
        self.jobs
            .iter()
            .filter(|(_, job)| job.life == JobLife::Failed)
            .map(|(id, _)| JobRef::new(id))
            .collect()
    }

    fn running(&self) -> Vec<JobRef> {
        self.jobs
            .iter()
            .filter(|(_, job)| job.life == JobLife::Running)
            .map(|(id, _)| JobRef::new(id))
            .collect()
    }

    fn job(&self, id: &JobRef) -> Option<JobView> {
        self.jobs.get(id.as_str()).map(|job| JobView {
            id: id.clone(),
            life: job.life,
            attempts: job.attempts,
            lease_expires_at: job.lease_expires_at,
        })
    }

    fn retry(&mut self, id: &JobRef, _now: i64) -> Result<JobChange, OpError> {
        let job = self
            .jobs
            .get_mut(id.as_str())
            .ok_or_else(|| OpError::new("job_retry", &format!("no job `{}`", id.as_str())))?;
        if job.life != JobLife::Failed {
            return Err(OpError::new(
                "job_retry",
                &format!("job `{}` is {}, not failed", id.as_str(), job.life.as_str()),
            ));
        }
        let before = job.life;
        job.life = JobLife::Queued;
        job.lease_expires_at = None;
        Ok(JobChange {
            id: id.clone(),
            before,
            after: JobLife::Queued,
        })
    }

    fn unstick(&mut self, id: &JobRef, now: i64) -> Result<JobChange, OpError> {
        let job = self
            .jobs
            .get_mut(id.as_str())
            .ok_or_else(|| OpError::new("job_unstick", &format!("no job `{}`", id.as_str())))?;
        if job.life != JobLife::Running {
            return Err(OpError::new(
                "job_unstick",
                &format!(
                    "job `{}` is {}, not running",
                    id.as_str(),
                    job.life.as_str()
                ),
            ));
        }
        match job.lease_expires_at {
            Some(expires) if expires <= now => {}
            _ => {
                return Err(OpError::new(
                    "job_unstick",
                    &format!("the lease on job `{}` has not expired", id.as_str()),
                ));
            }
        }
        let before = job.life;
        job.life = JobLife::Queued;
        job.lease_expires_at = None;
        Ok(JobChange {
            id: id.clone(),
            before,
            after: JobLife::Queued,
        })
    }

    fn restore(&mut self, id: &JobRef, life: JobLife) -> Result<(), OpError> {
        let job = self
            .jobs
            .get_mut(id.as_str())
            .ok_or_else(|| OpError::new("job_restore", &format!("no job `{}`", id.as_str())))?;
        job.life = life;
        Ok(())
    }
}

/// The fake cache store.
#[derive(Clone, Debug, Default)]
pub struct FakeCache {
    namespaces: BTreeMap<String, usize>,
}

impl FakeCache {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds a namespace.
    pub fn seed(&mut self, ns: &str, entries: usize) {
        self.namespaces.insert(ns.to_owned(), entries);
    }
}

impl CacheOps for FakeCache {
    fn namespaces(&self) -> Vec<CacheNs> {
        self.namespaces.keys().map(|ns| CacheNs::new(ns)).collect()
    }

    fn entries(&self, ns: &CacheNs) -> usize {
        self.namespaces.get(ns.as_str()).copied().unwrap_or(0)
    }

    fn flush(&mut self, ns: &CacheNs) -> Result<CacheChange, OpError> {
        let entries = self.namespaces.get_mut(ns.as_str()).ok_or_else(|| {
            OpError::new("cache_flush", &format!("no namespace `{}`", ns.as_str()))
        })?;
        let before = *entries;
        *entries = 0;
        Ok(CacheChange {
            ns: ns.clone(),
            before,
            after: 0,
        })
    }

    fn undo_flush(&mut self, change: &CacheChange) -> Result<(), OpError> {
        if !self.namespaces.contains_key(change.ns.as_str()) {
            return Err(OpError::new(
                "cache_undo_flush",
                &format!("no namespace `{}`", change.ns.as_str()),
            ));
        }
        // A cache is derived data: the reversal records the intent without restoring entries.
        Ok(())
    }
}

/// The fake retrieval-index store.
#[derive(Clone, Debug, Default)]
pub struct FakeIndex {
    indexes: BTreeMap<String, FakeIndexEntry>,
}

#[derive(Clone, Debug)]
struct FakeIndexEntry {
    health: crate::ports::IndexHealth,
    generation: u64,
}

impl FakeIndex {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds an index.
    pub fn seed(&mut self, name: &str, health: crate::ports::IndexHealth, generation: u64) {
        self.indexes
            .insert(name.to_owned(), FakeIndexEntry { health, generation });
    }
}

impl IndexOps for FakeIndex {
    fn stale(&self) -> Vec<IndexRef> {
        self.indexes
            .iter()
            .filter(|(_, entry)| entry.health == crate::ports::IndexHealth::Stale)
            .map(|(name, _)| IndexRef::new(name))
            .collect()
    }

    fn index(&self, name: &IndexRef) -> Option<IndexView> {
        self.indexes.get(name.as_str()).map(|entry| IndexView {
            name: name.clone(),
            health: entry.health,
            generation: entry.generation,
        })
    }

    fn rebuild(&mut self, name: &IndexRef) -> Result<IndexChange, OpError> {
        let entry = self.indexes.get_mut(name.as_str()).ok_or_else(|| {
            OpError::new("index_rebuild", &format!("no index `{}`", name.as_str()))
        })?;
        let before = entry.health;
        let before_generation = entry.generation;
        entry.health = crate::ports::IndexHealth::Fresh;
        entry.generation += 1;
        Ok(IndexChange {
            name: name.clone(),
            before,
            after: crate::ports::IndexHealth::Fresh,
            before_generation,
            after_generation: entry.generation,
        })
    }

    fn restore(&mut self, change: &IndexChange) -> Result<(), OpError> {
        let entry = self.indexes.get_mut(change.name.as_str()).ok_or_else(|| {
            OpError::new(
                "index_restore",
                &format!("no index `{}`", change.name.as_str()),
            )
        })?;
        entry.health = change.before;
        entry.generation = change.before_generation;
        Ok(())
    }
}

/// The fake edge store.
#[derive(Clone, Debug, Default)]
pub struct FakeEdge {
    edges: BTreeMap<String, FakeEdgeEntry>,
}

#[derive(Clone, Debug)]
struct FakeEdgeEntry {
    expected: u64,
    actual: u64,
}

impl FakeEdge {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds an edge.
    pub fn seed(&mut self, edge: &str, expected: u64, actual: u64) {
        self.edges
            .insert(edge.to_owned(), FakeEdgeEntry { expected, actual });
    }
}

impl EdgeOps for FakeEdge {
    fn drifted(&self) -> Vec<EdgeRef> {
        self.edges
            .iter()
            .filter(|(_, entry)| entry.expected != entry.actual)
            .map(|(edge, _)| EdgeRef::new(edge))
            .collect()
    }

    fn edge(&self, edge: &EdgeRef) -> Option<EdgeView> {
        self.edges.get(edge.as_str()).map(|entry| EdgeView {
            edge: edge.clone(),
            expected: entry.expected,
            actual: entry.actual,
        })
    }

    fn repair(&mut self, edge: &EdgeRef) -> Result<EdgeChange, OpError> {
        let entry = self
            .edges
            .get_mut(edge.as_str())
            .ok_or_else(|| OpError::new("edge_repair", &format!("no edge `{}`", edge.as_str())))?;
        let before = entry.actual;
        entry.actual = entry.expected;
        Ok(EdgeChange {
            edge: edge.clone(),
            before,
            after: entry.actual,
        })
    }

    fn restore(&mut self, change: &EdgeChange) -> Result<(), OpError> {
        let entry = self.edges.get_mut(change.edge.as_str()).ok_or_else(|| {
            OpError::new(
                "edge_restore",
                &format!("no edge `{}`", change.edge.as_str()),
            )
        })?;
        entry.actual = change.before;
        Ok(())
    }
}

/// The fake quarantine store.
#[derive(Clone, Debug, Default)]
pub struct FakeQuarantine {
    items: BTreeMap<String, QuarantineState>,
}

impl FakeQuarantine {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds an item.
    pub fn seed(&mut self, id: &str, state: QuarantineState) {
        self.items.insert(id.to_owned(), state);
    }
}

impl QuarantineOps for FakeQuarantine {
    fn held(&self) -> Vec<QuarantineRef> {
        self.items
            .iter()
            .filter(|(_, state)| **state == QuarantineState::Held)
            .map(|(id, _)| QuarantineRef::new(id))
            .collect()
    }

    fn item(&self, id: &QuarantineRef) -> Option<QuarantineItem> {
        self.items.get(id.as_str()).map(|state| QuarantineItem {
            id: id.clone(),
            state: *state,
        })
    }

    fn retry(&mut self, id: &QuarantineRef) -> Result<QuarantineChange, OpError> {
        let state = self.items.get_mut(id.as_str()).ok_or_else(|| {
            OpError::new(
                "quarantine_retry",
                &format!("no quarantined item `{}`", id.as_str()),
            )
        })?;
        if *state != QuarantineState::Held {
            return Err(OpError::new(
                "quarantine_retry",
                &format!("item `{}` is {}, not held", id.as_str(), state.as_str()),
            ));
        }
        let before = *state;
        *state = QuarantineState::Retrying;
        Ok(QuarantineChange {
            id: id.clone(),
            before,
            after: QuarantineState::Retrying,
        })
    }

    fn restore(&mut self, change: &QuarantineChange) -> Result<(), OpError> {
        let state = self.items.get_mut(change.id.as_str()).ok_or_else(|| {
            OpError::new(
                "quarantine_restore",
                &format!("no quarantined item `{}`", change.id.as_str()),
            )
        })?;
        *state = change.before;
        Ok(())
    }
}

/// The fake BPM case store.
#[derive(Clone, Debug, Default)]
pub struct FakeCase {
    cases: BTreeMap<String, CaseLife>,
}

impl FakeCase {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds a case.
    pub fn seed(&mut self, id: &str, life: CaseLife) {
        self.cases.insert(id.to_owned(), life);
    }
}

impl CaseOps for FakeCase {
    fn stuck(&self) -> Vec<CaseRef> {
        self.cases
            .iter()
            .filter(|(_, life)| **life == CaseLife::Stuck)
            .map(|(id, _)| CaseRef::new(id))
            .collect()
    }

    fn case(&self, id: &CaseRef) -> Option<CaseView> {
        self.cases.get(id.as_str()).map(|life| CaseView {
            id: id.clone(),
            life: *life,
        })
    }

    fn unstick(&mut self, id: &CaseRef, _now: i64) -> Result<CaseChange, OpError> {
        let life = self
            .cases
            .get_mut(id.as_str())
            .ok_or_else(|| OpError::new("case_unstick", &format!("no case `{}`", id.as_str())))?;
        if *life != CaseLife::Stuck {
            return Err(OpError::new(
                "case_unstick",
                &format!("case `{}` is {}, not stuck", id.as_str(), life.as_str()),
            ));
        }
        let before = *life;
        *life = CaseLife::Running;
        Ok(CaseChange {
            id: id.clone(),
            before,
            after: CaseLife::Running,
        })
    }

    fn restore(&mut self, change: &CaseChange) -> Result<(), OpError> {
        let life = self.cases.get_mut(change.id.as_str()).ok_or_else(|| {
            OpError::new("case_restore", &format!("no case `{}`", change.id.as_str()))
        })?;
        *life = change.before;
        Ok(())
    }
}

/// The fake document store.
#[derive(Clone, Debug, Default)]
pub struct FakeDocuments {
    docs: BTreeMap<String, DocumentView>,
}

impl FakeDocuments {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Seeds a document.
    pub fn seed(&mut self, doc: &str, state: DocState, financial: bool) {
        self.docs
            .insert(doc.to_owned(), DocumentView { state, financial });
    }
}

impl DocumentOps for FakeDocuments {
    fn view(&self, doc: &DocRef) -> Option<DocumentView> {
        self.docs.get(doc.as_str()).copied()
    }

    fn run(&mut self, doc: &DocRef, op: &DocOp) -> Result<DocumentChange, OpError> {
        let view = self
            .docs
            .get_mut(doc.as_str())
            .ok_or_else(|| OpError::new(op.name(), &format!("no document `{}`", doc.as_str())))?;
        let before = view.state;
        let detail = match op {
            DocOp::Recompute { field } => format!("recomputed `{field}`"),
            DocOp::Repost => {
                view.state = DocState::Submitted;
                "reposted".to_owned()
            }
            DocOp::Relink { link } => format!("relinked `{link}`"),
            DocOp::Cancel { reason } => {
                view.state = DocState::Cancelled;
                format!("cancelled: {reason}")
            }
            DocOp::Amend => {
                view.state = DocState::Submitted;
                "amended".to_owned()
            }
            DocOp::JournalEntry { entry } => format!("journal entry `{entry}`"),
        };
        Ok(DocumentChange {
            doc: doc.clone(),
            before,
            after: view.state,
            detail,
        })
    }
}

/// The whole fixture: one fake per port, plus its own audit sink.
#[derive(Clone, Debug, Default)]
pub struct FakeWorld {
    /// The job store.
    pub jobs: FakeJobs,
    /// The cache store.
    pub cache: FakeCache,
    /// The retrieval-index store.
    pub index: FakeIndex,
    /// The edge store.
    pub edge: FakeEdge,
    /// The quarantine store.
    pub quarantine: FakeQuarantine,
    /// The case store.
    pub case: FakeCase,
    /// The document store.
    pub document: FakeDocuments,
    /// The audit sink.
    pub audit: MemoryAudit,
}

impl FakeWorld {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds the world from a scenario's spec, with the fault in it.
    #[must_use]
    pub fn from_spec(spec: &WorldSpec) -> Self {
        let mut world = Self::new();
        for job in &spec.jobs {
            world
                .jobs
                .seed(&job.id, job.life, job.attempts, job.lease_expires_at);
        }
        for cache in &spec.cache {
            world.cache.seed(&cache.ns, cache.entries);
        }
        for index in &spec.indexes {
            world
                .index
                .seed(&index.name, index.health, index.generation);
        }
        for edge in &spec.edges {
            world.edge.seed(&edge.edge, edge.expected, edge.actual);
        }
        for item in &spec.quarantine {
            world.quarantine.seed(&item.id, item.state);
        }
        for case in &spec.cases {
            world.case.seed(&case.id, case.life);
        }
        for document in &spec.documents {
            world
                .document
                .seed(&document.doc, document.state, document.financial);
        }
        world
    }

    /// A context over every port, the actor, the instant, the settings and the audit sink.
    pub fn ctx<'a>(
        &'a mut self,
        actor: &'a str,
        now: i64,
        settings: &'a RemedySettings,
    ) -> RemedyCtx<'a> {
        RemedyCtx::new(
            &mut self.jobs,
            &mut self.cache,
            &mut self.index,
            &mut self.edge,
            &mut self.quarantine,
            &mut self.case,
            &mut self.document,
            &mut self.audit,
            actor,
            now,
            settings,
        )
    }

    /// One job's state, for a scenario invariant.
    #[must_use]
    pub fn job_life(&self, id: &str) -> Option<JobLife> {
        self.jobs.job(&JobRef::new(id)).map(|view| view.life)
    }

    /// How many jobs are failed.
    #[must_use]
    pub fn failed_jobs(&self) -> usize {
        self.jobs.failed().len()
    }

    /// One cache namespace's entry count.
    #[must_use]
    pub fn cache_entries(&self, ns: &str) -> usize {
        self.cache.entries(&CacheNs::new(ns))
    }

    /// One index's health.
    #[must_use]
    pub fn index_health(&self, name: &str) -> Option<crate::ports::IndexHealth> {
        self.index
            .index(&IndexRef::new(name))
            .map(|view| view.health)
    }

    /// One edge's actual count.
    #[must_use]
    pub fn edge_actual(&self, edge: &str) -> Option<u64> {
        self.edge.edge(&EdgeRef::new(edge)).map(|view| view.actual)
    }

    /// One quarantined item's state.
    #[must_use]
    pub fn quarantine_state(&self, id: &str) -> Option<QuarantineState> {
        self.quarantine
            .item(&QuarantineRef::new(id))
            .map(|view| view.state)
    }

    /// One case's state.
    #[must_use]
    pub fn case_life(&self, id: &str) -> Option<CaseLife> {
        self.case.case(&CaseRef::new(id)).map(|view| view.life)
    }

    /// One document's state.
    #[must_use]
    pub fn doc_state(&self, doc: &str) -> Option<DocState> {
        self.document.view(&DocRef::new(doc)).map(|view| view.state)
    }
}
