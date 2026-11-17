//! The production [`TaskPort`].

use std::sync::Arc;

use spot_http::api::ports::task::{
    CompleteIn, CompleteOut, DelegateIn, ReassignIn, TaskDetailOut, TaskListIn, TaskPageOut,
    TaskPort, TaskPortError, TaskView,
};
use spot_http::api::service::{BoxFuture, RequestCtx};

use super::ops::{Core, outcome_class};
use super::threads::plain;

/// The production [`TaskPort`]: authenticates the caller, then runs the operation on a plain
/// thread. See the module docs.
pub struct Inbox {
    pub(super) core: Arc<Core>,
}

impl Inbox {
    /// The caller of `ctx`, with a refused sign-in audited (no user, only the code).
    async fn caller(
        &self,
        action: &'static str,
        ctx: &RequestCtx,
    ) -> std::result::Result<String, TaskPortError> {
        match self.core.callers.caller(ctx).await {
            Ok(who) => Ok(who),
            Err(error) => {
                self.core.audit(action, "-", "-", "-", error.code());
                Err(error)
            }
        }
    }

    async fn offload<T, F>(&self, work: F) -> std::result::Result<T, TaskPortError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Core>) -> std::result::Result<T, TaskPortError> + Send + 'static,
    {
        let core = Arc::clone(&self.core);
        plain(move || work(core))
            .await
            .unwrap_or(Err(TaskPortError::Unavailable))
    }
}

impl TaskPort for Inbox {
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: &'a TaskListIn,
    ) -> BoxFuture<'a, std::result::Result<TaskPageOut, TaskPortError>> {
        Box::pin(async move {
            let who = self.caller("list", ctx).await?;
            let query = query.clone();
            self.offload(move |core| {
                core.audited("list", &who, "-", "-", || core.list(&who, &query))
            })
            .await
        })
    }

    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, std::result::Result<TaskDetailOut, TaskPortError>> {
        Box::pin(async move {
            let who = self.caller("get", ctx).await?;
            let id = id.to_owned();
            self.offload(move |core| core.audited("get", &who, &id, "-", || core.get(&who, &id)))
                .await
        })
    }

    fn claim<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
    ) -> BoxFuture<'a, std::result::Result<TaskView, TaskPortError>> {
        Box::pin(async move {
            let who = self.caller("claim", ctx).await?;
            let id = id.to_owned();
            self.offload(move |core| {
                core.audited("claim", &who, &id, "-", || core.claim(&who, &id, expected))
            })
            .await
        })
    }

    fn release<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
    ) -> BoxFuture<'a, std::result::Result<TaskView, TaskPortError>> {
        Box::pin(async move {
            let who = self.caller("release", ctx).await?;
            let id = id.to_owned();
            self.offload(move |core| {
                core.audited("release", &who, &id, "-", || {
                    core.release(&who, &id, expected)
                })
            })
            .await
        })
    }

    fn complete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
        body: &'a CompleteIn,
    ) -> BoxFuture<'a, std::result::Result<CompleteOut, TaskPortError>> {
        Box::pin(async move {
            let who = self.caller("complete", ctx).await?;
            let id = id.to_owned();
            let body = body.clone();
            let key = ctx.idempotency_key.clone();
            self.offload(move |core| {
                let class = outcome_class(&body.outcome);
                core.audited("complete", &who, &id, class, || {
                    core.complete(&who, &id, expected, &body, key.as_deref())
                })
            })
            .await
        })
    }

    fn delegate<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
        body: &'a DelegateIn,
    ) -> BoxFuture<'a, std::result::Result<TaskView, TaskPortError>> {
        Box::pin(async move {
            let who = self.caller("delegate", ctx).await?;
            let id = id.to_owned();
            let body = body.clone();
            self.offload(move |core| {
                core.audited("delegate", &who, &id, "-", || {
                    core.delegate(&who, &id, expected, &body)
                })
            })
            .await
        })
    }

    fn reassign<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
        body: &'a ReassignIn,
    ) -> BoxFuture<'a, std::result::Result<TaskView, TaskPortError>> {
        Box::pin(async move {
            let who = self.caller("reassign", ctx).await?;
            let id = id.to_owned();
            let body = body.clone();
            self.offload(move |core| {
                core.audited("reassign", &who, &id, "-", || {
                    core.reassign(&who, &id, expected, &body)
                })
            })
            .await
        })
    }
}
