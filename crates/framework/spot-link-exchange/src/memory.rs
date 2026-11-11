//! In-memory stores: the reference for tests and for a synchronous host.
//!
//! Each method takes one lock for its whole body, so `commit` is atomic exactly as the
//! PostgreSQL version is (one transaction).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

use spot_tenant_link::LinkId;

use crate::error::{Error, Result};
use crate::ids::{Conversation, MessageId};
use crate::message::TOMBSTONE_PAYLOAD;
use crate::notice::{Notice, NoticeDraft, NoticeFeed};
use crate::store::{
    ConversationView, EntryStatus, InboxCommit, InboxEntry, InboxStore, LinkUsage, OutboxEntry,
    OutboxStore, ParkedEntry, ProducerEvent, PurgedPayload, SeenKind,
};

fn poisoned<T>(_: std::sync::PoisonError<T>) -> Error {
    Error::StoreFailed {
        reason: "a store lock is poisoned",
    }
}

#[derive(Default)]
struct OutboxState {
    entries: BTreeMap<MessageId, OutboxEntry>,
    filler_index: BTreeMap<MessageId, MessageId>,
    events: Vec<ProducerEvent>,
}

/// An in-memory [`OutboxStore`].
#[derive(Default)]
pub struct MemoryOutbox {
    state: Mutex<OutboxState>,
}

impl MemoryOutbox {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, OutboxState>> {
        self.state.lock().map_err(poisoned)
    }

    /// Every entry, live or settled (tests and screens).
    ///
    /// # Errors
    /// A poisoned lock.
    pub fn all(&self) -> Result<Vec<OutboxEntry>> {
        let state = self.lock()?;
        let mut all: Vec<OutboxEntry> = state.entries.values().cloned().collect();
        all.sort_by(|a, b| {
            (&a.message.link, &a.message.conversation, a.message.seq).cmp(&(
                &b.message.link,
                &b.message.conversation,
                b.message.seq,
            ))
        });
        Ok(all)
    }
}

/// The version-guarded write both `update` and `settle` make: one place, so a `settle` can never
/// store a state its event does not describe.
fn store_update(state: &mut OutboxState, mut entry: OutboxEntry) -> Result<()> {
    let stored = state
        .entries
        .get(&entry.message.id)
        .ok_or(Error::NotFound)?;
    if stored.version != entry.version {
        return Err(Error::Conflict {
            reason: "the entry changed since it was read",
        });
    }
    entry.version += 1;
    if let Some(filler) = &entry.filler {
        state
            .filler_index
            .insert(filler.message.id, entry.message.id);
    }
    state.entries.insert(entry.message.id, entry);
    Ok(())
}

impl OutboxStore for MemoryOutbox {
    fn insert(&self, mut entry: OutboxEntry) -> Result<()> {
        let mut state = self.lock()?;
        if state.entries.contains_key(&entry.message.id) {
            return Err(Error::Conflict {
                reason: "the message id exists",
            });
        }
        let last = state
            .entries
            .values()
            .filter(|e| {
                e.message.link == entry.message.link
                    && e.message.conversation == entry.message.conversation
            })
            .map(|e| e.message.seq)
            .max()
            .unwrap_or(0);
        if entry.message.seq != last.saturating_add(1) {
            return Err(Error::Conflict {
                reason: "the sequence number is not the conversation's next",
            });
        }
        entry.version = 1;
        state.entries.insert(entry.message.id, entry);
        Ok(())
    }

    fn find(&self, id: &MessageId) -> Result<Option<OutboxEntry>> {
        let state = self.lock()?;
        let key = state.filler_index.get(id).copied().unwrap_or(*id);
        Ok(state.entries.get(&key).cloned())
    }

    fn update(&self, entry: OutboxEntry) -> Result<()> {
        let mut state = self.lock()?;
        store_update(&mut state, entry)
    }

    fn settle(&self, entry: OutboxEntry, event: Option<ProducerEvent>) -> Result<()> {
        let mut state = self.lock()?;
        store_update(&mut state, entry)?;
        if let Some(event) = event {
            state.events.push(event);
        }
        Ok(())
    }

    fn last_seq(&self, link: &LinkId, conversation: &Conversation) -> Result<u64> {
        let state = self.lock()?;
        Ok(state
            .entries
            .values()
            .filter(|e| &e.message.link == link && &e.message.conversation == conversation)
            .map(|e| e.message.seq)
            .max()
            .unwrap_or(0))
    }

    fn live(&self, link: Option<&LinkId>) -> Result<Vec<OutboxEntry>> {
        let state = self.lock()?;
        let mut live: Vec<OutboxEntry> = state
            .entries
            .values()
            .filter(|e| e.is_live() && link.is_none_or(|l| l == &e.message.link))
            .cloned()
            .collect();
        live.sort_by(|a, b| {
            (&a.message.link, &a.message.conversation, a.message.seq).cmp(&(
                &b.message.link,
                &b.message.conversation,
                b.message.seq,
            ))
        });
        Ok(live)
    }

    fn push_event(&self, event: ProducerEvent) -> Result<()> {
        self.lock()?.events.push(event);
        Ok(())
    }

    fn events_from(&self, after: u64, limit: usize) -> Result<Vec<(u64, ProducerEvent)>> {
        let state = self.lock()?;
        let skip = usize::try_from(after).unwrap_or(usize::MAX);
        Ok(state
            .events
            .iter()
            .enumerate()
            .skip(skip)
            .take(limit)
            .map(|(index, event)| (index as u64 + 1, event.clone()))
            .collect())
    }
}

#[derive(Default)]
struct Conv {
    next_seq: u64,
    parked: BTreeMap<u64, ParkedEntry>,
}

#[derive(Default)]
struct InboxState {
    convs: BTreeMap<(LinkId, Conversation), Conv>,
    /// Keyed by `(link, id)`: a message id is only meaningful on the link that carried it, so two
    /// links may use the same id without seeing each other's outcome.
    seen: BTreeMap<(LinkId, MessageId), SeenKind>,
    entries: BTreeMap<(LinkId, MessageId), InboxEntry>,
    /// What each purged payload was, keyed like the entry it belongs to.
    purged: BTreeMap<(LinkId, MessageId), PurgedPayload>,
    notices: Vec<Notice>,
    acks: BTreeMap<String, u64>,
}

impl InboxState {
    fn append(&mut self, draft: NoticeDraft) {
        let cursor = self.notices.len() as u64 + 1;
        self.notices.push(Notice {
            cursor,
            link: draft.link,
            topic: draft.topic,
            message_id: draft.message_id,
            conversation: draft.conversation,
            source: draft.source,
            received_at: draft.received_at,
        });
    }
}

/// An in-memory [`InboxStore`] and [`NoticeFeed`] in one: the notice is appended under the same
/// lock as the entry it announces.
#[derive(Default)]
pub struct MemoryInbox {
    state: Mutex<InboxState>,
}

impl MemoryInbox {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, InboxState>> {
        self.state.lock().map_err(poisoned)
    }

    /// Every stored entry, in `(link, conversation, seq)` order (tests and screens).
    ///
    /// # Errors
    /// A poisoned lock.
    pub fn stored(&self) -> Result<Vec<(LinkId, InboxEntry)>> {
        let state = self.lock()?;
        let mut all: Vec<(LinkId, InboxEntry)> = state
            .entries
            .iter()
            .map(|((link, _), entry)| (*link, entry.clone()))
            .collect();
        all.sort_by(|a, b| {
            (&a.0, &a.1.message.conversation, a.1.message.seq).cmp(&(
                &b.0,
                &b.1.message.conversation,
                b.1.message.seq,
            ))
        });
        Ok(all)
    }
}

impl InboxStore for MemoryInbox {
    fn seen(&self, link: &LinkId, id: &MessageId) -> Result<Option<SeenKind>> {
        Ok(self.lock()?.seen.get(&(*link, *id)).copied())
    }

    fn usage(&self, link: &LinkId) -> Result<LinkUsage> {
        let state = self.lock()?;
        let mut usage = LinkUsage {
            conversations: state.convs.keys().filter(|(held, _)| held == link).count() as u64,
            ..LinkUsage::default()
        };
        for ((held, _), conv) in &state.convs {
            if held != link {
                continue;
            }
            for parked in conv.parked.values() {
                usage.parked += 1;
                usage.parked_bytes += parked.message.payload.len() as u64;
            }
        }
        Ok(usage)
    }

    fn conversation(&self, link: &LinkId, conversation: &Conversation) -> Result<ConversationView> {
        let state = self.lock()?;
        Ok(match state.convs.get(&(*link, conversation.clone())) {
            Some(conv) => ConversationView {
                next_seq: conv.next_seq.max(1),
                parked: conv.parked.values().cloned().collect(),
            },
            None => ConversationView {
                next_seq: 1,
                parked: Vec::new(),
            },
        })
    }

    fn commit(&self, commit: InboxCommit) -> Result<()> {
        let mut state = self.lock()?;
        let key = (commit.link, commit.conversation.clone());
        let current = state.convs.get(&key).map_or(1, |c| c.next_seq.max(1));
        if current != commit.expected_next {
            return Err(Error::Conflict {
                reason: "the conversation moved on",
            });
        }
        let leaving: BTreeSet<MessageId> = commit.unpark.iter().copied().collect();
        let free =
            |state: &InboxState, link: LinkId, id: &MessageId| match state.seen.get(&(link, *id)) {
                None => true,
                Some(SeenKind::Parked) => leaving.contains(id),
                Some(_) => false,
            };
        let clash = commit
            .store
            .iter()
            .any(|e| !free(&state, commit.link, &e.message.id))
            || commit
                .rejected
                .iter()
                .any(|(id, link, _)| !free(&state, *link, id))
            || commit
                .park
                .iter()
                .any(|p| state.seen.contains_key(&(commit.link, p.message.id)));
        if clash {
            return Err(Error::Conflict {
                reason: "a message in the plan is already known",
            });
        }
        let conv = state.convs.entry(key).or_default();
        for id in &commit.unpark {
            conv.parked.retain(|_, p| &p.message.id != id);
        }
        for parked in &commit.park {
            conv.parked.insert(parked.message.seq, parked.clone());
        }
        conv.next_seq = commit.next_seq;
        // A parked message that leaves without being stored or refused was a tombstone: its slot
        // is consumed and a repeat is answered as applied.
        for id in &commit.unpark {
            let settled = commit.store.iter().any(|e| &e.message.id == id)
                || commit.rejected.iter().any(|(r, _, _)| r == id);
            if !settled && let Some(kind) = state.seen.get_mut(&(commit.link, *id)) {
                *kind = SeenKind::Stored(EntryStatus::Applied);
            }
        }
        for parked in commit.park {
            state
                .seen
                .insert((commit.link, parked.message.id), SeenKind::Parked);
        }
        for entry in commit.store {
            state.seen.insert(
                (commit.link, entry.message.id),
                SeenKind::Stored(entry.status),
            );
            state.entries.insert((commit.link, entry.message.id), entry);
        }
        for (id, link, code) in commit.rejected {
            state.seen.insert((link, id), SeenKind::Rejected(code));
        }
        for notice in commit.notices {
            state.append(notice);
        }
        Ok(())
    }

    fn finish_record(
        &self,
        id: &MessageId,
        link: &LinkId,
        status: EntryStatus,
        notice: Option<NoticeDraft>,
    ) -> Result<Option<InboxEntry>> {
        let mut state = self.lock()?;
        let Some(entry) = state.entries.get_mut(&(*link, *id)) else {
            return Ok(None);
        };
        if entry.status != EntryStatus::PendingApply {
            return Ok(None);
        }
        entry.status = status;
        let done = entry.clone();
        if let Some(kind) = state.seen.get_mut(&(*link, *id)) {
            *kind = SeenKind::Stored(status);
        }
        if let Some(notice) = notice {
            state.append(notice);
        }
        Ok(Some(done))
    }

    fn pending_records(&self, limit: usize) -> Result<Vec<(LinkId, InboxEntry)>> {
        Ok(self
            .stored()?
            .into_iter()
            .filter(|(_, e)| e.status == EntryStatus::PendingApply)
            .take(limit)
            .collect())
    }

    fn parked(&self) -> Result<Vec<(LinkId, ParkedEntry)>> {
        let state = self.lock()?;
        Ok(state
            .convs
            .iter()
            .flat_map(|((link, _), conv)| conv.parked.values().map(|p| (*link, p.clone())))
            .collect())
    }

    fn entry(&self, link: &LinkId, id: &MessageId) -> Result<Option<InboxEntry>> {
        Ok(self.lock()?.entries.get(&(*link, *id)).cloned())
    }

    fn purged(&self, link: &LinkId, id: &MessageId) -> Result<Option<PurgedPayload>> {
        Ok(self.lock()?.purged.get(&(*link, *id)).cloned())
    }

    fn purge_applied(&self, older_than: u64, now: u64, limit: usize) -> Result<u64> {
        let mut state = self.lock()?;
        // The digest and the signature are of the payload as it was: they are taken before the
        // payload is replaced, under the same lock, so a purged row always carries its evidence
        //.
        let due: Vec<(LinkId, MessageId)> = state
            .entries
            .iter()
            .filter(|((link, id), entry)| {
                entry.status == EntryStatus::Applied
                    && entry.received_at <= older_than
                    && !state.purged.contains_key(&(*link, *id))
            })
            .map(|((link, id), _)| (*link, *id))
            .take(limit)
            .collect();
        for key in &due {
            let Some(entry) = state.entries.get_mut(key) else {
                continue;
            };
            let digest = PurgedPayload::digest_hex(&entry.message.payload_digest());
            let signature = entry.message.sender_signature;
            entry.message.payload = TOMBSTONE_PAYLOAD.to_owned();
            state.purged.insert(
                *key,
                PurgedPayload {
                    link: key.0,
                    id: key.1,
                    digest,
                    signature,
                    purged_at: now,
                },
            );
        }
        Ok(due.len() as u64)
    }
}

impl NoticeFeed for MemoryInbox {
    fn read_from(&self, after: u64, limit: usize) -> Result<Vec<Notice>> {
        let state = self.lock()?;
        let skip = usize::try_from(after).unwrap_or(usize::MAX);
        Ok(state
            .notices
            .iter()
            .skip(skip)
            .take(limit)
            .cloned()
            .collect())
    }

    fn ack(&self, consumer: &str, cursor: u64) -> Result<()> {
        let mut state = self.lock()?;
        let held = state.acks.entry(consumer.to_owned()).or_insert(0);
        if cursor > *held {
            *held = cursor;
        }
        Ok(())
    }

    fn acked(&self, consumer: &str) -> Result<u64> {
        Ok(self.lock()?.acks.get(consumer).copied().unwrap_or(0))
    }
}
