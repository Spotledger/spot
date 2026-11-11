//! The node side of a sealed field: write only (ADR-070 rule 12.13 a, f).
//!
//! A `sealed` group is the answer for entry cases such as bank details and salary lines: a
//! branch creates the value and **can never read it back**, because it is sealed to a key only
//! head office holds. This module therefore has no read that returns a value. [`NodeFieldStore`]
//! stores sealed values and [`NodeFieldStore::read`] answers
//! [`SealedFieldsError::SealedUnreadable`] — the specific error the acceptance criterion names
//! — whatever the caller's grants are.
//!
//! The module is compiled on both sides: a hub uses the same writer to seal a value it also
//! keeps, but only the hub module can open one.

use std::collections::BTreeMap;

use spot_protected_keys::{GroupName, NodeId, TenantId};

use crate::bind::SealedAd;
use crate::class::SyncClass;
use crate::error::SealedFieldsError;
use crate::sealbox::SealedWriter;
use crate::sealbox::{HubPublicKey, Sealed};

/// A stored sealed value and the place it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedField {
    /// The field path.
    pub field: String,
    /// The protection group.
    pub group: GroupName,
    /// The document the value belongs to.
    pub document: String,
    /// The sealed blob.
    pub sealed: Sealed,
}

/// What one sealed write is for: the field it fills and the hub key version it was prepared
/// against. The tenant and the DocType are not here: the store owns them.
#[derive(Clone, Copy, Debug)]
pub struct SealTarget<'a> {
    /// The protection group of the field.
    pub group: &'a GroupName,
    /// The group's sync class; only `sealed` is held here.
    pub class: SyncClass,
    /// The field path.
    pub field: &'a str,
    /// The document the value belongs to.
    pub document: &'a str,
    /// The hub public key (the key version) the caller expects the store to seal to.
    pub hub_key: &'a HubPublicKey,
}

/// The node's store of sealed values: write-only by construction.
///
/// The store holds only [`Sealed`] blobs, and there is no method that returns a value. A caller
/// that wants to read must reach the hub, and the hub's reveal is audited.
pub struct NodeFieldStore {
    node: NodeId,
    tenant: TenantId,
    doctype: String,
    writer: SealedWriter,
    values: BTreeMap<(String, String), SealedField>,
}

impl NodeFieldStore {
    /// A store for `node`, holding the sealed fields of one DocType of one tenant, sealing to
    /// `hub`.
    ///
    /// The tenant and the DocType are bound here, once, so a write cannot be handed a matching
    /// but wrong pair: [`NodeFieldStore::write`] compares the write's associated data with these
    /// two values and with nothing the caller of `write` supplies.
    #[must_use]
    pub fn new(
        node: NodeId,
        tenant: TenantId,
        doctype: impl Into<String>,
        hub: HubPublicKey,
    ) -> Self {
        Self {
            node,
            tenant,
            doctype: doctype.into(),
            writer: SealedWriter::from_public_key(hub),
            values: BTreeMap::new(),
        }
    }

    /// The tenant the store is bound to.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// The DocType the store is bound to.
    #[must_use]
    pub fn doctype(&self) -> &str {
        &self.doctype
    }

    /// The node.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// The hub public key it seals to.
    #[must_use]
    pub fn hub_key(&self) -> HubPublicKey {
        self.writer.sealing_key().public_key()
    }

    /// Seals `plaintext` and keeps it, for the field `target` names.
    ///
    /// `everywhere`, `encrypted` and `home_only` are refused here: this store is for `sealed`
    /// groups only, and a value of another class belongs on its own path.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::Malformed`] when the RNG is unavailable, when `ad` is not bound to
    /// this store's tenant and DocType and to exactly the field and document `target` names,
    /// or when `target` names a hub key version other than the one this store seals to (a
    /// stale key after a rotation); and [`SealedFieldsError::SettingInvalid`] for a class this
    /// store does not hold. Every refusal happens before anything is sealed, stored or synced.
    pub fn write(
        &mut self,
        target: &SealTarget<'_>,
        plaintext: &[u8],
        ad: &SealedAd,
    ) -> Result<SealedField, SealedFieldsError> {
        let SealTarget {
            group,
            class,
            field,
            document,
            hub_key,
        } = *target;
        if !class.is_sealed() {
            return Err(SealedFieldsError::SettingInvalid {
                detail: "the node's sealed store holds sealed groups only".to_owned(),
            });
        }
        // The associated data is what the hub checks when it opens the blob. A write whose ad
        // names another tenant, DocType, field or document, or none, would store a blob the
        // place it sits in cannot authenticate (or one lifted from elsewhere), so it is
        // refused here, before anything is persisted or synced. The tenant and the
        // DocType come from the store, not from the caller of this call.
        if ad.tenant() != Some(&self.tenant)
            || ad.doctype() != Some(self.doctype.as_str())
            || ad.field() != Some(field)
            || ad.document() != Some(document)
        {
            return Err(SealedFieldsError::Malformed(
                "the associated data does not name this tenant, DocType, field and document",
            ));
        }
        // The key version of a sealed box is the hub public key it is sealed to. A write that
        // names another one was prepared against a key this node no longer seals to.
        if hub_key != &self.hub_key() {
            return Err(SealedFieldsError::Malformed(
                "the write names another hub key version",
            ));
        }
        let sealed = self.writer.seal(plaintext, ad)?;
        let stored = SealedField {
            field: field.to_owned(),
            group: group.clone(),
            document: document.to_owned(),
            sealed,
        };
        self.values
            .insert((document.to_owned(), field.to_owned()), stored.clone());
        tracing::debug!(
            node = %self.node,
            group = %group,
            field = field,
            document = document,
            "sealed a value on the node"
        );
        Ok(stored)
    }

    /// Reads a sealed value back **on the node**. Always refuses.
    ///
    /// This is the behaviour the class exists for: no grant, no key and no offline session
    /// lets a node read a sealed value. The answer is the specific error, not a missing field,
    /// so a caller learns the value exists and must be revealed at the hub.
    ///
    /// # Errors
    ///
    /// Always [`SealedFieldsError::SealedUnreadable`].
    pub fn read(&self, document: &str, field: &str) -> Result<Sealed, SealedFieldsError> {
        let _ = self.values.get(&(document.to_owned(), field.to_owned()));
        Err(SealedFieldsError::SealedUnreadable {
            field: field.to_owned(),
        })
    }

    /// The stored blob, for the sync writer that ships it to the hub. This is a blob, not a
    /// value: it is the one way a sealed field leaves the node.
    #[must_use]
    pub fn blob(&self, document: &str, field: &str) -> Option<&Sealed> {
        self.values
            .get(&(document.to_owned(), field.to_owned()))
            .map(|stored| &stored.sealed)
    }

    /// Every stored field, in `(document, field)` order.
    #[must_use]
    pub fn stored(&self) -> Vec<&SealedField> {
        self.values.values().collect()
    }

    /// How many sealed values the node holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the node holds no sealed value.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    // The node knows only a public key; these tests need no private half, so they run in a node
    // build too.
    fn hub_key(seed: u8) -> HubPublicKey {
        HubPublicKey::from_bytes([seed; crate::sealbox::PUBLIC_KEY_LEN])
    }

    fn group(name: &str) -> GroupName {
        GroupName::new(name).expect("group")
    }

    fn node() -> NodeId {
        NodeId::new("branch-1").expect("node")
    }

    fn tenant() -> TenantId {
        TenantId::new("acme").expect("tenant")
    }

    fn store() -> NodeFieldStore {
        NodeFieldStore::new(node(), tenant(), "Bank Account", hub_key(9))
    }

    fn ad() -> SealedAd {
        SealedAd::of_field(tenant(), "Bank Account", "iban", "BA-1")
    }

    fn target<'a>(group: &'a GroupName, key: &'a HubPublicKey) -> SealTarget<'a> {
        SealTarget {
            group,
            class: SyncClass::Sealed,
            field: "iban",
            document: "BA-1",
            hub_key: key,
        }
    }

    #[test]
    fn a_node_writes_a_sealed_value_and_cannot_read_it_back() {
        let mut store = store();
        let (g, key) = (group("bank_details"), hub_key(9));
        let stored = store
            .write(&target(&g, &key), b"DE89", &ad())
            .expect("write");
        assert_eq!(stored.document, "BA-1");
        let error = store
            .read("BA-1", "iban")
            .expect_err("a node cannot read it");
        assert_eq!(error.code(), crate::error::SEALED_UNREADABLE);
        assert!(error.to_string().contains("iban"));
    }

    #[test]
    fn the_store_owns_its_tenant_and_doctype() {
        let store = store();
        assert_eq!(store.tenant(), &tenant());
        assert_eq!(store.doctype(), "Bank Account");
    }

    #[test]
    fn the_stored_blob_is_not_the_value() {
        let mut store = store();
        let (g, key) = (group("bank_details"), hub_key(9));
        let stored = store
            .write(&target(&g, &key), b"DE89", &ad())
            .expect("write");
        assert!(!stored.sealed.to_bytes().windows(4).any(|w| w == b"DE89"));
        assert_eq!(store.blob("BA-1", "iban"), Some(&stored.sealed));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn the_store_refuses_a_non_sealed_class() {
        let mut store = store();
        let (g, key) = (group("notes"), hub_key(9));
        let mut wrong = target(&g, &key);
        wrong.class = SyncClass::Everywhere;
        let error = store.write(&wrong, b"x", &ad()).expect_err("not sealed");
        assert_eq!(error.code(), crate::error::SETTING_INVALID);
        assert!(store.is_empty());
    }

    #[test]
    fn a_write_naming_another_hub_key_version_is_refused_and_stores_nothing() {
        let mut store = store();
        let (g, stale) = (group("bank_details"), hub_key(8));
        let error = store
            .write(&target(&g, &stale), b"DE89", &ad())
            .expect_err("stale hub key");
        assert_eq!(error.code(), crate::error::MALFORMED);
        assert!(store.is_empty());
    }

    #[test]
    fn a_write_bound_to_another_tenant_or_doctype_than_the_store_is_refused() {
        let mut store = store();
        let (g, key) = (group("bank_details"), hub_key(9));
        let other_tenant = SealedAd::of_field(
            TenantId::new("other").expect("tenant"),
            "Bank Account",
            "iban",
            "BA-1",
        );
        let other_doctype = SealedAd::of_field(tenant(), "Other", "iban", "BA-1");
        for ad in [other_tenant, other_doctype, SealedAd::unbound()] {
            let error = store
                .write(&target(&g, &key), b"DE89", &ad)
                .expect_err("not this store's place");
            assert_eq!(error.code(), crate::error::MALFORMED);
        }
        assert!(store.is_empty());
    }

    #[test]
    fn a_read_of_something_never_written_is_the_same_refusal() {
        let store = store();
        let error = store.read("BA-9", "iban").expect_err("nothing");
        assert_eq!(error.code(), crate::error::SEALED_UNREADABLE);
    }
}
