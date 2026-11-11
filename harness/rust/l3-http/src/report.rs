//! Case outcomes, the suite report and the JUnit-style document CI uploads.
//!
//! A case is one behaviour. It either passes, fails with the observation that proved it wrong,
//! or is **blocked** by a surface the stack does not serve yet — and a blocked case names the
//! work package that owns the gap, so the gap is visible in every run instead of being deleted.

use std::fmt::Write as _;
use std::time::Duration;

/// What a case observed instead of the behaviour it pins.
#[derive(Debug, Clone)]
pub struct Failure {
    /// One paragraph a reviewer can act on: what was expected and what came back.
    pub message: String,
}

impl Failure {
    /// A failure with the observation.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// A case's result.
pub type CaseResult = Result<(), Failure>;

/// Builds a [`Failure`] with `format!` arguments.
#[macro_export]
macro_rules! failure {
    ($($arg:tt)*) => {
        $crate::report::Failure::new(format!($($arg)*))
    };
}

/// Fails the case with a formatted message unless `condition` holds.
#[macro_export]
macro_rules! ensure {
    ($condition:expr, $($arg:tt)*) => {
        if !$condition {
            return Err($crate::report::Failure::new(format!($($arg)*)));
        }
    };
}

/// How a case ended.
#[derive(Debug, Clone)]
pub enum Status {
    /// The behaviour held.
    Passed,
    /// The stack answered something other than the pinned behaviour.
    Failed(Failure),
    /// The surface the case needs is not served yet; `owner` is the work package that adds it.
    Blocked {
        /// The task that owns the missing surface.
        owner: &'static str,
        /// What is missing, in one sentence.
        reason: &'static str,
    },
}

/// One case's outcome.
#[derive(Debug, Clone)]
pub struct Outcome {
    /// The case's name, as it appears in the report and the JUnit document.
    pub case: &'static str,
    /// How it ended.
    pub status: Status,
    /// How long the case took.
    pub duration: Duration,
    /// Whether the case belongs to the quick (pull request) subset.
    pub core: bool,
}

/// Which cases to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Every case, including the blocked ones (their gaps are reported, never hidden).
    All,
    /// The core subset the pull-request job runs: the 15 behaviours a change is most likely to
    /// break, in under two minutes.
    Quick,
}

impl Filter {
    /// The filter the environment asks for: `L3_QUICK=1` (or `true`) selects [`Filter::Quick`].
    pub fn from_env() -> Filter {
        match std::env::var("L3_QUICK").as_deref() {
            Ok("1") | Ok("true") => Filter::Quick,
            _ => Filter::All,
        }
    }
    /// Whether a case with this `core` flag runs under the filter.
    pub fn admits(self, core: bool) -> bool {
        match self {
            Filter::All => true,
            Filter::Quick => core,
        }
    }
}

/// Every case's outcome for one run.
#[derive(Debug, Default)]
pub struct Report {
    /// The outcomes, in the order the cases ran.
    pub outcomes: Vec<Outcome>,
}

impl Report {
    /// A report with no outcomes yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one outcome.
    pub fn push(&mut self, outcome: Outcome) {
        self.outcomes.push(outcome);
    }

    /// Cases that passed.
    pub fn passed(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, Status::Passed))
            .count()
    }

    /// Cases that failed.
    pub fn failed(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, Status::Failed(_)))
            .count()
    }

    /// Cases blocked by a surface the stack does not serve yet.
    pub fn blocked(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, Status::Blocked { .. }))
            .count()
    }

    /// The failure text of every failed case, one block each, for the panic message.
    pub fn failures(&self) -> String {
        let mut out = String::new();
        for outcome in &self.outcomes {
            if let Status::Failed(failure) = &outcome.status {
                let _ = writeln!(out, "--- {} ---\n{}\n", outcome.case, failure);
            }
        }
        out
    }

    /// The blocked cases and their owners, one line each.
    pub fn blocked_summary(&self) -> String {
        let mut out = String::new();
        for outcome in &self.outcomes {
            if let Status::Blocked { owner, reason } = &outcome.status {
                let _ = writeln!(out, "  {} -> {owner}: {reason}", outcome.case);
            }
        }
        out
    }

    /// One line per case, then a total: what a person watching the run reads.
    pub fn summary(&self) -> String {
        let mut out = String::new();
        for outcome in &self.outcomes {
            let mark = match &outcome.status {
                Status::Passed => "ok  ",
                Status::Failed(_) => "FAIL",
                Status::Blocked { .. } => "gap ",
            };
            let _ = writeln!(
                out,
                "{mark} {:>6.2}s  {}{}",
                outcome.duration.as_secs_f64(),
                outcome.case,
                match &outcome.status {
                    Status::Blocked { owner, .. } => format!("  [{owner}]"),
                    _ => String::new(),
                }
            );
        }
        let _ = writeln!(
            out,
            "\n{} cases: {} passed, {} failed, {} blocked",
            self.outcomes.len(),
            self.passed(),
            self.failed(),
            self.blocked()
        );
        out
    }

    /// Asserts the suite is green: no failures. Blocked cases do not fail the run — they are
    /// reported and named, and the README explains why they may not be deleted.
    pub fn assert_green(&self, what: &str) {
        if self.failed() == 0 {
            return;
        }
        panic!(
            "{what}: {} of {} cases failed\n\n{}\nblocked cases (not failures):\n{}",
            self.failed(),
            self.outcomes.len(),
            self.failures(),
            self.blocked_summary()
        );
    }

    /// The JUnit-style XML document CI uploads on failure (`junit.xml`).
    pub fn junit(&self) -> String {
        let mut out = String::new();
        let time: f64 = self.outcomes.iter().map(|o| o.duration.as_secs_f64()).sum();
        let _ = writeln!(out, "<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
        let _ = writeln!(
            out,
            "<testsuite name=\"l3-http\" tests=\"{}\" failures=\"{}\" skipped=\"{}\" time=\"{time:.3}\">",
            self.outcomes.len(),
            self.failed(),
            self.blocked()
        );
        for outcome in &self.outcomes {
            let name = escape(outcome.case);
            match &outcome.status {
                Status::Passed => {
                    let _ = writeln!(
                        out,
                        "  <testcase classname=\"l3-http\" name=\"{name}\" time=\"{:.3}\"/>",
                        outcome.duration.as_secs_f64()
                    );
                }
                Status::Failed(failure) => {
                    let _ = writeln!(
                        out,
                        "  <testcase classname=\"l3-http\" name=\"{name}\" time=\"{:.3}\">",
                        outcome.duration.as_secs_f64()
                    );
                    let _ = writeln!(
                        out,
                        "    <failure message=\"{}\">{}</failure>",
                        escape(first_line(&failure.message)),
                        escape(&failure.message)
                    );
                    let _ = writeln!(out, "  </testcase>");
                }
                Status::Blocked { owner, reason } => {
                    let _ = writeln!(
                        out,
                        "  <testcase classname=\"l3-http\" name=\"{name}\" time=\"{:.3}\">",
                        outcome.duration.as_secs_f64()
                    );
                    let _ = writeln!(
                        out,
                        "    <skipped message=\"{}\"/>",
                        escape(&format!("{owner}: {reason}"))
                    );
                    let _ = writeln!(out, "  </testcase>");
                }
            }
        }
        let _ = writeln!(out, "</testsuite>");
        out
    }

    /// Writes [`Report::junit`] to `path` when `L3_JUNIT` names a file; prints the summary.
    pub fn finish(&self, what: &str) {
        println!("{}", self.summary());
        let blocked = self.blocked_summary();
        if !blocked.is_empty() {
            println!("blocked behaviours (named, not hidden):\n{blocked}");
        }
        if let Ok(path) = std::env::var("L3_JUNIT")
            && let Err(error) = std::fs::write(&path, self.junit())
        {
            eprintln!("could not write the JUnit document to {path}: {error}");
        }
        self.assert_green(what);
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or_default()
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\n', "&#10;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(case: &'static str, status: Status, core: bool) -> Outcome {
        Outcome {
            case,
            status,
            duration: Duration::from_millis(250),
            core,
        }
    }

    fn sample() -> Report {
        let mut report = Report::new();
        report.push(outcome("passes", Status::Passed, true));
        report.push(outcome(
            "fails",
            Status::Failed(Failure::new("expected 201, got 403 <denied> & \"quoted\"")),
            false,
        ));
        report.push(outcome(
            "blocked",
            Status::Blocked {
                owner: "WP-PLT-08.T2",
                reason: "the realtime route is not mounted",
            },
            false,
        ));
        report
    }

    #[test]
    fn the_counts_separate_passed_failed_and_blocked() {
        let report = sample();
        assert_eq!(
            (report.passed(), report.failed(), report.blocked()),
            (1, 1, 1)
        );
    }

    #[test]
    fn the_failure_text_names_the_case_and_the_observation() {
        let text = sample().failures();
        assert!(text.contains("--- fails ---"), "{text}");
        assert!(text.contains("expected 201, got 403"), "{text}");
        assert!(!text.contains("--- passes ---"), "{text}");
    }

    #[test]
    fn a_green_report_does_not_panic_and_a_red_one_does() {
        let mut green = Report::new();
        green.push(outcome("passes", Status::Passed, true));
        green.assert_green("the suite");
        let red = std::panic::catch_unwind(|| sample().assert_green("the suite"));
        assert!(red.is_err(), "a failing case must fail the run");
    }

    #[test]
    fn the_junit_document_carries_every_case_and_escapes_its_text() {
        let xml = sample().junit();
        assert!(
            xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"),
            "{xml}"
        );
        assert!(
            xml.contains("<testsuite name=\"l3-http\" tests=\"3\" failures=\"1\" skipped=\"1\""),
            "{xml}"
        );
        assert!(xml.contains("name=\"passes\""), "{xml}");
        assert!(xml.contains("<failure message="), "{xml}");
        assert!(
            xml.contains("&lt;denied&gt; &amp; &quot;quoted&quot;"),
            "{xml}"
        );
        assert!(
            xml.contains("<skipped message=\"WP-PLT-08.T2: the realtime route is not mounted\"/>"),
            "{xml}"
        );
        assert!(xml.trim_end().ends_with("</testsuite>"), "{xml}");
    }

    #[test]
    fn the_source_summary_lists_the_blocked_cases_with_their_owner() {
        let blocked = sample().blocked_summary();
        assert!(blocked.contains("blocked -> WP-PLT-08.T2"), "{blocked}");
        let summary = sample().summary();
        assert!(summary.contains("FAIL"), "{summary}");
        assert!(summary.contains("fails"), "{summary}");
        assert!(summary.contains("blocked  [WP-PLT-08.T2]"), "{summary}");
        assert!(
            summary.contains("3 cases: 1 passed, 1 failed, 1 blocked"),
            "{summary}"
        );
    }

    #[test]
    fn the_quick_filter_admits_only_core_cases() {
        assert!(Filter::All.admits(true));
        assert!(Filter::All.admits(false));
        assert!(Filter::Quick.admits(true));
        assert!(!Filter::Quick.admits(false));
    }

    #[test]
    fn only_the_first_line_of_a_failure_is_the_junit_message() {
        let failure = Failure::new("first line\nsecond line");
        let mut report = Report::new();
        report.push(outcome("multi", Status::Failed(failure), false));
        let xml = report.junit();
        assert!(xml.contains("message=\"first line\""), "{xml}");
        assert!(xml.contains("first line&#10;second line"), "{xml}");
    }
}
