//! The store traits behind the outbox and the inbox.
//!
//! Both are narrow on purpose: the PostgreSQL stores of WP-CTL-10.T3 implement exactly these,
//! each `commit` as one transaction. The in-memory versions in [`crate::memory`] are the
//! reference.

use serde::{Deserialize, Serialize};
use spot_tenant_link::{LinkId, SignatureBytes, Topic};

use crate::error::Result;
use crate::ids::{Conversation, MessageId};
use crate::message::Message;
use crate::notice::NoticeDraft;
use crate::state::{MessageState, RejectCode};

/// The tombstone that fills the slot of an expired or rejected message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filler {
    /// The tombstone message, same conversation and sequence number as the original.
    pub message: Message,
    /// Whether the receiver confirmed it.
    pub acknowledged: bool,
    /// When it was last handed to the carrier.
    pub last_sent_at: Option<u64>,
}

/// One message on the sender side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEntry {
    /// The signed message.
    pub message: Message,
    /// Where it is.
    pub state: MessageState,
    /// Unix milliseconds the state was entered.
    pub state_at: u64,
    /// How many times it was handed to the carrier.
    pub attempts: u32,
    /// When it was last handed to the carrier.
    pub last_sent_at: Option<u64>,
    /// The slot filler, once the message ended without being applied.
    pub filler: Option<Filler>,
    /// Store-managed version for optimistic updates.
    pub version: u64,
}

impl OutboxEntry {
    /// Whether the entry still has something to deliver or to wait for.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.state.is_open() || self.filler.as_ref().is_some_and(|f| !f.acknowledged)
    }
}

/// What a producer is told about its message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// The receiver applied it.
    Applied,
    /// The receiver refused it.
    Rejected(RejectCode),
    /// No confirmation arrived before it expired.
    Expired,
}

/// An event for the producer of a message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProducerEvent {
    /// The message.
    pub message: MessageId,
    /// Its link.
    pub link: LinkId,
    /// Its conversation.
    pub conversation: Conversation,
    /// Its sequence number.
    pub seq: u64,
    /// Its topic.
    pub topic: Topic,
    /// What happened.
    pub kind: EventKind,
    /// Unix milliseconds.
    pub at: u64,
}

/// The sender's durable state.
pub trait OutboxStore: Send + Sync {
    /// Adds a new entry (version 0 in, stored at version 1).
    ///
    /// # Errors
    /// `Conflict` when the id exists or the sequence number is not the conversation's next.
    fn insert(&self, entry: OutboxEntry) -> Result<()>;

    /// The entry whose message, or whose filler, has this id.
    ///
    /// # Errors
    /// A store failure.
    fn find(&self, id: &MessageId) -> Result<Option<OutboxEntry>>;

    /// Replaces an entry if its stored version is `entry.version`; the stored version becomes
    /// one higher.
    ///
    /// # Errors
    /// `Conflict` when the stored version differs, `NotFound` when absent.
    fn update(&self, entry: OutboxEntry) -> Result<()>;

    /// The highest sequence number used in the conversation (0 for none).
    ///
    /// # Errors
    /// A store failure.
    fn last_seq(&self, link: &LinkId, conversation: &Conversation) -> Result<u64>;

    /// Entries that are still live, oldest sequence first within a conversation. `None` lists
    /// every link.
    ///
    /// # Errors
    /// A store failure.
    fn live(&self, link: Option<&LinkId>) -> Result<Vec<OutboxEntry>>;

    /// Moves `entry` to its next state **and** appends its producer event in one transaction, so
    /// a crash can never leave an `Expired`, `Applied` or `Rejected` entry without the event
    /// that announces it. `event` is `None` when the move produces none.
    ///
    /// # Errors
    /// `Conflict` when the stored version differs, [`Error::NotFound`] when the entry is gone, a
    /// store failure.
    fn settle(&self, entry: OutboxEntry, event: Option<ProducerEvent>) -> Result<()>;

    /// Appends an event for a producer.
    ///
    /// # Errors
    /// A store failure.
    fn push_event(&self, event: ProducerEvent) -> Result<()>;

    /// Up to `limit` events after position `after` (0 reads from the start), each with its
    /// position.
    ///
    /// # Errors
    /// A store failure.
    fn events_from(&self, after: u64, limit: usize) -> Result<Vec<(u64, ProducerEvent)>>;
}

/// An inbox entry's progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryStatus {
    /// A record waiting for the apply path.
    PendingApply,
    /// Applied (a message topic is applied by being stored).
    Applied,
    /// The apply path refused it.
    Failed(RejectCode),
}

/// A message stored in the inbox, in order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEntry {
    /// The message.
    pub message: Message,
    /// Unix milliseconds the inbox took it into sequence.
    pub received_at: u64,
    /// Its progress.
    pub status: EntryStatus,
}

/// A message held behind a gap in its conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParkedEntry {
    /// The message.
    pub message: Message,
    /// Unix milliseconds it was parked.
    pub parked_at: u64,
}

/// What became of one message id on one link, and nothing about any other link's messages: the
/// inbox's dedupe is keyed by `(link, id)`, so one link can neither suppress nor replay another
/// link's message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeenKind {
    /// Stored in order.
    Stored(EntryStatus),
    /// Held behind a gap.
    Parked,
    /// Refused earlier.
    Rejected(RejectCode),
}

/// One conversation as the inbox holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConversationView {
    /// The next sequence number the inbox will take in order (1 for a new conversation).
    pub next_seq: u64,
    /// Messages held behind a gap, lowest sequence number first.
    pub parked: Vec<ParkedEntry>,
}

/// Everything one receive changes, applied atomically by [`InboxStore::commit`].
#[derive(Clone, Debug)]
pub struct InboxCommit {
    /// The link.
    pub link: LinkId,
    /// The conversation.
    pub conversation: Conversation,
    /// The `next_seq` the plan was made against; the commit fails with `Conflict` otherwise.
    pub expected_next: u64,
    /// The `next_seq` after the commit.
    pub next_seq: u64,
    /// Entries taken into sequence.
    pub store: Vec<InboxEntry>,
    /// Messages to hold behind the gap.
    pub park: Vec<ParkedEntry>,
    /// Parked messages leaving the parking area (stored or rejected by this commit).
    pub unpark: Vec<MessageId>,
    /// Messages refused by this commit.
    pub rejected: Vec<(MessageId, LinkId, RejectCode)>,
    /// Notices to append in the same transaction.
    pub notices: Vec<NoticeDraft>,
}

impl InboxCommit {
    /// An empty plan against `expected_next`.
    #[must_use]
    pub fn new(link: LinkId, conversation: Conversation, expected_next: u64) -> Self {
        Self {
            link,
            conversation,
            expected_next,
            next_seq: expected_next,
            store: Vec::new(),
            park: Vec::new(),
            unpark: Vec::new(),
            rejected: Vec::new(),
            notices: Vec::new(),
        }
    }
}

/// What one link is holding in the receiver's parking area (a recorded security finding: the caps are per link, so
/// one link's flood never refuses another link's messages).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkUsage {
    /// Messages parked for this link, across its conversations.
    pub parked: u64,
    /// The payload bytes of those parked messages.
    pub parked_bytes: u64,
    /// Conversations this link holds.
    pub conversations: u64,
}

/// What a purge keeps of the payload it emptied: the digest of the payload as it was
/// and the signature that attests to it, so a purged message can still be shown to be the one the
/// sender signed — the row keeps its ids, times and state, and now its evidence too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PurgedPayload {
    /// The link the message came on.
    pub link: LinkId,
    /// The message id.
    pub id: MessageId,
    /// SHA-256 of the payload as it was before the purge, lower-case hex.
    pub digest: String,
    /// The sender's signature over the message, verbatim.
    pub signature: SignatureBytes,
    /// Unix milliseconds it was purged.
    pub purged_at: u64,
}

impl PurgedPayload {
    /// The length of [`PurgedPayload::digest`]: a SHA-256 digest in lower-case hex.
    pub const DIGEST_HEX_LEN: usize = 64;

    /// `digest` as the lower-case hex a purge stores and reads back.
    #[must_use]
    pub fn digest_hex(digest: &[u8; 32]) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(Self::DIGEST_HEX_LEN);
        for byte in digest {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }
}

/// The receiver's durable state.
pub trait InboxStore: Send + Sync {
    /// What became of `id` **on `link`**. A message id another link used is not known here: the
    /// dedupe key is `(link, id)`.
    ///
    /// # Errors
    /// A store failure.
    fn seen(&self, link: &LinkId, id: &MessageId) -> Result<Option<SeenKind>>;

    /// What `link` holds in the parking area, for the per-link caps.
    ///
    /// # Errors
    /// A store failure.
    fn usage(&self, link: &LinkId) -> Result<LinkUsage>;

    /// One conversation.
    ///
    /// # Errors
    /// A store failure.
    fn conversation(&self, link: &LinkId, conversation: &Conversation) -> Result<ConversationView>;

    /// Applies a plan atomically: entries, parking, rejections, the sequence cursor and the
    /// notices, all or nothing.
    ///
    /// # Errors
    /// `Conflict` when `expected_next` no longer matches or a stored id already exists.
    fn commit(&self, commit: InboxCommit) -> Result<()>;

    /// Moves a pending record to its final status, appending `notice` in the same transaction.
    /// Returns the entry when it changed, `None` when it was no longer pending.
    ///
    /// # Errors
    /// A store failure.
    fn finish_record(
        &self,
        id: &MessageId,
        link: &LinkId,
        status: EntryStatus,
        notice: Option<NoticeDraft>,
    ) -> Result<Option<InboxEntry>>;

    /// Records waiting for the apply path, in `(conversation, seq)` order within a link.
    ///
    /// # Errors
    /// A store failure.
    fn pending_records(&self, limit: usize) -> Result<Vec<(LinkId, InboxEntry)>>;

    /// Every parked message, across links.
    ///
    /// # Errors
    /// A store failure.
    fn parked(&self) -> Result<Vec<(LinkId, ParkedEntry)>>;

    /// One stored entry, of `link`: an id another link stored is not this link's entry.
    ///
    /// # Errors
    /// A store failure.
    fn entry(&self, link: &LinkId, id: &MessageId) -> Result<Option<InboxEntry>>;

    /// What the purge kept of the payload of `(link, id)`, if it was purged.
    ///
    /// # Errors
    /// A store failure.
    fn purged(&self, link: &LinkId, id: &MessageId) -> Result<Option<PurgedPayload>>;

    /// Empties the payload of applied entries received no later than `older_than`, keeping the
    /// row, its state, its ids, its times and the digest and signature of the payload it replaced
    ///. Returns how many rows it purged; at most `limit` per call.
    ///
    /// # Errors
    /// A store failure.
    fn purge_applied(&self, older_than: u64, now: u64, limit: usize) -> Result<u64>;
}
