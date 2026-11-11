//! The self-test vocabulary: a scenario describes the fault, the invariants around it and what
//! undo is expected to do, and a [`TestSpec`] is what a remedy ships so the harness can prove it.

use serde_json::Value as Json;

use crate::fixture::FakeWorld;
use crate::ports::{CaseLife, DocState, IndexHealth, JobLife, QuarantineState};

/// What a remedy's self-test expects undo to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UndoExpectation {
    /// Undo restores the pre-apply state; the harness runs it and checks the invariant.
    Restored,
    /// The remedy is irreversible; the harness runs undo and expects it to refuse.
    Refused,
}

/// An invariant over the fixture: `Ok(())` when it holds, `Err(sentence)` when it does not.
pub type Invariant = fn(&FakeWorld) -> Result<(), String>;

/// One seeded job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobSeed {
    /// The job id.
    pub id: String,
    /// Its state.
    pub life: JobLife,
    /// Its attempts.
    pub attempts: u32,
    /// Its lease expiry, if running.
    pub lease_expires_at: Option<i64>,
}

/// One seeded cache namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheSeed {
    /// The namespace.
    pub ns: String,
    /// Its entry count.
    pub entries: usize,
}

/// One seeded index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSeed {
    /// The index name.
    pub name: String,
    /// Its health.
    pub health: IndexHealth,
    /// Its generation.
    pub generation: u64,
}

/// One seeded edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeSeed {
    /// The edge reference.
    pub edge: String,
    /// The expected count.
    pub expected: u64,
    /// The actual count.
    pub actual: u64,
}

/// One seeded quarantined item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineSeed {
    /// The item id.
    pub id: String,
    /// Its state.
    pub state: QuarantineState,
}

/// One seeded case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseSeed {
    /// The case id.
    pub id: String,
    /// Its state.
    pub life: CaseLife,
}

/// One seeded document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentSeed {
    /// The document reference.
    pub doc: String,
    /// Its state.
    pub state: DocState,
    /// Whether it is a financial record.
    pub financial: bool,
}

/// A declarative description of the fixture: the world, with the fault in it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorldSpec {
    /// The jobs.
    pub jobs: Vec<JobSeed>,
    /// The cache namespaces.
    pub cache: Vec<CacheSeed>,
    /// The indexes.
    pub indexes: Vec<IndexSeed>,
    /// The edges.
    pub edges: Vec<EdgeSeed>,
    /// The quarantined items.
    pub quarantine: Vec<QuarantineSeed>,
    /// The cases.
    pub cases: Vec<CaseSeed>,
    /// The documents.
    pub documents: Vec<DocumentSeed>,
}

impl WorldSpec {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The world with one job added.
    #[must_use]
    pub fn with_job(mut self, id: &str, life: JobLife, attempts: u32, lease: Option<i64>) -> Self {
        self.jobs.push(JobSeed {
            id: id.to_owned(),
            life,
            attempts,
            lease_expires_at: lease,
        });
        self
    }

    /// The world with one cache namespace added.
    #[must_use]
    pub fn with_cache(mut self, ns: &str, entries: usize) -> Self {
        self.cache.push(CacheSeed {
            ns: ns.to_owned(),
            entries,
        });
        self
    }

    /// The world with one index added.
    #[must_use]
    pub fn with_index(mut self, name: &str, health: IndexHealth, generation: u64) -> Self {
        self.indexes.push(IndexSeed {
            name: name.to_owned(),
            health,
            generation,
        });
        self
    }

    /// The world with one edge added.
    #[must_use]
    pub fn with_edge(mut self, edge: &str, expected: u64, actual: u64) -> Self {
        self.edges.push(EdgeSeed {
            edge: edge.to_owned(),
            expected,
            actual,
        });
        self
    }

    /// The world with one quarantined item added.
    #[must_use]
    pub fn with_quarantine(mut self, id: &str, state: QuarantineState) -> Self {
        self.quarantine.push(QuarantineSeed {
            id: id.to_owned(),
            state,
        });
        self
    }

    /// The world with one case added.
    #[must_use]
    pub fn with_case(mut self, id: &str, life: CaseLife) -> Self {
        self.cases.push(CaseSeed {
            id: id.to_owned(),
            life,
        });
        self
    }

    /// The world with one document added.
    #[must_use]
    pub fn with_document(mut self, doc: &str, state: DocState, financial: bool) -> Self {
        self.documents.push(DocumentSeed {
            doc: doc.to_owned(),
            state,
            financial,
        });
        self
    }
}

/// A remedy's self-test: the fixture with the fault, the parameters, the invariants and the undo
/// expectation.
pub struct Scenario {
    /// The scenario's name.
    pub name: String,
    /// The fixture, with the fault in it.
    pub world: WorldSpec,
    /// The parameters the remedy runs with.
    pub params: Json,
    /// What undo is expected to do.
    pub undo: UndoExpectation,
    /// The invariant that must hold after apply (the fault is repaired).
    pub expect_apply: Invariant,
    /// The invariant that must hold after undo (the world is restored).
    pub expect_undo: Invariant,
}

impl Scenario {
    /// A scenario.
    #[must_use]
    pub fn new(
        name: &str,
        world: WorldSpec,
        params: Json,
        undo: UndoExpectation,
        expect_apply: Invariant,
        expect_undo: Invariant,
    ) -> Self {
        Self {
            name: name.to_owned(),
            world,
            params,
            undo,
            expect_apply,
            expect_undo,
        }
    }
}

/// What a remedy ships so the harness can prove it: the scenario.
pub struct TestSpec {
    /// The scenario.
    pub scenario: Scenario,
}

impl TestSpec {
    /// A test spec from a scenario.
    #[must_use]
    pub fn new(scenario: Scenario) -> Self {
        Self { scenario }
    }
}
