//! The replay engine: sample the journal, re-execute every sampled operation on the clone with
//! the original instant and principal while every external effect stays held, and diff each
//! outcome against what the old release recorded.

use std::sync::Arc;
use std::time::Duration;

use spot_app_api::prelude::{Clock, OffsetDateTime, SiteId};

use crate::budget::Budget;
use crate::clone::CloneRef;
use crate::declaration::{DeclaredBehaviour, NoDeclarations};
use crate::divergence::{Divergence, DivergenceClass, diff};
use crate::error::{Error, Result};
use crate::hold::EffectHold;
use crate::journal::{Cursor, ReplaySource};
use crate::policy::{ReplayPolicy, select};
use crate::report::{Coverage, ReplayReport};
use crate::runner::{ReplayRequest, ReplayRunner};

/// The page size a replay reads the journal in when the caller does not say otherwise.
pub const DEFAULT_PAGE_SIZE: usize = 500;

/// The most candidates a replay reads before it stops reading, when the caller does not say
/// otherwise. A safety bound on memory, far above any window a site sets.
pub const DEFAULT_MAX_CANDIDATES: usize = 100_000;

/// The behavioural replay engine. It owns the journal reader, the runner, the clone, the policy,
/// the budget, the clock, the effect hold and the declared behaviour;
/// [`run`](Replay::run) puts them together.
pub struct Replay {
    source: Arc<dyn ReplaySource>,
    runner: Arc<dyn ReplayRunner>,
    clone: CloneRef,
    site: SiteId,
    policy: ReplayPolicy,
    reference_at: OffsetDateTime,
    budget: Budget,
    clock: Arc<dyn Clock>,
    hold: EffectHold,
    declaration: Arc<dyn DeclaredBehaviour>,
    page_size: usize,
    max_candidates: usize,
}

impl Replay {
    /// A replay with the default policy, the default budget, no declared behaviour and the
    /// default page size.
    ///
    /// `hold` must already be engaged (see [`EffectHold::engage`]): a replay refuses to be built
    /// over an unheld site.
    pub fn new(
        source: Arc<dyn ReplaySource>,
        runner: Arc<dyn ReplayRunner>,
        clone: CloneRef,
        site: SiteId,
        clock: Arc<dyn Clock>,
        hold: EffectHold,
    ) -> Replay {
        Replay {
            source,
            runner,
            clone,
            site,
            policy: ReplayPolicy::default(),
            reference_at: OffsetDateTime::UNIX_EPOCH,
            budget: Budget::default(),
            clock,
            hold,
            declaration: Arc::new(NoDeclarations),
            page_size: DEFAULT_PAGE_SIZE,
            max_candidates: DEFAULT_MAX_CANDIDATES,
        }
    }

    /// Sets the sample policy.
    #[must_use]
    pub fn with_policy(mut self, policy: ReplayPolicy) -> Replay {
        self.policy = policy;
        self
    }

    /// Sets the instant the policy's day window is measured from (a caller argument; the engine
    /// reads no wall clock).
    #[must_use]
    pub fn with_reference_at(mut self, reference_at: OffsetDateTime) -> Replay {
        self.reference_at = reference_at;
        self
    }

    /// Sets the budget.
    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Replay {
        self.budget = budget;
        self
    }

    /// Sets the declared behaviour (WP-MIG-02's release metadata).
    #[must_use]
    pub fn with_declaration(mut self, declaration: Arc<dyn DeclaredBehaviour>) -> Replay {
        self.declaration = declaration;
        self
    }

    /// Sets the journal page size.
    #[must_use]
    pub fn with_page_size(mut self, page_size: usize) -> Replay {
        self.page_size = page_size;
        self
    }

    /// Sets the most candidates the run reads.
    #[must_use]
    pub fn with_max_candidates(mut self, max_candidates: usize) -> Replay {
        self.max_candidates = max_candidates;
        self
    }

    /// Runs the replay over the whole window.
    ///
    /// # Errors
    /// An invalid policy or budget, a journal read failure, or a runner failure.
    pub async fn run(&self) -> Result<ReplayReport> {
        self.run_from(None).await
    }

    /// Runs the replay from `cursor`, re-drawing the sample from the entries after it. A run
    /// stopped by its budget resumes exactly where it stopped.
    ///
    /// # Errors
    /// As [`run`](Replay::run).
    pub async fn resume(&self, cursor: Cursor) -> Result<ReplayReport> {
        self.run_from(Some(cursor)).await
    }

    /// The one run path, from an optional cursor.
    async fn run_from(&self, cursor: Option<Cursor>) -> Result<ReplayReport> {
        self.policy.validate()?;
        self.budget.validate()?;
        if self.page_size == 0 {
            return Err(Error::InvalidInput {
                what: "page size".to_owned(),
                reason: "must be at least one".to_owned(),
            });
        }
        if self.max_candidates == 0 {
            return Err(Error::InvalidInput {
                what: "max candidates".to_owned(),
                reason: "must be at least one".to_owned(),
            });
        }
        if self.site != *self.hold.site() {
            return Err(Error::InvalidInput {
                what: "site".to_owned(),
                reason: "does not match the effect hold's site".to_owned(),
            });
        }
        if self.clone.site() != self.site.as_str() {
            // A clone of another site is not this replay's clone: refuse rather than write to it.
            return Err(Error::NotClone {
                label: self.clone.label().to_owned(),
            });
        }

        let (candidates, read_truncated, last_read) = self.read_candidates(cursor.clone()).await?;
        let sample = select(&self.policy, self.reference_at, candidates);

        let started = self.clock.monotonic();
        let mut replayed: u64 = 0;
        let mut divergences: Vec<Divergence> = Vec::new();
        let mut stop = None;
        let mut processed = cursor.clone();

        for entry in &sample.entries {
            let elapsed = self.clock.monotonic().saturating_sub(started);
            if let Some(reason) = self.budget.exhausted(replayed, elapsed) {
                stop = Some(reason);
                break;
            }
            let request = ReplayRequest {
                clone: &self.clone,
                site: &self.site,
                at: entry.occurred_at,
                principal: &entry.principal,
                entry,
                hold: &self.hold,
            };
            let produced = self.runner.run(request).await?;
            for divergence in diff(&entry.recorded, &produced) {
                divergences.push(self.classify(divergence, entry));
            }
            replayed += 1;
            processed = Some(Cursor::of(entry));
        }

        let elapsed = self.clock.monotonic().saturating_sub(started);
        let complete = stop.is_none() && !read_truncated;
        // A budget stop resumes after the last replayed entry; a truncated read resumes after the
        // last candidate read, so a later run continues the window rather than the sample.
        let resume_cursor = match stop {
            Some(_) => processed,
            None => {
                if read_truncated {
                    last_read.or(processed)
                } else {
                    None
                }
            }
        };
        let coverage = Coverage {
            considered: sample.considered,
            selected: sample.len(),
            replayed,
            strata: sample.strata,
            complete,
            stop,
            elapsed_micros: micros(elapsed),
        };
        let outcome = ReplayReport::outcome_of(&coverage, &divergences);
        Ok(ReplayReport {
            site: self.site.to_string(),
            clone: self.clone.label().to_owned(),
            policy: self.policy,
            coverage,
            divergences,
            cursor: resume_cursor,
            outcome,
        })
    }

    /// Reads the candidates for the window, paging through the source.
    async fn read_candidates(
        &self,
        cursor: Option<Cursor>,
    ) -> Result<(Vec<crate::journal::JournalEntry>, bool, Option<Cursor>)> {
        let mut entries = Vec::new();
        let mut read_cursor = cursor;
        let mut last_read = None;
        let mut truncated = false;
        loop {
            if entries.len() >= self.max_candidates {
                truncated = true;
                break;
            }
            let page = self
                .source
                .read(read_cursor.as_ref(), self.page_size)
                .await?;
            if page.entries.is_empty() {
                break;
            }
            for entry in page.entries {
                last_read = Some(Cursor::of(&entry));
                entries.push(entry);
                if entries.len() >= self.max_candidates {
                    truncated = true;
                    break;
                }
            }
            if truncated || page.exhausted {
                break;
            }
            match page.next {
                Some(next) => read_cursor = Some(next),
                None => break,
            }
        }
        Ok((entries, truncated, last_read))
    }

    /// Attributes a divergence to its entry and applies the declared behaviour.
    fn classify(
        &self,
        mut divergence: Divergence,
        entry: &crate::journal::JournalEntry,
    ) -> Divergence {
        divergence.source = entry.source.clone();
        divergence.sequence = entry.sequence;
        if let Some(id) = self.declaration.declared_for(&divergence) {
            divergence.class = DivergenceClass::Expected;
            divergence.declared_by = Some(id);
        }
        divergence
    }
}

/// A duration in microseconds, saturating.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}
