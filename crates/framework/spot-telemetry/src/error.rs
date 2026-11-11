//! Errors from telemetry setup.

/// Why telemetry could not be set up.
#[derive(Debug, thiserror::Error)]
pub enum TelemetryError {
    /// `init` was already called in this process.
    #[error("telemetry is already initialised in this process")]
    AlreadyInitialised,
    /// The filter string is not valid `EnvFilter` syntax.
    #[error("invalid log filter {filter:?}: {reason}")]
    InvalidFilter {
        /// The rejected filter string.
        filter: String,
        /// What the parser said.
        reason: String,
    },
    /// An OTLP endpoint was configured but the `otlp` cargo feature is off.
    #[error("OTLP export was configured but the `otlp` feature is not enabled")]
    OtlpNotCompiled,
    /// The OTLP pipeline could not be built.
    #[error("could not build the OTLP pipeline: {0}")]
    Otlp(String),
    /// Another metrics recorder is already installed in this process.
    #[error("could not install the Prometheus recorder: {0}")]
    Metrics(String),
}
