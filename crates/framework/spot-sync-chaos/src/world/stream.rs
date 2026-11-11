//! The hub's record stream: what is written, in which order, and the images behind it.

use spot_sync_closure::RecordKey;
use spot_sync_hub::StreamEntry;
use spot_sync_obligations::Lane;

use super::{ChaosError, ENTITIES, Entry, HUB_SOURCE, MAJOR, TOPIC, World};

impl World {
    /// Plan the stream: one wave per version, the record order shuffled within a wave.
    pub(super) fn plan_stream(&mut self) {
        let mut keys: Vec<RecordKey> = Vec::with_capacity(self.cfg.records);
        for index in 0..self.cfg.records {
            let entity = ENTITIES[index % ENTITIES.len()];
            keys.push(RecordKey::new(entity, format!("{entity}-{index:03}")));
        }
        for version in 1..=self.cfg.versions {
            let mut wave: Vec<(RecordKey, u64)> =
                keys.iter().map(|key| (key.clone(), version)).collect();
            self.rng.shuffle(&mut wave);
            self.plan.push(wave);
        }
    }

    /// Write the next wave of records to the hub's stream, plus the other tenant's noise.
    pub(super) fn append_wave(&mut self) -> Result<(), ChaosError> {
        let seed = self.cfg.seed;
        let Some(wave) = self.plan.get(self.appended).cloned() else {
            return Ok(());
        };
        self.appended += 1;
        let mut txn_left = 0;
        let mut groups: Vec<(String, Vec<(RecordKey, u64)>)> = Vec::new();
        let group_max = self.cfg.group_max.max(1);
        for item in wave {
            if txn_left == 0 {
                txn_left = 1 + usize::try_from(self.rng.below(group_max as u64)).unwrap_or(0);
                self.next_txn += 1;
                groups.push((format!("txn-{:04}", self.next_txn), Vec::new()));
            }
            txn_left -= 1;
            if let Some((_, group)) = groups.last_mut() {
                group.push(item);
            }
        }
        let tenant = self.cfg.tenant.clone();
        let foreign = self.cfg.foreign_tenant.clone();
        let source = HUB_SOURCE.to_owned();
        for (txn, group) in groups {
            for (key, _version) in group {
                self.next_position += 1;
                let position = self.next_position;
                self.images
                    .set(key.clone(), image_of(&key, &tenant, position), position);
                let entry = StreamEntry {
                    position,
                    tenant: tenant.clone(),
                    source: source.clone(),
                    origin: None,
                    txn: txn.clone(),
                    role: spot_sync_hub::EntryRole::Root,
                    key: key.clone(),
                    version: position,
                    topic: TOPIC.to_owned(),
                    major: MAJOR,
                    lane: lane_of(&key, self.cfg.mandatory_customers),
                };
                self.hub.stream_mut().append(entry);
                self.entries.push(Entry {
                    position,
                    tenant: tenant.clone(),
                    key,
                    version: position,
                });
            }
            if self.cfg.write_foreign {
                // The other tenant's records travel the same stream. The relay drops them, so no
                // node of this tenant may ever hold one.
                self.next_position += 1;
                let position = self.next_position;
                let key = RecordKey::new(ENTITIES[0], format!("beta-{position:04}"));
                self.hub.stream_mut().append(StreamEntry {
                    position,
                    tenant: foreign.clone(),
                    source: source.clone(),
                    origin: None,
                    txn: format!("txn-beta-{position}"),
                    role: spot_sync_hub::EntryRole::Root,
                    key: key.clone(),
                    version: position,
                    topic: TOPIC.to_owned(),
                    major: MAJOR,
                    lane: Lane::Bulk,
                });
                self.entries.push(Entry {
                    position,
                    tenant: foreign.clone(),
                    key,
                    version: position,
                });
            }
        }
        if self.next_position > 0 {
            self.transcript.push(format!(
                "{:04} produced wave {} through position {}",
                self.round, self.appended, self.next_position
            ));
        }
        let _ = seed;
        Ok(())
    }
}

/// The image of a record at one position: full state, so a later version supersedes an earlier.
fn image_of(key: &RecordKey, tenant: &str, position: u64) -> serde_json::Value {
    serde_json::json!({
        "name": key.id,
        "entity": key.entity,
        "tenant": tenant,
        "version": position,
    })
}

/// The lane of an entity. A record's lane is a function of its entity, so its versions never
/// swap lanes.
fn lane_of(key: &RecordKey, mandatory_customers: bool) -> Lane {
    if mandatory_customers && key.entity == "Customer" {
        Lane::Mandatory
    } else {
        Lane::Bulk
    }
}
