//! The write filter: nothing classified as business data enters the memory namespace.
//!
//! `spot-agent-memory` already keeps business data out by construction (an entry names a record by
//! type and id, its only free text is a `Summary` the sanitiser produced). This filter is the
//! host's second wall, run on **every** write before the store sees it:
//!
//! 1. **The sanitiser again**, over the entry's own text. Text a store row or a buggy caller made
//!    without the sanitiser is scanned here, and anything the sanitiser would refuse (a secret, a
//!    financial value) or would have to redact (personal data) is refused.
//! 2. **Document field values.** An episode names the record it is about by DocType and id. The
//!    filter reads that record **as the platform** through [`RecordValues`] and refuses the write
//!    when the entry's text contains any of the record's text field values (of at least
//!    [`MIN_VALUE_CHARS`] characters, compared without case). A value of the incident's own
//!    document is business data however it is phrased.
//!
//! Every verdict is **counted** ([`Counters`]) and **audited**: a `tracing` event
//! `spot::audit::agent_memory` per refusal and per acceptance (who, kind, verdict class; never the
//! text), and the same as an AI audit entry when the site hosts that chain (`AiAuditPort`), where a
//! write the chain cannot record is refused: nothing an agent does goes unrecorded. A refusal says
//! the **class** (`secret`, `financial`, `pii`, `document_field_value`) and never what matched.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_agent_memory::{Entry, Sanitiser};
use spot_core::Clock;

use crate::services::ai_audit::{ActionKind, AiAction, AiAuditPort, Outcome};

/// The shortest document field value the filter looks for. Shorter values ("yes", "A1") would
/// match everything.
pub const MIN_VALUE_CHARS: usize = 6;

/// A boxed future.
pub type Fut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The text field values of the record an entry is about, read as the platform. A host fills it
/// with the `SystemReader` port; an absent or unreadable record has no values (the sanitiser is
/// still the first wall).
pub trait RecordValues: Send + Sync {
    /// The text values of `doctype`'s record `id`.
    fn values<'a>(&'a self, doctype: &'a str, id: &'a str) -> Fut<'a, Vec<String>>;
}

/// Why a write was refused: the class only, never the matched text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// `secret`, `financial`, `pii`, `document_field_value` or `unclassified`.
    pub class: &'static str,
}

/// The filter's counters since the node started.
#[derive(Debug, Default)]
pub struct Counters {
    accepted: AtomicU64,
    refused: AtomicU64,
}

impl Counters {
    /// How many writes passed.
    #[must_use]
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::SeqCst)
    }

    /// How many writes were refused.
    #[must_use]
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::SeqCst)
    }
}

/// The filter.
pub struct WriteFilter {
    site: String,
    sanitiser: Sanitiser,
    values: Option<Arc<dyn RecordValues>>,
    audit: Option<Arc<dyn AiAuditPort>>,
    clock: Arc<dyn Clock>,
    counters: Counters,
    sequence: AtomicU64,
}

impl WriteFilter {
    /// A filter for `site`.
    #[must_use]
    pub fn new(
        site: &str,
        sanitiser: Sanitiser,
        values: Option<Arc<dyn RecordValues>>,
        audit: Option<Arc<dyn AiAuditPort>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            site: site.to_owned(),
            sanitiser,
            values,
            audit,
            clock,
            counters: Counters::default(),
            sequence: AtomicU64::new(0),
        }
    }

    /// The counters.
    #[must_use]
    pub fn counters(&self) -> &Counters {
        &self.counters
    }

    /// Judges one write by `actor` (`agent:<id>` or `user:<id>`), counts and audits the verdict.
    ///
    /// # Errors
    ///
    /// [`Refusal`] with the class of what was found, or `unclassified` when the audit chain could
    /// not record the write (a write that cannot be recorded is not made).
    pub async fn admit(&self, actor: &str, agent: &str, entry: &Entry) -> Result<(), Refusal> {
        let verdict = self.judge(entry).await;
        match &verdict {
            Ok(()) => self.counters.accepted.fetch_add(1, Ordering::SeqCst),
            Err(_) => self.counters.refused.fetch_add(1, Ordering::SeqCst),
        };
        let class = verdict.as_ref().err().map(|refusal| refusal.class);
        tracing::info!(
            target: "spot::audit::agent_memory",
            site = %self.site,
            actor,
            kind = entry.kind().as_str(),
            verdict = if class.is_some() { "refused" } else { "accepted" },
            class = class.unwrap_or(""),
            "agent memory write"
        );
        if let Some(audit) = &self.audit {
            let at_micros = self
                .clock
                .now_utc()
                .unix_timestamp()
                .saturating_mul(1_000_000);
            let seq = self.sequence.fetch_add(1, Ordering::SeqCst);
            let action = AiAction {
                kind: ActionKind::ToolCall,
                event_id: format!("agent-memory:{}:{at_micros}:{seq}", self.site),
                agent: agent.to_owned(),
                acting_user: actor.to_owned(),
                tool: "agent_memory_write".to_owned(),
                arguments: vec![
                    ("kind".to_owned(), entry.kind().as_str().to_owned()),
                    ("class".to_owned(), class.unwrap_or("none").to_owned()),
                ],
                outcome: if class.is_some() {
                    Outcome::Refused
                } else {
                    Outcome::Ok
                },
                subject: None,
                refers_to: None,
                at_micros,
            };
            if audit.append(action).await.is_err() && verdict.is_ok() {
                self.counters.accepted.fetch_sub(1, Ordering::SeqCst);
                self.counters.refused.fetch_add(1, Ordering::SeqCst);
                return Err(Refusal {
                    class: "unclassified",
                });
            }
        }
        verdict
    }

    /// The verdict, before counting.
    async fn judge(&self, entry: &Entry) -> Result<(), Refusal> {
        let text = entry.text();
        match self.sanitiser.check(&text) {
            Err(error) => {
                return Err(Refusal {
                    class: error.class().map_or("unclassified", |class| class.as_str()),
                });
            }
            Ok(summary) if summary.was_redacted() => return Err(Refusal { class: "pii" }),
            Ok(_) => {}
        }
        if let (Entry::Episode(episode), Some(values)) = (entry, &self.values) {
            let haystack = text.to_lowercase();
            let record = values
                .values(&episode.incident.doctype, &episode.incident.id)
                .await;
            for value in record {
                let needle = value.trim().to_lowercase();
                if needle.chars().count() >= MIN_VALUE_CHARS && haystack.contains(&needle) {
                    return Err(Refusal {
                        class: "document_field_value",
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_agent_memory::testing::{episode_saying, fact, sanitiser};
    use spot_core::ManualClock;
    use time::macros::datetime;

    struct Values(Vec<String>);

    impl RecordValues for Values {
        fn values<'a>(&'a self, _doctype: &'a str, _id: &'a str) -> Fut<'a, Vec<String>> {
            Box::pin(async move { self.0.clone() })
        }
    }

    fn filter(values: Vec<String>) -> WriteFilter {
        WriteFilter::new(
            "acme",
            sanitiser(),
            Some(Arc::new(Values(values))),
            None,
            Arc::new(ManualClock::new(datetime!(2026-11-20 10:00:00 UTC))),
        )
    }

    #[tokio::test]
    async fn an_entry_that_holds_no_business_data_passes_and_is_counted() {
        let filter = filter(vec!["Wavelength Textiles".to_owned()]);
        let entry = Entry::Episode(episode_saying(
            "acme",
            1,
            "the running job was unstuck and finished",
            1_000,
        ));
        filter.admit("agent:a1", "a1", &entry).await.unwrap();
        assert_eq!(
            (filter.counters().accepted(), filter.counters().refused()),
            (1, 0)
        );
        filter
            .admit("agent:a1", "a1", &Entry::Fact(fact("acme", 1)))
            .await
            .unwrap();
        assert_eq!(filter.counters().accepted(), 2);
    }

    #[tokio::test]
    async fn a_document_field_value_in_the_text_is_refused_by_class_only() {
        let filter = filter(vec!["Wavelength Textiles".to_owned(), "x".to_owned()]);
        let entry = Entry::Episode(episode_saying(
            "acme",
            1,
            "customer wavelength textiles was affected by the stuck job",
            1_000,
        ));
        let refusal = filter.admit("agent:a1", "a1", &entry).await.unwrap_err();
        assert_eq!(refusal.class, "document_field_value");
        assert!(!format!("{refusal:?}").to_lowercase().contains("wavelength"));
        assert_eq!(
            (filter.counters().accepted(), filter.counters().refused()),
            (0, 1)
        );
    }

    #[tokio::test]
    async fn a_value_too_short_to_mean_anything_is_ignored() {
        let filter = filter(vec!["job".to_owned()]);
        let entry = Entry::Episode(episode_saying("acme", 1, "the running job finished", 1_000));
        filter.admit("agent:a1", "a1", &entry).await.unwrap();
    }
}
