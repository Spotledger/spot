//! Receive-side ownership: `spot-partition`'s owner check over a received record.
//!
//! ADR-062 G1 requires the receiver to reject a record whose sender is not its owner, and
//! ADR-063 says the receiver decides that from **its own** effective-dated view. `spot-partition`
//! already implements the check ([`spot_partition::OwnerCheck::allow_replica`]) over a
//! [`ReplicaRecord`]; this module presents the apply path's decoded item to it and supplies the
//! two inputs the check needs:
//!
//! * [`ScopeSource`] — which scope keys a received document carries. ADR-063's `owner_key`
//!   (which field supplies which key type) is still *Proposed*, so it is configuration here
//!   ([`ScopeFields`]) behind a trait, never a rule baked into the apply path.
//! * the ownership map at the item's instant, through a [`PartitionRegistry`]. The instant is
//!   [`ApplyCtx::at`](crate::ApplyCtx::at); no function here reads a clock (ADR-077).
//!
//! [`PartitionOwners`] is the [`OwnerCheck`] an apply context holds. It runs the partition check
//! before any write and reports whether this node owns the record (store it as a local document)
//! or holds it as a read-only mirror.
//!
//! # A received record is a [`ReplicaRecord`] without copying the document
//!
//! [`ReceivedRecordView`] borrows the apply path's decoded [`ReceivedRecord`] and answers the
//! three questions the check asks: the DocType, the origin node and the scope values. The
//! document, its child rows and its entries are never cloned or re-encoded to run the check.
//!
//! # The origin node on arrival
//!
//! A sync record carries an opaque payload, so at receive the sender is the only evidence of the
//! record's origin: ADR-063 section 2 says that for a new record the sender *is* `origin_node`.
//! The independent fact the receiver holds is the scope key: a record that carries one is
//! accepted only from the node its own ownership map names. A record with no partition scope has
//! nothing to compare, so it is accepted (its origin owns it).
//!
//! # What is not decided here
//!
//! ADR-063 (presets, `owner_key`, key holdings) is still *Proposed*. This module implements the
//! mechanism and leaves the policy choices — which key type a DocType uses, and whether a hub
//! owns `master` records it did not create — to that ADR; the places it will matter are listed
//! under `## Open questions` in the task report.

use std::sync::Arc;

use spot_partition::{OwnerCheck as PartitionCheck, PartitionRegistry, ReplicaRecord, ScopeKey};
use spot_storage_contract::{DocumentData, Name, Value};
use spot_sync_protocol::{NodeId, SourceId};
use time::OffsetDateTime;

use crate::ctx::{OwnerCheck, Ownership};
use crate::error::Reason;
use crate::record::ReceivedRecord;

/// Where a received document's partition scope keys come from.
///
/// ADR-063's `owner_key` setting names the scope key type and the field that supplies its value;
/// this trait is that decision behind a seam. [`ScopeFields`] is the configured implementation.
pub trait ScopeSource: Send + Sync {
    /// The scope keys `document` carries, one per configured key type, in configuration order.
    ///
    /// A configured field that is absent or null contributes no key: a document without a
    /// partition scope is owned by its origin node (ADR-063 section 2).
    ///
    /// # Errors
    /// [`Reason::InvalidScope`] when the configured field is not a storage name, holds a value
    /// that is not text, or holds text that cannot be a scope key. The value itself is never
    /// echoed.
    fn scope_values(&self, doctype: &str, document: &DocumentData)
    -> Result<Vec<ScopeKey>, Reason>;
}

/// A [`ScopeSource`] configured from `(key type, field)` pairs.
///
/// This is the shape an app declares once in its metadata and an administrator sees as "owned by
/// the branch holding warehouse `W7`". The default is empty: a node that has not configured a
/// partition scope accepts every record as origin-owned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeFields {
    fields: Vec<(String, String)>,
}

impl ScopeFields {
    /// No configured scope keys.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same configuration with `key_type` supplied by `field` (replacing an earlier entry
    /// for the same key type).
    #[must_use]
    pub fn with(mut self, key_type: &str, field: &str) -> Self {
        match self.fields.iter_mut().find(|(t, _)| t == key_type) {
            Some((_, existing)) => *existing = field.to_owned(),
            None => self.fields.push((key_type.to_owned(), field.to_owned())),
        }
        self
    }

    /// Whether no scope key is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// The `(key type, field)` pairs, in configuration order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.fields
            .iter()
            .map(|(key_type, field)| (key_type.as_str(), field.as_str()))
    }
}

impl ScopeSource for ScopeFields {
    fn scope_values(
        &self,
        _doctype: &str,
        document: &DocumentData,
    ) -> Result<Vec<ScopeKey>, Reason> {
        let mut keys = Vec::with_capacity(self.fields.len());
        for (key_type, field) in &self.fields {
            let name = Name::new(field).map_err(|_| Reason::InvalidScope {
                key_type: key_type.clone(),
                field: field.clone(),
                why: "the configured field is not a storage name".to_owned(),
            })?;
            match document.fields.get(&name) {
                None | Some(Value::Null) => {}
                Some(Value::Text(value)) => {
                    let key =
                        ScopeKey::new(key_type, value).map_err(|error| Reason::InvalidScope {
                            key_type: key_type.clone(),
                            field: field.clone(),
                            why: error.to_string(),
                        })?;
                    keys.push(key);
                }
                Some(_) => {
                    return Err(Reason::InvalidScope {
                        key_type: key_type.clone(),
                        field: field.clone(),
                        why: "the field is not text".to_owned(),
                    });
                }
            }
        }
        Ok(keys)
    }
}

/// A received record as the partition owner check sees it.
///
/// The view borrows the apply path's decoded [`ReceivedRecord`] and holds only what the check
/// needs: the origin node (the sender, see the module docs) and the record's scope values. The
/// document, its child rows and its entries are not copied.
#[derive(Debug)]
pub struct ReceivedRecordView<'a> {
    record: &'a ReceivedRecord,
    origin: NodeId,
    scope: Vec<ScopeKey>,
}

impl<'a> ReceivedRecordView<'a> {
    /// A view over `record` carrying `scope`.
    ///
    /// # Errors
    /// [`Reason::InvalidIdentifier`] when the sender is not a valid node id. A validated
    /// [`SourceId`] and a [`NodeId`] share one grammar, so this cannot happen for a record the
    /// sync protocol decoded; it is reported rather than unwrapped.
    pub fn of(record: &'a ReceivedRecord, scope: Vec<ScopeKey>) -> Result<Self, Reason> {
        let origin =
            NodeId::new(record.source.as_str()).map_err(|_| Reason::InvalidIdentifier {
                what: "source node".to_owned(),
            })?;
        Ok(Self {
            record,
            origin,
            scope,
        })
    }

    /// The apply key's version part.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.record.version
    }

    /// The record the view borrows.
    #[must_use]
    pub fn record(&self) -> &'a ReceivedRecord {
        self.record
    }
}

impl ReplicaRecord for ReceivedRecordView<'_> {
    fn doctype(&self) -> &str {
        self.record.document.entity.as_str()
    }

    fn origin_node(&self) -> &NodeId {
        &self.origin
    }

    fn scope_values(&self) -> &[ScopeKey] {
        &self.scope
    }
}

/// The [`OwnerCheck`] over a [`PartitionRegistry`]: `spot-partition`'s receive check, resolved at
/// the item's instant.
///
/// A record whose scope keys the sender owns is accepted; this node stores it as a local document
/// when it sent the record itself (the rebuild-from-journal case) and as a read-only mirror
/// otherwise.
pub struct PartitionOwners {
    registry: Arc<dyn PartitionRegistry>,
    scope: Arc<dyn ScopeSource>,
    local: NodeId,
}

impl PartitionOwners {
    /// A check over `registry`, with scope keys from `scope`, for the node `local`.
    #[must_use]
    pub fn new(
        registry: Arc<dyn PartitionRegistry>,
        scope: Arc<dyn ScopeSource>,
        local: NodeId,
    ) -> Self {
        Self {
            registry,
            scope,
            local,
        }
    }
}

impl OwnerCheck for PartitionOwners {
    fn allow_replica(
        &self,
        sender: &SourceId,
        record: &ReceivedRecord,
        at: i64,
    ) -> Result<Ownership, Reason> {
        let sender_node = NodeId::new(sender.as_str()).map_err(|_| Reason::InvalidIdentifier {
            what: "source node".to_owned(),
        })?;
        let scope_values = self
            .scope
            .scope_values(record.document.entity.as_str(), &record.document)?;
        let view = ReceivedRecordView::of(record, scope_values)?;
        let map = Arc::new(self.registry.map_at(instant_at(at)?));
        PartitionCheck::new(map)
            .allow_replica(&sender_node, &view)
            .map_err(|refusal| Reason::NotOwner {
                owner: refusal
                    .owner()
                    .map_or_else(|| "no node".to_owned(), ToString::to_string),
            })?;
        Ok(if sender_node == self.local {
            Ownership::Owned
        } else {
            Ownership::Mirror
        })
    }
}

/// The item's instant as an [`OffsetDateTime`].
///
/// The apply context carries Unix microseconds, never a clock read (ADR-077).
fn instant_at(micros: i64) -> Result<OffsetDateTime, Reason> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(micros) * 1_000).map_err(|_| {
        Reason::InvalidIdentifier {
            what: "instant".to_owned(),
        }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeMap;

    use spot_storage_contract::{Fields, RecordId};
    use spot_sync_protocol::RecordName;

    use super::*;

    fn name(text: &str) -> Name {
        Name::new(text).unwrap()
    }

    fn stock(warehouse: Value) -> DocumentData {
        let mut fields = Fields::new();
        fields.insert(name("warehouse"), warehouse);
        DocumentData {
            entity: name("stock_entry"),
            id: RecordId::new_v7(),
            fields,
            children: BTreeMap::new(),
        }
    }

    fn record(document: DocumentData) -> ReceivedRecord {
        ReceivedRecord {
            name: RecordName::new("stock_entry:x").unwrap(),
            version: 7,
            seq: 3,
            source: SourceId::new("head.office.app#hub").unwrap(),
            topic: None,
            conversation: None,
            meta_version: 1,
            document,
            entries: Vec::new(),
        }
    }

    #[test]
    fn the_view_presents_the_received_item_without_the_document() {
        let record = record(stock(Value::Text("W7".to_owned())));
        let scope = vec![ScopeKey::new("warehouse", "W7").unwrap()];
        let view = ReceivedRecordView::of(&record, scope).unwrap();

        assert_eq!(view.doctype(), "stock_entry");
        assert_eq!(view.origin_node().as_str(), "head.office.app#hub");
        assert_eq!(view.scope_values().len(), 1);
        assert_eq!(view.version(), 7);
        assert_eq!(view.record().document, record.document);
    }

    #[test]
    fn a_configured_field_supplies_its_key_and_an_empty_one_supplies_none() {
        let scope = ScopeFields::new().with("warehouse", "warehouse");

        let keys = scope
            .scope_values("stock_entry", &stock(Value::Text("W7".to_owned())))
            .unwrap();
        assert_eq!(keys, vec![ScopeKey::new("warehouse", "W7").unwrap()]);

        let none = scope
            .scope_values("stock_entry", &stock(Value::Null))
            .unwrap();
        assert!(none.is_empty(), "an empty owner key carries no scope");
    }

    #[test]
    fn a_non_text_or_unusable_value_is_refused_without_echoing_it() {
        let scope = ScopeFields::new().with("warehouse", "warehouse");

        let error = scope
            .scope_values("stock_entry", &stock(Value::Int(7)))
            .unwrap_err();
        assert_eq!(error.code(), "replica_apply.invalid_scope");

        let long = format!("W7{}", "x".repeat(200));
        let error = scope
            .scope_values("stock_entry", &stock(Value::Text(long)))
            .unwrap_err();
        assert_eq!(error.code(), "replica_apply.invalid_scope");
        assert!(
            !error.to_string().contains("xxxx"),
            "the value is never echoed: {error}"
        );
    }

    #[test]
    fn an_empty_configuration_accepts_any_document() {
        let scope = ScopeFields::new();
        assert!(scope.is_empty());
        assert!(
            scope
                .scope_values("anything", &stock(Value::Text("W7".to_owned())))
                .unwrap()
                .is_empty()
        );
    }
}
