//! The seams of the server: audit, approvals and the operator allow-list.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use serde::Serialize;
use spot_mgmt_ops::{OperationSpec, Risk};

/// A boxed `Send` future, so every port stays object safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How a request ended, for the audit record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    /// The request was served, or the command accepted.
    Allowed,
    /// The request was refused.
    Refused,
    /// An accepted command finished successfully.
    Succeeded,
    /// An accepted command failed.
    Failed,
    /// An accepted command was cancelled.
    Cancelled,
}

/// One audit record. It never carries a secret, a signature, a nonce or a parameter value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AuditRecord {
    /// The request id (also in the response).
    pub request_id: String,
    /// The operator principal, when the certificate was on the allow-list.
    pub principal: Option<String>,
    /// SHA-256 fingerprint (hex) of the client certificate, when one was presented.
    pub certificate_fingerprint: Option<String>,
    /// What was asked, for example `mgmt.operation.list` or a catalogue audit id.
    pub action: String,
    /// The catalogue operation id, when the request named a valid one.
    pub operation: Option<String>,
    /// The operation's risk, when known.
    pub risk: Option<Risk>,
    /// The outcome.
    pub outcome: AuditOutcome,
    /// The reason code of a refusal or failure.
    pub reason: Option<String>,
    /// The command id, when a command exists.
    pub command_id: Option<String>,
    /// Unix seconds, from the injected clock.
    pub at: i64,
}

/// The audit sink failed; the record may not have been written.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("the audit sink failed")]
pub struct AuditError;

/// Where audit records go (the site's audit chain in production).
pub trait AuditSink: Send + Sync {
    /// Writes the record; returns its audit reference.
    ///
    /// # Errors
    /// [`AuditError`] when the record could not be written.
    fn record(&self, record: AuditRecord) -> BoxFuture<'_, Result<String, AuditError>>;
}

/// An in-memory sink for tests and the development profile.
#[derive(Debug, Default)]
pub struct MemoryAuditSink {
    records: Mutex<Vec<AuditRecord>>,
}

impl MemoryAuditSink {
    /// An empty sink.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A copy of the records written so far.
    #[must_use]
    pub fn records(&self) -> Vec<AuditRecord> {
        self.records
            .lock()
            .map(|records| records.clone())
            .unwrap_or_default()
    }
}

impl AuditSink for MemoryAuditSink {
    fn record(&self, record: AuditRecord) -> BoxFuture<'_, Result<String, AuditError>> {
        let result = match self.records.lock() {
            Ok(mut records) => {
                records.push(record);
                Ok(format!("audit-{}", records.len()))
            }
            Err(_) => Err(AuditError),
        };
        Box::pin(std::future::ready(result))
    }
}

/// What an approval check is asked about.
#[derive(Clone, Debug)]
pub struct ApprovalRequest<'a> {
    /// The operation id.
    pub operation: &'a str,
    /// The operation's risk.
    pub risk: Risk,
    /// The approval kinds the operation requires.
    pub kinds: &'a [String],
    /// The approval records the command carried (opaque).
    pub approvals: &'a [String],
    /// The command id the approvals must be bound to.
    pub command_id: &'a str,
}

/// The approval records of WP-MIG-04 / WP-ADM-03 are checked behind this trait.
pub trait ApprovalCheck: Send + Sync {
    /// The approval kinds `spec` requires (empty: none beyond the signature).
    fn required(&self, spec: &OperationSpec) -> Vec<String>;

    /// Verifies the carried approvals; any error refuses the command.
    fn verify<'a>(&'a self, request: ApprovalRequest<'a>) -> BoxFuture<'a, Result<(), ()>>;
}

/// An approval check that requires nothing (operations that declare no approval kind).
#[derive(Clone, Copy, Debug, Default)]
pub struct NoApprovals;

impl ApprovalCheck for NoApprovals {
    fn required(&self, _spec: &OperationSpec) -> Vec<String> {
        Vec::new()
    }

    fn verify<'a>(&'a self, _request: ApprovalRequest<'a>) -> BoxFuture<'a, Result<(), ()>> {
        Box::pin(std::future::ready(Ok(())))
    }
}

/// Which operations an operator may call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OperationScope {
    /// Every operation.
    All,
    /// These operation ids, or dotted prefixes (`site.backup` covers `site.backup.run`).
    Only(Vec<String>),
}

impl OperationScope {
    /// Whether `operation` is in scope.
    #[must_use]
    pub fn allows(&self, operation: &str) -> bool {
        match self {
            Self::All => true,
            Self::Only(list) => list.iter().any(|entry| {
                operation == entry
                    || (operation.starts_with(entry.as_str())
                        && operation.as_bytes().get(entry.len()) == Some(&b'.'))
            }),
        }
    }
}

/// One entry of the operator subject allow-list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperatorEntry {
    /// The certificate subject (RFC 2253 text) this entry matches exactly.
    pub subject: String,
    /// The principal name recorded in the audit.
    pub principal: String,
    /// The operations this operator may call.
    pub scope: OperationScope,
    /// May read any command's status, not only its own.
    pub audit_right: bool,
    /// May cancel any command, not only its own.
    pub cancel_right: bool,
}

impl OperatorEntry {
    /// An operator with every operation and no extra rights.
    #[must_use]
    pub fn new(subject: &str, principal: &str) -> Self {
        Self {
            subject: subject.to_owned(),
            principal: principal.to_owned(),
            scope: OperationScope::All,
            audit_right: false,
            cancel_right: false,
        }
    }
}

/// The subject allow-list. Default deny: an empty list refuses everyone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperatorAllowList {
    entries: Vec<OperatorEntry>,
}

impl OperatorAllowList {
    /// A list of these entries.
    #[must_use]
    pub fn new(entries: Vec<OperatorEntry>) -> Self {
        Self { entries }
    }

    /// The entry for a certificate subject, when allowed.
    #[must_use]
    pub fn resolve(&self, subject: &str) -> Option<&OperatorEntry> {
        self.entries.iter().find(|entry| entry.subject == subject)
    }
}
