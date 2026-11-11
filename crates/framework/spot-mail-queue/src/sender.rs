//! `MailSender`: the `platform:send_mail` job handler.
//!
//! Per attempt: read the sent log (a message already sent completes without sending), build
//! and validate the message, resolve the account through the directory, check the From
//! address against the account, authorise `host:port` through the egress guard with
//! `Purpose::Mail`, load attachments from the file store, build or reuse the account's
//! transport (the credential is resolved only when a transport is built, and never leaves this
//! module), send, record the send, and write bounce records for what will not arrive.
//!
//! # Delivery guarantee
//!
//! SMTP cannot deduplicate, so the queue is **at-least-once with a narrow window**: the sent
//! log stops every resend once a send is recorded, including after a crash between the record
//! and the job's completion. The window that remains is a crash (or a timeout that
//! drops the attempt) between the server's acceptance and `record_sent`; the retry then sends
//! once more. A timeout leaves acceptance unknown, so the exhaustion bounce below can name a
//! message the server did accept: the record says the queue gave up, which is what a caller
//! waiting on the mail needs to know, and the message id lets it reconcile.
//! After a successful send the handler never asks for a retry, whatever fails later (the sent
//! log write, a bounce write): a retry could only send again. Those failures are logged with
//! their code and the message id.
//!
//! # When the retries run out
//!
//! A retryable failure on the **last** attempt the job is allowed is turned into a final one:
//! every recipient gets a [`BounceRecord`] of [`BounceClass::RetriesExhausted`] carrying the last
//! error's code, its SMTP reply code and the attempt count, and the job **completes** — it is
//! neither retried nor dead-lettered, so nothing waits forever on a message that will never
//! arrive. The record never carries the server's reply text, an address list or a
//! credential.
//!
//! "Last" is the engine's own signal: the handler asks [`JobContext::is_last_attempt`] and
//! nothing else. The sender holds no copy of a queue policy, so a deployment whose policy
//! differs from the job's stored limit cannot make the two disagree.
//!
//! Three rules bound that path:
//!
//! - A message the sent log already has is **not** bounced: the server accepted it (or part of
//!   it), so a bounce record would be a lie. The sent log is read again just before bouncing,
//!   because another attempt of the same job may have recorded the send meanwhile.
//! - A sent log that cannot be read is not treated as "not sent": the job asks for a retry with
//!   [`codes::SENT_LOG_UNAVAILABLE`] instead of bouncing a message that may have arrived.
//! - A bounce write that fails is **never swallowed**: the attempt asks for a retry with
//!   [`codes::BOUNCE_UNAVAILABLE`], which on the last attempt is the engine's dead-letter rule,
//!   and logs the code and the message id. The loop stops at the first failing recipient, so a
//!   retry after a partial write asks the sink for records it may already hold: [`BounceSink`]
//!   is idempotent on [`BounceRecord::key`] (job, recipient, class), so each bounce lands once.
//!
//! # A last attempt the handler never finishes (second half)
//!
//! If the last attempt is killed by the worker's queue timeout, a cancellation or a panic, the
//! engine dead-letters the job without the handler's result. [`MailSender`] implements
//! [`FinalAttemptHook`]: register it with `Worker::with_final_attempt_hook` and the worker calls
//! it once for that dead job. The hook writes a [`BounceClass::RetriesExhausted`] record for every
//! recipient the stored message names that the sent log's receipt does not already show as
//! accepted or rejected, and it writes nothing when the sent log cannot be read. The hook only
//! acts on `platform:send_mail` jobs (the worker calls it for every job). The reason the worker
//! reports (`job.timeout`, `job.handler_panicked`, a cancellation) is logged, not stored in the
//! record. Its time stamp comes from the sender's clock ([`MailSender::with_clock`]), because a
//! hook has no job context.
//!
//! # Secrets
//!
//! The strings returned to the job engine (and stored as the job's last error) are a catalogue
//! code plus at most an SMTP reply code. Neither server text, addresses, subjects nor
//! credentials appear in them, in bounce records or in log lines.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use spot_core::Clock;
use spot_core::SiteId;
use spot_egress::{Authorized, EgressError, Guard, Purpose};
use spot_files::error::FileError;
use spot_files::key::FileKey;
use spot_files::store::FileStore;
use spot_jobs::QueuePolicy;
use spot_jobs::model::{Job, JobId};
use spot_jobs::worker::{
    BoxFuture, FinalAttemptHook, HandlerResult, JobContext, JobHandlerRegistry,
};
use spot_mail::{
    Attachment, InlineImage, MailError, MailTransport, OutgoingMessage, PermanentFailure,
    TransientFailure,
};
use time::OffsetDateTime;
use tokio::io::AsyncReadExt;

use crate::error::codes;
use crate::job::MailJob;
use crate::ports::{
    AccountDirectory, BounceClass, BounceRecord, BounceSink, CredentialError, CredentialResolver,
    SentLog, SmtpAccount, TransportFactory,
};
use crate::{MAIL_JOB_NAME, MAIL_JOB_VERSION};

/// Most bytes of attachments plus inline images read for one message.
pub const MAX_TOTAL_FILE_BYTES: u64 = 25 * 1024 * 1024;

/// Why an attempt stopped before completing.
#[derive(Debug)]
enum Stop {
    /// Try again later (no recipient is known to have the message).
    Retry {
        code: &'static str,
        reply: Option<u16>,
    },
    /// Never retry; bounce every recipient.
    Fail {
        code: &'static str,
        class: BounceClass,
        reply: Option<u16>,
    },
}

impl Stop {
    fn retry(code: &'static str) -> Self {
        Self::Retry { code, reply: None }
    }

    fn fail(code: &'static str, class: BounceClass) -> Self {
        Self::Fail {
            code,
            class,
            reply: None,
        }
    }
}

fn text(code: &str, reply: Option<u16>) -> String {
    match reply {
        Some(reply) => format!("{code} reply={reply}"),
        None => code.to_owned(),
    }
}

struct CachedTransport {
    account: SmtpAccount,
    /// The authorised addresses the transport dials, sorted: a transport (and with it its idle
    /// connections) is reused only for the same set. A changed set means a re-authorisation
    /// saw a different DNS answer, so the old transport and its pool are dropped.
    pinned: Vec<SocketAddr>,
    transport: Arc<dyn MailTransport>,
}

/// The services a [`MailSender`] sends through: its ports, grouped so the constructor stays
/// readable (seven ports plus a queue policy is more than a positional list can carry).
pub struct MailPorts {
    /// Looks up the account a job names.
    pub accounts: Arc<dyn AccountDirectory>,
    /// Turns a credential reference into a secret.
    pub credentials: Arc<dyn CredentialResolver>,
    /// Builds a transport from an authorised, pinned configuration.
    pub factory: Arc<dyn TransportFactory>,
    /// Authorises `host:port` before every connection.
    pub guard: Arc<Guard>,
    /// Reads attachment and inline-image bodies.
    pub files: Arc<dyn FileStore>,
    /// Remembers which messages the server accepted.
    pub sent_log: Arc<dyn SentLog>,
    /// Receives bounce records.
    pub bounces: Arc<dyn BounceSink>,
}

/// The handler for `platform:send_mail`.
pub struct MailSender {
    accounts: Arc<dyn AccountDirectory>,
    credentials: Arc<dyn CredentialResolver>,
    factory: Arc<dyn TransportFactory>,
    guard: Arc<Guard>,
    files: Arc<dyn FileStore>,
    sent_log: Arc<dyn SentLog>,
    bounces: Arc<dyn BounceSink>,
    transports: Mutex<HashMap<(SiteId, String), CachedTransport>>,
    require_pinned: bool,
    /// Time stamps for records the dead-job hook writes (a hook has no job context).
    clock: Arc<dyn Clock>,
}

impl MailSender {
    /// A sender over its ports. Whether an attempt is the last one comes from the job's own
    /// context ([`JobContext::is_last_attempt`]); the sender is given no queue policy.
    pub fn from_ports(ports: MailPorts) -> Self {
        Self {
            accounts: ports.accounts,
            credentials: ports.credentials,
            factory: ports.factory,
            guard: ports.guard,
            files: ports.files,
            sent_log: ports.sent_log,
            bounces: ports.bounces,
            transports: Mutex::new(HashMap::new()),
            require_pinned: true,
            clock: Arc::new(spot_core::SystemClock::new()),
        }
    }

    /// The constructor from before the final-attempt signal existed. `queue` is **ignored**; it
    /// stays only so a caller outside this crate (the CLI's mail service) keeps compiling until
    /// it drops the argument. Use [`from_ports`](Self::from_ports).
    pub fn new(ports: MailPorts, queue: &QueuePolicy) -> Self {
        let _ = queue;
        Self::from_ports(ports)
    }

    /// Sets the clock that stamps the records the dead-job hook writes. The default is the
    /// system clock; tests pass a manual one.
    #[must_use]
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Sets whether a transport may be built without pinned addresses. The default (`true`)
    /// refuses to send when the egress guard handed back no address to dial, so a transport
    /// can never resolve the host name again after the check (DNS rebinding). Only
    /// tests with a stand-in factory turn this off.
    #[must_use]
    pub fn with_require_pinned(mut self, require_pinned: bool) -> Self {
        self.require_pinned = require_pinned;
        self
    }

    /// Registers this handler for `platform:send_mail` in a worker's registry.
    pub fn register(self: &Arc<Self>, registry: &mut JobHandlerRegistry) {
        let handler = Arc::clone(self);
        registry.register(MAIL_JOB_NAME, move |ctx| {
            let handler = Arc::clone(&handler);
            Box::pin(async move { handler.execute(ctx).await })
        });
    }

    fn cache(&self) -> MutexGuard<'_, HashMap<(SiteId, String), CachedTransport>> {
        self.transports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// The job-engine entry point: one attempt, mapped onto a [`HandlerResult`]. Never returns
    /// `Err`: every failure is a retry, a permanent fail with a catalogue code, or — on the last
    /// attempt the job is allowed — a bounce for every recipient and a completed job.
    pub async fn execute(&self, ctx: JobContext) -> spot_jobs::Result<HandlerResult> {
        let job: MailJob = match serde_json::from_value(ctx.job.args.clone()) {
            Ok(job) => job,
            Err(_) => return Ok(HandlerResult::Fail(codes::MALFORMED_JOB.to_owned())),
        };
        if job.version != MAIL_JOB_VERSION {
            return Ok(HandlerResult::Fail(codes::UNSUPPORTED_VERSION.to_owned()));
        }
        let site = &ctx.job.site;
        let last = ctx.is_last_attempt();
        match self.attempt(site, &job, ctx.job.id, ctx.clock).await {
            Ok(()) => Ok(HandlerResult::Ok),
            Err(Stop::Retry { code, reply }) if last => {
                self.exhausted(&ctx, &job, code, reply).await
            }
            Err(Stop::Retry { code, reply }) => Ok(HandlerResult::Retry(text(code, reply))),
            Err(Stop::Fail { code, class, reply }) => {
                match self.bounce_all(&ctx, &job, class, code, reply).await {
                    Ok(()) => Ok(HandlerResult::Fail(text(code, reply))),
                    Err(()) => Ok(HandlerResult::Retry(codes::BOUNCE_UNAVAILABLE.to_owned())),
                }
            }
        }
    }

    /// No attempt is left and the last one failed with something that might have succeeded
    /// later: bounce every recipient and complete the job, so the sender learns the message will
    /// never arrive instead of a dead job nobody hears about.
    ///
    /// A message the sent log has is not bounced (the server accepted it or part of it), and a
    /// log that cannot be read is not taken as "not sent". A failed bounce write is logged and
    /// returned as a retry — on the last attempt that is the engine's dead-letter rule — never
    /// swallowed. The write stops at the first failing recipient, so a retry re-asks for the
    /// recipients already written: the sink's per-recipient idempotency is what keeps that from
    /// doubling a record.
    async fn exhausted(
        &self,
        ctx: &JobContext,
        job: &MailJob,
        code: &'static str,
        reply: Option<u16>,
    ) -> spot_jobs::Result<HandlerResult> {
        let (site, job_id, attempt, at) = (&ctx.job.site, ctx.job.id, ctx.attempt, ctx.clock);
        match self.sent_log.is_sent(site, &job.message_id).await {
            Ok(true) => {
                tracing::warn!(
                    code = codes::RETRIES_EXHAUSTED,
                    message_id = %job.message_id,
                    attempt,
                    "the retries are used up but the sent log has the message; nothing is bounced"
                );
                return Ok(HandlerResult::Ok);
            }
            Ok(false) => {}
            Err(_) => {
                tracing::error!(
                    code = codes::SENT_LOG_UNAVAILABLE,
                    message_id = %job.message_id,
                    attempt,
                    "the retries are used up and the sent log cannot be read; not bouncing a message that may have been sent"
                );
                return Ok(HandlerResult::Retry(codes::SENT_LOG_UNAVAILABLE.to_owned()));
            }
        }
        let mut bounced = 0_usize;
        for recipient in job.message.recipient_addresses() {
            let record = BounceRecord {
                site: site.clone(),
                job_id,
                message_id: job.message_id.clone(),
                recipient: recipient.to_owned(),
                class: BounceClass::RetriesExhausted,
                code,
                reply_code: reply,
                attempts: Some(attempt),
                at,
            };
            if self.bounces.record(record).await.is_err() {
                tracing::error!(
                    code = codes::BOUNCE_UNAVAILABLE,
                    message_id = %job.message_id,
                    attempt,
                    "the retries are used up but the bounce write failed; not swallowing it"
                );
                return Ok(HandlerResult::Retry(codes::BOUNCE_UNAVAILABLE.to_owned()));
            }
            bounced += 1;
        }
        tracing::error!(
            code = codes::RETRIES_EXHAUSTED,
            message_id = %job.message_id,
            attempt,
            recipients = %bounced,
            last_code = code,
            "the retries are used up; every recipient has a bounce record"
        );
        Ok(HandlerResult::Ok)
    }

    async fn bounce_all(
        &self,
        ctx: &JobContext,
        job: &MailJob,
        class: BounceClass,
        code: &'static str,
        reply_code: Option<u16>,
    ) -> Result<(), ()> {
        let (site, job_id, at) = (&ctx.job.site, ctx.job.id, ctx.clock);
        for recipient in job.message.recipient_addresses() {
            let record = BounceRecord {
                site: site.clone(),
                job_id,
                message_id: job.message_id.clone(),
                recipient: recipient.to_owned(),
                class,
                code,
                reply_code,
                attempts: None,
                at,
            };
            self.bounces.record(record).await.map_err(|_| ())?;
        }
        Ok(())
    }

    async fn attempt(
        &self,
        site: &SiteId,
        job: &MailJob,
        job_id: JobId,
        now: OffsetDateTime,
    ) -> Result<(), Stop> {
        let already = self
            .sent_log
            .is_sent(site, &job.message_id)
            .await
            .map_err(|_| Stop::retry(codes::SENT_LOG_UNAVAILABLE))?;
        if already {
            return Ok(());
        }
        let mut outgoing = job
            .message
            .to_outgoing(&job.message_id)
            .map_err(|_| Stop::fail(codes::INVALID_MESSAGE, BounceClass::InvalidMessage))?;
        let account = self
            .accounts
            .account(site, &job.account)
            .await
            .map_err(|_| Stop::retry(codes::DIRECTORY_UNAVAILABLE))?
            .ok_or(Stop::fail(
                codes::UNKNOWN_ACCOUNT,
                BounceClass::Configuration,
            ))?;
        if !outgoing
            .from
            .address
            .to_string()
            .eq_ignore_ascii_case(&account.from_address)
        {
            return Err(Stop::fail(
                codes::SENDER_MISMATCH,
                BounceClass::Configuration,
            ));
        }
        let authorized = self.authorise(site, job, &account).await?;
        let pinned = self.pinned_addrs(&authorized)?;
        self.load_files(site, job, &mut outgoing).await?;
        let transport = self.transport_for(site, job, &account, pinned).await?;
        match transport.send(&outgoing).await {
            Ok(receipt) => self.finish(site, job, job_id, receipt, now).await,
            Err(error) => Err(self.classify(site, job, error)),
        }
    }

    async fn authorise(
        &self,
        site: &SiteId,
        job: &MailJob,
        account: &SmtpAccount,
    ) -> Result<Authorized, Stop> {
        let actor = Some(format!("mail_queue:{}", job.account));
        match self
            .guard
            .authorize_addr(
                &account.host,
                account.port,
                Purpose::Mail,
                site.as_str(),
                actor,
            )
            .await
        {
            Ok(authorized) => Ok(authorized),
            Err(EgressError::ResolutionFailed) => Err(Stop::Retry {
                code: EgressError::ResolutionFailed.code(),
                reply: None,
            }),
            Err(denied) => Err(Stop::fail(denied.code(), BounceClass::EgressDenied)),
        }
    }

    /// The addresses the transport must dial: exactly what the guard authorised. With
    /// `require_pinned` an empty answer is refused instead of falling back to the host name.
    fn pinned_addrs(&self, authorized: &Authorized) -> Result<Option<Vec<SocketAddr>>, Stop> {
        let addrs = authorized.socket_addrs();
        if addrs.is_empty() {
            return if self.require_pinned {
                Err(Stop::fail(
                    codes::PINNED_REQUIRED,
                    BounceClass::Configuration,
                ))
            } else {
                Ok(None)
            };
        }
        Ok(Some(addrs.to_vec()))
    }

    async fn load_files(
        &self,
        site: &SiteId,
        job: &MailJob,
        outgoing: &mut OutgoingMessage,
    ) -> Result<(), Stop> {
        let invalid = || Stop::fail(codes::INVALID_MESSAGE, BounceClass::InvalidMessage);
        let mut remaining = MAX_TOTAL_FILE_BYTES;
        for queued in &job.message.attachments {
            let bytes = self.read(site, &queued.file_key, &mut remaining).await?;
            let attachment = Attachment::new(&queued.filename, &queued.content_type, bytes)
                .map_err(|_| invalid())?
                .with_content_id(queued.content_id.clone());
            outgoing.attachments.push(attachment);
        }
        for queued in &job.message.inline {
            let bytes = self.read(site, &queued.file_key, &mut remaining).await?;
            let image = InlineImage::new(&queued.content_id, &queued.content_type, bytes)
                .map_err(|_| invalid())?;
            outgoing.inline.push(image);
        }
        Ok(())
    }

    async fn read(&self, site: &SiteId, key: &str, remaining: &mut u64) -> Result<Vec<u8>, Stop> {
        let too_large = || Stop::fail(codes::ATTACHMENT_TOO_LARGE, BounceClass::Attachment);
        let (_, key) = FileKey::parse(key)
            .map_err(|_| Stop::fail(codes::INVALID_MESSAGE, BounceClass::InvalidMessage))?;
        let (meta, body) = match self.files.get(site, key).await {
            Ok(found) => found,
            Err(FileError::NotFound { .. }) => {
                return Err(Stop::fail(
                    codes::ATTACHMENT_MISSING,
                    BounceClass::Attachment,
                ));
            }
            Err(_) => return Err(Stop::retry(codes::ATTACHMENT_UNAVAILABLE)),
        };
        if meta.size > *remaining {
            return Err(too_large());
        }
        let mut bytes = Vec::new();
        body.take(*remaining + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| Stop::retry(codes::ATTACHMENT_UNAVAILABLE))?;
        let len = u64::try_from(bytes.len()).map_err(|_| too_large())?;
        if len > *remaining {
            return Err(too_large());
        }
        *remaining -= len;
        Ok(bytes)
    }

    async fn transport_for(
        &self,
        site: &SiteId,
        job: &MailJob,
        account: &SmtpAccount,
        pinned: Option<Vec<SocketAddr>>,
    ) -> Result<Arc<dyn MailTransport>, Stop> {
        let key = (site.clone(), job.account.clone());
        let mut pinned_set = pinned.clone().unwrap_or_default();
        pinned_set.sort();
        if let Some(cached) = self.cache().get(&key)
            && cached.account == *account
            && cached.pinned == pinned_set
        {
            return Ok(Arc::clone(&cached.transport));
        }
        let secret = match (&account.username, &account.credential_ref) {
            (Some(_), Some(reference)) => match self.credentials.resolve(site, reference).await {
                Ok(secret) => Some(secret),
                Err(CredentialError::NotFound) => {
                    return Err(Stop::fail(
                        codes::CREDENTIAL_NOT_FOUND,
                        BounceClass::Configuration,
                    ));
                }
                Err(CredentialError::Unavailable) => {
                    return Err(Stop::retry(codes::CREDENTIAL_UNAVAILABLE));
                }
            },
            (None, None) => None,
            _ => {
                return Err(Stop::fail(
                    codes::TRANSPORT_CONFIG,
                    BounceClass::Configuration,
                ));
            }
        };
        let mut config = account.config(secret);
        config.pinned = pinned;
        if self.require_pinned && config.pinned.is_none() {
            return Err(Stop::fail(
                codes::PINNED_REQUIRED,
                BounceClass::Configuration,
            ));
        }
        let transport = self
            .factory
            .build(config)
            .map_err(|_| Stop::fail(codes::TRANSPORT_CONFIG, BounceClass::Configuration))?;
        self.cache().insert(
            key,
            CachedTransport {
                account: account.clone(),
                pinned: pinned_set,
                transport: Arc::clone(&transport),
            },
        );
        Ok(transport)
    }

    fn classify(&self, site: &SiteId, job: &MailJob, error: MailError) -> Stop {
        let reply = error.reply().map(|reply| reply.code);
        match error {
            MailError::Transient(
                TransientFailure::Reply(_)
                | TransientFailure::ConnectionRefused
                | TransientFailure::ConnectionReset
                | TransientFailure::Timeout
                | TransientFailure::TlsHandshake,
            ) => Stop::Retry {
                code: codes::SMTP_TRANSIENT,
                reply,
            },
            MailError::Permanent(PermanentFailure::Authentication { .. }) => {
                // The credential may have been rotated: rebuild the transport next time.
                self.cache().remove(&(site.clone(), job.account.clone()));
                Stop::Fail {
                    code: codes::SMTP_AUTHENTICATION,
                    class: BounceClass::Authentication,
                    reply,
                }
            }
            MailError::Permanent(PermanentFailure::Reply(_)) => Stop::Fail {
                code: codes::SMTP_PERMANENT,
                class: BounceClass::Rejected,
                reply,
            },
            MailError::Permanent(PermanentFailure::InvalidMessage(_)) => {
                Stop::fail(codes::INVALID_MESSAGE, BounceClass::InvalidMessage)
            }
            MailError::Permanent(PermanentFailure::Client(_)) => {
                Stop::fail(codes::TRANSPORT_CONFIG, BounceClass::Configuration)
            }
        }
    }

    /// After the server answered: nothing is retried once any recipient accepted the message.
    async fn finish(
        &self,
        site: &SiteId,
        job: &MailJob,
        job_id: JobId,
        receipt: spot_mail::SendReceipt,
        now: OffsetDateTime,
    ) -> Result<(), Stop> {
        if receipt.accepted.is_empty()
            && !receipt.rejected.is_empty()
            && receipt
                .rejected
                .iter()
                .all(|(_, reply)| reply.is_transient())
        {
            // Nobody got the message and every refusal was temporary: safe to try again.
            let reply = receipt.rejected.first().map(|(_, reply)| reply.code);
            return Err(Stop::Retry {
                code: codes::SMTP_TRANSIENT,
                reply,
            });
        }
        match self
            .sent_log
            .record_sent(site, &job.message_id, &receipt)
            .await
        {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                message_id = %job.message_id,
                "another attempt recorded this message first"
            ),
            Err(_) => tracing::error!(
                code = codes::SENT_LOG_UNAVAILABLE,
                message_id = %job.message_id,
                "message sent but the sent log write failed; not retrying to avoid a second send"
            ),
        }
        for (address, reply) in &receipt.rejected {
            let class = if reply.is_transient() {
                BounceClass::Deferred
            } else {
                BounceClass::Rejected
            };
            let record = BounceRecord {
                site: site.clone(),
                job_id,
                message_id: job.message_id.clone(),
                recipient: address.to_string(),
                class,
                code: codes::RECIPIENT_REJECTED,
                reply_code: Some(reply.code),
                attempts: None,
                at: now,
            };
            if self.bounces.record(record).await.is_err() {
                tracing::error!(
                    code = codes::BOUNCE_UNAVAILABLE,
                    message_id = %job.message_id,
                    "recipient rejected but the bounce write failed; the receipt is in the sent log"
                );
            }
        }
        Ok(())
    }
}

impl MailSender {
    /// The hook body: the worker killed the last attempt of `job` (`reason` is its code), so no
    /// recipient has been told. Bounce every recipient the sent log's receipt does not already
    /// account for. Fails closed: a sent log that cannot be read, or arguments that cannot be
    /// parsed, write nothing, because a bounce for a message that may have arrived is a lie.
    async fn bounce_dead_job(&self, job: &Job, reason: &str) {
        if job.name != MAIL_JOB_NAME {
            return;
        }
        let Ok(mail) = serde_json::from_value::<MailJob>(job.args.clone()) else {
            tracing::error!(
                code = codes::MALFORMED_JOB,
                job_id = job.id.0,
                "a dead mail job has unreadable arguments; no bounce can be written"
            );
            return;
        };
        let receipt = match self.sent_log.recorded(&job.site, &mail.message_id).await {
            Ok(receipt) => receipt,
            Err(_) => {
                tracing::error!(
                    code = codes::SENT_LOG_UNAVAILABLE,
                    message_id = %mail.message_id,
                    job_id = job.id.0,
                    "a mail job died on its last attempt and the sent log cannot be read; not bouncing a message that may have been sent"
                );
                return;
            }
        };
        let at = self.clock.now_utc();
        let (mut written, mut failed) = (0_usize, 0_usize);
        for recipient in mail.message.recipient_addresses() {
            if receipt.as_ref().is_some_and(|receipt| {
                receipt
                    .accepted
                    .iter()
                    .map(ToString::to_string)
                    .chain(receipt.rejected.iter().map(|(a, _)| a.to_string()))
                    .any(|known| known.eq_ignore_ascii_case(recipient))
            }) {
                continue;
            }
            let record = BounceRecord {
                site: job.site.clone(),
                job_id: job.id,
                message_id: mail.message_id.clone(),
                recipient: recipient.to_owned(),
                class: BounceClass::RetriesExhausted,
                code: codes::RETRIES_EXHAUSTED,
                reply_code: None,
                attempts: Some(job.attempt),
                at,
            };
            // Keep going after a failed write: the other recipients still deserve theirs, and
            // the sink is idempotent, so nothing here is repeated by a later pass.
            if self.bounces.record(record).await.is_ok() {
                written += 1;
            } else {
                failed += 1;
            }
        }
        tracing::error!(
            code = codes::RETRIES_EXHAUSTED,
            message_id = %mail.message_id,
            job_id = job.id.0,
            reason,
            written,
            failed,
            "a mail job died on its last attempt; its recipients have bounce records"
        );
    }
}

impl FinalAttemptHook for MailSender {
    fn on_dead<'a>(&'a self, job: &'a Job, reason: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(self.bounce_dead_job(job, reason))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn authorized(addrs: Vec<SocketAddr>) -> Authorized {
        Authorized {
            addrs,
            original_host: "smtp.example.test".to_owned(),
        }
    }

    fn sender(require_pinned: bool) -> MailSender {
        use crate::ports::{InMemoryBounceSink, InMemorySentLog, SmtpTransportFactory};
        struct Nothing;
        #[async_trait::async_trait]
        impl AccountDirectory for Nothing {
            async fn account(
                &self,
                _: &SiteId,
                _: &str,
            ) -> Result<Option<SmtpAccount>, crate::ports::Unavailable> {
                Ok(None)
            }
        }
        #[async_trait::async_trait]
        impl CredentialResolver for Nothing {
            async fn resolve(
                &self,
                _: &SiteId,
                _: &str,
            ) -> Result<spot_mail::Secret, CredentialError> {
                Err(CredentialError::NotFound)
            }
        }
        let guard = Arc::new(Guard::new(
            spot_egress::EgressPolicy::default(),
            Arc::new(spot_egress::TestResolver::new()),
            Arc::new(spot_egress::InMemorySink::new()),
            Arc::new(spot_core::ManualClock::new(time::macros::datetime!(
                2026-10-21 08:00 UTC
            ))),
        ));
        MailSender::from_ports(crate::MailPorts {
            accounts: Arc::new(Nothing),
            credentials: Arc::new(Nothing),
            factory: Arc::new(SmtpTransportFactory),
            guard,
            files: Arc::new(spot_files::memory::MemoryFileStore::new()),
            sent_log: Arc::new(InMemorySentLog::new()),
            bounces: Arc::new(InMemoryBounceSink::new()),
        })
        .with_require_pinned(require_pinned)
    }

    #[test]
    fn production_wiring_refuses_an_empty_authorised_list() {
        let stop = sender(true)
            .pinned_addrs(&authorized(Vec::new()))
            .unwrap_err();
        assert!(matches!(
            stop,
            Stop::Fail {
                code: codes::PINNED_REQUIRED,
                ..
            }
        ));
    }

    #[test]
    fn a_test_sender_may_run_unpinned() {
        assert_eq!(
            sender(false).pinned_addrs(&authorized(Vec::new())).unwrap(),
            None
        );
    }

    #[test]
    fn the_authorised_addresses_pass_through_in_order() {
        let addrs: Vec<SocketAddr> = vec![
            "1.1.1.1:587".parse().unwrap(),
            "93.184.216.34:587".parse().unwrap(),
        ];
        assert_eq!(
            sender(true)
                .pinned_addrs(&authorized(addrs.clone()))
                .unwrap(),
            Some(addrs)
        );
    }
}
