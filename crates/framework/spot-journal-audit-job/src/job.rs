//! The platform job: `platform:journal_audit`.

use std::sync::{Arc, Mutex, PoisonError};

use spot_conditions::{AuditSink, ConditionEngine};
use spot_jobs::worker::{BoxFuture, HandlerResult, JobContext};
use spot_journal_audit_run::Clock;
use tokio::runtime::Handle;

use crate::error::AuditJobError;
use crate::providers::{AuditInputs, ResultSink, TenantDirectory};
use crate::run::{RunSummary, run_due};

/// The name the handler is registered under with `JobHandlerRegistry::register`.
pub const JOB_NAME: &str = "platform:journal_audit";

/// Everything the job needs, shared behind an `Arc` by the handler.
pub struct AuditJob<S: AuditSink> {
    directory: Arc<dyn TenantDirectory>,
    inputs: Arc<dyn AuditInputs>,
    sink: Arc<dyn ResultSink>,
    clock: Arc<dyn SyncClock>,
    conditions: Mutex<ConditionEngine<S>>,
}

/// A clock that can be shared across threads.
pub trait SyncClock: Clock + Send + Sync {}
impl<T: Clock + Send + Sync> SyncClock for T {}

impl<S: AuditSink> AuditJob<S> {
    /// Wires the job. `clock` measures the rebuild; the time the pass runs at is the job
    /// context's.
    pub fn new(
        directory: Arc<dyn TenantDirectory>,
        inputs: Arc<dyn AuditInputs>,
        sink: Arc<dyn ResultSink>,
        clock: Arc<dyn SyncClock>,
        conditions: ConditionEngine<S>,
    ) -> Self {
        Self {
            directory,
            inputs,
            sink,
            clock,
            conditions: Mutex::new(conditions),
        }
    }

    /// One pass at `now_millis`; blocking (see [`run_due`]).
    ///
    /// # Errors
    ///
    /// [`AuditJobError::ProviderFailed`] when the directory cannot be read.
    pub fn run_at(&self, exec: &Handle, now_millis: i64) -> Result<RunSummary, AuditJobError> {
        // A poisoned lock only means an earlier pass panicked outside a tenant's audit; the
        // engine's state is still a valid state machine.
        let mut engine = self
            .conditions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        run_due(
            exec,
            now_millis,
            self.directory.as_ref(),
            self.inputs.as_ref(),
            self.clock.as_ref(),
            &mut engine,
            self.sink.as_ref(),
        )
    }

    /// Reads the condition engine, for example to list the open conditions.
    pub fn with_conditions<R>(&self, f: impl FnOnce(&ConditionEngine<S>) -> R) -> R {
        let engine = self
            .conditions
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        f(&engine)
    }
}

/// The handler for `JobHandlerRegistry::register(JOB_NAME, ..)`.
///
/// The audit is blocking and heavy, so each run goes to the blocking pool. It returns
/// [`HandlerResult::Ok`] when the pass finished, also when a tenant's audit failed (that is a
/// result row and a condition, not a retry storm), and [`HandlerResult::Retry`] only when the
/// providers themselves could not be reached or the pool task died.
pub fn handler<S>(
    job: Arc<AuditJob<S>>,
) -> impl Fn(JobContext) -> BoxFuture<'static, spot_jobs::Result<HandlerResult>> + Send + Sync + 'static
where
    S: AuditSink + Send + 'static,
{
    move |context: JobContext| {
        let job = Arc::clone(&job);
        Box::pin(async move {
            let now_millis = context.clock.unix_timestamp().saturating_mul(1000);
            let exec = Handle::current();
            let joined = tokio::task::spawn_blocking(move || job.run_at(&exec, now_millis)).await;
            Ok(match joined {
                Ok(Ok(_)) => HandlerResult::Ok,
                Ok(Err(error)) => HandlerResult::Retry(error.code().to_owned()),
                Err(_) => HandlerResult::Retry(AuditJobError::PassAborted.code().to_owned()),
            })
        })
    }
}
