//! Typed errors for the PDF renderer (WP-PLT-13).

use std::time::Duration;

use spot_template::TemplateError;

/// Everything that can fail while turning a template into a PDF.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PrintError {
    /// The template failed in `spot-template` (unknown template, undefined
    /// value, sandbox limit, ...). Passed through unchanged.
    #[error(transparent)]
    Template(#[from] TemplateError),
    /// Typst could not compile the rendered markup.
    ///
    /// `template_line` is the 1-based line in the **rendered Typst markup**
    /// where the first error points (the engine does not expose template
    /// sources, so a mapping to the `.j2` line is not possible; see the
    /// crate docs). `None` when the diagnostic has no location.
    #[error("typst compile error{}: {message}", line_suffix(*template_line))]
    Compile {
        /// The first diagnostic's message, with its hints.
        message: String,
        /// 1-based line in the rendered markup, when known.
        template_line: Option<usize>,
    },
    /// The document asked for a file or package outside the asset map.
    #[error("sandbox violation: `{path}` is not available to documents")]
    Sandbox {
        /// The path (or `@namespace/name:version` package) that was refused.
        path: String,
    },
    /// The document has more pages than [`crate::RenderLimits::max_pages`].
    #[error("document has {pages} pages, the limit is {limit}")]
    PageLimitExceeded {
        /// The configured maximum.
        limit: usize,
        /// The pages the document has.
        pages: usize,
    },
    /// The wall-clock budget ran out.
    #[error("render exceeded the time budget of {limit:?}")]
    TimeLimitExceeded {
        /// The configured budget.
        limit: Duration,
    },
    /// A font could not be parsed.
    #[error("invalid font data: {message}")]
    Font {
        /// What was wrong.
        message: String,
    },
    /// A render option was invalid (for example an unrepresentable date).
    #[error("invalid render option: {message}")]
    Options {
        /// What was wrong.
        message: String,
    },
    /// Typst failed while writing the PDF.
    #[error("pdf export failed: {message}")]
    Export {
        /// The exporter's message.
        message: String,
    },
    /// The blocking render task panicked or was cancelled.
    #[error("render task failed: {message}")]
    Task {
        /// The join error text.
        message: String,
    },
}

fn line_suffix(line: Option<usize>) -> String {
    match line {
        Some(line) => format!(" at line {line}"),
        None => String::new(),
    }
}
