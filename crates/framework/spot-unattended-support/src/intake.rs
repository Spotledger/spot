//! The vendor side of crash reporting: dedupe, group, rate-limit and file.
//!
//! [`CrashIntake::ingest`] gives each report a [`signature`] (panic location, version and a
//! hash of the top frames, with addresses and line noise stripped). The first report of a
//! signature becomes a [`WorkItem`] filed through the [`Sink`]; later ones only raise its
//! count, and the sink is updated at most once per `min_update_secs` per item (call
//! [`CrashIntake::flush`] to push the rest). New work items are rate-limited per window, so a
//! crash storm cannot flood the pipeline; limited reports are counted, not lost.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::crash::CrashReport;
use crate::error::{SupportError, codes};

/// How many top frames go into the signature.
pub const SIGNATURE_FRAMES: usize = 5;

/// A group of equal crashes, as filed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    /// The signature (hex).
    pub signature: String,
    /// A short title for the work item.
    pub title: String,
    /// How many reports share the signature.
    pub count: u64,
    /// When the first one arrived.
    pub first_seen: u64,
    /// When the latest one arrived.
    pub last_seen: u64,
    /// The versions that crashed, sorted.
    pub versions: Vec<String>,
    /// Where the symbols for the build can be found, when the lookup hook knew.
    pub symbols: Option<String>,
    /// The first report of the group, as the evidence.
    pub sample: CrashReport,
}

/// Where work items go.
pub trait Sink {
    /// Files a new item or updates the filed one (same `signature`) with its new count.
    ///
    /// # Errors
    /// `sink_failed`.
    fn upsert(&mut self, item: &WorkItem) -> Result<(), SupportError>;
}

/// The default sink: one JSON file per signature in a folder the pipeline reads.
#[derive(Clone, Debug)]
pub struct JsonFileSink {
    dir: PathBuf,
}

impl JsonFileSink {
    /// A sink writing into `dir`, created on first use.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

impl Sink for JsonFileSink {
    fn upsert(&mut self, item: &WorkItem) -> Result<(), SupportError> {
        let fail = |what: &str| SupportError::new(codes::SINK_FAILED, what.to_owned());
        let text = serde_json::to_string_pretty(item).map_err(|_| fail("serialise"))?;
        fs::create_dir_all(&self.dir).map_err(|_| fail("create the folder"))?;
        fs::write(self.dir.join(format!("{}.json", item.signature)), text)
            .map_err(|_| fail("write the file"))
    }
}

/// Finds symbols for a build id (the "symbol/build-id lookup hook").
pub trait SymbolLookup {
    /// A locator for the build's symbols, if known.
    fn lookup(&self, build_id: &str) -> Option<String>;
}

/// No symbol lookup.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSymbols;

impl SymbolLookup for NoSymbols {
    fn lookup(&self, _build_id: &str) -> Option<String> {
        None
    }
}

/// Limits for the intake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntakeConfig {
    /// The rate-limit window, seconds.
    pub window_secs: u64,
    /// The most new work items filed in one window.
    pub max_new_per_window: u32,
    /// The least time between sink updates of one item, seconds.
    pub min_update_secs: u64,
    /// The most groups held in memory. At the limit a new signature evicts the oldest group
    /// that was never filed; when every group is filed the new one is refused. Both are
    /// counted in [`IntakeStats`], never silent.
    pub max_groups: usize,
}

impl Default for IntakeConfig {
    fn default() -> Self {
        Self {
            window_secs: 3_600,
            max_new_per_window: 20,
            min_update_secs: 600,
            max_groups: 2_000,
        }
    }
}

/// What happened to one report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntakeOutcome {
    /// A new group was filed.
    Filed {
        /// The group's signature.
        signature: String,
    },
    /// The report joined an existing group.
    Grouped {
        /// The group's signature.
        signature: String,
        /// The group's count after this report.
        count: u64,
    },
    /// A new group was held back by the rate limit; it is filed by a later `ingest` or `flush`.
    RateLimited {
        /// The group's signature.
        signature: String,
    },
    /// A new group was refused because `max_groups` groups are held and all are filed.
    Refused {
        /// The signature that was refused.
        signature: String,
    },
}

/// What the intake dropped, so nothing is lost silently.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IntakeStats {
    /// Reports refused by [`CrashReport::validate`].
    pub invalid: u64,
    /// Never-filed groups evicted to make room for a new signature.
    pub evicted: u64,
    /// Reports of new signatures refused because every held group is filed.
    pub refused: u64,
}

struct Group {
    item: WorkItem,
    filed: bool,
    last_pushed: u64,
    dirty: bool,
}

/// The intake. One per vendor-side service; not `Sync`, wrap it in a mutex to share.
pub struct CrashIntake<S: Sink, L: SymbolLookup = NoSymbols> {
    sink: S,
    symbols: L,
    config: IntakeConfig,
    groups: BTreeMap<String, Group>,
    window_start: u64,
    new_in_window: u32,
    stats: IntakeStats,
}

impl<S: Sink> CrashIntake<S, NoSymbols> {
    /// An intake with no symbol lookup.
    #[must_use]
    pub fn new(sink: S, config: IntakeConfig) -> Self {
        Self::with_symbols(sink, NoSymbols, config)
    }
}

impl<S: Sink, L: SymbolLookup> CrashIntake<S, L> {
    /// An intake with a symbol lookup hook.
    #[must_use]
    pub fn with_symbols(sink: S, symbols: L, config: IntakeConfig) -> Self {
        Self {
            sink,
            symbols,
            config,
            groups: BTreeMap::new(),
            window_start: 0,
            new_in_window: 0,
            stats: IntakeStats::default(),
        }
    }

    /// What has been refused or evicted so far.
    #[must_use]
    pub fn stats(&self) -> IntakeStats {
        self.stats
    }

    /// Takes one report at `now`. The report is untrusted: it is validated first, and the
    /// number of groups held is bounded by `max_groups`.
    ///
    /// # Errors
    /// `report_invalid` when the report breaks the size or shape rules (counted in the
    /// stats); `sink_failed` when the sink refuses, in which case the group is kept and
    /// retried by `flush`.
    pub fn ingest(
        &mut self,
        report: &CrashReport,
        now: u64,
    ) -> Result<IntakeOutcome, SupportError> {
        if let Err(e) = report.validate() {
            self.stats.invalid += 1;
            return Err(e);
        }
        let signature = signature(report);
        if let Some(group) = self.groups.get_mut(&signature) {
            group.item.count = group.item.count.saturating_add(1);
            group.item.last_seen = now;
            if !group.item.versions.contains(&report.version) {
                group.item.versions.push(report.version.clone());
                group.item.versions.sort();
            }
            group.dirty = true;
            let count = group.item.count;
            if group.filed {
                if now.saturating_sub(group.last_pushed) >= self.config.min_update_secs {
                    Self::push(&mut self.sink, group, now)?;
                }
                return Ok(IntakeOutcome::Grouped { signature, count });
            }
            return self.try_file(&signature, now);
        }
        if self.groups.len() >= self.config.max_groups.max(1) && !self.evict_oldest_unfiled() {
            self.stats.refused += 1;
            return Ok(IntakeOutcome::Refused { signature });
        }
        let item = WorkItem {
            signature: signature.clone(),
            title: format!("Crash at {} in {}", report.panic_location, report.version),
            count: 1,
            first_seen: now,
            last_seen: now,
            versions: vec![report.version.clone()],
            symbols: self.symbols.lookup(&report.build_id),
            sample: report.clone(),
        };
        self.groups.insert(
            signature.clone(),
            Group {
                item,
                filed: false,
                last_pushed: 0,
                dirty: true,
            },
        );
        self.try_file(&signature, now)
    }

    /// Drops the unfiled group seen first; false when every group is filed.
    fn evict_oldest_unfiled(&mut self) -> bool {
        let oldest = self
            .groups
            .iter()
            .filter(|(_, g)| !g.filed)
            .min_by_key(|(sig, g)| (g.item.first_seen, (*sig).clone()))
            .map(|(sig, _)| sig.clone());
        match oldest {
            Some(sig) => {
                self.groups.remove(&sig);
                self.stats.evicted += 1;
                true
            }
            None => false,
        }
    }

    fn try_file(&mut self, signature: &str, now: u64) -> Result<IntakeOutcome, SupportError> {
        if now.saturating_sub(self.window_start) >= self.config.window_secs {
            self.window_start = now;
            self.new_in_window = 0;
        }
        let signature = signature.to_owned();
        if self.new_in_window >= self.config.max_new_per_window {
            return Ok(IntakeOutcome::RateLimited { signature });
        }
        let Some(group) = self.groups.get_mut(&signature) else {
            return Ok(IntakeOutcome::RateLimited { signature });
        };
        Self::push(&mut self.sink, group, now)?;
        group.filed = true;
        self.new_in_window += 1;
        Ok(IntakeOutcome::Filed { signature })
    }

    fn push(sink: &mut S, group: &mut Group, now: u64) -> Result<(), SupportError> {
        sink.upsert(&group.item)?;
        group.last_pushed = now;
        group.dirty = false;
        Ok(())
    }

    /// Pushes every filed group whose count changed since its last push, and files held-back
    /// groups the rate limit now allows. Returns how many sink calls were made.
    ///
    /// # Errors
    /// `sink_failed` at the first failure; the rest are retried by the next flush.
    pub fn flush(&mut self, now: u64) -> Result<usize, SupportError> {
        let pending: Vec<String> = self
            .groups
            .iter()
            .filter(|(_, g)| g.dirty)
            .map(|(k, _)| k.clone())
            .collect();
        let mut calls = 0;
        for signature in pending {
            let filed = self.groups.get(&signature).is_some_and(|g| g.filed);
            if filed {
                if let Some(group) = self.groups.get_mut(&signature) {
                    Self::push(&mut self.sink, group, now)?;
                    calls += 1;
                }
            } else if let IntakeOutcome::Filed { .. } = self.try_file(&signature, now)? {
                calls += 1;
            }
        }
        Ok(calls)
    }

    /// The current groups, sorted by signature.
    #[must_use]
    pub fn items(&self) -> Vec<&WorkItem> {
        self.groups.values().map(|g| &g.item).collect()
    }

    /// The sink, for inspection.
    #[must_use]
    pub fn sink(&self) -> &S {
        &self.sink
    }
}

/// The signature of a report: panic location, version and the top frames, normalised so the
/// same crash in two builds of one version, or at two addresses, groups together.
#[must_use]
pub fn signature(report: &CrashReport) -> String {
    let mut hasher = Sha256::new();
    hasher.update(report.panic_location.as_bytes());
    hasher.update([0]);
    hasher.update(report.version.as_bytes());
    for frame in report.backtrace.iter().take(SIGNATURE_FRAMES) {
        hasher.update([0]);
        hasher.update(normalise_frame(frame).as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Drops hex addresses and the trailing `::h<hash>` a symbol carries.
fn normalise_frame(frame: &str) -> String {
    frame
        .split_whitespace()
        .filter(|w| !w.starts_with("0x"))
        .map(|w| match w.rfind("::h") {
            Some(at)
                if w[at + 3..].len() == 16
                    && w[at + 3..].bytes().all(|b| b.is_ascii_hexdigit()) =>
            {
                &w[..at]
            }
            _ => w,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A GitHub issue sink over a host-supplied HTTP client.
#[cfg(feature = "github")]
pub mod github {
    use std::collections::BTreeMap;

    use super::{Sink, WorkItem};
    use crate::error::{SupportError, codes};

    /// POSTs a JSON body to a path on the GitHub REST API and returns the status and body. The
    /// host owns the HTTP client, the credential and the egress policy.
    pub trait HttpPost {
        /// Posts `body` to `path` (for example `/repos/<owner>/<repo>/issues`).
        ///
        /// # Errors
        /// A transport failure.
        fn post(&mut self, path: &str, body: &str) -> Result<(u16, String), String>;
    }

    /// Files each group as an issue and later counts as comments on that issue.
    pub struct GithubSink<H: HttpPost> {
        repo: String,
        http: H,
        issues: BTreeMap<String, u64>,
    }

    impl<H: HttpPost> GithubSink<H> {
        /// A sink for `repo` (`owner/name`).
        #[must_use]
        pub fn new(repo: impl Into<String>, http: H) -> Self {
            Self {
                repo: repo.into(),
                http,
                issues: BTreeMap::new(),
            }
        }
    }

    /// The longest title the sink writes.
    pub const MAX_TITLE: usize = 120;

    /// A title with everything that markdown or GitHub would act on removed: only letters,
    /// digits, spaces and a few plain punctuation marks survive, so a sender cannot put a
    /// link, a mention, an image, a reference or a line break in it.
    #[must_use]
    pub fn safe_title(text: &str) -> String {
        let cleaned: String = text
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric()
                    || matches!(c, ' ' | '.' | ':' | '_' | '-' | '/' | '(' | ')')
                {
                    c
                } else {
                    '_'
                }
            })
            .take(MAX_TITLE)
            .collect();
        cleaned.replace("@", "_")
    }

    /// A code fence longer than any run of backticks in `text`, so `text` cannot close it.
    #[must_use]
    pub fn fence_for(text: &str) -> String {
        let mut longest = 0;
        let mut run = 0;
        for c in text.chars() {
            if c == '`' {
                run += 1;
                longest = longest.max(run);
            } else {
                run = 0;
            }
        }
        "`".repeat((longest + 1).max(3))
    }

    fn failed(what: &str) -> SupportError {
        SupportError::new(codes::SINK_FAILED, what.to_owned())
    }

    impl<H: HttpPost> Sink for GithubSink<H> {
        fn upsert(&mut self, item: &WorkItem) -> Result<(), SupportError> {
            if let Some(number) = self.issues.get(&item.signature) {
                let path = format!("/repos/{}/issues/{number}/comments", self.repo);
                let body = serde_json::json!({ "body": format!("Seen {} times, last at {}.", item.count, item.last_seen) });
                let (status, _) = self
                    .http
                    .post(&path, &body.to_string())
                    .map_err(|_| failed("transport"))?;
                return if status < 300 {
                    Ok(())
                } else {
                    Err(failed("the comment was refused"))
                };
            }
            let text =
                serde_json::to_string_pretty(&item.sample).map_err(|_| failed("serialise"))?;
            let fence = fence_for(&text);
            let body = serde_json::json!({
                "title": safe_title(&item.title),
                "body": format!("Signature {}\n\n{fence}json\n{text}\n{fence}", item.signature),
                "labels": ["crash-report"],
            });
            let path = format!("/repos/{}/issues", self.repo);
            let (status, reply) = self
                .http
                .post(&path, &body.to_string())
                .map_err(|_| failed("transport"))?;
            if status >= 300 {
                return Err(failed("the issue was refused"));
            }
            let number = serde_json::from_str::<serde_json::Value>(&reply)
                .ok()
                .and_then(|v| v.get("number").and_then(serde_json::Value::as_u64))
                .ok_or_else(|| failed("the reply has no issue number"))?;
            self.issues.insert(item.signature.clone(), number);
            Ok(())
        }
    }
}
