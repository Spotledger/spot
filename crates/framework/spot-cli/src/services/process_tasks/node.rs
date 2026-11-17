//! The Decision node, hosted.

use spot_bpm_engine::{NodeCtx, NodeHandler, NodeKind, NodeOutcome};
use spot_bpm_tasks::DecisionNodeHandler;
use tokio::runtime::{Handle, RuntimeFlavor};

use super::{CODE_NO_RUNTIME, CODE_THREAD};

/// The Decision node handler of the process engine, hosted: the engine calls it on a runtime
/// worker, so the node leaves the runtime (`block_in_place`) and runs on a scoped plain thread,
/// where the task store may be called. A current-thread runtime, or no runtime, fails the node
/// closed with `bpm_tasks.no_runtime`.
pub struct HostDecisionHandler {
    pub(super) inner: DecisionNodeHandler,
}

impl NodeHandler for HostDecisionHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Decision
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        match Handle::try_current() {
            Ok(runtime) if runtime.runtime_flavor() != RuntimeFlavor::CurrentThread => {}
            _ => {
                return NodeOutcome::fail(
                    CODE_NO_RUNTIME,
                    "the task inbox needs a multi-thread async runtime",
                );
            }
        }
        tokio::task::block_in_place(|| {
            std::thread::scope(|scope| {
                let spawned = std::thread::Builder::new()
                    .name("process-tasks-node".to_owned())
                    .spawn_scoped(scope, || self.inner.run(ctx));
                match spawned {
                    Ok(handle) => handle.join().unwrap_or_else(|_| {
                        NodeOutcome::fail(CODE_THREAD, "the decision node's thread failed")
                    }),
                    Err(_) => {
                        NodeOutcome::fail(CODE_THREAD, "the decision node's thread did not start")
                    }
                }
            })
        })
    }
}
