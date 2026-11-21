//! The after-commit event buffer (ADR-25: events publish **after commit**).

use spot_core::SiteId;

use crate::bus::RealtimeBus;
use crate::room::Room;

/// One buffered publish, filled in by [`PendingEvents::push`].
#[derive(Debug)]
struct Pending {
    site: SiteId,
    room: Room,
    kind: String,
    payload: serde_json::Value,
}

/// Events accumulated during a database transaction and published only once it commits.
///
/// The transaction layer [`push`](PendingEvents::push)es notifications while it works; on a
/// successful commit it calls [`flush`](PendingEvents::flush), which publishes them in
/// insertion order. **Flushing is the commit path only.** Dropping a `PendingEvents` — or
/// calling [`discard`](PendingEvents::discard) — without flushing is the rollback path: the
/// buffered events are thrown away and no subscriber ever sees them. Nothing here blocks or
/// awaits — `flush` is synchronous because [`RealtimeBus::publish`] is.
///
/// ```text
/// let mut pending = PendingEvents::new();
/// tx.do_work(&mut pending)?;    // push doc_updated etc. as rows change
/// tx.commit()?;                 // only after this line...
/// pending.flush(&bus);          // ...do subscribers see the events
/// // on Err: `pending` drops here; nothing was published.
/// ```
#[derive(Debug, Default)]
pub struct PendingEvents {
    events: Vec<Pending>,
}

impl PendingEvents {
    /// An empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Buffers one notification for `(site, room)`. Publishes nothing yet.
    pub fn push(
        &mut self,
        site: &SiteId,
        room: &Room,
        kind: impl Into<String>,
        payload: serde_json::Value,
    ) {
        self.events.push(Pending {
            site: site.clone(),
            room: room.clone(),
            kind: kind.into(),
            payload,
        });
    }

    /// Publishes every buffered event to `bus` in insertion order and empties the buffer.
    /// Returns the `seq` of each published event, in the same order. Call this only after a
    /// successful commit — that is the ADR-25 contract with the transaction layer.
    pub fn flush(&mut self, bus: &RealtimeBus) -> Vec<u64> {
        self.events
            .drain(..)
            .map(|p| bus.publish(&p.site, &p.room, p.kind, p.payload))
            .collect()
    }

    /// Discards every buffered event without publishing — the explicit rollback path. An
    /// unflushed `PendingEvents` dropped at the end of scope is equivalent.
    pub fn discard(&mut self) {
        self.events.clear();
    }

    /// Buffered events not yet flushed.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}
