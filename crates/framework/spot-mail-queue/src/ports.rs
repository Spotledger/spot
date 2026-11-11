//! The queue's ports: the account directory, the credential resolver, the transport factory,
//! the sent log and the bounce sink, with in-memory implementations of the last two for tests
//! and single-node setups. Every port is scoped by the job's `SiteId`; nothing crosses sites.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use spot_core::SiteId;
use spot_jobs::JobId;
use spot_mail::{
    MailError, MailTransport, PermanentFailure, Secret, SendReceipt, SmtpConfig, SmtpTransport,
    TransportSecurity,
};
use time::OffsetDateTime;

/// A backing service could not be reached. Carries no detail on purpose: details of a
/// failing store can include connection strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("backing service unavailable")]
pub struct Unavailable;

/// Why a credential could not be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CredentialError {
    /// The reference resolves to nothing. Permanent.
    #[error("credential not found")]
    NotFound,
    /// The credential service could not be reached. Retryable.
    #[error("credential service unavailable")]
    Unavailable,
}

/// An outgoing mail account as the directory knows it. Holds a credential **reference**, never
/// the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpAccount {
    /// The server host.
    pub host: String,
    /// The server port; authorised through the egress guard before every connection.
    pub port: u16,
    /// How the connection is protected.
    pub security: TransportSecurity,
    /// The account's own address; a message's `From` must equal it.
    pub from_address: String,
    /// The user name for authentication, when the server needs it.
    pub username: Option<String>,
    /// The reference the [`CredentialResolver`] turns into the secret; set together with
    /// `username`.
    pub credential_ref: Option<String>,
    /// Timeout for each transport phase.
    pub timeout: Duration,
    /// Idle connections kept by the transport (0 disables pooling).
    pub pool_size: usize,
    /// A CA or self-signed certificate the server must be trusted by.
    pub root_certificate_pem: Option<Vec<u8>>,
}

impl SmtpAccount {
    /// The transport configuration for this account with an already resolved secret.
    pub fn config(&self, secret: Option<Secret>) -> SmtpConfig {
        let credentials = match (&self.username, secret) {
            (Some(user), Some(secret)) => Some(spot_mail::Credentials {
                user: user.clone(),
                secret,
            }),
            _ => None,
        };
        SmtpConfig {
            host: self.host.clone(),
            port: self.port,
            security: self.security,
            credentials,
            timeout: self.timeout,
            pool_size: self.pool_size,
            allow_insecure: false,
            root_certificate_pem: self.root_certificate_pem.clone(),
            // Set by the sender from the egress guard's answer, never from the account.
            pinned: None,
        }
    }
}

/// Looks up outgoing accounts by site and id.
#[async_trait]
pub trait AccountDirectory: Send + Sync {
    /// The account, or `None` when this site has no such account.
    async fn account(&self, site: &SiteId, id: &str) -> Result<Option<SmtpAccount>, Unavailable>;
}

/// Turns a credential reference into a secret. The secret is used to build a transport and is
/// never stored in a job, a log line or an error.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// The secret behind `reference` for `site`.
    async fn resolve(&self, site: &SiteId, reference: &str) -> Result<Secret, CredentialError>;
}

/// Builds a transport from a configuration. Production uses [`SmtpTransportFactory`]; tests
/// hand out a capture transport.
pub trait TransportFactory: Send + Sync {
    /// Builds the transport.
    fn build(&self, config: SmtpConfig) -> Result<Arc<dyn MailTransport>, MailError>;
}

/// The production factory: a real [`SmtpTransport`]. It refuses a configuration without pinned
/// addresses, so a transport built here can never resolve the host name itself.
#[derive(Debug, Default, Clone, Copy)]
pub struct SmtpTransportFactory;

impl TransportFactory for SmtpTransportFactory {
    fn build(&self, config: SmtpConfig) -> Result<Arc<dyn MailTransport>, MailError> {
        if config.pinned.is_none() {
            return Err(MailError::Permanent(PermanentFailure::Client(
                "the production transport requires the egress-authorised addresses".to_owned(),
            )));
        }
        Ok(Arc::new(SmtpTransport::new(config)?))
    }
}

/// The record of a message the server accepted. The queue consults it before each send.
///
/// Every implementation must pass the contract suite in the crate's `contract` module (behind
/// the `testing` feature): record is first-writer-wins and per site, so a message is never sent
/// twice once one attempt recorded it.
#[async_trait]
pub trait SentLog: Send + Sync {
    /// Whether `message_id` was already recorded as sent for `site`.
    async fn is_sent(&self, site: &SiteId, message_id: &str) -> Result<bool, Unavailable>;

    /// Records that `message_id` was accepted. Atomic, first writer wins: returns `true` for
    /// the first call and `false` when an entry already exists (which is left unchanged).
    async fn record_sent(
        &self,
        site: &SiteId,
        message_id: &str,
        receipt: &SendReceipt,
    ) -> Result<bool, Unavailable>;

    /// The receipt `record_sent` stored for `message_id` on `site`, or `None` when nothing did.
    ///
    /// The queue does not read this; the contract suite does, because "the first record is left
    /// unchanged" is otherwise unobservable — `is_sent` alone cannot tell a preserved receipt
    /// from an overwritten one. It holds no server text, only the addresses and reply codes the
    /// transport already reported.
    async fn recorded(
        &self,
        site: &SiteId,
        message_id: &str,
    ) -> Result<Option<SendReceipt>, Unavailable>;
}

/// An in-memory [`SentLog`].
#[derive(Debug, Default)]
pub struct InMemorySentLog {
    entries: Mutex<BTreeMap<(SiteId, String), SendReceipt>>,
}

impl InMemorySentLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<(SiteId, String), SendReceipt>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The number of recorded messages.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True when nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

#[async_trait]
impl SentLog for InMemorySentLog {
    async fn is_sent(&self, site: &SiteId, message_id: &str) -> Result<bool, Unavailable> {
        Ok(self
            .lock()
            .contains_key(&(site.clone(), message_id.to_owned())))
    }

    async fn record_sent(
        &self,
        site: &SiteId,
        message_id: &str,
        receipt: &SendReceipt,
    ) -> Result<bool, Unavailable> {
        let mut entries = self.lock();
        let key = (site.clone(), message_id.to_owned());
        if entries.contains_key(&key) {
            return Ok(false);
        }
        entries.insert(key, receipt.clone());
        Ok(true)
    }

    async fn recorded(
        &self,
        site: &SiteId,
        message_id: &str,
    ) -> Result<Option<SendReceipt>, Unavailable> {
        Ok(self
            .lock()
            .get(&(site.clone(), message_id.to_owned()))
            .cloned())
    }
}

/// Why a recipient will not get the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BounceClass {
    /// The server permanently refused the message or the recipient (5xx).
    Rejected,
    /// The server temporarily refused a recipient of a partly accepted message; it is not
    /// retried because the accepted recipients already have the message.
    Deferred,
    /// The server refused the account's credentials.
    Authentication,
    /// The message is invalid.
    InvalidMessage,
    /// The egress guard denied the destination.
    EgressDenied,
    /// An attachment is missing or too large.
    Attachment,
    /// The account, its credential or its transport settings are unusable.
    Configuration,
    /// Every attempt the job was allowed failed with something that might have succeeded
    /// later, so the message is given up on. The record's [`code`](BounceRecord::code)
    /// is the last error's code and [`attempts`](BounceRecord::attempts) says how many attempts
    /// ran out; the send was never recorded, so no recipient has it.
    RetriesExhausted,
}

/// A permanent delivery failure for one recipient. Holds the code and the SMTP reply code only:
/// never the message body, subject or server text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BounceRecord {
    /// The site that owns the message.
    pub site: SiteId,
    /// The job that produced the record. With the site, the recipient and the class it forms
    /// the record's [`key`](Self::key).
    pub job_id: JobId,
    /// The message id.
    pub message_id: String,
    /// The recipient address that did not get the message.
    pub recipient: String,
    /// The failure class.
    pub class: BounceClass,
    /// The stable catalogue code (`mail_queue.*` or `egress.*`).
    pub code: &'static str,
    /// The SMTP reply code, when the server produced one.
    pub reply_code: Option<u16>,
    /// How many attempts the job was allowed, when the record is the result of running out of
    /// them ([`BounceClass::RetriesExhausted`]); `None` for a failure that ended one attempt.
    pub attempts: Option<u32>,
    /// When the failure was recorded (the job's clock).
    pub at: OffsetDateTime,
}

/// The deterministic identity of a bounce record: the job, the recipient and the failure class,
/// within a site. Two records with one key are the same bounce.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BounceKey {
    /// The site that owns the job.
    pub site: SiteId,
    /// The job that produced the record.
    pub job_id: JobId,
    /// The recipient address.
    pub recipient: String,
    /// The failure class.
    pub class: BounceClass,
}

impl BounceRecord {
    /// The record's idempotency key: (site, job id, recipient, class). It is derived only from
    /// the fields that say what happened, never from the time or the attempt count, so a retry
    /// after a partial write produces the same key for the same bounce.
    pub fn key(&self) -> BounceKey {
        BounceKey {
            site: self.site.clone(),
            job_id: self.job_id,
            recipient: self.recipient.clone(),
            class: self.class,
        }
    }
}

/// Receives bounce records.
///
/// # Idempotency
///
/// `record` is idempotent on [`BounceRecord::key`]: a second record with a key the sink already
/// holds is a no-op that returns `Ok(())` and leaves the first record unchanged. A durable sink
/// must make that check and the write one atomic step (a unique constraint on the key, with
/// `ON CONFLICT DO NOTHING` or the like), not a read followed by a write. The sender writes
/// recipients one at a time and stops at the first failure, and the dead-job hook can run after
/// a partial write, so a retried or repeated pass asks for records the sink may already hold —
/// an append-only sink would double them.
#[async_trait]
pub trait BounceSink: Send + Sync {
    /// Records one bounce. A record whose [`key`](BounceRecord::key) the sink already holds is
    /// a no-op returning `Ok(())`.
    async fn record(&self, record: BounceRecord) -> Result<(), Unavailable>;
}

/// An in-memory [`BounceSink`], keyed by [`BounceRecord::key`].
#[derive(Debug, Default)]
pub struct InMemoryBounceSink {
    records: Mutex<BTreeMap<BounceKey, BounceRecord>>,
}

impl InMemoryBounceSink {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record, ordered by key (site, job id, recipient, class).
    pub fn records(&self) -> Vec<BounceRecord> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }
}

#[async_trait]
impl BounceSink for InMemoryBounceSink {
    async fn record(&self, record: BounceRecord) -> Result<(), Unavailable> {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(record.key())
            .or_insert(record);
        Ok(())
    }
}
