//! The customer's view of break-glass: what sessions ran, why, for how long, and the
//! recording digest — read from the tenant's audit chain, not from the vendor.
//!
//! Nothing here can delete a session or a recording. [`RecordingArchive`] exposes the
//! append-only recording files for the customer's administrators to retrieve, and an
//! explicit [`RecordingArchive::erase`] that refuses: a vendor-side caller is told the
//! recording is not theirs to remove, and an append-only archive never deletes in place.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use spot_audit_chain::{ChainEntry, PayloadValue};

use crate::audit::{ACTIVE, BreakGlassEvent, ENDED, EXPIRED, RECORDING, REQUESTED, REVOKED};
use crate::error::BreakGlassError;
use crate::recorder::{FileRecorder, Replay, replay};
use crate::request::{Principal, PrincipalKind};

/// One break-glass session, as the customer reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    /// The session id.
    pub session: String,
    /// The tenant.
    pub tenant: String,
    /// Who asked (the vendor-side requester).
    pub who: String,
    /// Why (the request reason).
    pub reason: String,
    /// When the request was made (unix seconds).
    pub requested_at: i64,
    /// When the session started acting, if it did.
    pub started_at: Option<i64>,
    /// When the session ended, expired or was revoked, if it did.
    pub ended_at: Option<i64>,
    /// The running duration in seconds, when both ends are known.
    pub duration_secs: Option<i64>,
    /// How many commands the session recorded.
    pub commands: u64,
    /// The recording head digest, as hex, when one was committed.
    pub recording_digest: Option<String>,
    /// The latest lifecycle state the chain shows.
    pub state: String,
}

/// The customer's view: every break-glass session the audit chain records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CustomerView {
    sessions: Vec<SessionSummary>,
}

impl CustomerView {
    /// Builds the view from the audit chain's entries.
    ///
    /// Every entry whose subject is a `BreakGlassSession` contributes; entries that do not
    /// parse are ignored rather than guessed at, because the chain is the customer's and
    /// the view must never invent a session.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::AuditUnavailable`] when a `BreakGlassSession` entry's payload is
    /// not the event JSON it must be.
    pub fn collect(entries: &[ChainEntry]) -> Result<CustomerView, BreakGlassError> {
        let mut order: Vec<String> = Vec::new();
        let mut by_id: BTreeMap<String, SessionSummary> = BTreeMap::new();
        for entry in entries {
            let Some(subject) = &entry.record.subject else {
                continue;
            };
            if subject.doctype != "BreakGlassSession" {
                continue;
            }
            let Some(PayloadValue::Text(body)) = entry.record.payload.get("event") else {
                continue;
            };
            let event: BreakGlassEvent =
                serde_json::from_str(body).map_err(|_| BreakGlassError::AuditUnavailable)?;
            let summary = match by_id.get_mut(&event.session) {
                Some(summary) => summary,
                None => {
                    order.push(event.session.clone());
                    by_id
                        .entry(event.session.clone())
                        .or_insert_with(|| SessionSummary {
                            session: event.session.clone(),
                            tenant: event.tenant.clone(),
                            who: String::new(),
                            reason: String::new(),
                            requested_at: 0,
                            started_at: None,
                            ended_at: None,
                            duration_secs: None,
                            commands: 0,
                            recording_digest: None,
                            state: "requested".to_owned(),
                        })
                }
            };
            apply(summary, &event);
        }
        let sessions = order
            .into_iter()
            .filter_map(|id| by_id.remove(&id))
            .collect();
        Ok(CustomerView { sessions })
    }

    /// Every session, in the order the chain first showed them.
    #[must_use]
    pub fn sessions(&self) -> &[SessionSummary] {
        &self.sessions
    }

    /// The summary for `session`, if the chain shows it.
    #[must_use]
    pub fn session(&self, session: &str) -> Option<&SessionSummary> {
        self.sessions.iter().find(|s| s.session == session)
    }
}

fn apply(summary: &mut SessionSummary, event: &BreakGlassEvent) {
    if event.kind == REQUESTED {
        summary.who = event.who.clone();
        summary.reason = event.reason.clone();
        summary.requested_at = event.at;
        summary.state = "requested".to_owned();
    }
    if event.kind == ACTIVE {
        summary.started_at = Some(event.at);
        summary.state = "active".to_owned();
    }
    if event.kind == RECORDING {
        summary.recording_digest = event.recording_digest.clone();
        summary.commands = summary.commands.max(event.commands);
    }
    if event.kind == crate::audit::COMMAND {
        summary.commands = summary.commands.max(event.commands);
    }
    if event.kind == ENDED {
        summary.ended_at = Some(event.at);
        summary.state = "ended".to_owned();
    }
    if event.kind == EXPIRED {
        summary.ended_at = Some(event.at);
        summary.state = "expired".to_owned();
    }
    if event.kind == REVOKED {
        summary.ended_at = Some(event.at);
        summary.state = "revoked".to_owned();
    }
    let start = summary.started_at.unwrap_or(summary.requested_at);
    summary.duration_secs = summary.ended_at.map(|end| end - start);
}

/// The customer's append-only recording archive: the recording files on the customer's
/// side, retrievable by their administrators and never deleted by the vendor.
#[derive(Clone, Debug)]
pub struct RecordingArchive {
    root: PathBuf,
}

impl RecordingArchive {
    /// An archive rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> RecordingArchive {
        RecordingArchive { root: root.into() }
    }

    /// The directory the recordings live in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The recording file for `session`.
    #[must_use]
    pub fn recording_path(&self, session: &str) -> PathBuf {
        FileRecorder::recording_path(&self.root, session)
    }

    /// The manifest file for `session`.
    #[must_use]
    pub fn manifest_path(&self, session: &str) -> PathBuf {
        FileRecorder::manifest_path(&self.root, session)
    }

    /// Reads the recording bytes for `session` (the customer's administrators retrieve
    /// them here).
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::RecordingNotFound`] when no recording exists.
    pub fn read(&self, session: &str) -> Result<Vec<u8>, BreakGlassError> {
        fs::read(self.recording_path(session)).map_err(|_| BreakGlassError::RecordingNotFound)
    }

    /// Replays the recording for `session`, verifying its chain.
    #[must_use]
    pub fn replay(&self, session: &str) -> Replay {
        replay(&self.recording_path(session))
    }

    /// Attempts to remove a recording. There is no delete: recordings are append-only and
    /// customer-held, so a vendor-side caller is refused explicitly and any caller is told
    /// the archive never deletes in place.
    ///
    /// # Errors
    ///
    /// [`BreakGlassError::VendorDeleteRefused`] for a vendor principal;
    /// [`BreakGlassError::AppendOnly`] otherwise.
    pub fn erase(&self, principal: &Principal, session: &str) -> Result<(), BreakGlassError> {
        let _ = session;
        if principal.kind == PrincipalKind::Vendor {
            return Err(BreakGlassError::VendorDeleteRefused);
        }
        Err(BreakGlassError::AppendOnly)
    }
}
