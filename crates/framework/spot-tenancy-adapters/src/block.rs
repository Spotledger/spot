//! Running an asynchronous call from a synchronous port method.

use std::future::Future;

/// Blocks until `future` completes.
///
/// From a plain thread this blocks on the runtime directly; from inside a multi-thread runtime
/// it first hands the worker's other tasks to another thread (`block_in_place`), which is the
/// pattern Tokio documents. It panics on a current-thread runtime, which is why the adapters
/// document that they must not be used from one.
pub(crate) fn block_on<T>(runtime: &tokio::runtime::Runtime, future: impl Future<Output = T>) -> T {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(|| runtime.block_on(future))
    } else {
        runtime.block_on(future)
    }
}
