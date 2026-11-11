//! [`PdfRenderer`]: template -> Typst markup -> PDF.

use std::sync::Arc;
use std::thread::available_parallelism;
use std::time::{Duration, Instant};

use spot_template::{Context, TemplateEngine, TemplateSource};
use time::{OffsetDateTime, UtcOffset};
use tokio::sync::Semaphore;
use typst::WorldExt;
use typst::diag::SourceDiagnostic;
use typst::foundations::Datetime;
use typst::syntax::Source;
use typst_layout::PagedDocument;

use crate::error::PrintError;
use crate::fonts::FontSet;
use crate::world::{AssetMap, PrintWorld, Shared};

/// Resource limits for one renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderLimits {
    /// Renders running at once (default: the number of CPUs). Others wait.
    pub max_concurrent: usize,
    /// Pages a document may have (default 500). Checked right after layout.
    pub max_pages: usize,
    /// Wall-clock budget per render, measured from when the render starts
    /// running (not from when it queued). Typst cannot be interrupted while
    /// laying out, so it is checked between template rendering, compile and
    /// export; a single stage may overrun it (default 60 s).
    pub max_duration: Duration,
    /// Typst's memoization cache is evicted after every render, dropping
    /// entries not used by the last `cache_retention` renders (default 3).
    /// `0` clears the whole cache every time: lowest memory (SPK-03: about
    /// 250 MiB for a 2,000-row report is held until evicted), but no reuse
    /// between renders. Larger values keep repeated documents fast.
    pub cache_retention: usize,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            max_concurrent: available_parallelism().map_or(1, usize::from),
            max_pages: 500,
            max_duration: Duration::from_secs(60),
            cache_retention: 3,
        }
    }
}

/// A PDF standard to enforce.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfStandard {
    /// PDF/A-2b, for archival documents (validated with veraPDF in SPK-03).
    A2b,
}

/// Per-render options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderOptions {
    /// The document's creation date and "today" inside the template. An
    /// input, so the same inputs always give the same bytes.
    pub created_at: OffsetDateTime,
    /// A standard to enforce, if any.
    pub pdf_standard: Option<PdfStandard>,
}

impl RenderOptions {
    /// Options for the given creation time, with no PDF standard.
    #[must_use]
    pub fn new(created_at: OffsetDateTime) -> Self {
        Self {
            created_at,
            pdf_standard: None,
        }
    }

    /// Enforces a PDF standard.
    #[must_use]
    pub fn with_standard(mut self, standard: PdfStandard) -> Self {
        self.pdf_standard = Some(standard);
        self
    }
}

/// A rendered PDF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdf {
    /// The file bytes.
    pub bytes: Vec<u8>,
    /// The number of pages.
    pub page_count: usize,
}

struct Inner<S: TemplateSource> {
    engine: Arc<TemplateEngine<S>>,
    shared: Shared,
    limits: RenderLimits,
    permits: Semaphore,
}

/// Renders templates to PDFs with embedded Typst.
///
/// Cheap to clone; clones share fonts, limits and the concurrency budget.
pub struct PdfRenderer<S: TemplateSource + Send + Sync + 'static> {
    inner: Arc<Inner<S>>,
}

impl<S: TemplateSource + Send + Sync + 'static> Clone for PdfRenderer<S> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<S: TemplateSource + Send + Sync + 'static> PdfRenderer<S> {
    /// Creates a renderer. Fonts are parsed once, here.
    ///
    /// `max_concurrent` and `max_pages` are raised to at least 1.
    #[must_use]
    pub fn new(engine: Arc<TemplateEngine<S>>, fonts: FontSet, limits: RenderLimits) -> Self {
        let limits = RenderLimits {
            max_concurrent: limits.max_concurrent.max(1),
            max_pages: limits.max_pages.max(1),
            ..limits
        };
        Self {
            inner: Arc::new(Inner {
                engine,
                shared: Shared::new(&fonts),
                permits: Semaphore::new(limits.max_concurrent),
                limits,
            }),
        }
    }

    /// Renders template `template` (its name must end in `.typ`, so values
    /// are escaped as Typst strings) with `context` into a PDF.
    ///
    /// Runs on tokio's blocking pool behind the concurrency semaphore.
    ///
    /// # Errors
    /// [`PrintError`]: template errors from `spot-template`, Typst compile
    /// errors, sandbox violations, page and time limits, export failures.
    pub async fn render(
        &self,
        template: &str,
        context: &Context,
        assets: &AssetMap,
        opts: RenderOptions,
    ) -> Result<Pdf, PrintError> {
        let permit = self
            .inner
            .permits
            .acquire()
            .await
            .map_err(|error| PrintError::Task {
                message: error.to_string(),
            })?;
        let inner = Arc::clone(&self.inner);
        let template = template.to_owned();
        let context = context.clone();
        let assets = assets.clone();
        let joined = tokio::task::spawn_blocking(move || {
            render_blocking(&inner, &template, &context, &assets, opts)
        })
        .await;
        drop(permit);
        match joined {
            Ok(result) => result,
            Err(error) => Err(PrintError::Task {
                message: error.to_string(),
            }),
        }
    }
}

fn render_blocking<S: TemplateSource>(
    inner: &Inner<S>,
    template: &str,
    context: &Context,
    assets: &AssetMap,
    opts: RenderOptions,
) -> Result<Pdf, PrintError> {
    let started = Instant::now();
    let result = run(inner, template, context, assets, opts, started);
    // Whatever happened, do not let memoized layout grow without bound.
    typst::comemo::evict(inner.limits.cache_retention);
    result
}

fn run<S: TemplateSource>(
    inner: &Inner<S>,
    template: &str,
    context: &Context,
    assets: &AssetMap,
    opts: RenderOptions,
    started: Instant,
) -> Result<Pdf, PrintError> {
    let limits = &inner.limits;
    let check_time = || {
        if started.elapsed() > limits.max_duration {
            Err(PrintError::TimeLimitExceeded {
                limit: limits.max_duration,
            })
        } else {
            Ok(())
        }
    };
    let utc = opts.created_at.to_offset(UtcOffset::UTC);
    let month = u8::from(utc.month());
    let today = Datetime::from_ymd_hms(
        utc.year(),
        month,
        utc.day(),
        utc.hour(),
        utc.minute(),
        utc.second(),
    )
    .ok_or_else(|| PrintError::Options {
        message: format!("created_at {utc} is not a representable document date"),
    })?;

    let markup = inner.engine.render(template, context)?;
    check_time()?;

    let world = PrintWorld::new(&inner.shared, markup, assets, today);
    let compiled = typst::compile::<PagedDocument>(&world);
    let document = match compiled.output {
        Ok(document) => document,
        Err(errors) => {
            if let Some(path) = world.first_violation() {
                return Err(PrintError::Sandbox { path });
            }
            return Err(compile_error(&world, &errors));
        }
    };
    if document.pages().len() > limits.max_pages {
        return Err(PrintError::PageLimitExceeded {
            limit: limits.max_pages,
            pages: document.pages().len(),
        });
    }
    check_time()?;

    let options = pdf_options(&opts, today)?;
    let bytes = typst_pdf::pdf(&document, &options).map_err(|errors| PrintError::Export {
        message: first_message(&errors),
    })?;
    check_time()?;
    Ok(Pdf {
        bytes,
        page_count: document.pages().len(),
    })
}

fn pdf_options(opts: &RenderOptions, today: Datetime) -> Result<typst_pdf::PdfOptions, PrintError> {
    let standards = match opts.pdf_standard {
        Some(PdfStandard::A2b) => typst_pdf::PdfStandards::new(&[typst_pdf::PdfStandard::A_2b])
            .map_err(|error| PrintError::Options {
                message: format!("{error:?}"),
            })?,
        None => typst_pdf::PdfStandards::default(),
    };
    let options = typst_pdf::PdfOptions {
        timestamp: Some(typst_pdf::Timestamp::new_utc(today)),
        creator: typst::foundations::Smart::Custom(Some("Spotledger spot-print".to_owned())),
        standards,
        ..typst_pdf::PdfOptions::default()
    };
    Ok(options)
}

fn compile_error(world: &PrintWorld<'_>, errors: &[SourceDiagnostic]) -> PrintError {
    let first = errors.first();
    let template_line = first.and_then(|diagnostic| line_of(world, diagnostic));
    PrintError::Compile {
        message: first_message(errors),
        template_line,
    }
}

fn first_message(errors: &[SourceDiagnostic]) -> String {
    let Some(first) = errors.first() else {
        return "unknown error".to_owned();
    };
    let mut message = first.message.to_string();
    for hint in &first.hints {
        message.push_str(" (hint: ");
        message.push_str(&hint.v);
        message.push(')');
    }
    message
}

fn line_of(world: &PrintWorld<'_>, diagnostic: &SourceDiagnostic) -> Option<usize> {
    let range = world.range(diagnostic.span)?;
    let source: &Source = world.main_source();
    if diagnostic.span.id() != Some(source.id()) {
        return None;
    }
    source
        .lines()
        .byte_to_line(range.start)
        .map(|zero| zero + 1)
}
