//! Crash reports: the record, the collector that builds it from a panic, and the panic hook.
//!
//! A report holds only what the brief lists: version, platform, build id, the panic location,
//! a redacted backtrace, redacted log breadcrumbs, a config summary and the deployment model
//! digest. It never holds the panic message (which can quote document data), SQL parameters or
//! secrets. Everything is scrubbed **before** it is written, and the size caps are enforced
//! on the scrubbed text.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::consent::{ConsentState, ConsentStore};
use crate::error::{SupportError, codes};
use crate::scrub::Scrubber;
use crate::spool::Spool;
use crate::telemetry::Uploader;

/// The most bytes a serialised report may have.
pub const MAX_REPORT_BYTES: usize = 64 * 1024;
/// The most breadcrumbs a report carries (the newest are kept).
pub const MAX_BREADCRUMBS: usize = 50;
/// The most backtrace frames a report carries (the innermost are kept).
pub const MAX_FRAMES: usize = 64;
/// The longest one line of a report may be.
pub const MAX_LINE_BYTES: usize = 512;
/// The report format version.
pub const REPORT_VERSION: u32 = 1;
/// The most config summary entries a report carries.
pub const MAX_CONFIG_ENTRIES: usize = 64;
/// The room a capped line has beyond [`MAX_LINE_BYTES`]: the ellipsis a cut line ends with.
const CAP_SLACK: usize = 4;

/// What the host knows about itself and tells the collector once at start-up.
#[derive(Clone, Debug, Default)]
pub struct CrashContext {
    /// The product version.
    pub version: String,
    /// The platform, for example `linux-x86_64`.
    pub platform: String,
    /// The build id used to find symbols.
    pub build_id: String,
    /// A summary of the effective configuration: names and non-secret values only.
    pub config_summary: BTreeMap<String, String>,
    /// The digest of the deployment model.
    pub deployment_model_digest: String,
}

/// One crash report, already scrubbed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrashReport {
    /// The report format version.
    pub format: u32,
    /// The product version.
    pub version: String,
    /// The platform.
    pub platform: String,
    /// The build id.
    pub build_id: String,
    /// Where the panic happened: `file:line`, with the user's directories scrubbed.
    pub panic_location: String,
    /// The backtrace frames, innermost first, scrubbed.
    pub backtrace: Vec<String>,
    /// The latest log lines, oldest first, scrubbed.
    pub breadcrumbs: Vec<String>,
    /// The configuration summary, scrubbed.
    pub config_summary: BTreeMap<String, String>,
    /// The digest of the deployment model.
    pub deployment_model_digest: String,
    /// When the crash happened, in seconds since the Unix epoch.
    pub occurred_at: u64,
}

impl CrashReport {
    /// The report as JSON bytes.
    ///
    /// # Errors
    /// `report_too_large` when it cannot be serialised within [`MAX_REPORT_BYTES`].
    pub fn to_bytes(&self) -> Result<Vec<u8>, SupportError> {
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| SupportError::new(codes::SPOOL_FAILED, e.to_string()))?;
        if bytes.len() > MAX_REPORT_BYTES {
            return Err(SupportError::new(
                codes::REPORT_TOO_LARGE,
                "the report is over the size cap",
            ));
        }
        Ok(bytes)
    }
}

impl CrashReport {
    /// Checks the size and shape rules the collector enforces when it builds a report, so a
    /// receiver need not trust the sender to have kept them: the format version, the byte cap
    /// on the whole report, the cap on every line, and the counts of frames, breadcrumbs and
    /// config entries.
    ///
    /// # Errors
    /// `report_invalid` naming the first rule broken (never the content).
    pub fn validate(&self) -> Result<(), SupportError> {
        let bad = |what: &str| Err(SupportError::new(codes::REPORT_INVALID, what.to_owned()));
        if self.format != REPORT_VERSION {
            return bad("unknown format version");
        }
        if self.backtrace.len() > MAX_FRAMES {
            return bad("too many backtrace frames");
        }
        if self.breadcrumbs.len() > MAX_BREADCRUMBS {
            return bad("too many breadcrumbs");
        }
        if self.config_summary.len() > MAX_CONFIG_ENTRIES {
            return bad("too many config entries");
        }
        let fixed = [
            &self.version,
            &self.platform,
            &self.build_id,
            &self.panic_location,
            &self.deployment_model_digest,
        ];
        let lines = fixed
            .into_iter()
            .chain(&self.backtrace)
            .chain(&self.breadcrumbs)
            .chain(self.config_summary.keys())
            .chain(self.config_summary.values());
        for line in lines {
            if line.len() > MAX_LINE_BYTES + CAP_SLACK {
                return bad("a line is over the length cap");
            }
        }
        match self.to_bytes() {
            Ok(_) => Ok(()),
            Err(_) => bad("the report is over the size cap"),
        }
    }
}

/// Builds scrubbed reports and keeps the breadcrumb ring. Share it as an `Arc`.
pub struct CrashCollector {
    context: CrashContext,
    scrubber: Scrubber,
    breadcrumbs: Mutex<VecDeque<String>>,
}

impl CrashCollector {
    /// A collector for this host.
    #[must_use]
    pub fn new(context: CrashContext, scrubber: Scrubber) -> Self {
        Self {
            context,
            scrubber,
            breadcrumbs: Mutex::new(VecDeque::new()),
        }
    }

    fn crumbs(&self) -> MutexGuard<'_, VecDeque<String>> {
        // A poisoned ring is still a valid ring: take it rather than lose the breadcrumbs.
        self.breadcrumbs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records a log line as a breadcrumb. It is scrubbed when the report is built, not here,
    /// so recording stays cheap; it is never written anywhere before then.
    pub fn breadcrumb(&self, line: &str) {
        let mut ring = self.crumbs();
        if ring.len() >= MAX_BREADCRUMBS {
            ring.pop_front();
        }
        ring.push_back(cap(line, MAX_LINE_BYTES));
    }

    /// Builds the scrubbed report for a panic at `location` with `frames` (innermost first).
    #[must_use]
    pub fn build(&self, location: &str, frames: &[String], now: u64) -> CrashReport {
        let s = |text: &str| cap(&self.scrubber.scrub(text), MAX_LINE_BYTES);
        let mut report = CrashReport {
            format: REPORT_VERSION,
            version: s(&self.context.version),
            platform: s(&self.context.platform),
            build_id: s(&self.context.build_id),
            panic_location: s(location),
            backtrace: frames.iter().take(MAX_FRAMES).map(|f| s(f)).collect(),
            breadcrumbs: self.crumbs().iter().map(|c| s(c)).collect(),
            config_summary: self
                .context
                .config_summary
                .iter()
                .map(|(k, v)| (s(k), s(v)))
                .collect(),
            deployment_model_digest: s(&self.context.deployment_model_digest),
            occurred_at: now,
        };
        trim_to_cap(&mut report);
        report
    }

    /// Builds a report and spools it when `consent` allows. Returns the spool id, or `None`
    /// when consent is `off`.
    ///
    /// # Errors
    /// `report_too_large` or `spool_failed`.
    pub fn record(
        &self,
        consent: &ConsentState,
        spool: &Spool,
        location: &str,
        frames: &[String],
        now: u64,
    ) -> Result<Option<String>, SupportError> {
        if !consent.may_spool_crash() {
            return Ok(None);
        }
        spool.write(&self.build(location, frames, now)).map(Some)
    }
}

/// Drops the oldest breadcrumbs, then the outermost frames, until the report fits the cap.
fn trim_to_cap(report: &mut CrashReport) {
    while report.to_bytes().is_err() {
        if !report.breadcrumbs.is_empty() {
            report.breadcrumbs.remove(0);
        } else if report.backtrace.len() > 8 {
            report.backtrace.pop();
        } else if !report.config_summary.is_empty() {
            report.config_summary.clear();
        } else {
            return;
        }
    }
}

fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Installs a panic hook that records a report through `collector` and then runs the previous
/// hook. `consent` is read at panic time (so a change in settings applies at once), `now`
/// supplies the time because this crate never reads the clock, and a failure to spool is
/// swallowed: a crash handler must not panic or block the crash.
///
/// The decision is taken from [`ConsentStore::last_known`], the lock-free copy of the crash
/// switch: the hook takes no lock and allocates nothing beyond the report it may write, so a
/// panic on a thread that holds the consent lock cannot stall and, more importantly, cannot
/// read a busy lock as the first-run default and spool a report the user turned off.
pub fn install_panic_hook(
    collector: Arc<CrashCollector>,
    consent: Arc<ConsentStore>,
    spool: Arc<Spool>,
    now: fn() -> u64,
) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let trace = std::backtrace::Backtrace::force_capture().to_string();
        let frames = parse_frames(&trace);
        let _ = collector.record(&consent.last_known(), &spool, &location, &frames, now());
        previous(info);
    }));
}

/// The frame lines of a rendered backtrace: the `N: name` lines and their `at file:line` lines
/// joined, innermost first.
#[must_use]
pub fn parse_frames(trace: &str) -> Vec<String> {
    let mut frames: Vec<String> = Vec::new();
    for line in trace.lines() {
        let t = line.trim();
        if t.starts_with("at ") {
            if let Some(last) = frames.last_mut() {
                last.push(' ');
                last.push_str(t);
            }
        } else if t
            .split_once(':')
            .is_some_and(|(n, _)| n.trim().parse::<u32>().is_ok())
        {
            frames.push(
                t.split_once(':')
                    .map_or(t, |(_, rest)| rest)
                    .trim()
                    .to_owned(),
            );
        }
    }
    frames
}

/// Uploads the spooled reports the consent allows: all of them when crash reporting is `on`,
/// only the user-approved ones when it is `ask`, none when it is `off`. A report is deleted
/// from the spool once the uploader accepts it. Returns how many were sent.
///
/// # Errors
/// `spool_failed`, or `upload_failed` at the first report the uploader refuses (the rest
/// stay spooled).
pub fn upload_spooled(
    consent: &ConsentState,
    spool: &Spool,
    uploader: &dyn Uploader,
) -> Result<usize, SupportError> {
    if !consent.may_spool_crash() {
        return Ok(0);
    }
    let mut sent = 0;
    for entry in spool.list()? {
        if !(consent.may_upload_crash() || entry.approved) {
            continue;
        }
        let body = spool.view(&entry.id)?;
        uploader.upload("crash", body.as_bytes()).map_err(|_| {
            SupportError::new(codes::UPLOAD_FAILED, "the uploader reported a failure")
        })?;
        spool.delete(&entry.id)?;
        sent += 1;
    }
    Ok(sent)
}
