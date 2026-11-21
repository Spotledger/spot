//! What one scenario produced, and what the whole run produced.

/// The result of one scenario.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The scenario's name.
    pub scenario: &'static str,
    /// Whether every assertion held.
    pub passed: bool,
    /// What went wrong, when it did not.
    pub failure: Option<String>,
    /// The behaviours this scenario pinned beyond pass/fail: probe participations (a host
    /// without a layer records "not supported"), recorded wall times, and notes about
    /// behaviours it could not pin on this host (with the reason). Notes never turn a failure
    /// into a pass; they are the report's "it says so" trail.
    pub notes: Vec<String>,
}

/// The whole run's results.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// One per scenario, in suite order.
    pub outcomes: Vec<Outcome>,
}

impl Report {
    /// The failed scenarios.
    #[must_use]
    pub fn failures(&self) -> Vec<&Outcome> {
        self.outcomes.iter().filter(|o| !o.passed).collect()
    }

    /// Panics with every failure listed, unless all scenarios passed.
    pub fn assert_all_passed(&self) {
        let failed = self.failures();
        if !failed.is_empty() {
            let lines: Vec<String> = failed
                .iter()
                .map(|o| {
                    format!(
                        "  {}: {}",
                        o.scenario,
                        o.failure.as_deref().unwrap_or("failed")
                    )
                })
                .collect();
            panic!(
                "{} of {} liveness scenarios failed:\n{}",
                failed.len(),
                self.outcomes.len(),
                lines.join("\n")
            );
        }
    }
}
