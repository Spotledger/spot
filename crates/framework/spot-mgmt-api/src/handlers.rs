//! Operation handlers: what runs once a command is accepted.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use serde_json::Value;
use spot_core::Clock;
use spot_mgmt_ops::{OpId, Validated};
use tokio::sync::watch;

use crate::lifecycle::{CommandStore, ProgressLine};
use crate::ports::BoxFuture;

/// How a handler ended without a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandlerError {
    /// A stable dotted code (a fixed string: never a secret, a path or a parameter value).
    pub code: &'static str,
    /// True when the handler stopped because cancel was requested.
    pub cancelled: bool,
}

impl HandlerError {
    /// A failure with a stable code.
    #[must_use]
    pub fn failed(code: &'static str) -> Self {
        Self {
            code,
            cancelled: false,
        }
    }

    /// The handler observed the cancel request and stopped.
    #[must_use]
    pub fn cancelled() -> Self {
        Self {
            code: "mgmt.cancelled",
            cancelled: true,
        }
    }
}

/// A handler's result: redacted JSON (at most 64 KiB) or a [`HandlerError`].
pub type HandlerResult = Result<Value, HandlerError>;

/// What a handler is given.
#[derive(Debug)]
pub struct HandlerRequest {
    /// The command id.
    pub command_id: String,
    /// The principal that issued it.
    pub principal: String,
    /// The operation id.
    pub operation: OpId,
    /// The validated parameters (secrets present, redacted in `Debug`).
    pub params: Validated,
}

/// Progress reporting and cooperative cancel for a running handler.
#[derive(Clone)]
pub struct HandlerContext {
    command_id: String,
    commands: Arc<dyn CommandStore>,
    cancel: watch::Receiver<bool>,
    clock: Arc<dyn Clock>,
}

impl HandlerContext {
    pub(crate) fn new(
        command_id: String,
        commands: Arc<dyn CommandStore>,
        cancel: watch::Receiver<bool>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            command_id,
            commands,
            cancel,
            clock,
        }
    }

    /// Records a progress line (step truncated to 120 characters, percent capped at 100). A store
    /// failure is ignored: progress is advisory.
    pub async fn progress(&self, step: &str, percent: u8) {
        let line = ProgressLine {
            step: step.chars().take(120).collect(),
            percent: percent.min(100),
            at: self.clock.now_utc().unix_timestamp(),
        };
        let _ = self.commands.progress(&self.command_id, line).await;
    }

    /// Whether cancel was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        *self.cancel.borrow()
    }

    /// Completes when cancel is requested.
    pub async fn cancelled(&self) {
        let mut receiver = self.cancel.clone();
        let _requested = receiver.wait_for(|requested| *requested).await.is_ok();
    }
}

/// Runs one operation.
pub trait Handler: Send + Sync {
    /// Runs the operation to completion.
    fn call(
        &self,
        context: HandlerContext,
        request: HandlerRequest,
    ) -> BoxFuture<'static, HandlerResult>;
}

struct FnHandler<F>(F);

impl<F, Fut> Handler for FnHandler<F>
where
    F: Fn(HandlerContext, HandlerRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HandlerResult> + Send + 'static,
{
    fn call(
        &self,
        context: HandlerContext,
        request: HandlerRequest,
    ) -> BoxFuture<'static, HandlerResult> {
        Box::pin((self.0)(context, request))
    }
}

/// Wraps an async closure as a handler.
pub fn handler_fn<F, Fut>(function: F) -> Arc<dyn Handler>
where
    F: Fn(HandlerContext, HandlerRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HandlerResult> + Send + 'static,
{
    Arc::new(FnHandler(function))
}

/// A registered handler and whether it supports cooperative cancel.
#[derive(Clone)]
pub struct Registered {
    /// The handler.
    pub handler: Arc<dyn Handler>,
    /// Whether cancel is supported.
    pub cancellable: bool,
}

/// Registering the same operation twice.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a handler is already registered for this operation")]
pub struct DuplicateHandler;

/// Operation id to handler.
#[derive(Clone, Default)]
pub struct HandlerRegistry {
    by_op: BTreeMap<String, Registered>,
}

impl HandlerRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a handler.
    ///
    /// # Errors
    /// [`DuplicateHandler`] when the operation already has one.
    pub fn register(
        &mut self,
        operation: &str,
        handler: Arc<dyn Handler>,
        cancellable: bool,
    ) -> Result<(), DuplicateHandler> {
        if self.by_op.contains_key(operation) {
            return Err(DuplicateHandler);
        }
        self.by_op.insert(
            operation.to_owned(),
            Registered {
                handler,
                cancellable,
            },
        );
        Ok(())
    }

    /// The handler for an operation.
    #[must_use]
    pub fn get(&self, operation: &str) -> Option<&Registered> {
        self.by_op.get(operation)
    }
}
