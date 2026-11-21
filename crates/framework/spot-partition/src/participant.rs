//! The owner check as a document-engine transaction participant (WP-EDG-03.T2).
//!
//! `OwnerCheckSeam` and `WriteGate` are only as strong as the caller that remembers to ask.
//! [`OwnerParticipant`] makes the check unavoidable: it is a
//! [`TxParticipant`] whose `before_write` the engine calls inside the document's own
//! transaction, for every write path (create, update, post, cancel, delete, rename, update of a
//! posted document, both halves of a correction, and the writes a method or step makes), after
//! authorisation and before the storage write. A refusal rolls the whole operation back: no
//! document, no lifecycle event in the outbox, nothing a participant wrote.
//!
//! # Which scope keys a document carries
//!
//! The platform never interprets a scope key (ADR-062); ADR-063's `owner_key` (which field
//! supplies which key type) is still proposed, so it is configuration here: [`ScopeFields`] maps
//! a DocType to `(key type, field)` pairs, the same shape the receive side uses.
//!
//! * A DocType with **no** configured pair is a local record: the participant allows its writes
//!   untouched (the same rule as [`OwnerCheck::allow_write`] for a record with no scope keys),
//!   **unless** the participant was told the DocType is partitioned
//!   ([`OwnerParticipant::checked`]): then the write is refused (`partition.scope_uncovered`),
//!   because a partitioned DocType nobody configured is unprotected, never allowed.
//! * A DocType **with** configured pairs must carry every key. A document where a configured
//!   field is empty, null or not text or a link id has no resolvable partition, and the write is
//!   **refused** (`partition.invalid_scope`), never allowed.
//! * The keys of the document **as stored** (`old`) and **as about to be written** (`new`) must
//!   both be owned by this node on an update, so a document cannot be moved out of a partition
//!   this node does not own, nor into one it does not own. Create checks `new`; delete checks
//!   `old`.
//! * A key with no assignment at the invocation's instant is refused (`partition.not_owner`
//!   naming no owner). There is no bypass for any actor: system and job principals are checked
//!   exactly like users, because ownership is a property of the node, not of the principal.
//!
//! The instant is the invocation's business time (`Invocation::now`); nothing here reads a
//! clock (ADR-077). The decision itself is [`check_write`], shared with the seam.
//!
//! The receive side (`allow_replica`) belongs to WP-EDG-06.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::prelude::Value;
use spot_app_api::{AppError, AppResult, BoxFuture, Document, ErrorClass, Operation};
use spot_document::engine::{TxParticipant, WriteIntent};
use spot_storage_contract::Transaction;
use spot_sync_protocol::NodeId;

use crate::error::PartitionError;
use crate::map::PartitionRegistry;
use crate::owner::{ReplicaRecord, check_write};
use crate::scope::ScopeKey;

/// Which fields of which DocTypes supply partition scope keys.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScopeFields {
    by_doctype: BTreeMap<String, Vec<(String, String)>>,
}

impl ScopeFields {
    /// No DocType carries a partition scope.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares that `field` of `doctype` supplies a scope key of type `key_type`.
    #[must_use]
    pub fn with(mut self, doctype: &str, key_type: &str, field: &str) -> Self {
        self.by_doctype
            .entry(doctype.to_owned())
            .or_default()
            .push((key_type.to_owned(), field.to_owned()));
        self
    }

    /// The `(key type, field)` pairs of `doctype`; `None` when it has no partition scope.
    #[must_use]
    pub fn fields(&self, doctype: &str) -> Option<&[(String, String)]> {
        self.by_doctype.get(doctype).map(Vec::as_slice)
    }

    /// Checks that every DocType in `partitioned` has a scope field entry.
    ///
    /// A partitioned DocType with no entry is **unprotected**: the owner check skips a DocType
    /// it knows no scope for. This names every such DocType at once (sorted, without repeats;
    /// the message lists at most [`MAX_NAMED_DOCTYPES`](crate::error::MAX_NAMED_DOCTYPES)). An
    /// empty list is valid and checks nothing; a DocType with an entry that is not in the list is
    /// allowed (extra protection).
    ///
    /// # Errors
    ///
    /// `partition.scope_uncovered`, naming DocTypes only.
    pub fn check_covers(&self, partitioned: &[&str]) -> Result<(), PartitionError> {
        let mut missing: Vec<String> = partitioned
            .iter()
            .filter(|d| self.fields(d).is_none())
            .map(|d| (*d).to_owned())
            .collect();
        missing.sort();
        missing.dedup();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(PartitionError::ScopeUncovered { doctypes: missing })
        }
    }

    /// The scope keys `document` carries, one per configured pair, in configuration order.
    ///
    /// # Errors
    ///
    /// `partition.invalid_scope` when a configured field is empty, null, not text or a link id,
    /// or holds text that cannot be a scope key. The write must then be refused.
    pub fn scope_values(
        &self,
        doctype: &str,
        document: &Document,
    ) -> Result<Vec<ScopeKey>, PartitionError> {
        let Some(pairs) = self.fields(doctype) else {
            return Ok(Vec::new());
        };
        let mut keys = Vec::with_capacity(pairs.len());
        for (key_type, field) in pairs {
            let text = match document.get(field) {
                Value::Text(text) => text.clone(),
                Value::Id(id) => id.to_string(),
                _ => {
                    return Err(PartitionError::InvalidScope {
                        reason: format!(
                            "`{doctype}` carries no `{key_type}` partition scope in field `{field}`"
                        ),
                    });
                }
            };
            keys.push(ScopeKey::new(key_type, &text)?);
        }
        Ok(keys)
    }
}

/// A document of this node, presented as a [`ReplicaRecord`]: its DocType, the node that writes
/// it, and the scope keys it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentRecord {
    doctype: String,
    origin_node: NodeId,
    scope_values: Vec<ScopeKey>,
}

impl DocumentRecord {
    /// The record for `document` written by `node`, with its scope keys read through `fields`.
    ///
    /// # Errors
    ///
    /// `partition.invalid_scope`, see [`ScopeFields::scope_values`].
    pub fn new(
        node: &NodeId,
        doctype: &str,
        document: &Document,
        fields: &ScopeFields,
    ) -> Result<Self, PartitionError> {
        Ok(Self {
            doctype: doctype.to_owned(),
            origin_node: node.clone(),
            scope_values: fields.scope_values(doctype, document)?,
        })
    }
}

impl ReplicaRecord for DocumentRecord {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn origin_node(&self) -> &NodeId {
        &self.origin_node
    }

    fn scope_values(&self) -> &[ScopeKey] {
        &self.scope_values
    }
}

/// The transaction participant that refuses a write this node does not own.
#[derive(Clone)]
pub struct OwnerParticipant {
    node: NodeId,
    registry: Arc<dyn PartitionRegistry>,
    fields: ScopeFields,
    partitioned: BTreeSet<String>,
}

impl OwnerParticipant {
    /// A participant for `node` over the ownership registry, with **no coverage guarantee**.
    ///
    /// It does not know which DocTypes are partitioned, so a DocType with no scope field entry
    /// is allowed through untouched. For tests; a host builds the participant with
    /// [`OwnerParticipant::checked`].
    #[must_use]
    pub fn new(node: NodeId, registry: Arc<dyn PartitionRegistry>, fields: ScopeFields) -> Self {
        Self {
            node,
            registry,
            fields,
            partitioned: BTreeSet::new(),
        }
    }

    /// Declares more partitioned DocTypes **without** checking their coverage.
    ///
    /// A write to a listed DocType with no scope field entry is then refused at write time with
    /// `partition.scope_uncovered`, so even a participant built with [`OwnerParticipant::new`]
    /// fails closed for what it was told is partitioned. Prefer [`OwnerParticipant::checked`],
    /// which also refuses to be built.
    #[must_use]
    pub fn with_partitioned(mut self, doctypes: &[&str]) -> Self {
        self.partitioned
            .extend(doctypes.iter().map(|d| (*d).to_owned()));
        self
    }

    /// A participant for `node` that has been shown every partitioned DocType is covered.
    ///
    /// `partitioned` is the deployment's list of partitioned DocTypes (the `[partition]
    /// doctypes` entry of the site config until ADR-063 is accepted). The participant keeps it:
    /// a write to a listed DocType that has no scope field entry is refused with
    /// `partition.scope_uncovered`, never allowed.
    ///
    /// # Errors
    ///
    /// `partition.scope_uncovered` naming every listed DocType with no scope field entry
    /// ([`ScopeFields::check_covers`]); the host must refuse to start the node.
    pub fn checked(
        node: NodeId,
        registry: Arc<dyn PartitionRegistry>,
        fields: ScopeFields,
        partitioned: &[&str],
    ) -> Result<Self, PartitionError> {
        fields.check_covers(partitioned)?;
        Ok(Self {
            node,
            registry,
            fields,
            partitioned: partitioned.iter().map(|d| (*d).to_owned()).collect(),
        })
    }

    /// The decision for one write; synchronous, so it needs no runtime to test.
    fn decide(&self, write: &WriteIntent<'_>) -> AppResult<()> {
        if self.fields.fields(write.doctype).is_none() {
            if self.partitioned.contains(write.doctype) {
                return Err(invalid(PartitionError::ScopeUncovered {
                    doctypes: vec![write.doctype.to_owned()],
                }));
            }
            return Ok(());
        }
        let (use_old, use_new) = match write.operation {
            Operation::Create => (false, true),
            Operation::Delete => (true, false),
            _ => (true, true),
        };
        let mut scope_values: Vec<ScopeKey> = Vec::new();
        for (wanted, document) in [(use_old, write.old), (use_new, write.new)] {
            if !wanted {
                continue;
            }
            let document = document.ok_or_else(|| {
                invalid(PartitionError::InvalidScope {
                    reason: format!(
                        "the document of a `{}` write of `{}` is not available to check",
                        format!("{:?}", write.operation).to_lowercase(),
                        write.doctype
                    ),
                })
            })?;
            let record = DocumentRecord::new(&self.node, write.doctype, document, &self.fields)
                .map_err(invalid)?;
            for key in record.scope_values() {
                if !scope_values.contains(key) {
                    scope_values.push(key.clone());
                }
            }
        }
        check_write(
            self.registry.as_ref(),
            &self.node,
            write.doctype,
            &scope_values,
            write.invocation.now,
        )
    }
}

fn invalid(error: PartitionError) -> AppError {
    AppError::new(
        ErrorClass::PermissionDenied,
        error.code(),
        error.to_string(),
    )
}

impl<Tx: Transaction> TxParticipant<Tx> for OwnerParticipant {
    fn before_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        write: &'a WriteIntent<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move { self.decide(write) })
    }
}
