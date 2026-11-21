//! An in-memory sink that records what it was handed and rebuilds a model of the database.
//!
//! It is the test double for the executor and a reference for what a real sink does: it keeps the
//! latest image per record at its recorded version and position, removes deleted records, keeps
//! the audit entries, and nothing becomes visible as [`RecordingSink::state`] until `commit`.

use std::collections::BTreeMap;

use spot_events_contract::envelope::Position;
use spot_image_upcast::Image;

use crate::sink::{
    AppliedAudit, AppliedDelete, AppliedImage, ApplySink, PlanSummary, SinkError, SinkFuture,
};

/// One call the sink received, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    /// `begin`.
    Begin(PlanSummary),
    /// `apply_image`.
    Image(AppliedImage),
    /// `apply_delete`.
    Delete(AppliedDelete),
    /// `apply_audit`.
    Audit(AppliedAudit),
    /// `commit`.
    Commit(Position),
    /// `abort`, with its reason.
    Abort(String),
}

/// A record in the model: its version, where it was recorded and its document.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordedImage {
    /// The version stored.
    pub version: u64,
    /// The position it was recorded at.
    pub position: Position,
    /// The document.
    pub image: Image,
}

/// The recording sink.
#[derive(Debug, Default)]
pub struct RecordingSink {
    calls: Vec<Call>,
    working: BTreeMap<(String, String), RecordedImage>,
    committed: Option<BTreeMap<(String, String), RecordedImage>>,
    fail_after: Option<usize>,
}

impl RecordingSink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A sink that refuses the apply call after `n` successful apply calls (for a failure test).
    #[must_use]
    pub fn failing_after(n: usize) -> Self {
        Self {
            fail_after: Some(n),
            ..Self::default()
        }
    }

    /// Every call received, in order.
    #[must_use]
    pub fn calls(&self) -> &[Call] {
        &self.calls
    }

    /// The committed model, or `None` before a commit (and after an abort).
    #[must_use]
    pub fn state(&self) -> Option<&BTreeMap<(String, String), RecordedImage>> {
        self.committed.as_ref()
    }

    /// The audit entries received, in order.
    #[must_use]
    pub fn audits(&self) -> Vec<&AppliedAudit> {
        self.calls
            .iter()
            .filter_map(|call| match call {
                Call::Audit(audit) => Some(audit),
                _ => None,
            })
            .collect()
    }

    /// Whether the rebuild was aborted, and why.
    #[must_use]
    pub fn aborted(&self) -> Option<&str> {
        self.calls.iter().find_map(|call| match call {
            Call::Abort(reason) => Some(reason.as_str()),
            _ => None,
        })
    }

    fn applies(&self) -> usize {
        self.calls
            .iter()
            .filter(|c| matches!(c, Call::Image(_) | Call::Delete(_) | Call::Audit(_)))
            .count()
    }

    fn gate(&self) -> Result<(), SinkError> {
        match self.fail_after {
            Some(n) if self.applies() >= n => {
                Err(SinkError::new("the recording sink was told to fail"))
            }
            _ => Ok(()),
        }
    }
}

fn done(result: Result<(), SinkError>) -> SinkFuture<'static> {
    Box::pin(std::future::ready(result))
}

impl ApplySink for RecordingSink {
    fn begin<'a>(&'a mut self, summary: &'a PlanSummary) -> SinkFuture<'a> {
        self.calls.push(Call::Begin(summary.clone()));
        done(Ok(()))
    }

    fn apply_image(&mut self, image: AppliedImage) -> SinkFuture<'_> {
        let gate = self.gate();
        if gate.is_ok() {
            self.working.insert(
                (image.entity.clone(), image.id.clone()),
                RecordedImage {
                    version: image.version,
                    position: image.position,
                    image: image.image.clone(),
                },
            );
            self.calls.push(Call::Image(image));
        }
        done(gate)
    }

    fn apply_delete(&mut self, delete: AppliedDelete) -> SinkFuture<'_> {
        let gate = self.gate();
        if gate.is_ok() {
            self.working
                .remove(&(delete.entity.clone(), delete.id.clone()));
            self.calls.push(Call::Delete(delete));
        }
        done(gate)
    }

    fn apply_audit(&mut self, audit: AppliedAudit) -> SinkFuture<'_> {
        let gate = self.gate();
        if gate.is_ok() {
            self.calls.push(Call::Audit(audit));
        }
        done(gate)
    }

    fn commit(&mut self, last_position: Position) -> SinkFuture<'_> {
        self.calls.push(Call::Commit(last_position));
        self.committed = Some(self.working.clone());
        done(Ok(()))
    }

    fn abort<'a>(&'a mut self, reason: &'a str) -> SinkFuture<'a> {
        self.calls.push(Call::Abort(reason.to_owned()));
        self.working.clear();
        self.committed = None;
        done(Ok(()))
    }
}
