//! The clock seam: the driver reads no clock and starts no timer of its own.

use std::future::Future;
use std::pin::Pin;

/// Time for the driver. Tests use a virtual clock; the node wiring supplies the real one.
pub trait Runtime: Send + Sync {
    /// The current time in milliseconds. Must not run backwards.
    fn now_millis(&self) -> u64;

    /// A future that completes once [`Runtime::now_millis`] has reached `deadline_millis`
    /// (immediately when it already has).
    fn sleep_until(&self, deadline_millis: u64) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}
