//! Where a controller came from and the reference an app install names it by.
//!
//! Native Rust, WASM and TypeScript controllers implement the same [`Controller`] trait and the
//! engine treats them alike. The [`ControllerSource`] is recorded for diagnostics and the admin
//! listing only. An install names a controller with a [`ControllerRef`]: `native:<name>` for a
//! controller built into the host, `wasm:<export>` and `ts:<path>` for the adapters WP-EXT-10 and
//! the WASM host work add later.
//!
//! [`Controller`]: spot_app_api::Controller

use std::fmt;

use spot_app_api::{AppError, AppResult, ErrorClass};

use crate::codes;

/// How a controller is implemented. The engine treats all three alike; only the loader differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ControllerSource {
    /// A Rust type compiled into the host.
    Native,
    /// A controller loaded from a WASM module.
    Wasm,
    /// A controller loaded from a TypeScript module.
    TypeScript,
}

impl ControllerSource {
    /// The reference prefix: `native`, `wasm` or `ts`.
    pub fn prefix(self) -> &'static str {
        match self {
            ControllerSource::Native => "native",
            ControllerSource::Wasm => "wasm",
            ControllerSource::TypeScript => "ts",
        }
    }

    /// The name of the host adapter that loads this kind, as an operator sees it. The error a
    /// missing adapter raises names this.
    pub fn adapter_name(self) -> &'static str {
        match self {
            ControllerSource::Native => "native",
            ControllerSource::Wasm => "WASM",
            ControllerSource::TypeScript => "TypeScript",
        }
    }

    /// The source a reference prefix names, or `None`.
    pub fn from_prefix(prefix: &str) -> Option<ControllerSource> {
        match prefix {
            "native" => Some(ControllerSource::Native),
            "wasm" => Some(ControllerSource::Wasm),
            "ts" => Some(ControllerSource::TypeScript),
            _ => None,
        }
    }
}

impl fmt::Display for ControllerSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.adapter_name())
    }
}

/// A controller reference from an app install: `native:<name>`, `wasm:<export>` or `ts:<path>`.
///
/// A [`ControllerFactory`](crate::ControllerFactory) maps one to an `Arc<dyn Controller>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ControllerRef {
    source: ControllerSource,
    value: String,
}

impl ControllerRef {
    /// A native reference to a controller built into the host under `name`.
    pub fn native(name: &str) -> ControllerRef {
        ControllerRef {
            source: ControllerSource::Native,
            value: name.to_owned(),
        }
    }

    /// Parses `native:<name>`, `wasm:<export>` or `ts:<path>`.
    ///
    /// # Errors
    ///
    /// `controller.invalid_reference` when the text has no `:`-separated prefix this build
    /// knows, or when its value is empty.
    pub fn parse(text: &str) -> AppResult<ControllerRef> {
        let invalid = || {
            AppError::new(
                ErrorClass::Internal,
                codes::INVALID_REFERENCE,
                format!(
                    "A controller reference is `native:<name>`, `wasm:<export>` or `ts:<path>`, \
                     not {text:?}."
                ),
            )
        };
        let (prefix, value) = text.split_once(':').ok_or_else(invalid)?;
        let source = ControllerSource::from_prefix(prefix).ok_or_else(invalid)?;
        if value.is_empty() {
            return Err(invalid());
        }
        Ok(ControllerRef {
            source,
            value: value.to_owned(),
        })
    }

    /// Which kind of controller it names.
    pub fn source(&self) -> ControllerSource {
        self.source
    }

    /// The name, export or path after the prefix.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for ControllerRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.source.prefix(), self.value)
    }
}
