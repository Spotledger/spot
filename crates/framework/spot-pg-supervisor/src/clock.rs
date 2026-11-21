//! Clock seam: every timestamp and backoff delay flows through this trait so tests can
//! inject a manual clock (and tokio pause) instead of sleeping. Production uses
//! [`SystemClock`].

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Source of wall time for event timestamps and restart windows.
///
/// Object-safe on purpose: `Supervisor` carries `Arc<dyn Clock>`. `sleep` returns a
/// boxed future so the trait stays dyn-compatible.
pub trait Clock: Send + Sync + 'static {
    /// Milliseconds since an arbitrary but fixed epoch (production: the Unix epoch).
    fn now_ms(&self) -> u64;
    /// Asynchronous sleep; implemented so tests can drive time manually.
    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Production clock: the system clock and real tokio sleeps.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default()
    }

    fn sleep(&self, d: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(tokio::time::sleep(d))
    }
}
