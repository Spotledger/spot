//! The review report: the one page a human reviewer reads, in JSON and in markdown.

use serde::{Deserialize, Serialize};
use spot_manifest::AppManifest;

use crate::finding::{Finding, Severity, SeverityCounts};

/// What the review concluded. There is deliberately no `Approved`: the review produces
/// evidence, a person decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// A blocker was found; the bundle cannot be accepted as it stands.
    Rejected,
    /// A person must read the findings before the app is accepted.
    NeedsHumanReview,
    /// No finding above `Info`. Still for a person to accept.
    LooksClean,
}

impl Verdict {
    /// The one plain sentence the markdown page opens with.
    pub fn sentence(self) -> &'static str {
        match self {
            Self::Rejected => {
                "This bundle is rejected: a blocker means it cannot be accepted as it stands."
            }
            Self::NeedsHumanReview => {
                "This bundle needs a human decision: it carries findings a person must read. \
                 The review never approves an app."
            }
            Self::LooksClean => {
                "No finding above Info. The review never approves an app; a person decides \
                 whether to accept it."
            }
        }
    }
}

/// How one profiled call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    /// The export returned normally.
    Ok,
    /// `wasm.out_of_fuel`.
    OutOfFuel,
    /// `wasm.timeout`.
    Timeout,
    /// `wasm.memory`.
    Memory,
    /// `wasm.too_many_host_calls`.
    TooManyHostCalls,
    /// `wasm.output_too_large`.
    OutputTooLarge,
    /// `wasm.trap`.
    Trap,
    /// `wasm.missing_export`.
    MissingExport,
    /// `wasm.capability_denied`.
    CapabilityDenied,
    /// `wasm.app_error`: the app's own error value.
    AppError,
    /// `wasm.bad_arguments`.
    BadArguments,
    /// `wasm.bad_component`.
    BadComponent,
    /// `wasm.internal`.
    Internal,
}

impl CallOutcome {
    /// The outcome for a `spot-wasm` error code, as [`spot_wasm::WasmError::code`] spells it.
    pub fn from_code(code: &str) -> Self {
        match code {
            "wasm.out_of_fuel" => Self::OutOfFuel,
            "wasm.timeout" => Self::Timeout,
            "wasm.memory" => Self::Memory,
            "wasm.too_many_host_calls" => Self::TooManyHostCalls,
            "wasm.output_too_large" => Self::OutputTooLarge,
            "wasm.trap" => Self::Trap,
            "wasm.missing_export" => Self::MissingExport,
            "wasm.capability_denied" => Self::CapabilityDenied,
            "wasm.app_error" => Self::AppError,
            "wasm.bad_arguments" => Self::BadArguments,
            "wasm.bad_component" => Self::BadComponent,
            _ => Self::Internal,
        }
    }
}

/// One call the resource profile made and what it observed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallRecord {
    /// The export that was called.
    pub entry: String,
    /// How the call ended.
    pub outcome: CallOutcome,
    /// The bytes of the returned JSON, as the review measured it.
    pub output_bytes: usize,
    /// The host calls the review's own `Imports` wrapper counted.
    pub host_calls: u32,
    /// The wall time between the profile's marks, in microseconds.
    pub wall_micros: u128,
}

/// The manifest as the report shows it: identity and the declared capabilities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestSummary {
    /// The app name.
    pub app_id: String,
    /// The app version.
    pub version: String,
    /// The human title, empty when the manifest carries none.
    pub title: String,
    /// The host range the app requires, when it states one.
    pub requires_host: Option<String>,
    /// Declared `host:port` endpoints, sorted.
    pub network: Vec<String>,
    /// Whether the e-mail capability is declared.
    pub email: bool,
    /// The file access word: `none`, `read` or `read_write`.
    pub files: String,
    /// DocTypes the app may read, sorted.
    pub doctypes_read: Vec<String>,
    /// DocTypes the app may write, sorted.
    pub doctypes_write: Vec<String>,
    /// How many patches the manifest declares.
    pub patch_count: usize,
    /// How many assets the manifest declares.
    pub asset_count: usize,
}

impl ManifestSummary {
    /// Summarises a manifest the way the report shows it.
    pub fn from_manifest(manifest: &AppManifest) -> Self {
        let caps = &manifest.capabilities;
        let mut network: Vec<String> = caps
            .network
            .iter()
            .map(|endpoint| format!("{}:{}", endpoint.host, endpoint.port))
            .collect();
        network.sort();
        let mut doctypes_read = caps.doctypes.read.clone();
        doctypes_read.sort();
        let mut doctypes_write = caps.doctypes.write.clone();
        doctypes_write.sort();
        Self {
            app_id: manifest.app.name.as_str().to_owned(),
            version: manifest.app.version.to_string(),
            title: manifest.app.title.clone(),
            requires_host: manifest
                .app
                .requires_host
                .as_ref()
                .map(std::string::ToString::to_string),
            network,
            email: caps.email,
            files: file_word(caps.files),
            doctypes_read,
            doctypes_write,
            patch_count: manifest.patches.len(),
            asset_count: manifest.assets.len(),
        }
    }
}

/// The manifest's word for a file access level.
fn file_word(files: spot_manifest::FileAccess) -> String {
    match files {
        spot_manifest::FileAccess::None => "none",
        spot_manifest::FileAccess::Read => "read",
        spot_manifest::FileAccess::ReadWrite => "read_write",
    }
    .to_owned()
}

/// The full review report.
///
/// Fields serialise in this order, so two runs over the same bytes and configuration produce
/// byte-identical JSON apart from [`Report::generated_at`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The caller's timestamp; the review never reads a clock.
    pub generated_at: String,
    /// The verdict.
    pub verdict: Verdict,
    /// The canonical bundle digest in hex, when the bundle opened.
    pub digest: Option<String>,
    /// The manifest summary, when the bundle opened.
    pub manifest: Option<ManifestSummary>,
    /// How many findings of each severity.
    pub counts: SeverityCounts,
    /// The resource profile's per-call table.
    pub resources: Vec<CallRecord>,
    /// Every finding, most serious first.
    pub findings: Vec<Finding>,
}

impl Report {
    /// Assembles a report: sorts the findings by severity, counts them and decides the
    /// verdict.
    pub(crate) fn assemble(
        mut findings: Vec<Finding>,
        resources: Vec<CallRecord>,
        manifest: Option<ManifestSummary>,
        digest: Option<String>,
        generated_at: &str,
    ) -> Self {
        findings.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then_with(|| a.rule.cmp(&b.rule))
                .then_with(|| a.where_.cmp(&b.where_))
                .then_with(|| a.message.cmp(&b.message))
        });
        let mut counts = SeverityCounts::default();
        for finding in &findings {
            counts.add(finding.severity);
        }
        let verdict = if counts.blocker > 0 {
            Verdict::Rejected
        } else if counts.high > 0 {
            Verdict::NeedsHumanReview
        } else {
            Verdict::LooksClean
        };
        Self {
            generated_at: generated_at.to_owned(),
            verdict,
            digest,
            manifest,
            counts,
            resources,
            findings,
        }
    }

    /// The report as one JSON line. The shape has no non-string map key, so this cannot fail;
    /// a serialisation fault would be a bug, and is reported as a sentence, not JSON.
    pub fn to_json(&self) -> String {
        match serde_json::to_string(self) {
            Ok(text) => text,
            Err(err) => format!("the review report could not be serialised: {err}"),
        }
    }

    /// The page a human reviewer reads: the verdict sentence, the counts, then every finding
    /// sorted by severity and the resource table.
    pub fn to_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str("# App review\n\n");
        out.push_str(self.verdict.sentence());
        out.push_str("\n\n");
        out.push_str(&format!("- Verdict: {:?}\n", self.verdict));
        out.push_str(&format!("- Generated at: {}\n", self.generated_at));
        if let Some(digest) = &self.digest {
            out.push_str(&format!("- Bundle digest: {digest}\n"));
        }
        out.push_str(&format!(
            "- Findings: {} ({} blocker, {} high, {} medium, {} low, {} info)\n",
            self.counts.total(),
            self.counts.blocker,
            self.counts.high,
            self.counts.medium,
            self.counts.low,
            self.counts.info
        ));
        if let Some(manifest) = &self.manifest {
            out.push('\n');
            out.push_str("## Manifest\n\n");
            out.push_str(&format!(
                "- App: {} {}\n",
                manifest.app_id, manifest.version
            ));
            if !manifest.title.is_empty() {
                out.push_str(&format!("- Title: {}\n", manifest.title));
            }
            out.push_str(&format!(
                "- Requires host: {}\n",
                manifest.requires_host.as_deref().unwrap_or("(any)")
            ));
            out.push_str(&format!(
                "- Capabilities: network {:?}, email {}, files {}, doctypes read {:?}, write {:?}\n",
                manifest.network,
                manifest.email,
                manifest.files,
                manifest.doctypes_read,
                manifest.doctypes_write
            ));
        }
        out.push_str("\n## Findings\n\n");
        if self.findings.is_empty() {
            out.push_str("No findings.\n");
        }
        for finding in &self.findings {
            out.push_str(&format!(
                "### {} — {}\n\n",
                titlecase(finding.severity),
                finding.rule
            ));
            if let Some(where_) = &finding.where_ {
                out.push_str(&format!("- Where: {where_}\n"));
            }
            out.push_str(&format!("- Risk: {}\n", finding.risk_id));
            out.push_str(&format!("- {}\n\n", finding.message));
        }
        out.push_str("\n## Resource profile\n\n");
        if self.resources.is_empty() {
            out.push_str("No calls were made.\n");
        } else {
            out.push_str("| Entry | Outcome | Output bytes | Host calls | Wall µs |\n");
            out.push_str("| --- | --- | --- | --- | --- |\n");
            for record in &self.resources {
                out.push_str(&format!(
                    "| {} | {:?} | {} | {} | {} |\n",
                    record.entry,
                    record.outcome,
                    record.output_bytes,
                    record.host_calls,
                    record.wall_micros
                ));
            }
        }
        out
    }
}

/// The heading word for a severity: `Info`, `Low`, `Medium`, `High`, `Blocker`.
fn titlecase(severity: Severity) -> &'static str {
    match severity {
        Severity::Info => "Info",
        Severity::Low => "Low",
        Severity::Medium => "Medium",
        Severity::High => "High",
        Severity::Blocker => "Blocker",
    }
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn finding(severity: Severity, rule: &str) -> Finding {
        Finding::new(rule, severity, None, "message", "R-023")
    }

    #[test]
    fn a_blocker_rejects_and_a_high_needs_review() {
        let report = Report::assemble(
            vec![finding(Severity::Blocker, "bundle.bad_archive")],
            Vec::new(),
            None,
            None,
            "t",
        );
        assert_eq!(report.verdict, Verdict::Rejected);

        let report = Report::assemble(
            vec![finding(Severity::High, "app_review.scan.endpoint.private")],
            Vec::new(),
            None,
            None,
            "t",
        );
        assert_eq!(report.verdict, Verdict::NeedsHumanReview);
    }

    #[test]
    fn medium_and_below_stay_looks_clean() {
        let report = Report::assemble(
            vec![
                finding(Severity::Medium, "app_review.scan.files.read_write_unused"),
                finding(Severity::Low, "app_review.static.title_missing"),
            ],
            Vec::new(),
            None,
            None,
            "t",
        );
        assert_eq!(report.verdict, Verdict::LooksClean);
        assert_eq!(report.counts.medium, 1);
        assert_eq!(report.counts.low, 1);
    }

    #[test]
    fn findings_sort_most_serious_first() {
        let report = Report::assemble(
            vec![
                finding(Severity::Low, "app_review.static.title_missing"),
                finding(Severity::Blocker, "bundle.bad_archive"),
                finding(Severity::High, "app_review.scan.endpoint.private"),
            ],
            Vec::new(),
            None,
            None,
            "t",
        );
        let severities: Vec<Severity> = report.findings.iter().map(|f| f.severity).collect();
        assert_eq!(
            severities,
            vec![Severity::Blocker, Severity::High, Severity::Low]
        );
    }

    #[test]
    fn the_markdown_never_claims_safety() {
        let report = Report::assemble(Vec::new(), Vec::new(), None, None, "2026-01-01T00:00:00Z");
        let page = report.to_markdown();
        assert!(page.contains("a person decides"));
        assert!(!page.contains("Approved"));
        assert!(!page.contains("safe"));
    }

    #[test]
    fn call_outcome_maps_every_limit_code() {
        assert_eq!(
            CallOutcome::from_code("wasm.out_of_fuel"),
            CallOutcome::OutOfFuel
        );
        assert_eq!(CallOutcome::from_code("wasm.memory"), CallOutcome::Memory);
        assert_eq!(
            CallOutcome::from_code("wasm.too_many_host_calls"),
            CallOutcome::TooManyHostCalls
        );
        assert_eq!(CallOutcome::from_code("wasm.timeout"), CallOutcome::Timeout);
        assert_eq!(CallOutcome::from_code("wasm.trap"), CallOutcome::Trap);
        assert_eq!(
            CallOutcome::from_code("wasm.output_too_large"),
            CallOutcome::OutputTooLarge
        );
        assert_eq!(
            CallOutcome::from_code("wasm.app_error"),
            CallOutcome::AppError
        );
    }
}
