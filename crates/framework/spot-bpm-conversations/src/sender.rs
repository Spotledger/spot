//! The sending side: the [`MessageSender`] seam and its adapter over the link exchange's outbox.
//!
//! The handlers never touch an outbox directly. They ask a [`MessageSender`] to queue a message
//! under an id *they* chose, and the sender must be idempotent on that id: asking twice queues
//! one message. That is what makes a replay of a node attempt after a crash harmless, because
//! the id is derived from the attempt's idempotency key ([`message_id`]) and recorded before
//! the send is attempted.

use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use spot_link_exchange::{
    Conversation, IdSource, Lifetimes, MessageId, MessageState, Outbox, OutboxStore, Submit,
};
use spot_tenant_link::{AuditSink, ExchangePayload, IdentitySigner, Link, LinkId, Topic};

use crate::error::{ConversationError, Result};

/// What one send asks for.
#[derive(Clone, Copy)]
pub struct SendRequest<'a> {
    /// The conversation on the link.
    pub conversation: &'a Conversation,
    /// The topic the contract message travels on.
    pub topic: &'a Topic,
    /// The contract message's schema version.
    pub schema_version: u32,
    /// The message id to queue it under; sending the same id again changes nothing.
    pub id: MessageId,
    /// The wire payload: the contract fields and the header.
    pub payload: &'a ExchangePayload,
    /// Unix milliseconds.
    pub now_ms: u64,
    /// `system`, `user:<id>` or `agent:<id>`, for the audit trail.
    pub actor: &'a str,
}

/// What a send did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sent {
    /// The message's id.
    pub id: MessageId,
    /// Its sequence number in the conversation.
    pub seq: u64,
}

/// A route to one link's outbox.
pub trait MessageSender: Send + Sync {
    /// The link this sender sends over.
    fn link(&self) -> LinkId;

    /// Queues the message, once per `request.id`.
    ///
    /// # Errors
    /// [`ConversationError::FieldNotGranted`] when the grant withholds a field,
    /// [`ConversationError::Send`] when the link exchange refuses.
    fn send(&self, request: &SendRequest<'_>) -> Result<Sent>;

    /// The delivery state of a message this sender queued; `None` for an unknown id.
    ///
    /// # Errors
    /// [`ConversationError::Send`] on a store failure.
    fn state(&self, id: &MessageId) -> Result<Option<MessageState>>;
}

/// The link message id for a send, a pure function of the instant, the link and the attempt's
/// idempotency key. The instant is the one recorded with the decision to send, so a replay
/// derives the same id.
#[must_use]
pub fn message_id(now_ms: u64, link: &LinkId, key: &str) -> MessageId {
    let text = format!("{link}|{key}");
    let first = fnv64(text.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let second = fnv64(text.as_bytes(), 0x8422_2325_cbf2_9ce4);
    let mut entropy = [0u8; 10];
    entropy[..8].copy_from_slice(&first.to_be_bytes());
    entropy[8..].copy_from_slice(&second.to_be_bytes()[..2]);
    MessageId::new_v7(now_ms, entropy)
}

fn fnv64(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Hands the outbox the id the handler chose instead of drawing a fresh one.
#[derive(Default)]
struct PresetId(Mutex<Option<MessageId>>);

impl PresetId {
    fn lock(&self) -> MutexGuard<'_, Option<MessageId>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl IdSource for PresetId {
    fn next_id(&self, now: u64) -> MessageId {
        let preset = *self.lock();
        preset.unwrap_or_else(|| MessageId::new_v7(now, [0u8; 10]))
    }
}

/// An identity signer that can be shared between threads.
pub type SharedSigner = Arc<dyn SendSigner>;

/// [`IdentitySigner`] that is also `Send + Sync`, so a sender holding one can sit in the
/// engine's handler registry.
pub trait SendSigner: IdentitySigner + Send + Sync {}

impl<T: IdentitySigner + Send + Sync> SendSigner for T {}

/// The link as the host currently holds it; revoking or expiring it there stops the sender.
pub type SharedLink = Arc<RwLock<Link>>;

/// A [`MessageSender`] over `spot_link_exchange::Outbox` for one link.
pub struct LinkSender {
    link: SharedLink,
    id: LinkId,
    store: Arc<dyn OutboxStore>,
    audit: Arc<dyn AuditSink>,
    signer: SharedSigner,
    lifetimes: Lifetimes,
    preset: PresetId,
    serial: Mutex<()>,
}

impl LinkSender {
    /// A sender over `store`, signing as `signer`, for the link held in `link`.
    #[must_use]
    pub fn new(
        link: SharedLink,
        store: Arc<dyn OutboxStore>,
        audit: Arc<dyn AuditSink>,
        signer: SharedSigner,
        lifetimes: Lifetimes,
    ) -> Self {
        let id = link
            .read()
            .map(|guard| guard.id)
            .unwrap_or_else(|poisoned| poisoned.into_inner().id);
        Self {
            link,
            id,
            store,
            audit,
            signer,
            lifetimes,
            preset: PresetId::default(),
            serial: Mutex::new(()),
        }
    }

    fn current_link(&self) -> Link {
        self.link
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl MessageSender for LinkSender {
    fn link(&self) -> LinkId {
        self.id
    }

    fn send(&self, request: &SendRequest<'_>) -> Result<Sent> {
        let _serial = self
            .serial
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(entry) = self
            .store
            .find(&request.id)
            .map_err(|error| ConversationError::Send { code: error.code() })?
        {
            return Ok(Sent {
                id: request.id,
                seq: entry.message.seq,
            });
        }
        let link = self.current_link();
        if let Some(grant) = link.grant(request.topic) {
            let withheld = grant.fields.filter(request.payload).withheld().len();
            if withheld > 0 {
                return Err(ConversationError::FieldNotGranted { withheld });
            }
        }
        *self.preset.lock() = Some(request.id);
        let outbox = Outbox::new(
            &*self.store,
            &*self.audit,
            &*self.signer,
            &self.preset,
            self.lifetimes.clone(),
        );
        let queued = outbox.enqueue_message(
            &Submit {
                link: &link,
                conversation: request.conversation,
                topic: request.topic,
                schema_version: request.schema_version,
                now: request.now_ms,
                actor: request.actor,
                oracle: None,
            },
            request.payload,
        );
        *self.preset.lock() = None;
        let queued = queued.map_err(|error| ConversationError::Send { code: error.code() })?;
        Ok(Sent {
            id: queued.id,
            seq: queued.seq,
        })
    }

    fn state(&self, id: &MessageId) -> Result<Option<MessageState>> {
        Ok(self
            .store
            .find(id)
            .map_err(|error| ConversationError::Send { code: error.code() })?
            .map(|entry| entry.state))
    }
}
