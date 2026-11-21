#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Shared fakes: an in-memory old version, its invariants and a recording step runner.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_core::SiteId;
use spot_migration_apply::{
    BoxFuture as ApplyFuture, Checkpoint, Idempotency, RestorePoint, StepContext, StepDescriptor,
    StepOutcome, StepRunner,
};
use spot_migration_plan::ApplyStep;
use spot_permissions::Principal;
use spot_rescue::{
    Error as RescueError, InvariantCheck, InvariantPhase, InvariantReport, Invariants, Reenter,
    ReenterError, ReenterOutcome, ReentryItem, Result as RescueResult,
};

/// A valid site id.
pub fn site() -> SiteId {
    SiteId::new("localhost").expect("localhost is a valid site")
}

/// A principal for `name`.
pub fn principal(name: &str) -> Principal {
    Principal::with_roles(name, &["Accounts User"])
}

/// A recording `StepRunner`: records each step id, optionally failing one.
#[derive(Debug, Default)]
pub struct FakeRunner {
    calls: Mutex<Vec<String>>,
    fail_on: Mutex<Option<String>>,
}

impl FakeRunner {
    /// A runner that succeeds every step.
    pub fn new() -> Self {
        Self::default()
    }

    /// A runner that fails the step with this id.
    pub fn failing_on(id: &str) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail_on: Mutex::new(Some(id.to_owned())),
        }
    }

    /// The step ids that ran, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl StepRunner for FakeRunner {
    fn describe(&self, index: usize, step: &ApplyStep) -> StepDescriptor {
        StepDescriptor {
            index,
            step: step.clone(),
            transactional: true,
            idempotency: Some(Idempotency::Idempotent),
            timeout: Some(Duration::from_secs(30)),
        }
    }

    fn restore_point(&self, _descriptor: &StepDescriptor) -> Option<RestorePoint> {
        None
    }

    fn run<'a>(
        &'a self,
        descriptor: &'a StepDescriptor,
        _ctx: &'a StepContext,
    ) -> ApplyFuture<'a, Result<StepOutcome, spot_migration_apply::Error>> {
        Box::pin(async move {
            let id = descriptor.step.id.clone();
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(id.clone());
            if self
                .fail_on
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_deref()
                == Some(id.as_str())
            {
                return Err(spot_migration_apply::Error::StepFailed {
                    index: descriptor.index,
                    id,
                    reason: "the fake runner was told to fail".to_owned(),
                });
            }
            Ok(StepOutcome {
                checkpoint: Some(Checkpoint {
                    key: format!("after:{id}"),
                    detail: format!("ran {id}"),
                }),
                detail: format!("ran {id}"),
            })
        })
    }

    fn rollback<'a>(
        &'a self,
        _restore_point: &'a RestorePoint,
        _ctx: &'a StepContext,
    ) -> ApplyFuture<'a, Result<(), spot_migration_apply::Error>> {
        Box::pin(async { Ok(()) })
    }
}

/// A document the fake old version holds: its amount, for the totals invariant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OldDocument {
    /// The amount posted.
    pub amount: i64,
    /// The principal that wrote it.
    pub principal: String,
}

/// The fake old-version engine: an in-memory document table and a call log.
#[derive(Debug, Default)]
pub struct FakeReenter {
    documents: Mutex<BTreeMap<String, OldDocument>>,
    calls: Mutex<Vec<String>>,
    fail_on: Mutex<Option<String>>,
}

impl FakeReenter {
    /// An empty old version.
    pub fn new() -> Self {
        Self::default()
    }

    /// An old version that rejects the item with this key.
    pub fn failing_on(key: &str) -> Self {
        let fake = Self::new();
        *fake
            .fail_on
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(key.to_owned());
        fake
    }

    /// How many documents were written.
    pub fn count(&self) -> usize {
        self.documents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// The total of every posted amount.
    pub fn total(&self) -> i64 {
        self.documents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .map(|document| document.amount)
            .sum()
    }

    /// The item keys presented, in order, including repeated attempts.
    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The documents, keyed by item key.
    pub fn documents(&self) -> BTreeMap<String, OldDocument> {
        self.documents
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl Reenter for FakeReenter {
    fn reenter<'a>(
        &'a self,
        item: &'a ReentryItem,
    ) -> ApplyFuture<'a, std::result::Result<ReenterOutcome, ReenterError>> {
        Box::pin(async move {
            let key = item.key.as_str().to_owned();
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(key.clone());
            if self
                .fail_on
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_deref()
                == Some(key.as_str())
            {
                return Err(ReenterError::new(
                    "sales_invoice.invalid",
                    "the old version rejected the document",
                ));
            }
            let amount = item
                .payload
                .get("amount")
                .and_then(serde_json::Value::as_i64)
                .ok_or_else(|| ReenterError::new("sales_invoice.required", "amount is missing"))?;
            self.documents
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(
                    key,
                    OldDocument {
                        amount,
                        principal: item.principal.user.clone(),
                    },
                );
            Ok(ReenterOutcome::Done)
        })
    }
}

/// Invariants over a [`FakeReenter`]: a fixed expected document count and total.
#[derive(Debug)]
pub struct FakeInvariants {
    engine: Arc<FakeReenter>,
    expected_count: usize,
    expected_total: i64,
    fail_after_reentry: bool,
}

impl FakeInvariants {
    /// Invariants expecting `expected_count` documents totalling `expected_total`.
    pub fn new(engine: Arc<FakeReenter>, expected_count: usize, expected_total: i64) -> Self {
        Self {
            engine,
            expected_count,
            expected_total,
            fail_after_reentry: false,
        }
    }

    /// The same invariants failing the final pass.
    pub fn failing_final(mut self) -> Self {
        self.fail_after_reentry = true;
        self
    }
}

impl Invariants for FakeInvariants {
    fn check(&self, phase: InvariantPhase) -> ApplyFuture<'_, RescueResult<InvariantReport>> {
        Box::pin(async move {
            let checks = match phase {
                InvariantPhase::AfterRestore | InvariantPhase::AfterDownPatches => {
                    vec![InvariantCheck::passed("no orphaned postings")]
                }
                InvariantPhase::AfterReentry => {
                    let mut checks = Vec::new();
                    if self.engine.count() == self.expected_count {
                        checks.push(InvariantCheck::passed("document count"));
                    } else {
                        checks.push(InvariantCheck::failed(
                            "document count",
                            format!(
                                "expected {} documents, found {}",
                                self.expected_count,
                                self.engine.count()
                            ),
                        ));
                    }
                    if self.engine.total() == self.expected_total {
                        checks.push(InvariantCheck::passed("posted totals"));
                    } else {
                        checks.push(InvariantCheck::failed(
                            "posted totals",
                            format!(
                                "expected total {}, found {}",
                                self.expected_total,
                                self.engine.total()
                            ),
                        ));
                    }
                    if self.fail_after_reentry {
                        checks.push(InvariantCheck::failed(
                            "trial balance",
                            "debits and credits differ",
                        ));
                    }
                    checks
                }
            };
            Ok(InvariantReport::new(phase, checks))
        })
    }
}

/// A rescue error that is expected to be absent in passing cases.
#[allow(dead_code)]
pub fn assert_not_killed(result: &RescueResult<spot_rescue::RescueOutcome>) {
    if let Err(RescueError::Killed { at }) = result {
        panic!("unexpected kill at {at}");
    }
}
