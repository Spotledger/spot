//! One document version and its field-level changes (WP-DOC-07.T1).
//!
//! A [`Version`] is one save of a document: which stored version it produced, when, by whom,
//! under which command, and what the save changed (a [`crate::diff::diff_documents`] over the
//! before and after images). The engine persists these as ordinary documents (WP-DOC-04's
//! storage); nothing here writes anything.
//!
//! The [`Version`] name is this crate's own: the storage contract's `Version` is a counter, so
//! it is stored in `doc_version`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use spot_app_api::Actor;
use spot_app_api::document::ChildRow;
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, Value};

use crate::diff::{FieldChange, diff_documents};

/// One save of a document, with what it changed.
///
/// The changes list is the diff of the save's before and after images, in the diff's order
/// (field declaration order, then row order). Construct with [`Version::record`], which runs
/// the diff; the fields are public so the engine can rebuild one from stored data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    /// The stored version this save produced (the storage contract's counter).
    #[serde(with = "crate::wire::doc_version")]
    pub doc_version: spot_app_api::prelude::Version,
    /// When the save happened: the invocation's business time (a parameter, never a clock
    /// read).
    #[serde(with = "crate::wire::rfc3339")]
    pub at: OffsetDateTime,
    /// Who saved it.
    #[serde(with = "actor")]
    pub actor: Actor,
    /// The command that saved it (the API's `Idempotency-Key`), when there was one.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "opt_command_id"
    )]
    pub command_id: Option<CommandId>,
    /// What the save changed, in the diff's stable order.
    pub changes: Vec<FieldChange>,
}

impl Version {
    /// Records one save: diffs the two images against the effective metadata of `doctype`.
    // The signature mirrors the four images plus their metadata; callers name every argument,
    // so the width is deliberate rather than a smell.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn record(
        doc_version: spot_app_api::prelude::Version,
        at: OffsetDateTime,
        actor: Actor,
        command_id: Option<CommandId>,
        before_fields: &BTreeMap<Name, Value>,
        before_tables: &BTreeMap<Name, Vec<ChildRow>>,
        after_fields: &BTreeMap<Name, Value>,
        after_tables: &BTreeMap<Name, Vec<ChildRow>>,
        meta: &spot_meta::EffectiveMeta,
        doctype: &str,
    ) -> Version {
        let changes = diff_documents(
            before_fields,
            before_tables,
            after_fields,
            after_tables,
            meta,
            doctype,
        );
        Version {
            doc_version,
            at,
            actor,
            command_id,
            changes,
        }
    }

    /// True when the save changed nothing (a no-op save the engine may still record).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// The fields and rows this save touched, as the diff wrote their paths.
    #[must_use]
    pub fn touched_paths(&self) -> BTreeSet<&str> {
        self.changes.iter().map(|c| c.path.as_str()).collect()
    }
}

/// Who caused an activity record: the same `Actor` the events carry, re-declared with serde
/// locally (the events contract's type derives serde, so this is its mirror for the records
/// this crate writes).
pub(crate) mod actor {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use spot_app_api::Actor;

    /// The actor, as the events contract writes it.
    pub fn serialize<S: Serializer>(a: &Actor, s: S) -> Result<S::Ok, S::Error> {
        a.serialize(s)
    }

    /// The actor, as the events contract writes it.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Actor, D::Error> {
        Actor::deserialize(d)
    }
}

/// The `Option<CommandId>` serde helpers: a command id is a visible-ASCII string.
pub(crate) mod opt_command_id {
    use serde::{Deserialize, Deserializer, Serializer};
    use spot_app_api::prelude::CommandId;

    /// The command id as its string, or absent.
    pub fn serialize<S: Serializer>(id: &Option<CommandId>, s: S) -> Result<S::Ok, S::Error> {
        match id {
            Some(id) => s.serialize_str(id.as_str()),
            None => s.serialize_none(),
        }
    }

    /// The command id from its string, or absent.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<CommandId>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|t| CommandId::new(&t).map_err(serde::de::Error::custom))
            .transpose()
    }
}
