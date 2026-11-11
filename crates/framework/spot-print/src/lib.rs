//! PDF generation for the Spotledger Platform (WP-PLT-13, SPK-03).
//!
//! The flow is MiniJinja ([`spot_template`], `.typ` templates) -> Typst
//! markup -> `typst::compile` -> `typst_pdf`, all inside the one binary with
//! no external process.
//!
//! * [`PdfRenderer`] renders a named template with a [`Context`] to a [`Pdf`].
//! * The Typst world is sandboxed: the only files are the [`AssetMap`], the
//!   only fonts are the [`FontSet`], and `#import "@preview/..."` or any path
//!   outside the map fails with [`PrintError::Sandbox`] naming the path.
//! * Output is deterministic: the creation date is an input
//!   ([`RenderOptions::created_at`]), so equal inputs give equal bytes.
//! * Rendering is CPU work run on tokio's blocking pool behind a semaphore
//!   ([`RenderLimits`]), with a page cap, a wall-clock budget and Typst's
//!   memoization cache evicted after each render.
//!
//! [`PrintError::Compile::template_line`] is a line in the rendered Typst
//! markup: [`spot_template::TemplateEngine`] gives no read access to template
//! sources, so a mapping back to the `.j2` text is not possible from here.
//!
//! Typst interns every distinct file path a document names in a process-wide
//! table limited to 65,535 entries; templates must not build asset paths from
//! data.

mod error;
mod fonts;
mod renderer;
mod world;

pub use error::PrintError;
pub use fonts::FontSet;
pub use renderer::{Pdf, PdfRenderer, PdfStandard, RenderLimits, RenderOptions};
pub use world::AssetMap;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-print";
