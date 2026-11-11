//! The conversation record: what one tenant knows of one conversation, and where it keeps it.
//!
//! A record is the durable memory of a conversation on this side: which contract version it runs
//! on, which role each side plays, the messages sent and received in the order this side saw
//! them, which received messages a request has already consumed, the waits that are registered,
//! and how the conversation ended. Every handler and the driver read and write it through a
//! [`ConversationStore`] with an optimistic version, so a restart loses nothing and two writers
//! cannot overwrite each other.
//!
//! History is bounded ([`MAX_EVENTS`]): a contract's loops are bounded, so a conversation that
//! outgrows the cap is misbehaving, and refusing it is safer than growing without limit.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use serde_json::Value;
use spot_bpm_contract::{OutcomeKind, ProcessContract};
use spot_bpm_model::{RejectAction, TimeoutAction};
use spot_link_exchange::{Conversation, MessageId, RejectCode};
use spot_tenant_link::LinkId;

use crate::error::{ConversationError, Result};
use crate::order::{OrderState, ending_after, take_step};

/// The most messages (sent and received) one conversation record holds.
pub const MAX_EVENTS: usize = 512;
/// The most waits one conversation holds at once.
pub const MAX_WAITS: usize = 32;
/// The most rejected message ids one tenant remembers.
pub const MAX_REMEMBERED_REJECTIONS: usize = 4_096;

/// The bounded memory of what a message was rejected with.
///
/// A peer can send many bad messages, so the cache is bounded: forgetting the oldest only means
/// a very old duplicate is judged again (and refused again). The key is `(link, message id)`,
/// because a message id says nothing across links: the same id on another link is another
/// message, and a redelivery on *this* link must get *this* link's answer.
///
/// Every [`ConversationStore`] keeps its rejections in one of these, so the bound holds for
/// every backend rather than only for the reference one.
#[derive(Debug)]
pub struct RejectionCache {
    entries: BTreeMap<(MessageId, LinkId), RejectCode>,
    max: usize,
}

impl Default for RejectionCache {
    fn default() -> Self {
        Self::new(MAX_REMEMBERED_REJECTIONS)
    }
}

impl RejectionCache {
    /// An empty cache that holds at most `max` rejections.
    #[must_use]
    pub fn new(max: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            max,
        }
    }

    /// The code a message was rejected with earlier.
    #[must_use]
    pub fn get(&self, link: &LinkId, id: &MessageId) -> Option<RejectCode> {
        self.entries.get(&(*id, *link)).copied()
    }

    /// Remembers a rejection, forgetting the oldest when the bound is reached.
    pub fn remember(&mut self, link: LinkId, id: MessageId, code: RejectCode) {
        self.entries.insert((id, link), code);
        while self.entries.len() > self.max {
            // Ids are UUIDv7: the smallest is the oldest.
            let Some(oldest) = self.entries.keys().next().copied() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    /// How many rejections are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Which way a message went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// This side sent it.
    Sent,
    /// This side received it.
    Received,
}

/// One message in the conversation, in the order this side saw it.
#[derive(Clone, PartialEq)]
pub struct Event {
    /// Which way it went.
    pub direction: Direction,
    /// The contract message's name.
    pub message: String,
    /// The link message's id.
    pub id: MessageId,
    /// Unix milliseconds this side recorded it.
    pub at_ms: u64,
    /// The contract fields of a received message, as plain JSON; `None` for a sent one.
    pub payload: Option<Value>,
}

impl fmt::Debug for Event {
    /// The payload is business data and never printed.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Event")
            .field("direction", &self.direction)
            .field("message", &self.message)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// How far a delivery must have got before a send step continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaitFor {
    /// The message is in the outbox.
    #[default]
    Queued,
    /// The receiver holds it.
    Acknowledged,
    /// The receiver applied it.
    Applied,
}

impl WaitFor {
    /// Reads the node-config token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "queued" => Some(Self::Queued),
            "acknowledged" => Some(Self::Acknowledged),
            "applied" => Some(Self::Applied),
            _ => None,
        }
    }
}

/// What a registered wait is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitKind {
    /// A wait-for-message node waits for a contract message.
    Message {
        /// The contract message awaited.
        message: String,
        /// What happens when the limit passes.
        on_timeout: TimeoutAction,
    },
    /// A send node waits for its message to be delivered far enough.
    Delivery {
        /// The message sent.
        id: MessageId,
        /// How far it must get.
        wait_for: WaitFor,
        /// What happens when the receiver rejects it.
        on_reject: RejectAction,
    },
}

/// One registered wait of a node attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    /// The waiting node.
    pub node: String,
    /// The engine attempt that registered the wait.
    pub attempt: u32,
    /// What it is waiting for.
    pub kind: WaitKind,
    /// The Unix millisecond the wait runs out, when it has a limit.
    pub due_ms: Option<u64>,
}

/// How the conversation ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending {
    /// The contract outcome's name.
    pub outcome: String,
    /// What kind of ending it is.
    pub kind: OutcomeKind,
    /// Unix milliseconds.
    pub at_ms: u64,
    /// Whether the peer's message ended it.
    pub by_peer: bool,
    /// Whether a wait limit ran out.
    pub timed_out: bool,
}

/// The visible marker that a person has to look at the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attention {
    /// The stable code of the reason.
    pub code: String,
    /// Unix milliseconds it was raised.
    pub at_ms: u64,
}

/// A send the handler decided on, kept before the message is queued so a replay of the same
/// `(request, node, attempt)` sends the same message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    /// The link message id chosen for it.
    pub id: MessageId,
    /// The contract message's name.
    pub message: String,
}

/// What one tenant knows of one conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationRecord {
    /// The store's optimistic version; the store raises it on every save.
    pub version: u64,
    /// The link the conversation travels on.
    pub link: LinkId,
    /// The conversation's name on that link.
    pub conversation: Conversation,
    /// The contract's id.
    pub contract: String,
    /// The contract version the conversation runs on, pinned at its start.
    pub contract_version: u32,
    /// The role this side plays.
    pub role: String,
    /// The role the peer plays.
    pub peer_role: String,
    /// The request (Case) this conversation belongs to, once it has one.
    pub case: Option<String>,
    /// Sent and received messages, in the order this side saw them.
    pub events: Vec<Event>,
    /// How often each order step was taken.
    pub counts: BTreeMap<String, u32>,
    /// Sends decided, by idempotency key.
    pub intents: BTreeMap<String, Intent>,
    /// Received messages a request already consumed.
    pub consumed: BTreeSet<MessageId>,
    /// The received message each wait attempt `(node, attempt)` took, so a replay of the same
    /// attempt gets the same message instead of registering a wait for one that never returns.
    pub consumed_by: BTreeMap<(String, u32), MessageId>,
    /// Registered waits.
    pub waits: Vec<Wait>,
    /// How it ended, once it did.
    pub ending: Option<Ending>,
    /// The marker for a person, when the conversation is stuck on something.
    pub attention: Option<Attention>,
    /// Received messages whose arrival the driver has fully handled.
    pub arrivals_done: BTreeSet<MessageId>,
    /// Whether the compensation hook already ran for a peer's cancellation.
    pub compensated: bool,
}

impl ConversationRecord {
    /// A fresh record for a conversation on `link`.
    #[must_use]
    pub fn new(
        link: LinkId,
        conversation: Conversation,
        contract: &ProcessContract,
        role: impl Into<String>,
        peer_role: impl Into<String>,
    ) -> Self {
        Self {
            version: 0,
            link,
            conversation,
            contract: contract.id.clone(),
            contract_version: contract.version,
            role: role.into(),
            peer_role: peer_role.into(),
            case: None,
            events: Vec::new(),
            counts: BTreeMap::new(),
            intents: BTreeMap::new(),
            consumed: BTreeSet::new(),
            consumed_by: BTreeMap::new(),
            waits: Vec::new(),
            ending: None,
            attention: None,
            arrivals_done: BTreeSet::new(),
            compensated: false,
        }
    }

    /// The order state the contract rules are checked against.
    #[must_use]
    pub fn order_state(&self) -> OrderState<'_> {
        OrderState {
            last: self.events.last().map(|event| event.message.as_str()),
            counts: &self.counts,
            ended: self.ending.is_some(),
        }
    }

    /// Whether a message with this id is already part of the history.
    #[must_use]
    pub fn has_event(&self, id: &MessageId) -> bool {
        self.events.iter().any(|event| &event.id == id)
    }

    /// Records a message this side sent. Idempotent on the message id.
    ///
    /// # Errors
    /// [`ConversationError::TooLong`] when the history is full.
    pub fn append_sent(
        &mut self,
        contract: &ProcessContract,
        message: &str,
        id: MessageId,
        at_ms: u64,
    ) -> Result<()> {
        let by = self.role.clone();
        self.append(contract, &by, Direction::Sent, message, id, at_ms, None)
    }

    /// Records a message this side received. Idempotent on the message id.
    ///
    /// # Errors
    /// [`ConversationError::TooLong`] when the history is full.
    pub fn append_received(
        &mut self,
        contract: &ProcessContract,
        message: &str,
        id: MessageId,
        at_ms: u64,
        payload: Value,
    ) -> Result<()> {
        let by = self.peer_role.clone();
        self.append(
            contract,
            &by,
            Direction::Received,
            message,
            id,
            at_ms,
            Some(payload),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn append(
        &mut self,
        contract: &ProcessContract,
        by: &str,
        direction: Direction,
        message: &str,
        id: MessageId,
        at_ms: u64,
        payload: Option<Value>,
    ) -> Result<()> {
        if self.has_event(&id) {
            return Ok(());
        }
        if self.events.len() >= MAX_EVENTS {
            return Err(ConversationError::TooLong);
        }
        let last = self.events.last().map(|event| event.message.clone());
        take_step(contract, last.as_deref(), by, message, &mut self.counts);
        self.events.push(Event {
            direction,
            message: message.to_owned(),
            id,
            at_ms,
            payload,
        });
        if self.ending.is_none()
            && let Some(outcome) = ending_after(contract, message)
        {
            self.ending = Some(Ending {
                outcome: outcome.name.clone(),
                kind: outcome.kind,
                at_ms,
                by_peer: direction == Direction::Received,
                timed_out: false,
            });
        }
        Ok(())
    }

    /// The oldest received message with this contract name that no request consumed yet.
    #[must_use]
    pub fn unconsumed(&self, message: &str) -> Option<&Event> {
        self.events.iter().find(|event| {
            event.direction == Direction::Received
                && event.message == message
                && !self.consumed.contains(&event.id)
        })
    }

    /// The most recent sent message's name.
    #[must_use]
    pub fn last_sent(&self) -> Option<&Event> {
        self.events
            .iter()
            .rev()
            .find(|event| event.direction == Direction::Sent)
    }

    /// The most recent received message's name.
    #[must_use]
    pub fn last_received(&self) -> Option<&Event> {
        self.events
            .iter()
            .rev()
            .find(|event| event.direction == Direction::Received)
    }

    /// Registers a wait, replacing one the same node already registered for the same attempt.
    ///
    /// # Errors
    /// [`ConversationError::TooLong`] when too many waits are open.
    pub fn register_wait(&mut self, wait: Wait) -> Result<()> {
        self.waits
            .retain(|w| !(w.node == wait.node && w.attempt <= wait.attempt));
        if self.waits.len() >= MAX_WAITS {
            return Err(ConversationError::TooLong);
        }
        self.waits.push(wait);
        Ok(())
    }

    /// Raises the marker for a person; the first reason stays.
    pub fn mark_attention(&mut self, code: &str, at_ms: u64) {
        if self.attention.is_none() {
            self.attention = Some(Attention {
                code: code.to_owned(),
                at_ms,
            });
        }
    }
}

/// Where conversation records live.
///
/// Every method is `&self`; a store uses interior mutability or a connection pool. `save` and
/// `create` are optimistic: a record saved against a stale version is refused with
/// [`ConversationError::Conflict`], and callers reload and retry ([`update`]).
pub trait ConversationStore: Send + Sync {
    /// The record of a conversation on a link.
    ///
    /// # Errors
    /// [`ConversationError::Store`] on a backend failure.
    fn load(
        &self,
        link: &LinkId,
        conversation: &Conversation,
    ) -> Result<Option<ConversationRecord>>;

    /// Stores a new record (`version` 0), returning it as stored.
    ///
    /// # Errors
    /// [`ConversationError::Conflict`] when the conversation already has a record.
    fn create(&self, record: ConversationRecord) -> Result<ConversationRecord>;

    /// Replaces a record if its stored version is `record.version`, returning it as stored.
    ///
    /// # Errors
    /// [`ConversationError::Conflict`] when the stored version differs.
    fn save(&self, record: ConversationRecord) -> Result<ConversationRecord>;

    /// Every record with at least one registered wait.
    ///
    /// # Errors
    /// [`ConversationError::Store`] on a backend failure.
    fn waiting(&self) -> Result<Vec<ConversationRecord>>;

    /// Every record that belongs to a request.
    ///
    /// # Errors
    /// [`ConversationError::Store`] on a backend failure.
    fn of_case(&self, case: &str) -> Result<Vec<ConversationRecord>>;

    /// How many conversations on `link` are still open (records without an ending), so a
    /// receiving gate can bound what one partner can make this tenant hold.
    ///
    /// # Errors
    /// [`ConversationError::Store`] on a backend failure.
    fn open_on(&self, link: &LinkId) -> Result<usize>;

    /// The code a message was rejected with earlier, so a redelivery gets the same answer.
    ///
    /// The answer is per link: the same message id on another link is another message.
    ///
    /// # Errors
    /// [`ConversationError::Store`] on a backend failure.
    fn rejection(&self, link: &LinkId, id: &MessageId) -> Result<Option<RejectCode>>;

    /// Remembers a rejection. An implementation bounds what it remembers with a
    /// [`RejectionCache`] (a peer can send many bad messages); forgetting the oldest only means a
    /// very old duplicate is judged again.
    ///
    /// # Errors
    /// [`ConversationError::Store`] on a backend failure.
    fn note_rejection(&self, link: LinkId, id: MessageId, code: RejectCode) -> Result<()>;
}

/// Loads a record, applies `change`, saves it, and retries from a fresh load when another writer
/// got there first. Returns what `change` returned and the record as stored.
///
/// # Errors
/// [`ConversationError::NoConversation`] when the record does not exist, what `change` returns,
/// or [`ConversationError::Conflict`] when it still conflicts after a few tries.
pub fn update<R>(
    store: &dyn ConversationStore,
    link: &LinkId,
    conversation: &Conversation,
    mut change: impl FnMut(&mut ConversationRecord) -> Result<R>,
) -> Result<(R, ConversationRecord)> {
    for _ in 0..8 {
        let mut record = store
            .load(link, conversation)?
            .ok_or(ConversationError::NoConversation)?;
        let result = change(&mut record)?;
        match store.save(record) {
            Ok(saved) => return Ok((result, saved)),
            Err(ConversationError::Conflict) => {}
            Err(error) => return Err(error),
        }
    }
    Err(ConversationError::Conflict)
}

/// The in-memory reference [`ConversationStore`].
#[derive(Default)]
pub struct MemoryConversationStore {
    inner: Mutex<MemoryInner>,
}

#[derive(Default)]
struct MemoryInner {
    records: BTreeMap<(LinkId, Conversation), ConversationRecord>,
    rejections: RejectionCache,
}

impl MemoryConversationStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every record held, for tests and operator tooling over the reference store.
    #[must_use]
    pub fn records(&self) -> Vec<ConversationRecord> {
        self.lock().records.values().cloned().collect()
    }

    /// How many rejections the cache remembers, for tests and operator tooling.
    #[must_use]
    pub fn remembered_rejections(&self) -> usize {
        self.lock().rejections.len()
    }

    fn lock(&self) -> MutexGuard<'_, MemoryInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ConversationStore for MemoryConversationStore {
    fn load(
        &self,
        link: &LinkId,
        conversation: &Conversation,
    ) -> Result<Option<ConversationRecord>> {
        Ok(self
            .lock()
            .records
            .get(&(*link, conversation.clone()))
            .cloned())
    }

    fn create(&self, mut record: ConversationRecord) -> Result<ConversationRecord> {
        let mut inner = self.lock();
        let key = (record.link, record.conversation.clone());
        if inner.records.contains_key(&key) {
            return Err(ConversationError::Conflict);
        }
        record.version = 1;
        inner.records.insert(key, record.clone());
        Ok(record)
    }

    fn save(&self, mut record: ConversationRecord) -> Result<ConversationRecord> {
        let mut inner = self.lock();
        let key = (record.link, record.conversation.clone());
        match inner.records.get(&key) {
            Some(stored) if stored.version == record.version => {
                record.version = record.version.saturating_add(1);
                inner.records.insert(key, record.clone());
                Ok(record)
            }
            _ => Err(ConversationError::Conflict),
        }
    }

    fn waiting(&self) -> Result<Vec<ConversationRecord>> {
        Ok(self
            .lock()
            .records
            .values()
            .filter(|record| !record.waits.is_empty())
            .cloned()
            .collect())
    }

    fn of_case(&self, case: &str) -> Result<Vec<ConversationRecord>> {
        Ok(self
            .lock()
            .records
            .values()
            .filter(|record| record.case.as_deref() == Some(case))
            .cloned()
            .collect())
    }

    fn open_on(&self, link: &LinkId) -> Result<usize> {
        Ok(self
            .lock()
            .records
            .values()
            .filter(|record| &record.link == link && record.ending.is_none())
            .count())
    }

    fn rejection(&self, link: &LinkId, id: &MessageId) -> Result<Option<RejectCode>> {
        Ok(self.lock().rejections.get(link, id))
    }

    fn note_rejection(&self, link: LinkId, id: MessageId, code: RejectCode) -> Result<()> {
        self.lock().rejections.remember(link, id, code);
        Ok(())
    }
}
