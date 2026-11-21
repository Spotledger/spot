//! The sent-log contract: the properties every [`SentLog`] must satisfy, runnable against any
//! implementation.
//!
//! The in-memory log in [`ports`](crate::ports) passes it today; WP-PLT-11.T6 runs the same
//! cases against PostgreSQL. [`run_sent_log_contract`] exercises one implementation with the
//! cases the in-memory log passes, and [`SentLogContractCase`] names each one so a failure says
//! which property broke.
//!
//! The contract is what makes "it is a sent log" checkable: the queue's whole at-least-once
//! argument rests on [`record_sent`](SentLog::record_sent) being first-writer-wins and on
//! [`is_sent`](SentLog::is_sent) never saying "sent" for a message nothing recorded.
//!
//! Compiled behind the `testing` feature (and always in this crate's own tests) so a store
//! implementation in another crate runs the same cases through a dev-dependency.

use spot_core::SiteId;
use spot_mail::{Address, SendReceipt};

use crate::ports::{SentLog, Unavailable};

/// One property the sent-log contract checks; every implementation must pass all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SentLogContractCase {
    /// A message id nothing recorded is not sent.
    UnknownIsNotSent,
    /// Recording a message makes `is_sent` true for that site.
    IsSentAfterRecord,
    /// Recording the same message twice yields one receipt: the second call answers `false`
    /// and leaves the first record unchanged.
    IdempotentRecord,
    /// The same message id on two sites is two messages: recording one never marks the other.
    SitesAreIsolated,
    /// Two concurrent records of one message produce exactly one `true`.
    ConcurrentRecordYieldsOneReceipt,
}

impl SentLogContractCase {
    /// Every case, for a caller that wants to report coverage.
    #[must_use]
    pub fn all() -> [SentLogContractCase; 5] {
        [
            Self::UnknownIsNotSent,
            Self::IsSentAfterRecord,
            Self::IdempotentRecord,
            Self::SitesAreIsolated,
            Self::ConcurrentRecordYieldsOneReceipt,
        ]
    }

    /// A lower-case name safe to use as a site id, so every case runs on its own site.
    fn slug(self) -> &'static str {
        match self {
            Self::UnknownIsNotSent => "contract-unknown-is-not-sent",
            Self::IsSentAfterRecord => "contract-is-sent-after-record",
            Self::IdempotentRecord => "contract-idempotent-record",
            Self::SitesAreIsolated => "contract-sites-are-isolated",
            Self::ConcurrentRecordYieldsOneReceipt => "contract-concurrent-record",
        }
    }
}

/// Why a case did not pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SentLogContractFailure {
    /// The log could not be reached.
    #[error(transparent)]
    Unavailable(#[from] Unavailable),
    /// The log answered, and the answer broke the property the case checks. The string names
    /// the property; it carries no address, message id or store detail, so a caller can log it
    /// as-is.
    #[error("the sent log does not hold this property: {0}")]
    Violated(&'static str),
    /// A fixture this module builds was rejected. The built-in fixtures are valid site ids and
    /// addresses, so this means a rule changed and the contract needs a new fixture.
    #[error("the contract's own fixture was rejected")]
    Fixture,
}

/// A case's property did not hold. Returned rather than asserted so a caller running several
/// implementations sees a failure instead of a panic.
fn ensure(
    held: bool,
    case: SentLogContractCase,
    property: &'static str,
) -> Result<(), (SentLogContractCase, SentLogContractFailure)> {
    if held {
        Ok(())
    } else {
        Err((case, SentLogContractFailure::Violated(property)))
    }
}

/// Runs every contract case against `log`, returning the cases that passed.
///
/// A caller (WP-PLT-11.T6's PostgreSQL log) asserts the returned list is exactly
/// [`SentLogContractCase::all`]. The first failing case stops the run and is returned with the
/// reason, so the caller can print which property broke.
///
/// # Errors
/// The first case that failed, with either the log's error or a rejected fixture.
pub async fn run_sent_log_contract(
    log: &dyn SentLog,
) -> Result<Vec<SentLogContractCase>, (SentLogContractCase, SentLogContractFailure)> {
    let mut passed = Vec::new();
    for case in SentLogContractCase::all() {
        run_case(log, case).await?;
        passed.push(case);
    }
    Ok(passed)
}

async fn run_case(
    log: &dyn SentLog,
    case: SentLogContractCase,
) -> Result<(), (SentLogContractCase, SentLogContractFailure)> {
    let site = site(case)?;
    let message_id = format!("{}@contract.test", case.slug());
    let receipt = receipt_for(case, &message_id, "buyer@customer.test")?;
    match case {
        SentLogContractCase::UnknownIsNotSent => {
            let sent = log
                .is_sent(&site, &message_id)
                .await
                .map_err(|error| (case, error.into()))?;
            ensure(
                !sent,
                case,
                "an unrecorded message id must not read as sent",
            )
        }
        SentLogContractCase::IsSentAfterRecord => {
            let first = log
                .record_sent(&site, &message_id, &receipt)
                .await
                .map_err(|error| (case, error.into()))?;
            ensure(first, case, "the first record must win")?;
            let sent = log
                .is_sent(&site, &message_id)
                .await
                .map_err(|error| (case, error.into()))?;
            ensure(sent, case, "a recorded message must read as sent")
        }
        SentLogContractCase::IdempotentRecord => {
            let first = log
                .record_sent(&site, &message_id, &receipt)
                .await
                .map_err(|error| (case, error.into()))?;
            ensure(first, case, "the first record must win")?;
            // A different receipt, so an overwrite is visible when it is read back below.
            let conflicting = receipt_for(case, &message_id, "boss@customer.test")?;
            let second = log
                .record_sent(&site, &message_id, &conflicting)
                .await
                .map_err(|error| (case, error.into()))?;
            ensure(!second, case, "a repeated record must answer false")?;
            let stored = log
                .recorded(&site, &message_id)
                .await
                .map_err(|error| (case, error.into()))?;
            ensure(
                stored.as_ref() == Some(&receipt),
                case,
                "the first record's receipt must be left unchanged",
            )
        }
        SentLogContractCase::SitesAreIsolated => {
            let other = site_of(case, &format!("{}-other", case.slug()))?;
            let recorded = log
                .record_sent(&site, &message_id, &receipt)
                .await
                .map_err(|error| (case, error.into()))?;
            ensure(recorded, case, "the first record must win")?;
            let leaked = log
                .is_sent(&other, &message_id)
                .await
                .map_err(|error| (case, error.into()))?;
            ensure(
                !leaked,
                case,
                "the same message id on another site must not read as sent",
            )
        }
        SentLogContractCase::ConcurrentRecordYieldsOneReceipt => {
            let (first, second) = tokio::join!(
                log.record_sent(&site, &message_id, &receipt),
                log.record_sent(&site, &message_id, &receipt)
            );
            let first = first.map_err(|error| (case, error.into()))?;
            let second = second.map_err(|error| (case, error.into()))?;
            let winners = [first, second].into_iter().filter(|won| *won).count();
            ensure(
                winners == 1,
                case,
                "two concurrent records of one message must yield exactly one receipt",
            )
        }
    }
}

fn site(
    case: SentLogContractCase,
) -> Result<SiteId, (SentLogContractCase, SentLogContractFailure)> {
    site_of(case, case.slug())
}

fn site_of(
    case: SentLogContractCase,
    name: &str,
) -> Result<SiteId, (SentLogContractCase, SentLogContractFailure)> {
    SiteId::new(name).map_err(|_| (case, SentLogContractFailure::Fixture))
}

fn receipt_for(
    case: SentLogContractCase,
    message_id: &str,
    accepted: &str,
) -> Result<SendReceipt, (SentLogContractCase, SentLogContractFailure)> {
    let address: Address = accepted
        .parse()
        .map_err(|_| (case, SentLogContractFailure::Fixture))?;
    Ok(SendReceipt {
        message_id: message_id.to_owned(),
        accepted: vec![address],
        rejected: Vec::new(),
    })
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, PoisonError};

    use async_trait::async_trait;

    use super::*;
    use crate::ports::InMemorySentLog;

    #[tokio::test]
    async fn the_in_memory_sent_log_passes_the_contract() {
        let log = InMemorySentLog::new();
        let passed = run_sent_log_contract(&log)
            .await
            .expect("the in-memory log must pass every contract case");
        assert_eq!(passed, SentLogContractCase::all().to_vec());
        assert_eq!(log.len(), 4, "one record per recording case");
    }

    /// A log that overwrites on a repeated record: the properties before `IdempotentRecord` hold,
    /// so the run reaches it and the contract must report the overwrite as a violation, not
    /// panic.
    #[derive(Debug, Default)]
    struct OverwritingLog(Mutex<BTreeMap<(SiteId, String), SendReceipt>>);

    #[async_trait]
    impl SentLog for OverwritingLog {
        async fn is_sent(&self, site: &SiteId, message_id: &str) -> Result<bool, Unavailable> {
            Ok(self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&(site.clone(), message_id.to_owned())))
        }

        async fn record_sent(
            &self,
            site: &SiteId,
            message_id: &str,
            receipt: &SendReceipt,
        ) -> Result<bool, Unavailable> {
            let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            let key = (site.clone(), message_id.to_owned());
            let first = !entries.contains_key(&key);
            entries.insert(key, receipt.clone());
            Ok(first)
        }

        async fn recorded(
            &self,
            site: &SiteId,
            message_id: &str,
        ) -> Result<Option<SendReceipt>, Unavailable> {
            Ok(self
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&(site.clone(), message_id.to_owned()))
                .cloned())
        }
    }

    #[tokio::test]
    async fn a_log_that_overwrites_is_reported_as_a_violation() {
        let log = OverwritingLog::default();
        let (case, failure) = run_sent_log_contract(&log)
            .await
            .expect_err("a log that overwrites the first record must fail the contract");
        assert_eq!(case, SentLogContractCase::IdempotentRecord);
        assert!(
            matches!(
                failure,
                SentLogContractFailure::Violated(
                    "the first record's receipt must be left unchanged"
                )
            ),
            "{failure:?}"
        );
    }
}
