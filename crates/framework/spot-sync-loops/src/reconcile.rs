//! The reconcile step (d): compare what this node holds with what the hub says it should hold, by
//! digest, per scope (ADR-065; WP-RTB-02.T11).
//!
//! # The wire has no digest request yet
//!
//! `spot-sync-http` carries hello, pull and push; none of them asks "give me the digest of scope
//! S". The step is therefore written against a port with a typed request ([`DigestPort`]) and the
//! request kind it needs is named ([`DIGEST_REQUEST_KIND`]) so the architecture owner can add it to
//! the node protocol. **Without a provider the step says so**: it raises `RECONCILE_UNAVAILABLE`
//! (information) and fails with `not available`; it never reports a clean result it did not
//! compute. With a provider, a mismatch raises `DIVERGENCE` (amber, the catalogue's id) for the scope; healing
//! (step e) is what acts on it, not this step.

use crate::runner::{ConditionSink, Raised, Severity, Step, StepError, StepOutcome};

/// The request kind the node protocol would need to carry a digest request.
pub const DIGEST_REQUEST_KIND: &str = "digest";
/// A scope's digests differ.
pub const RECONCILE_DIFF: &str = "DIVERGENCE";
/// No digest provider is wired: the step could not run, and says so.
pub const RECONCILE_UNAVAILABLE: &str = "RECONCILE_UNAVAILABLE";

/// The typed digest request of one scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DigestRequest {
    /// The scope (a partition or class name: an identity).
    pub scope: String,
}

/// The answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DigestAnswer {
    /// This node's digest equals the hub's.
    Match,
    /// They differ; `records` is how many records the hub says differ (0 when it cannot count).
    Differ {
        /// How many records differ.
        records: u64,
    },
}

/// Why a digest could not be had.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DigestError {
    /// Nothing provides the request (the wire has no such kind yet).
    NotAvailable,
    /// The request failed; a reason without content.
    Failed(String),
}

/// Where a digest comes from.
pub trait DigestPort: Send {
    /// Compares the scope.
    ///
    /// # Errors
    /// [`DigestError::NotAvailable`] without a provider, [`DigestError::Failed`] otherwise.
    fn compare(&mut self, request: &DigestRequest) -> Result<DigestAnswer, DigestError>;
}

/// A port that has no provider: every request is `NotAvailable`.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoDigest;

impl DigestPort for NoDigest {
    fn compare(&mut self, _request: &DigestRequest) -> Result<DigestAnswer, DigestError> {
        Err(DigestError::NotAvailable)
    }
}

/// The step.
pub struct ReconcileStep<P: DigestPort> {
    port: P,
    scopes: Vec<String>,
    next: usize,
}

impl<P: DigestPort> ReconcileStep<P> {
    /// A step that reconciles `scopes`, a bounded number per run, round-robin.
    pub fn new(port: P, scopes: Vec<String>) -> Self {
        Self {
            port,
            scopes,
            next: 0,
        }
    }
}

impl<P: DigestPort> Step for ReconcileStep<P> {
    fn name(&self) -> &'static str {
        "reconcile"
    }

    fn interval_secs(&self) -> u64 {
        3_600
    }

    fn run(
        &mut self,
        _now: i64,
        max_items: usize,
        sink: &mut dyn ConditionSink,
    ) -> Result<StepOutcome, StepError> {
        let mut outcome = StepOutcome::default();
        if self.scopes.is_empty() {
            return Ok(outcome);
        }
        let take = max_items.min(self.scopes.len());
        outcome.truncated = take < self.scopes.len();
        for _ in 0..take {
            let scope = self.scopes[self.next % self.scopes.len()].clone();
            self.next = (self.next + 1) % self.scopes.len();
            match self.port.compare(&DigestRequest {
                scope: scope.clone(),
            }) {
                Ok(DigestAnswer::Match) => {
                    outcome.examined += 1;
                    sink.clear(RECONCILE_DIFF, &scope);
                    sink.clear(RECONCILE_UNAVAILABLE, &scope);
                    outcome.cleared += 1;
                }
                Ok(DigestAnswer::Differ { .. }) => {
                    outcome.examined += 1;
                    sink.raise(&Raised {
                        condition: RECONCILE_DIFF,
                        subject: scope,
                        severity: Severity::Amber,
                    });
                    outcome.raised += 1;
                }
                Err(DigestError::NotAvailable) => {
                    sink.raise(&Raised {
                        condition: RECONCILE_UNAVAILABLE,
                        subject: scope,
                        severity: Severity::Info,
                    });
                    return Err(StepError {
                        reason: format!(
                            "not available: no provider of the `{DIGEST_REQUEST_KIND}` request"
                        ),
                    });
                }
                Err(DigestError::Failed(reason)) => return Err(StepError { reason }),
            }
        }
        Ok(outcome)
    }
}

impl<T: DigestPort + ?Sized> DigestPort for Box<T> {
    fn compare(&mut self, request: &DigestRequest) -> Result<DigestAnswer, DigestError> {
        (**self).compare(request)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[derive(Default)]
    struct Conditions(Vec<String>);
    impl ConditionSink for Conditions {
        fn raise(&mut self, r: &Raised) {
            self.0.push(format!(
                "raise {} {} {:?}",
                r.condition, r.subject, r.severity
            ));
        }
        fn clear(&mut self, c: &'static str, s: &str) {
            self.0.push(format!("clear {c} {s}"));
        }
    }

    struct Answers(Vec<DigestAnswer>);
    impl DigestPort for Answers {
        fn compare(&mut self, _: &DigestRequest) -> Result<DigestAnswer, DigestError> {
            Ok(self.0.remove(0))
        }
    }

    #[test]
    fn without_a_provider_the_step_says_not_available_and_never_reports_clean() {
        let mut step = ReconcileStep::new(NoDigest, vec!["branch_01".to_owned()]);
        let mut sink = Conditions::default();
        let error = step.run(1, 10, &mut sink).unwrap_err();
        assert!(error.reason.starts_with("not available"));
        assert_eq!(sink.0, ["raise RECONCILE_UNAVAILABLE branch_01 Info"]);
        assert!(sink.0.iter().all(|line| !line.starts_with("clear")));
    }

    #[test]
    fn a_mismatch_raises_amber_and_a_match_clears_it() {
        let mut step = ReconcileStep::new(
            Answers(vec![
                DigestAnswer::Differ { records: 3 },
                DigestAnswer::Match,
            ]),
            vec!["branch_01".to_owned()],
        );
        let mut sink = Conditions::default();
        step.run(1, 10, &mut sink).unwrap();
        step.run(2, 10, &mut sink).unwrap();
        assert_eq!(sink.0[0], "raise DIVERGENCE branch_01 Amber");
        assert_eq!(sink.0[1], "clear DIVERGENCE branch_01");
    }

    #[test]
    fn the_bound_rotates_through_the_scopes() {
        let mut step = ReconcileStep::new(
            // Two runs of a bound of two: four comparisons.
            Answers(vec![
                DigestAnswer::Match,
                DigestAnswer::Match,
                DigestAnswer::Match,
                DigestAnswer::Match,
            ]),
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        );
        let mut sink = Conditions::default();
        let outcome = step.run(1, 2, &mut sink).unwrap();
        assert!(outcome.truncated);
        assert_eq!(step.next, 2);
        step.run(2, 2, &mut sink).unwrap();
        assert_eq!(step.next, 1);
    }
}
