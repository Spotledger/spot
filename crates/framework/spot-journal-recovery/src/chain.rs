//! The two inputs that come from the object store: the chain and manifest check and the snapshot
//! listing. Both are collected once, up front, and handed to the report as plain facts, so the
//! report itself stays pure. A store failure is recorded as unreadable, never as clean.

use serde::{Deserialize, Serialize};
use spot_journal_audit::{OldestSnapshot, Position};
use spot_journal_manifest::VerifyingKeys;
use spot_journal_snapshot::{SnapshotInfo, SnapshotKeys, list_snapshots};
use spot_journal_store::JournalStore;
use spot_journal_verify::{Expected, Mode, VerifyReport, verify_tenant};

/// How far the chain check got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainState {
    /// The check could not run (a store failure, a question that could not be asked, or a report
    /// about another tenant). Nothing can be said about the chain.
    Unreadable,
    /// The check ran but its byte budget stopped it early; what it did not read is unknown.
    Partial,
    /// The check covered the whole journal.
    Complete,
}

/// One finding of the chain check: a code and a range, never object bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChainFinding {
    /// The stable `journal_verify.*` code.
    pub code: String,
    /// The generation the finding is about.
    pub generation: u64,
    /// The first position of the range.
    pub first_seq: u64,
    /// The last position of the range.
    pub last_seq: u64,
    /// Whether the finding is evidence of tampering or loss.
    pub tampering: bool,
}

/// The chain and manifest check of one tenant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainFacts {
    /// The tenant the check was about.
    pub tenant: String,
    /// How far it got.
    pub state: ChainState,
    /// Everything it found, in a stable order.
    pub findings: Vec<ChainFinding>,
}

impl ChainFacts {
    /// A check that could not run.
    #[must_use]
    pub fn unreadable(tenant: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            state: ChainState::Unreadable,
            findings: Vec::new(),
        }
    }

    /// A check that ran with the given state and findings (sorted here, so the order the check
    /// found them in does not matter).
    #[must_use]
    pub fn new(
        tenant: impl Into<String>,
        state: ChainState,
        findings: impl IntoIterator<Item = ChainFinding>,
    ) -> Self {
        let mut findings: Vec<ChainFinding> = findings.into_iter().collect();
        findings.sort();
        findings.dedup();
        Self {
            tenant: tenant.into(),
            state,
            findings,
        }
    }

    /// The facts a verification report holds. A partial pass is `Partial`, never `Complete`.
    #[must_use]
    pub fn from_report(report: &VerifyReport) -> Self {
        let state = if report.partial {
            ChainState::Partial
        } else {
            ChainState::Complete
        };
        Self::new(
            report.tenant.clone(),
            state,
            report.findings.iter().map(|f| ChainFinding {
                code: f.code().to_owned(),
                generation: f.generation,
                first_seq: f.first_seq,
                last_seq: f.last_seq,
                tampering: f.is_tampering(),
            }),
        )
    }
}

/// Runs the chain and manifest verification for `expected` and keeps what the report needs.
///
/// A verification that cannot be asked or fails reading the store gives
/// [`ChainState::Unreadable`]; it is never turned into an empty, clean result.
pub async fn check_chain<S: JournalStore + ?Sized>(
    store: &S,
    keys: &VerifyingKeys,
    expected: &Expected,
    mode: Mode,
) -> ChainFacts {
    match verify_tenant(store, keys, expected, mode).await {
        Ok(report) => ChainFacts::from_report(&report),
        Err(_) => ChainFacts::unreadable(expected.tenant.to_string()),
    }
}

/// What is known about the tenant's snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotFacts {
    /// Whether the listing could be read.
    pub readable: bool,
    /// How many snapshots with a valid index exist.
    pub count: u64,
    /// How many of them a rebuild test has verified.
    pub verified: u64,
    /// The oldest of them.
    pub oldest: Option<OldestSnapshot>,
}

impl SnapshotFacts {
    /// A listing that could not be read.
    #[must_use]
    pub fn unreadable() -> Self {
        Self {
            readable: false,
            count: 0,
            verified: 0,
            oldest: None,
        }
    }

    /// The facts of a listing.
    #[must_use]
    pub fn from_infos(infos: &[SnapshotInfo]) -> Self {
        let oldest = infos
            .iter()
            .min_by_key(|s| (s.taken_at_millis, s.generation, s.position))
            .map(|s| OldestSnapshot {
                position: Position::new(s.generation, s.position),
                taken_at_millis: s.taken_at_millis,
            });
        Self {
            readable: true,
            count: infos.len() as u64,
            verified: infos.iter().filter(|s| s.verified).count() as u64,
            oldest,
        }
    }
}

/// Lists the tenant's snapshots and keeps what the report needs. A store failure gives an
/// unreadable listing, never an empty one.
pub async fn read_snapshots(
    store: &dyn JournalStore,
    tenant: &str,
    keys: &dyn SnapshotKeys,
) -> SnapshotFacts {
    match list_snapshots(store, tenant, keys).await {
        Ok(infos) => SnapshotFacts::from_infos(&infos),
        Err(_) => SnapshotFacts::unreadable(),
    }
}
