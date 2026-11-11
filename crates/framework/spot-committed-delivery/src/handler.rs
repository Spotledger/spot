//! The `sys:committed_steps` job handler.

use std::pin::Pin;
use std::sync::Arc;

use spot_app_api::prelude::Name;
use spot_app_api::{AppError, Document};
use spot_core::SiteId;
use spot_document::engine::{
    CommittedJobArgs, CommittedStep, CommittedSteps, JOB_COMMITTED_STEPS, StepSource,
    committed_point,
};
use spot_jobs::store::JobFilter;
use spot_jobs::worker::{HandlerResult, JobContext, JobHandlerRegistry};
use spot_jobs::{JobState, JobStore};

use crate::markers::StepMarkers;

/// Runs steps and reads the documents they are about, for one site.
pub trait StepRunner: CommittedSteps + StepSource {}

impl<T: CommittedSteps + StepSource> StepRunner for T {}

/// The runner that serves a site, if one does.
pub trait RunnerFor: Send + Sync {
    /// The runner for `site`; `None` if this host does not serve it.
    fn runner(&self, site: &SiteId) -> Option<Arc<dyn StepRunner>>;
}

/// A host that serves one site.
pub struct SingleSite {
    site: SiteId,
    runner: Arc<dyn StepRunner>,
}

impl SingleSite {
    /// `runner` serves `site` and no other.
    pub fn new(site: SiteId, runner: Arc<dyn StepRunner>) -> Self {
        Self { site, runner }
    }
}

impl RunnerFor for SingleSite {
    fn runner(&self, site: &SiteId) -> Option<Arc<dyn StepRunner>> {
        (*site == self.site).then(|| self.runner.clone())
    }
}

/// The handler of [`JOB_COMMITTED_STEPS`] jobs. See the crate docs.
pub struct CommittedStepsHandler {
    runners: Arc<dyn RunnerFor>,
    markers: Arc<dyn StepMarkers>,
    jobs: Arc<dyn JobStore>,
}

/// The codes the handler answers with (declared in `committed_delivery.toml`).
mod code {
    pub(super) const BAD_ARGS: &str = "committed_delivery.bad_args";
    pub(super) const WRONG_SITE: &str = "committed_delivery.wrong_site";
    pub(super) const UNKNOWN_SITE: &str = "committed_delivery.unknown_site";
    pub(super) const UNKNOWN_STEP: &str = "committed_delivery.unknown_step";
    pub(super) const PREDECESSOR: &str = "committed_delivery.predecessor_pending";
    pub(super) const STEP_UNAVAILABLE: &str = "committed_delivery.step_unavailable";
    pub(super) const UNAVAILABLE: &str = "committed_delivery.unavailable";
}

impl CommittedStepsHandler {
    /// A handler running steps through `runners`, keeping progress in `markers` and reading
    /// ordering from `jobs`.
    pub fn new(
        runners: Arc<dyn RunnerFor>,
        markers: Arc<dyn StepMarkers>,
        jobs: Arc<dyn JobStore>,
    ) -> Self {
        Self {
            runners,
            markers,
            jobs,
        }
    }

    /// Registers the handler under [`JOB_COMMITTED_STEPS`].
    pub fn register(self: &Arc<Self>, registry: &mut JobHandlerRegistry) {
        let handler = self.clone();
        registry.register(
            JOB_COMMITTED_STEPS,
            move |ctx: JobContext| -> Pin<
                Box<dyn std::future::Future<Output = spot_jobs::Result<HandlerResult>> + Send>,
            > {
                let handler = handler.clone();
                Box::pin(async move { Ok(handler.handle(&ctx).await) })
            },
        );
    }

    /// Runs one attempt of the job.
    pub async fn handle(&self, ctx: &JobContext) -> HandlerResult {
        let job = &ctx.job;
        let args = match CommittedJobArgs::from_json(&job.args) {
            Ok(args) => args,
            Err(_) => return HandlerResult::Fail(code::BAD_ARGS.to_owned()),
        };
        // The job's own site is authoritative; arguments naming another one are refused.
        if args.site != job.site {
            return HandlerResult::Fail(code::WRONG_SITE.to_owned());
        }
        let Some(runner) = self.runners.runner(&job.site) else {
            return HandlerResult::Fail(code::UNKNOWN_SITE.to_owned());
        };
        // Everything is parsed before the first step runs, so a bad point stops the job whole.
        let mut points = Vec::with_capacity(args.steps.len());
        for name in &args.steps {
            match committed_point(name) {
                Some(point) => points.push((name.as_str(), point)),
                None => return HandlerResult::Fail(code::UNKNOWN_STEP.to_owned()),
            }
        }
        match self.predecessor_pending(ctx, &args).await {
            Ok(false) => {}
            Ok(true) => return HandlerResult::Retry(code::PREDECESSOR.to_owned()),
            Err(()) => return HandlerResult::Retry(code::UNAVAILABLE.to_owned()),
        }
        let Ok(done) = self.markers.done(job.id).await else {
            return HandlerResult::Retry(code::UNAVAILABLE.to_owned());
        };
        if points.iter().all(|(name, _)| done.contains(*name)) {
            return HandlerResult::Ok;
        }
        let doc = match self.document(&*runner, &args).await {
            Ok(doc) => doc,
            Err(error) if error.retryable() => {
                return HandlerResult::Retry(code::STEP_UNAVAILABLE.to_owned());
            }
            Err(_) => return HandlerResult::Fail(code::BAD_ARGS.to_owned()),
        };
        let invocation = args.invocation();
        for (name, point) in points {
            if done.contains(name) {
                continue;
            }
            let step = CommittedStep {
                point,
                doc: doc.clone(),
                operation: args.operation,
            };
            let outcome = runner.run_checked(&invocation, step).await;
            if outcome.failure.as_ref().is_some_and(AppError::retryable) {
                // An infrastructure failure: no marker, the queue's backoff reruns from here.
                return HandlerResult::Retry(code::STEP_UNAVAILABLE.to_owned());
            }
            if self.markers.mark(job.id, name).await.is_err() {
                return HandlerResult::Retry(code::UNAVAILABLE.to_owned());
            }
        }
        HandlerResult::Ok
    }

    /// The document the steps are about, as storage holds it, or its bare identity if it is
    /// gone (a deletion, or deleted since).
    async fn document(
        &self,
        runner: &dyn StepRunner,
        args: &CommittedJobArgs,
    ) -> Result<Document, AppError> {
        let doctype = Name::new(&args.doctype).map_err(|_| {
            AppError::new(
                spot_app_api::ErrorClass::Internal,
                "platform.event",
                "The committed-steps job names an invalid DocType.",
            )
        })?;
        let current = runner.current(&args.doctype, args.id).await?;
        Ok(current.unwrap_or_else(|| Document::new(doctype, args.id)))
    }

    /// Whether an earlier write of the same document still has an unfinished job.
    async fn predecessor_pending(
        &self,
        ctx: &JobContext,
        args: &CommittedJobArgs,
    ) -> Result<bool, ()> {
        let job = &ctx.job;
        let jobs = self
            .jobs
            .list(JobFilter {
                site: Some(job.site.clone()),
                state: None,
                queue: Some(job.queue.clone()),
                limit: None,
            })
            .await
            .map_err(|_| ())?;
        let mine = args.order();
        Ok(jobs.iter().any(|other| {
            other.id != job.id
                && other.name == JOB_COMMITTED_STEPS
                && matches!(
                    other.state,
                    JobState::Pending | JobState::Running | JobState::Retrying { .. }
                )
                && CommittedJobArgs::from_json(&other.args).is_ok_and(|o| {
                    o.site == args.site
                        && o.doctype == args.doctype
                        && o.id == args.id
                        && o.order() < mine
                })
        }))
    }
}
