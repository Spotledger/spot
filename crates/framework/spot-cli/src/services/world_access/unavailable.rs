//! The stand-ins for a part the site does not have: reads answer empty, every change answers an
//! [`OpError`] saying the part is not available. A remedy whose precondition reads an unavailable
//! part is refused cleanly by the registry (the job, namespace, index, edge, item, case or
//! document it asks for "does not exist"); a remedy over the other parts is unaffected.

use spot_remedies::{
    CacheChange, CacheNs, CacheOps, CaseChange, CaseOps, CaseRef, CaseView, DocOp, DocRef,
    DocumentChange, DocumentOps, DocumentView, EdgeChange, EdgeOps, EdgeRef, EdgeView, IndexChange,
    IndexOps, IndexRef, IndexView, JobChange, JobLife, JobOps, JobRef, JobView, OpError,
    QuarantineChange, QuarantineItem, QuarantineOps, QuarantineRef,
};

/// The detail of every refusal from an unavailable part.
pub const NOT_AVAILABLE: &str = "not available on this site";

fn refuse<T>(operation: &str) -> Result<T, OpError> {
    Err(OpError::new(operation, NOT_AVAILABLE))
}

/// No jobs part.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableJobs;

impl JobOps for UnavailableJobs {
    fn failed(&self) -> Vec<JobRef> {
        Vec::new()
    }

    fn running(&self) -> Vec<JobRef> {
        Vec::new()
    }

    fn job(&self, _id: &JobRef) -> Option<JobView> {
        None
    }

    fn retry(&mut self, _id: &JobRef, _now: i64) -> Result<JobChange, OpError> {
        refuse("job_retry")
    }

    fn unstick(&mut self, _id: &JobRef, _now: i64) -> Result<JobChange, OpError> {
        refuse("job_unstick")
    }

    fn restore(&mut self, _id: &JobRef, _life: JobLife) -> Result<(), OpError> {
        refuse("job_restore")
    }
}

/// No cache part.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableCache;

impl CacheOps for UnavailableCache {
    fn namespaces(&self) -> Vec<CacheNs> {
        Vec::new()
    }

    fn entries(&self, _ns: &CacheNs) -> usize {
        0
    }

    fn flush(&mut self, _ns: &CacheNs) -> Result<CacheChange, OpError> {
        refuse("cache_flush")
    }

    fn undo_flush(&mut self, _change: &CacheChange) -> Result<(), OpError> {
        refuse("cache_undo_flush")
    }
}

/// No retrieval-index part.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableIndex;

impl IndexOps for UnavailableIndex {
    fn stale(&self) -> Vec<IndexRef> {
        Vec::new()
    }

    fn index(&self, _name: &IndexRef) -> Option<IndexView> {
        None
    }

    fn rebuild(&mut self, _name: &IndexRef) -> Result<IndexChange, OpError> {
        refuse("index_rebuild")
    }

    fn restore(&mut self, _change: &IndexChange) -> Result<(), OpError> {
        refuse("index_restore")
    }
}

/// No edge-sync part.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableEdge;

impl EdgeOps for UnavailableEdge {
    fn drifted(&self) -> Vec<EdgeRef> {
        Vec::new()
    }

    fn edge(&self, _edge: &EdgeRef) -> Option<EdgeView> {
        None
    }

    fn repair(&mut self, _edge: &EdgeRef) -> Result<EdgeChange, OpError> {
        refuse("edge_repair")
    }

    fn restore(&mut self, _change: &EdgeChange) -> Result<(), OpError> {
        refuse("edge_restore")
    }
}

/// No quarantine part.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableQuarantine;

impl QuarantineOps for UnavailableQuarantine {
    fn held(&self) -> Vec<QuarantineRef> {
        Vec::new()
    }

    fn item(&self, _id: &QuarantineRef) -> Option<QuarantineItem> {
        None
    }

    fn retry(&mut self, _id: &QuarantineRef) -> Result<QuarantineChange, OpError> {
        refuse("quarantine_retry")
    }

    fn restore(&mut self, _change: &QuarantineChange) -> Result<(), OpError> {
        refuse("quarantine_restore")
    }
}

/// No cases part.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableCases;

impl CaseOps for UnavailableCases {
    fn stuck(&self) -> Vec<CaseRef> {
        Vec::new()
    }

    fn case(&self, _id: &CaseRef) -> Option<CaseView> {
        None
    }

    fn unstick(&mut self, _id: &CaseRef, _now: i64) -> Result<CaseChange, OpError> {
        refuse("case_unstick")
    }

    fn restore(&mut self, _change: &CaseChange) -> Result<(), OpError> {
        refuse("case_restore")
    }
}

/// No documents part.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnavailableDocuments;

impl DocumentOps for UnavailableDocuments {
    fn view(&self, _doc: &DocRef) -> Option<DocumentView> {
        None
    }

    fn run(&mut self, _doc: &DocRef, op: &DocOp) -> Result<DocumentChange, OpError> {
        refuse(op.name())
    }
}
