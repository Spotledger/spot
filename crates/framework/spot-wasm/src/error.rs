//! Typed errors with stable `wasm.*` codes.

use thiserror::Error;

/// Why a call into an app failed. Every variant has a stable [`WasmError::code`]; none of them
/// affects the host or any other call.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum WasmError {
    /// The call used all of its fuel.
    #[error("wasm.out_of_fuel: the app used all {fuel} units of fuel")]
    OutOfFuel {
        /// The fuel the call was given.
        fuel: u64,
    },
    /// The call ran past its wall-clock limit.
    #[error("wasm.timeout: the app did not finish within {millis} ms")]
    Timeout {
        /// The limit, in milliseconds.
        millis: u128,
    },
    /// The app asked for more memory or table elements than its limit.
    #[error("wasm.memory: {0}")]
    Memory(String),
    /// The app trapped (unreachable code, a bad access, a stack overflow, a failed instantiation).
    #[error("wasm.trap: {0}")]
    Trap(String),
    /// The app made more host calls than its limit.
    #[error("wasm.too_many_host_calls: more than {limit} host calls")]
    TooManyHostCalls {
        /// The limit.
        limit: u32,
    },
    /// The bytes are not a valid component, or do not have the shape the host needs.
    #[error("wasm.bad_component: {0}")]
    BadComponent(String),
    /// The component has no such export.
    #[error("wasm.missing_export: the app has no export `{0}`")]
    MissingExport(String),
    /// The app imports (or called) something it was not granted.
    #[error("wasm.capability_denied: {0}")]
    CapabilityDenied(String),
    /// The result is larger than the call's output limit.
    #[error("wasm.output_too_large: the result is larger than {limit} bytes")]
    OutputTooLarge {
        /// The limit, in bytes.
        limit: usize,
    },
    /// The arguments do not fit the export's parameter types.
    #[error("wasm.bad_arguments: {0}")]
    BadArguments(String),
    /// The export returned an error value (`result<_, e>` in the world): the app's own error, not a
    /// failure of the host. The value is the error, as JSON.
    #[error("wasm.app_error: the app returned an error: {0}")]
    App(serde_json::Value),
    /// Something inside the host failed that no app caused.
    #[error("wasm.internal: {0}")]
    Internal(String),
}

impl WasmError {
    /// The stable machine-readable code, also the prefix of the message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::OutOfFuel { .. } => "wasm.out_of_fuel",
            Self::Timeout { .. } => "wasm.timeout",
            Self::Memory(_) => "wasm.memory",
            Self::Trap(_) => "wasm.trap",
            Self::TooManyHostCalls { .. } => "wasm.too_many_host_calls",
            Self::BadComponent(_) => "wasm.bad_component",
            Self::MissingExport(_) => "wasm.missing_export",
            Self::CapabilityDenied(_) => "wasm.capability_denied",
            Self::OutputTooLarge { .. } => "wasm.output_too_large",
            Self::BadArguments(_) => "wasm.bad_arguments",
            Self::App(_) => "wasm.app_error",
            Self::Internal(_) => "wasm.internal",
        }
    }
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, WasmError>;
