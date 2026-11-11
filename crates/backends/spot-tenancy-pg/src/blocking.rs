//! Running an async database call from a synchronous trait method.
//!
//! `ControlStore` and the provisioning ports are synchronous; `tokio-postgres` is not. From a
//! plain thread the call blocks on the runtime directly. From inside a multi-thread runtime it
//! first asks the runtime to hand the worker's other tasks to another thread
//! (`block_in_place`), then blocks on the future; this is the pattern Tokio documents for a
//! blocking call inside an async task. It panics on a current-thread runtime, which is why the
//! crate documents that it must not be used there.

use std::future::Future;

/// Blocks until `future` completes.
pub(crate) fn block_on<T>(runtime: &tokio::runtime::Runtime, future: impl Future<Output = T>) -> T {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| runtime.block_on(future))
    } else {
        runtime.block_on(future)
    }
}
