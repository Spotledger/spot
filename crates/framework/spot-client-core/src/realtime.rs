//! Realtime state: per-room sequence tracking, resync, and notification de-duplication.

use std::collections::{BTreeMap, VecDeque};

use serde_json::Value;

use crate::core::Core;
use crate::types::{Event, Output, RealtimeMsg};

/// Notification ids remembered for de-duplication.
const SEEN_LIMIT: usize = 256;

/// The rooms the application listens to.
#[derive(Debug, Default)]
pub(crate) struct Rooms {
    /// Room name to the last `seq` seen (`None` before the first event or after a resync).
    last_seq: BTreeMap<String, Option<u64>>,
    seen: VecDeque<String>,
}

impl Core {
    pub(crate) fn subscribe(&mut self, room: String, out: &mut Vec<Output>) {
        let resume_after = *self.rooms.last_seq.entry(room.clone()).or_insert(None);
        out.push(Output::Emit(Event::RealtimeJoin { room, resume_after }));
    }

    pub(crate) fn on_realtime(&mut self, message: RealtimeMsg, out: &mut Vec<Output>) {
        match message {
            RealtimeMsg::Joined { room, outcome } => {
                if !self.rooms.last_seq.contains_key(&room) {
                    return;
                }
                match outcome.as_str() {
                    "resync_required" => self.resync(&room, out),
                    "joined" => {
                        self.rooms.last_seq.insert(room, None);
                    }
                    _ => {}
                }
            }
            RealtimeMsg::Event {
                room,
                seq,
                event,
                payload,
            } => self.on_room_event(&room, seq, &event, &payload, out),
            RealtimeMsg::Disconnected => {
                for (room, last) in &self.rooms.last_seq {
                    out.push(Output::Emit(Event::RealtimeJoin {
                        room: room.clone(),
                        resume_after: *last,
                    }));
                }
            }
        }
    }

    fn on_room_event(
        &mut self,
        room: &str,
        seq: u64,
        event: &str,
        payload: &Value,
        out: &mut Vec<Output>,
    ) {
        let Some(last) = self.rooms.last_seq.get(room).copied() else {
            return;
        };
        if let Some(last) = last {
            if seq <= last {
                return;
            }
            if seq != last.saturating_add(1) {
                self.resync(room, out);
                return;
            }
        }
        self.rooms.last_seq.insert(room.to_owned(), Some(seq));
        if event == "notification" {
            self.on_notification(payload, out);
        } else {
            self.on_document_event(payload, out);
        }
    }

    fn on_document_event(&mut self, payload: &Value, out: &mut Vec<Output>) {
        let text = |key: &str| payload.get(key).and_then(Value::as_str);
        let (Some(doctype), Some(id)) = (text("doctype"), text("id")) else {
            return;
        };
        let version = payload.get("version").and_then(Value::as_u64);
        if let (Some(held), Some(version)) = (self.cache.doc_version(doctype, id), version)
            && version <= held
        {
            return;
        }
        self.cache.invalidate_doc(doctype, id);
        out.push(Output::Emit(Event::Invalidated {
            doctype: doctype.to_owned(),
            id: id.to_owned(),
        }));
    }

    fn on_notification(&mut self, payload: &Value, out: &mut Vec<Output>) {
        let (Some(event_id), Some(event)) = (
            payload.get("event_id").and_then(Value::as_str),
            payload.get("event").and_then(Value::as_str),
        ) else {
            return;
        };
        // Only `user_notice` is defined; an unknown class is ignored (realtime.md).
        if event != "user_notice" || self.rooms.seen.iter().any(|seen| seen == event_id) {
            return;
        }
        if self.rooms.seen.len() >= SEEN_LIMIT {
            self.rooms.seen.pop_front();
        }
        self.rooms.seen.push_back(event_id.to_owned());
        let data = payload.get("data").cloned().unwrap_or(Value::Null);
        out.push(Output::Emit(Event::Notification {
            event_id: event_id.to_owned(),
            class: event.to_owned(),
            data,
        }));
    }

    /// Events were lost: forget what the room's cache entries said and tell the application.
    fn resync(&mut self, room: &str, out: &mut Vec<Output>) {
        self.rooms.last_seq.insert(room.to_owned(), None);
        if let Some(rest) = room.strip_prefix("doc:") {
            if let Some((doctype, id)) = rest.split_once(':') {
                self.cache.invalidate_doc(doctype, id);
            }
        } else if let Some(doctype) = room.strip_prefix("doctype:") {
            self.cache.invalidate_doctype(doctype);
        }
        out.push(Output::Emit(Event::Resync {
            room: room.to_owned(),
        }));
    }
}
