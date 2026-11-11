//! The business-graph port: which records depend on a record.

use crate::model::RecordRef;

/// Answers "which records depend on this one" from the business graph (`spot-edge-sync` edges
/// read through `spot-graph-query`). The host implements it over its graph projection.
pub trait GraphDependants: Send + Sync {
    /// The records that depend on `record`, as the graph knows them. Order does not matter.
    ///
    /// # Errors
    /// A stable code when the graph cannot answer; the undo then fails closed.
    fn dependants(&self, record: &RecordRef) -> Result<Vec<RecordRef>, String>;
}

/// A graph that knows no edges: only the links inside the file count.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoGraph;

impl GraphDependants for NoGraph {
    fn dependants(&self, _record: &RecordRef) -> Result<Vec<RecordRef>, String> {
        Ok(Vec::new())
    }
}
