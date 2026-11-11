//! Stress: 8 publisher tasks and 64 subscribers on a multi-threaded runtime. Every
//! subscriber that is not closed sees a gap-free `seq` sequence in every room it joined.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use spot_core::{SiteId, SystemClock};
use spot_realtime::{AllowAll, BusConfig, Principal, RealtimeBus, Room};

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap_or_else(|e| panic!("test site name should be valid: {e}"))
}

fn room(name: &str) -> Room {
    Room::doctype(name).unwrap_or_else(|e| panic!("valid room: {e}"))
}

const ROOM_NAMES: [&str; 4] = ["R1", "R2", "R3", "R4"];
const PUBLISHERS: usize = 8;
const SUBSCRIBERS: usize = 64;
const PER_PUBLISHER: u64 = 500;

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_publish_and_consume_stays_gap_free() {
    // Round-robin over 4 rooms: each room takes PUBLISHERS * PER_PUBLISHER / 4 events.
    // Queue above every subscriber's total load across all rooms (all publishers × all
    // events): with all joins complete before publishing starts, nobody should
    // overflow; a closure would be a bus bug.
    let per_subscriber = PUBLISHERS * PER_PUBLISHER as usize;
    let per_room = per_subscriber / ROOM_NAMES.len();
    let bus = RealtimeBus::new(
        BusConfig {
            queue_capacity: per_subscriber + 64,
            ..BusConfig::default()
        },
        Arc::new(SystemClock::new()),
        Arc::new(AllowAll),
    );
    let site = site("acme.example");
    let rooms: Vec<Room> = ROOM_NAMES.iter().map(|n| room(n)).collect();

    // Join every room on every subscription up front, single-threaded: the publishing
    // phase then races only delivery, not membership.
    let mut subs = Vec::new();
    for id in 0..SUBSCRIBERS {
        let mut sub = bus.subscribe(site.clone(), Principal::user(format!("u{id}")));
        for room in &rooms {
            sub.join(room, None)
                .await
                .unwrap_or_else(|e| panic!("subscriber {id} join: {e}"));
        }
        subs.push(sub);
    }

    // Drain tasks: read until the stream ends. The bus is dropped after the writers join,
    // which closes every channel once its buffered events are drained — an exact,
    // race-free "all published events received" signal.
    let readers: Vec<_> = subs
        .into_iter()
        .enumerate()
        .map(|(id, mut sub)| {
            tokio::spawn(async move {
                let mut by_room: HashMap<String, Vec<u64>> = HashMap::new();
                let mut closed_as_slow_consumer = false;
                loop {
                    let next = tokio::time::timeout(Duration::from_secs(30), sub.recv())
                        .await
                        .unwrap_or_else(|_| panic!("subscriber {id} stalled: no event within 30s"));
                    match next {
                        Ok(event) => {
                            by_room
                                .entry(event.room.as_str().to_string())
                                .or_default()
                                .push(event.seq);
                        }
                        Err(spot_realtime::Closed::SlowConsumer) => {
                            closed_as_slow_consumer = true;
                            break;
                        }
                        Err(spot_realtime::Closed::ChannelClosed) => break,
                    }
                }
                (closed_as_slow_consumer, by_room)
            })
        })
        .collect();

    let mut writers = Vec::new();
    for w in 0..PUBLISHERS {
        let bus = bus.clone();
        let site = site.clone();
        let rooms = rooms.clone();
        writers.push(tokio::spawn(async move {
            for i in 0..PER_PUBLISHER {
                let idx = (w + usize::try_from(i).unwrap_or(usize::MAX)) % rooms.len();
                bus.publish(
                    &site,
                    &rooms[idx],
                    "e",
                    serde_json::json!({ "w": w, "i": i }),
                );
            }
        }));
    }
    for writer in writers {
        tokio::time::timeout(Duration::from_secs(60), writer)
            .await
            .map_or_else(
                |_| panic!("publisher task hung"),
                |res| res.unwrap_or_else(|e| panic!("publisher task failed: {e}")),
            );
    }

    // Drop every bus handle: the last one frees `Inner`, its sender map dies, each
    // subscriber's queue drains fully and then reports `ChannelClosed`.
    drop(bus);

    // Per room, the full room-local sequence. Every room is joined before any publish,
    // so each counts its own 1..=per_room; a room created after activity would be seeded
    // above the bus's high-water mark (see the crate docs), never at 1 again.
    let expected = |_room_index: usize| -> Vec<u64> { (1..=per_room as u64).collect() };
    for (id, reader) in readers.into_iter().enumerate() {
        let (slow, by_room) = tokio::time::timeout(Duration::from_secs(60), reader)
            .await
            .map_or_else(
                |_| panic!("reader {id} hung"),
                |res| res.unwrap_or_else(|e| panic!("reader {id} task failed: {e}")),
            );
        if slow {
            panic!("subscriber {id} was closed as a slow consumer despite sufficient capacity");
        }
        assert_eq!(
            by_room.len(),
            ROOM_NAMES.len(),
            "subscriber {id} must have received from every joined room"
        );
        for (room_index, room) in rooms.iter().enumerate() {
            let key = room.as_str().to_string();
            let seqs = by_room
                .get(&key)
                .unwrap_or_else(|| panic!("subscriber {id} received nothing from {key}"));
            assert!(
                seqs.windows(2).all(|pair| pair[0] + 1 == pair[1]),
                "subscriber {id} has a gap or reordering in {key}: {:?}",
                &seqs[..seqs.len().min(20)]
            );
            assert_eq!(
                *seqs,
                expected(room_index),
                "subscriber {id} must see the full room-local sequence in {key}"
            );
        }
    }
}
