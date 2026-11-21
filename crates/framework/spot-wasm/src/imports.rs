//! What the host provides to an app: the imports of the test world, behind a trait the caller
//! implements. The linker calls it only for imports the app was granted.

use wasmtime::component::{ComponentType, Lift, Lower};

/// A log level, as the world's `level` enum.
#[derive(ComponentType, Lift, Lower, Clone, Copy, Debug, PartialEq, Eq)]
#[component(enum)]
#[repr(u8)]
pub enum LogLevel {
    /// `debug`
    #[component(name = "debug")]
    Debug,
    /// `info`
    #[component(name = "info")]
    Info,
    /// `warn`
    #[component(name = "warn")]
    Warn,
    /// `error`
    #[component(name = "error")]
    Error,
}

/// The host side of the test world's imports. One value serves one call: it carries the
/// invocation (the time, the user, the site) that the app sees through these functions and no
/// other.
pub trait Imports: Send + Sync {
    /// `log(level, message)`: the app writes a log line. The host decides where it goes.
    fn log(&self, level: LogLevel, message: &str);

    /// `now() -> s64`: the time of the invocation, in microseconds since the Unix epoch. This is
    /// the only clock an app sees; the machine's clock is not exposed.
    fn now(&self) -> i64;

    /// `get-doc(doctype, id) -> option<string>`: a document as JSON text, or `None` if it does not
    /// exist. The host has already checked that the app may read `doctype`.
    fn get_doc(&self, doctype: &str, id: &str) -> Option<String>;
}
