//! The serialised form of a [`crate::HoldSet`].
//!
//! The in-memory maps are keyed by `(entity, id)` tuples, which no text format with string map
//! keys (JSON) can carry. On the wire each map is a **sequence of `(entity, id, value)` entries
//! sorted by key**, so the state is readable, diffable and restorable by an operator and works in
//! binary formats too. Loading validates the shape: a duplicate key, a queue that is not strictly
//! ascending by position and a queue over its limit are errors, never a silent repair
//! (`sync_skew.state_duplicate_key`, `sync_skew.state_disorder`, `sync_skew.state_over_limit`).

use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::{self, SeqAccess, Visitor};
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{STATE_DISORDER, STATE_DUPLICATE_KEY, STATE_OVER_LIMIT};
use crate::holds::{DEFAULT_HOLD_LIMIT, Hold, HoldSet, Pending};

/// Why a persisted hold set was refused. The text starts with the catalogue code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StateError {
    DuplicateKey {
        what: &'static str,
        entity: String,
        id: String,
    },
    Disorder {
        entity: String,
        id: String,
    },
    OverLimit {
        queued: usize,
        limit: usize,
    },
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateKey { what, entity, id } => write!(
                f,
                "{STATE_DUPLICATE_KEY}: entity {entity:?} id {id:?} appears twice in the {what}"
            ),
            Self::Disorder { entity, id } => write!(
                f,
                "{STATE_DISORDER}: the queue of entity {entity:?} id {id:?} is not strictly ascending by position"
            ),
            Self::OverLimit { queued, limit } => write!(
                f,
                "{STATE_OVER_LIMIT}: {queued} queued entries exceed the limit of {limit}"
            ),
        }
    }
}

impl std::error::Error for StateError {}

/// One entry on the wire.
type Entry<'a, T> = (&'a str, &'a str, &'a T);

fn serialize_entries<T, S>(
    map: &BTreeMap<(String, String), T>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    T: Serialize,
    S: Serializer,
{
    // A BTreeMap iterates in key order, which is the sort order of the wire form.
    let mut seq = serializer.serialize_seq(Some(map.len()))?;
    for ((entity, id), value) in map {
        let entry: Entry<'_, T> = (entity.as_str(), id.as_str(), value);
        seq.serialize_element(&entry)?;
    }
    seq.end()
}

struct EntriesVisitor<T> {
    what: &'static str,
    marker: PhantomData<T>,
}

impl<'de, T: Deserialize<'de>> Visitor<'de> for EntriesVisitor<T> {
    type Value = BTreeMap<(String, String), T>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "a sequence of (entity, id, value) entries")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut map = BTreeMap::new();
        while let Some((entity, id, value)) = seq.next_element::<(String, String, T)>()? {
            let key = (entity, id);
            if map.contains_key(&key) {
                return Err(de::Error::custom(StateError::DuplicateKey {
                    what: self.what,
                    entity: key.0,
                    id: key.1,
                }));
            }
            map.insert(key, value);
        }
        Ok(map)
    }
}

fn deserialize_entries<'de, T, D>(
    what: &'static str,
    deserializer: D,
) -> Result<BTreeMap<(String, String), T>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    deserializer.deserialize_seq(EntriesVisitor {
        what,
        marker: PhantomData,
    })
}

/// `#[serde(with = ...)]` for the map of holds.
pub(crate) mod holds_entries {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        map: &BTreeMap<(String, String), Hold>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serialize_entries(map, serializer)
    }

    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<(String, String), Hold>, D::Error> {
        deserialize_entries("holds", deserializer)
    }
}

/// `#[serde(with = ...)]` for the map of pending queues.
pub(crate) mod pending_entries {
    use super::*;

    pub(crate) fn serialize<S: Serializer>(
        map: &BTreeMap<(String, String), Vec<Pending>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serialize_entries(map, serializer)
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<(String, String), Vec<Pending>>, D::Error> {
        deserialize_entries("pending queues", deserializer)
    }
}

fn default_limit() -> usize {
    DEFAULT_HOLD_LIMIT
}

/// The wire shape of a [`HoldSet`]; [`HoldSet`] deserialises through it and validates.
#[derive(Deserialize)]
pub(crate) struct HoldSetRepr {
    #[serde(with = "holds_entries")]
    holds: BTreeMap<(String, String), Hold>,
    #[serde(default, with = "pending_entries")]
    pending: BTreeMap<(String, String), Vec<Pending>>,
    #[serde(default = "default_limit")]
    limit: usize,
}

impl TryFrom<HoldSetRepr> for HoldSet {
    type Error = StateError;

    fn try_from(repr: HoldSetRepr) -> Result<Self, Self::Error> {
        for ((entity, id), queue) in &repr.pending {
            let ascending = queue
                .windows(2)
                .all(|pair| pair[0].entry.position < pair[1].entry.position);
            if !ascending {
                return Err(StateError::Disorder {
                    entity: entity.clone(),
                    id: id.clone(),
                });
            }
        }
        let queued: usize = repr.pending.values().map(Vec::len).sum();
        if queued > repr.limit {
            return Err(StateError::OverLimit {
                queued,
                limit: repr.limit,
            });
        }
        Ok(HoldSet::from_parts(repr.holds, repr.pending, repr.limit))
    }
}
