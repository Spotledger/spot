//! The template engine of the Spotledger Platform (WP-PLT-12).
//!
//! [`TemplateEngine`] renders platform templates from a [`TemplateSource`]
//! inside a hardened MiniJinja environment: strict undefined values, a fuel
//! budget, a recursion limit and an output size limit. Money and points in
//! time enter the context as custom objects; the `money`, `number`,
//! `amount_in_words`, `date` and `datetime` filters format them exactly.
//!
//! The output kind is chosen from the template name's extension:
//! `.html`/`.htm` escape HTML, `.typ` prints every value as a Typst string
//! literal so data can never inject markup or code (SPK-03); everything else
//! renders as plain text.
//!
//! Money is exact (ADR-12): decimal context values never become floats and
//! templates cannot compute with them — they can only be formatted.

pub mod context;
pub mod engine;
pub mod error;

mod filters;
mod format;
mod value;

pub use context::{Context, CtxValue};
pub use engine::{
    EngineStatistics, MemorySource, OutputKind, TemplateEngine, TemplateSource, Version,
};
pub use error::TemplateError;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-template";
