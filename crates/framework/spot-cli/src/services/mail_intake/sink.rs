//! The sink: one message becomes one document of the target DocType, created as the `run_as`
//! user through the document engine, so that user's create right, field rights, validation and
//! hooks all apply. No system principal is involved at any point.
//!
//! The sink answers the engine in three ways: [`Accepted::Created`]; [`SinkError::Rejected`] for
//! a message that can never become a document (the document fails validation: a poison message,
//! counted and skipped); [`SinkError::Retryable`] for everything an operator can fix without
//! losing mail (the user is missing, disabled or lacks the right to create the document, the
//! engine is down). A retryable error stops the account at that message and pauses it with a
//! readiness note, so no mail is dropped while the configuration is wrong.
//!
//! Nothing here logs or returns an address, a subject, a body or an attachment name.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use spot_core::Clock;
use spot_mail::{IncomingMessage, MailAddress, thread_key};
use spot_mail_intake::{Accepted, Account, DedupeKey, IntakeSink, SinkError};
use spot_storage_contract::{RecordId, Value};
use time::OffsetDateTime;

use super::settings::{FieldMap, Target};
use super::status::{Status, record_skipped};

/// How long a resolved `run_as` user is remembered.
const USER_TTL: Duration = Duration::from_secs(30);

/// A seam future.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The user a document is created as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunAs {
    /// The user's record id.
    pub id: RecordId,
    /// Whether the user may sign in and act (not disabled or locked).
    pub active: bool,
}

/// Why the user directory could not answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectoryUnavailable;

/// Finds a site user by user name.
pub trait UserDirectory: Send + Sync {
    /// The user called `user_name`, or `None` when the site has none.
    fn find<'a>(
        &'a self,
        user_name: &'a str,
    ) -> Fut<'a, Result<Option<RunAs>, DirectoryUnavailable>>;
}

/// Why a document was not created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreateError {
    /// The document failed validation (or names an unknown field or DocType): a poison message.
    Invalid,
    /// The user may not create it: an operator fixes the user's rights.
    Denied,
    /// The engine could not answer; try again later.
    Unavailable,
}

/// Creates one document as a user, through the document engine.
pub trait DocumentCreator: Send + Sync {
    /// Creates a document of `doctype` with `fields`, as the user `run_as`, under the id
    /// `id` and the command id `command_id` (derived from the message's deduplication key, so
    /// an engine that honours command ids makes a redelivery after a crash harmless). The
    /// engine runs its own validation and hooks.
    fn create<'a>(
        &'a self,
        run_as: RecordId,
        doctype: &'a str,
        id: RecordId,
        command_id: String,
        fields: Vec<(String, Value)>,
    ) -> Fut<'a, Result<(), CreateError>>;
}

/// Stores one attachment as a private file attached to a created document.
pub trait AttachmentWriter: Send + Sync {
    /// Whether this host can store attachments at all. `false` makes an account with
    /// `store_attachments = true` invalid, with a clear reason, at start.
    fn supported(&self) -> bool;

    /// Stores `bytes` as a private file attached to the document, as the user `run_as`.
    fn store<'a>(
        &'a self,
        run_as: RecordId,
        doctype: &'a str,
        id: RecordId,
        file_name: &'a str,
        content_type: &'a str,
        bytes: &'a [u8],
    ) -> Fut<'a, Result<(), CreateError>>;
}

/// The attachment writer of a host whose files service has no in-process upload as a user.
pub struct NoAttachments;

impl AttachmentWriter for NoAttachments {
    fn supported(&self) -> bool {
        false
    }

    fn store<'a>(
        &'a self,
        _run_as: RecordId,
        _doctype: &'a str,
        _id: RecordId,
        _file_name: &'a str,
        _content_type: &'a str,
        _bytes: &'a [u8],
    ) -> Fut<'a, Result<(), CreateError>> {
        Box::pin(async { Err(CreateError::Unavailable) })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The sink of one site.
pub struct DocumentSink {
    site: String,
    targets: BTreeMap<String, Target>,
    users: Arc<dyn UserDirectory>,
    creator: Arc<dyn DocumentCreator>,
    attachments: Arc<dyn AttachmentWriter>,
    status: Arc<Status>,
    clock: Arc<dyn Clock>,
    cache: Mutex<BTreeMap<String, (OffsetDateTime, RunAs)>>,
}

/// What the sink is built from.
pub struct SinkParts {
    /// The site's name (a metric label).
    pub site: String,
    /// The target of each account, by account name.
    pub targets: BTreeMap<String, Target>,
    /// Finds the `run_as` user.
    pub users: Arc<dyn UserDirectory>,
    /// Creates the document.
    pub creator: Arc<dyn DocumentCreator>,
    /// Stores attachments.
    pub attachments: Arc<dyn AttachmentWriter>,
    /// The shared account status.
    pub status: Arc<Status>,
    /// The source of `now`.
    pub clock: Arc<dyn Clock>,
}

impl DocumentSink {
    /// A sink over `parts`.
    #[must_use]
    pub fn new(parts: SinkParts) -> Self {
        Self {
            site: parts.site,
            targets: parts.targets,
            users: parts.users,
            creator: parts.creator,
            attachments: parts.attachments,
            status: parts.status,
            clock: parts.clock,
            cache: Mutex::new(BTreeMap::new()),
        }
    }

    async fn run_as(&self, target: &Target) -> Result<RunAs, &'static str> {
        let now = self.clock.now_utc();
        if let Some((at, user)) = lock(&self.cache).get(&target.run_as).copied()
            && now - at < USER_TTL
        {
            return Ok(user);
        }
        match self.users.find(&target.run_as).await {
            Ok(Some(user)) if user.active => {
                lock(&self.cache).insert(target.run_as.clone(), (now, user));
                Ok(user)
            }
            Ok(_) => {
                lock(&self.cache).remove(&target.run_as);
                Err("run_as_unavailable")
            }
            Err(DirectoryUnavailable) => Err("run_as_unavailable"),
        }
    }
}

/// `Name <address>` or the bare address.
fn display(address: &MailAddress) -> String {
    match address.name.as_deref().filter(|n| !n.is_empty()) {
        Some(name) => format!("{name} <{}>", address.address),
        None => address.address.clone(),
    }
}

fn instant(message: &IncomingMessage, now: OffsetDateTime) -> OffsetDateTime {
    message
        .date
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
        .unwrap_or(now)
}

/// The fields of the document, by the field map. A part the message lacks is left out.
#[must_use]
pub fn build_fields(
    map: &FieldMap,
    message: &IncomingMessage,
    now: OffsetDateTime,
) -> Vec<(String, Value)> {
    let mut fields = Vec::new();
    let from = message.from.as_ref().map(display).unwrap_or_default();
    fields.push((map.from.clone(), Value::Text(from)));
    fields.push((
        map.subject.clone(),
        Value::Text(message.subject.clone().unwrap_or_default()),
    ));
    if let Some(name) = &map.to {
        let to = message
            .to
            .iter()
            .map(|a| a.address.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        fields.push((name.clone(), Value::Text(to)));
    }
    if let (Some(name), Some(text)) = (&map.text, &message.text) {
        fields.push((name.clone(), Value::Text(text.clone())));
    }
    if let (Some(name), Some(html)) = (&map.html, &message.html) {
        fields.push((name.clone(), Value::Text(html.clone())));
    }
    if let Some(name) = &map.received_at {
        fields.push((name.clone(), Value::DateTime(instant(message, now))));
    }
    if let (Some(name), Some(key)) = (&map.thread_key, thread_key(message)) {
        fields.push((name.clone(), Value::Text(key)));
    }
    if let (Some(name), Some(id)) = (&map.message_id, &message.message_id) {
        fields.push((name.clone(), Value::Text(id.clone())));
    }
    fields
}

/// The command id of the document a message becomes: `mi:<account>:<digest prefix>`, at most
/// 128 visible ASCII characters.
#[must_use]
pub fn command_id(account: &Account, key: &DedupeKey) -> String {
    let digest = key.as_str();
    let short = digest.get(..40).unwrap_or(digest);
    format!("mi:{}:{short}", account.name)
}

/// Whether the file name ends in a blocked extension.
#[must_use]
pub fn is_blocked(file_name: &str, blocked: &[String]) -> bool {
    let lower = file_name.to_ascii_lowercase();
    // Every dot-separated tail counts, so `invoice.pdf.exe` and `a.tar.sh` are both caught.
    lower
        .split('.')
        .skip(1)
        .any(|part| blocked.iter().any(|b| b == part))
}

#[async_trait]
impl IntakeSink for DocumentSink {
    async fn accept(
        &self,
        account: &Account,
        message: &IncomingMessage,
        key: &DedupeKey,
    ) -> Result<Accepted, SinkError> {
        let Some(target) = self.targets.get(&account.name) else {
            return Err(SinkError::Rejected("no_target"));
        };
        let user = match self.run_as(target).await {
            Ok(user) => user,
            Err(reason) => {
                self.status.pause(&account.name, reason);
                return Err(SinkError::Retryable);
            }
        };
        let now = self.clock.now_utc();
        let id = RecordId::new_v7();
        let fields = build_fields(&target.fields, message, now);
        match self
            .creator
            .create(
                user.id,
                &target.doctype,
                id,
                command_id(account, key),
                fields,
            )
            .await
        {
            Ok(()) => self.status.resume(&account.name),
            Err(CreateError::Invalid) => return Err(SinkError::Rejected("invalid_document")),
            Err(CreateError::Denied) => {
                self.status.pause(&account.name, "run_as_denied");
                return Err(SinkError::Retryable);
            }
            Err(CreateError::Unavailable) => return Err(SinkError::Retryable),
        }
        if target.store_attachments {
            self.store_attachments(account, target, user.id, id, message)
                .await;
        }
        Ok(Accepted::Created(id.to_string()))
    }
}

impl DocumentSink {
    /// Stores the allowed attachments. The document exists already, so a failure here is
    /// counted as a skip and never undoes it (a retry would create a second document).
    async fn store_attachments(
        &self,
        account: &Account,
        target: &Target,
        run_as: RecordId,
        id: RecordId,
        message: &IncomingMessage,
    ) {
        let mut stored = 0usize;
        let mut skipped = 0u64;
        for attachment in &message.attachments {
            if attachment.content_id.is_some() && message.html.is_some() {
                // An inline image of the HTML body is part of the body, not a file.
                continue;
            }
            if is_blocked(&attachment.filename, &target.blocked) || stored >= target.max_attachments
            {
                skipped += 1;
                continue;
            }
            let content_type = attachment.content_type_text();
            match self
                .attachments
                .store(
                    run_as,
                    &target.doctype,
                    id,
                    &attachment.filename,
                    &content_type,
                    &attachment.bytes,
                )
                .await
            {
                Ok(()) => stored += 1,
                Err(_) => skipped += 1,
            }
        }
        record_skipped(&self.site, &account.name, skipped);
    }
}
