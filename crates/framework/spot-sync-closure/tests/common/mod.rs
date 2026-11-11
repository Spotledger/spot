//! Test doubles: a destination, a source and image builders.
#![allow(dead_code)]

use serde_json::{Value, json};
use spot_sync_closure::{
    ClosureError, DestinationKnows, ExtrasSet, ImageSource, Position, RecordKey, SourceImage,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct Dest {
    pub owned: BTreeSet<RecordKey>,
    pub scope_entities: BTreeSet<String>,
    pub scope_ids: BTreeSet<RecordKey>,
    pub acked: Option<Position>,
    pub extras: ExtrasSet,
    pub held: BTreeSet<RecordKey>,
}

impl DestinationKnows for Dest {
    fn owns(&self, key: &RecordKey) -> bool {
        self.owned.contains(key)
    }
    fn in_scope(&self, key: &RecordKey) -> bool {
        self.scope_entities.contains(&key.entity) || self.scope_ids.contains(key)
    }
    fn acked_position(&self) -> Option<Position> {
        self.acked
    }
    fn extras(&self) -> &ExtrasSet {
        &self.extras
    }
    fn held(&self, key: &RecordKey) -> bool {
        self.held.contains(key)
    }
}

#[derive(Default)]
pub struct Src {
    pub images: BTreeMap<RecordKey, SourceImage>,
}

impl Src {
    pub fn put(&mut self, entity: &str, id: &str, image: Value, position: Position) -> SourceImage {
        let key = RecordKey::new(entity, id);
        let img = SourceImage::new(key.clone(), image, position);
        self.images.insert(key, img.clone());
        img
    }
}

impl ImageSource for Src {
    fn image(&self, key: &RecordKey) -> Result<Option<SourceImage>, ClosureError> {
        Ok(self.images.get(key).cloned())
    }
}

/// A record of entity `t` with `links` child rows pointing at entity `t` ids.
pub fn node(links: &[&str]) -> Value {
    json!({ "rows": links.iter().map(|l| json!({ "to": l })).collect::<Vec<_>>(), "note": "x" })
}

pub fn k(entity: &str, id: &str) -> RecordKey {
    RecordKey::new(entity, id)
}
