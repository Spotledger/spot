//! The in-memory parts: every part over the fixture stores of `spot_remedies::fixture`.
//!
//! This is the seam the tests (and any later rig) build a composer from without a site: a
//! [`FixtureWorld`] owns one fake store per port, [`FixtureWorld::parts`] hands out the seven parts
//! over them, and [`FixtureWorld::fingerprint`] reads the whole world back as text so a test can
//! prove "byte-identical after undo" and "a rehearsal never touched the live world".
//!
//! A fixture part's `live` ops share the store; its `rehearsal` ops are an independent clone taken
//! under the store's lock (a consistent snapshot), so two rehearsals are independent of each other
//! and of the live store.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use spot_remedies::fixture::{
    FakeCache, FakeCase, FakeDocuments, FakeEdge, FakeIndex, FakeJobs, FakeQuarantine,
};
use spot_remedies::{
    CacheChange, CacheNs, CacheOps, CaseChange, CaseLife, CaseOps, CaseRef, CaseView, DocOp,
    DocRef, DocumentChange, DocumentOps, DocumentView, EdgeChange, EdgeOps, EdgeRef, EdgeView,
    FakeWorld, IndexChange, IndexOps, IndexRef, IndexView, JobChange, JobLife, JobOps, JobRef,
    JobView, OpError, QuarantineChange, QuarantineItem, QuarantineOps, QuarantineRef,
};

use super::Parts;
use super::parts::{
    CachePart, CasesPart, DocumentsPart, EdgePart, IndexPart, JobsPart, QuarantinePart,
};
use crate::services::remedies::world::WorldError;

fn lock<T>(store: &Mutex<T>) -> MutexGuard<'_, T> {
    store.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One fixture store shared between the live ops handed out and the part that owns it.
#[derive(Debug)]
pub struct FixturePart<T>(Arc<Mutex<T>>);

impl<T> FixturePart<T> {
    /// A part over a new store.
    #[must_use]
    pub fn new(store: T) -> Self {
        Self(Arc::new(Mutex::new(store)))
    }

    fn shared(store: &Arc<Mutex<T>>) -> Self {
        Self(Arc::clone(store))
    }
}

impl<T: Clone> FixturePart<T> {
    /// A copy of the store as it is now.
    #[must_use]
    pub fn snapshot(&self) -> T {
        lock(&self.0).clone()
    }
}

/// The live ops of a fixture part: every call takes the store's lock for its own duration.
#[derive(Debug)]
pub struct SharedOps<T>(Arc<Mutex<T>>);

impl JobOps for SharedOps<FakeJobs> {
    fn failed(&self) -> Vec<JobRef> {
        lock(&self.0).failed()
    }

    fn running(&self) -> Vec<JobRef> {
        lock(&self.0).running()
    }

    fn job(&self, id: &JobRef) -> Option<JobView> {
        lock(&self.0).job(id)
    }

    fn retry(&mut self, id: &JobRef, now: i64) -> Result<JobChange, OpError> {
        lock(&self.0).retry(id, now)
    }

    fn unstick(&mut self, id: &JobRef, now: i64) -> Result<JobChange, OpError> {
        lock(&self.0).unstick(id, now)
    }

    fn restore(&mut self, id: &JobRef, life: JobLife) -> Result<(), OpError> {
        lock(&self.0).restore(id, life)
    }
}

impl CacheOps for SharedOps<FakeCache> {
    fn namespaces(&self) -> Vec<CacheNs> {
        lock(&self.0).namespaces()
    }

    fn entries(&self, ns: &CacheNs) -> usize {
        lock(&self.0).entries(ns)
    }

    fn flush(&mut self, ns: &CacheNs) -> Result<CacheChange, OpError> {
        lock(&self.0).flush(ns)
    }

    fn undo_flush(&mut self, change: &CacheChange) -> Result<(), OpError> {
        lock(&self.0).undo_flush(change)
    }
}

impl IndexOps for SharedOps<FakeIndex> {
    fn stale(&self) -> Vec<IndexRef> {
        lock(&self.0).stale()
    }

    fn index(&self, name: &IndexRef) -> Option<IndexView> {
        lock(&self.0).index(name)
    }

    fn rebuild(&mut self, name: &IndexRef) -> Result<IndexChange, OpError> {
        lock(&self.0).rebuild(name)
    }

    fn restore(&mut self, change: &IndexChange) -> Result<(), OpError> {
        lock(&self.0).restore(change)
    }
}

impl EdgeOps for SharedOps<FakeEdge> {
    fn drifted(&self) -> Vec<EdgeRef> {
        lock(&self.0).drifted()
    }

    fn edge(&self, edge: &EdgeRef) -> Option<EdgeView> {
        lock(&self.0).edge(edge)
    }

    fn repair(&mut self, edge: &EdgeRef) -> Result<EdgeChange, OpError> {
        lock(&self.0).repair(edge)
    }

    fn restore(&mut self, change: &EdgeChange) -> Result<(), OpError> {
        lock(&self.0).restore(change)
    }
}

impl QuarantineOps for SharedOps<FakeQuarantine> {
    fn held(&self) -> Vec<QuarantineRef> {
        lock(&self.0).held()
    }

    fn item(&self, id: &QuarantineRef) -> Option<QuarantineItem> {
        lock(&self.0).item(id)
    }

    fn retry(&mut self, id: &QuarantineRef) -> Result<QuarantineChange, OpError> {
        lock(&self.0).retry(id)
    }

    fn restore(&mut self, change: &QuarantineChange) -> Result<(), OpError> {
        lock(&self.0).restore(change)
    }
}

impl CaseOps for SharedOps<FakeCase> {
    fn stuck(&self) -> Vec<CaseRef> {
        lock(&self.0).stuck()
    }

    fn case(&self, id: &CaseRef) -> Option<CaseView> {
        lock(&self.0).case(id)
    }

    fn unstick(&mut self, id: &CaseRef, now: i64) -> Result<CaseChange, OpError> {
        lock(&self.0).unstick(id, now)
    }

    fn restore(&mut self, change: &CaseChange) -> Result<(), OpError> {
        lock(&self.0).restore(change)
    }
}

impl DocumentOps for SharedOps<FakeDocuments> {
    fn view(&self, doc: &DocRef) -> Option<DocumentView> {
        lock(&self.0).view(doc)
    }

    fn run(&mut self, doc: &DocRef, op: &DocOp) -> Result<DocumentChange, OpError> {
        lock(&self.0).run(doc, op)
    }
}

impl JobsPart for FixturePart<FakeJobs> {
    fn live(&self) -> Box<dyn JobOps + '_> {
        Box::new(SharedOps(Arc::clone(&self.0)))
    }

    fn rehearsal(&self) -> Result<Box<dyn JobOps>, WorldError> {
        Ok(Box::new(self.snapshot()))
    }
}

impl CachePart for FixturePart<FakeCache> {
    fn live(&self) -> Box<dyn CacheOps + '_> {
        Box::new(SharedOps(Arc::clone(&self.0)))
    }

    fn rehearsal(&self) -> Result<Box<dyn CacheOps>, WorldError> {
        Ok(Box::new(self.snapshot()))
    }
}

impl IndexPart for FixturePart<FakeIndex> {
    fn live(&self) -> Box<dyn IndexOps + '_> {
        Box::new(SharedOps(Arc::clone(&self.0)))
    }

    fn rehearsal(&self) -> Result<Box<dyn IndexOps>, WorldError> {
        Ok(Box::new(self.snapshot()))
    }
}

impl EdgePart for FixturePart<FakeEdge> {
    fn live(&self) -> Box<dyn EdgeOps + '_> {
        Box::new(SharedOps(Arc::clone(&self.0)))
    }

    fn rehearsal(&self) -> Result<Box<dyn EdgeOps>, WorldError> {
        Ok(Box::new(self.snapshot()))
    }
}

impl QuarantinePart for FixturePart<FakeQuarantine> {
    fn live(&self) -> Box<dyn QuarantineOps + '_> {
        Box::new(SharedOps(Arc::clone(&self.0)))
    }

    fn rehearsal(&self) -> Result<Box<dyn QuarantineOps>, WorldError> {
        Ok(Box::new(self.snapshot()))
    }
}

impl CasesPart for FixturePart<FakeCase> {
    fn live(&self) -> Box<dyn CaseOps + '_> {
        Box::new(SharedOps(Arc::clone(&self.0)))
    }

    fn rehearsal(&self) -> Result<Box<dyn CaseOps>, WorldError> {
        Ok(Box::new(self.snapshot()))
    }
}

impl DocumentsPart for FixturePart<FakeDocuments> {
    fn live(&self) -> Box<dyn DocumentOps + '_> {
        Box::new(SharedOps(Arc::clone(&self.0)))
    }

    fn rehearsal(&self) -> Result<Box<dyn DocumentOps>, WorldError> {
        Ok(Box::new(self.snapshot()))
    }
}

/// A whole in-memory world: one store per port, the "live site" of a test.
#[derive(Debug)]
pub struct FixtureWorld {
    jobs: Arc<Mutex<FakeJobs>>,
    cache: Arc<Mutex<FakeCache>>,
    index: Arc<Mutex<FakeIndex>>,
    edge: Arc<Mutex<FakeEdge>>,
    quarantine: Arc<Mutex<FakeQuarantine>>,
    cases: Arc<Mutex<FakeCase>>,
    documents: Arc<Mutex<FakeDocuments>>,
}

impl FixtureWorld {
    /// A world holding the stores of `world` (its audit sink is dropped: the composer's callers
    /// bring their own).
    #[must_use]
    pub fn new(world: FakeWorld) -> Self {
        Self {
            jobs: Arc::new(Mutex::new(world.jobs)),
            cache: Arc::new(Mutex::new(world.cache)),
            index: Arc::new(Mutex::new(world.index)),
            edge: Arc::new(Mutex::new(world.edge)),
            quarantine: Arc::new(Mutex::new(world.quarantine)),
            cases: Arc::new(Mutex::new(world.case)),
            documents: Arc::new(Mutex::new(world.document)),
        }
    }

    /// The seven parts over this world's stores (they share them: a change through a live part is
    /// visible in [`FixtureWorld::fingerprint`]).
    #[must_use]
    pub fn parts(&self) -> Parts {
        Parts {
            jobs: Some(Arc::new(FixturePart::shared(&self.jobs))),
            cache: Some(Arc::new(FixturePart::shared(&self.cache))),
            index: Some(Arc::new(FixturePart::shared(&self.index))),
            edge: Some(Arc::new(FixturePart::shared(&self.edge))),
            quarantine: Some(Arc::new(FixturePart::shared(&self.quarantine))),
            cases: Some(Arc::new(FixturePart::shared(&self.cases))),
            documents: Some(Arc::new(FixturePart::shared(&self.documents))),
        }
    }

    /// The whole world as text, for "nothing changed" and "restored exactly" comparisons.
    #[must_use]
    pub fn fingerprint(&self) -> String {
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}|{:?}|{:?}",
            *lock(&self.jobs),
            *lock(&self.cache),
            *lock(&self.index),
            *lock(&self.edge),
            *lock(&self.quarantine),
            *lock(&self.cases),
            *lock(&self.documents),
        )
    }

    /// One job's state, read from the live store.
    #[must_use]
    pub fn job_life(&self, id: &str) -> Option<JobLife> {
        lock(&self.jobs).job(&JobRef::new(id)).map(|view| view.life)
    }

    /// One case's state, read from the live store.
    #[must_use]
    pub fn case_life(&self, id: &str) -> Option<CaseLife> {
        lock(&self.cases)
            .case(&CaseRef::new(id))
            .map(|view| view.life)
    }
}
