//! Lease-bound reads of an `encrypted` group, and the home-only filter (ADR-070 rule 12.13 a,
//! c, e).
//!
//! An `encrypted` field is stored under its group data key. A node reads it only while the
//! lease on that key is live: [`EncryptedStore::read`] asks [`NodeKeyStore`] for the key at the
//! caller's [`MonoTime`] and fails closed with [`SealedFieldsError::LeaseExpired`] once it has
//! passed. Nothing here reads a clock.
//!
//! A `home_only` field does not sync at all. [`SyncPolicy::outgoing_filter`] is what a sync,
//! journal or snapshot writer calls for one group before it builds a batch, and
//! [`SyncPolicy::read_at_branch`] is what a branch read path calls: it answers
//! [`BranchValue::Unavailable`] rather than an error, because the field exists and the reader
//! may know it exists.

use std::collections::BTreeMap;

use spot_permissions::Tag;
use spot_protected_keys::{
    FieldAd, FieldCipher, GroupName, KeyEpoch, MonoTime, NodeKeyStore, ProtectedKeyError,
};
use spot_secrets::SecretBytes;

use crate::channel::{ChannelGuard, Destination, Emission};
use crate::class::{SyncClass, SyncClassTable};
use crate::error::SealedFieldsError;

/// One field's stored form on a node: its group, its epoch and the ciphertext.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EncryptedField {
    /// The protection group the value belongs to.
    pub group: GroupName,
    /// The epoch of the data key it was written under.
    pub epoch: KeyEpoch,
    /// The ciphertext (`nonce || body || tag`).
    pub ciphertext: Vec<u8>,
}

impl EncryptedField {
    /// The stored form of `ciphertext`.
    #[must_use]
    pub fn new(group: GroupName, epoch: KeyEpoch, ciphertext: Vec<u8>) -> Self {
        Self {
            group,
            epoch,
            ciphertext,
        }
    }
}

/// The node side of an `encrypted` field: write under a held key, read under a live lease.
pub struct EncryptedStore<'a> {
    keys: &'a mut NodeKeyStore,
}

impl<'a> EncryptedStore<'a> {
    /// A store over the node's held keys.
    pub fn new(keys: &'a mut NodeKeyStore) -> Self {
        Self { keys }
    }

    /// Encrypts `plaintext` under the node's held key for `group` at `epoch`, binding `ad`.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::LeaseExpired`] when the lease has passed,
    /// [`SealedFieldsError::UnknownGroup`] when the node holds no key for the group at that
    /// epoch, and [`SealedFieldsError::Tampered`] when the cipher fails.
    pub fn write(
        &mut self,
        group: &GroupName,
        epoch: KeyEpoch,
        ad: &FieldAd,
        plaintext: &[u8],
        now: MonoTime,
    ) -> Result<EncryptedField, SealedFieldsError> {
        // The binding (tenant, DocType, field, document, key version) must agree with the
        // context of the write: at least the key version (epoch) the caller named must be
        // the one the ciphertext is bound to.
        if ad.epoch() != epoch {
            return Err(SealedFieldsError::Malformed(
                "the associated data does not name this key version",
            ));
        }
        self.guard_lease(group, epoch, now)?;
        let sealed = self
            .keys
            .with_key(group, epoch, now, |key| {
                FieldCipher::encrypt(key, ad, plaintext)
            })
            .map_err(|error| map_key_error(error, group))?;
        let ciphertext = sealed.map_err(map_protected)?;
        Ok(EncryptedField::new(group.clone(), epoch, ciphertext))
    }

    /// Decrypts `field`, failing closed once the lease has passed.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::LeaseExpired`] when the lease has passed, and
    /// [`SealedFieldsError::Tampered`] for altered bytes or a moved ciphertext.
    pub fn read(
        &mut self,
        field: &EncryptedField,
        ad: &FieldAd,
        now: MonoTime,
    ) -> Result<SecretBytes, SealedFieldsError> {
        self.guard_lease(&field.group, field.epoch, now)?;
        let opened = self
            .keys
            .with_key(&field.group, field.epoch, now, |key| {
                FieldCipher::decrypt(key, ad, &field.ciphertext)
            })
            .map_err(|error| map_key_error(error, &field.group))?;
        opened.map_err(map_protected)
    }

    /// Whether the node still holds a live lease for `group` at `epoch`.
    #[must_use]
    pub fn lease_live(&self, group: &GroupName, epoch: KeyEpoch, now: MonoTime) -> bool {
        self.keys
            .lease_until(group, epoch)
            .is_some_and(|until| !now.is_at_or_after(until))
    }

    fn guard_lease(
        &self,
        group: &GroupName,
        epoch: KeyEpoch,
        now: MonoTime,
    ) -> Result<(), SealedFieldsError> {
        match self.keys.lease_until(group, epoch) {
            Some(until) if now.is_at_or_after(until) => Err(SealedFieldsError::LeaseExpired {
                group: group.clone(),
            }),
            Some(_) => Ok(()),
            None => Err(SealedFieldsError::UnknownGroup {
                group: group.clone(),
            }),
        }
    }
}

fn map_key_error(error: ProtectedKeyError, group: &GroupName) -> SealedFieldsError {
    match error {
        ProtectedKeyError::LeaseExpired { .. } => SealedFieldsError::LeaseExpired {
            group: group.clone(),
        },
        ProtectedKeyError::UnknownGroup { .. } | ProtectedKeyError::UnknownEpoch { .. } => {
            SealedFieldsError::UnknownGroup {
                group: group.clone(),
            }
        }
        ProtectedKeyError::NodeRevoked { node } => SealedFieldsError::NodeRevoked { node },
        ProtectedKeyError::Tampered => SealedFieldsError::Tampered,
        ProtectedKeyError::Malformed(detail) => SealedFieldsError::Malformed(detail),
        _ => SealedFieldsError::HubUnavailable,
    }
}

fn map_protected(error: ProtectedKeyError) -> SealedFieldsError {
    match error {
        ProtectedKeyError::Tampered => SealedFieldsError::Tampered,
        ProtectedKeyError::Malformed(detail) => SealedFieldsError::Malformed(detail),
        _ => SealedFieldsError::Tampered,
    }
}

/// One field of an outgoing batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingField {
    /// The field path.
    pub field: String,
    /// The protection group, when the field is protected.
    pub group: Option<GroupName>,
    /// The sync class, when the field is protected.
    pub class: Option<SyncClass>,
    /// The value's stored form.
    pub value: OutgoingValue,
}

impl OutgoingField {
    /// An unprotected field: it always travels.
    #[must_use]
    pub fn plain(field: impl Into<String>, value: OutgoingValue) -> Self {
        Self {
            field: field.into(),
            group: None,
            class: None,
            value,
        }
    }

    /// A protected field.
    #[must_use]
    pub fn protected(
        field: impl Into<String>,
        group: GroupName,
        class: SyncClass,
        value: OutgoingValue,
    ) -> Self {
        Self {
            field: field.into(),
            group: Some(group),
            class: Some(class),
            value,
        }
    }
}

/// What an outgoing batch carries for a field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutgoingValue {
    /// The value as itself.
    Plain(Vec<u8>),
    /// A sealed blob (safe to store and sync; a node can never open it).
    Sealed(Vec<u8>),
    /// A ciphertext under a group data key.
    Encrypted(Vec<u8>),
    /// The field's existence only.
    Placeholder,
}

/// The class-level half of the sync decision.
///
/// It delegates to [`ChannelGuard::decide`], so a sync writer and a channel cannot disagree. A
/// `home_only` field never leaves for a branch, an external channel or a backup; a `sealed`
/// field never leaves for a branch (a node writes and never reads).
#[must_use]
pub fn emission_for(class: SyncClass, destination: Destination) -> Emission {
    ChannelGuard::decide(class, destination)
}

/// A tenant's sync policy: the class of each declared protection group.
///
/// It is the object a sync, journal or snapshot writer holds. [`SyncPolicy::outgoing_filter`]
/// is the function the brief names: *does this group's field leave for this destination, and
/// how?*
pub struct SyncPolicy {
    table: SyncClassTable,
    tags: BTreeMap<GroupName, Vec<Tag>>,
}

impl SyncPolicy {
    /// A policy over `table`; `tags` gives each declared group's classification tags, which
    /// decide the default of a group the tenant did not list.
    #[must_use]
    pub fn new(table: SyncClassTable, tags: BTreeMap<GroupName, Vec<Tag>>) -> Self {
        Self { table, tags }
    }

    /// A policy where no group carries a tag: only the groups the table lists are known, and
    /// any other group fails closed ([`SyncPolicy::class_of`]).
    #[must_use]
    pub fn untagged(table: SyncClassTable) -> Self {
        Self::new(table, BTreeMap::new())
    }

    /// The table.
    #[must_use]
    pub fn table(&self) -> &SyncClassTable {
        &self.table
    }

    /// The class of `group`.
    ///
    /// A group the policy does not know (neither declared in the tag map nor chosen in the
    /// table) fails closed: it never falls through to `everywhere`, so a group removed from
    /// the policy after fields were written, or one the policy was never given, cannot start
    /// to sync freely.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::UnknownGroup`] for a group the policy does not know, and
    /// [`SealedFieldsError::ClassUnset`] for a tagged group with no explicit choice.
    pub fn class_of(&self, group: &GroupName) -> Result<SyncClass, SealedFieldsError> {
        let tags = match self.tags.get(group) {
            Some(tags) => tags.as_slice(),
            None if self.table.chosen(group).is_some() => &[],
            None => {
                return Err(SealedFieldsError::UnknownGroup {
                    group: group.clone(),
                });
            }
        };
        self.table.class_for(group, tags)
    }

    /// Whether `group`'s field may leave for `destination`, and how (deliverable 4).
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::UnknownGroup`] for an unmapped group and
    /// [`SealedFieldsError::ClassUnset`] when the group's class was never chosen.
    pub fn outgoing_filter(
        &self,
        group: &GroupName,
        destination: Destination,
    ) -> Result<Emission, SealedFieldsError> {
        Ok(emission_for(self.class_of(group)?, destination))
    }

    /// What a branch read path shows for `group`'s stored value.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::UnknownGroup`] for an unmapped group and
    /// [`SealedFieldsError::ClassUnset`] when the group's class was never chosen.
    pub fn read_at_branch(
        &self,
        group: &GroupName,
        stored: Option<Vec<u8>>,
    ) -> Result<BranchValue, SealedFieldsError> {
        Ok(read_at_branch(self.class_of(group)?, stored))
    }
}

/// Filters `fields` for an outgoing batch to `destination`.
///
/// A field the class does not allow through is removed from the batch: a `home_only` field for
/// a branch, a `sealed` field for a branch, and an `everywhere` or `encrypted` field that the
/// destination's rule masks is kept as its stored form. An unprotected field always travels; a grouped field with no class is dropped.
#[must_use]
pub fn filter_outgoing_batch(
    fields: Vec<OutgoingField>,
    destination: Destination,
) -> Vec<OutgoingField> {
    fields
        .into_iter()
        .filter(|field| match (&field.group, field.class) {
            (_, Some(class)) => !emission_for(class, destination).is_drop(),
            // A field in a protection group with no class is dropped, never let through.
            (Some(_), None) => false,
            (None, None) => true,
        })
        .collect()
}

/// What a branch read path shows for a field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BranchValue {
    /// The value may be shown (subject to the reader's grants).
    Available(Vec<u8>),
    /// The field does not sync to a branch; the screen shows it as unavailable.
    Unavailable,
}

impl BranchValue {
    /// Whether a value is available.
    #[must_use]
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }
}

/// What a branch read path answers for `class`.
///
/// `home_only` and `sealed` are [`BranchValue::Unavailable`]: neither can be read at a branch.
/// `everywhere` and `encrypted` are available, and an encrypted value is decrypted by the
/// caller through [`EncryptedStore`] under its lease.
#[must_use]
pub fn read_at_branch(class: SyncClass, stored: Option<Vec<u8>>) -> BranchValue {
    match class {
        SyncClass::HomeOnly | SyncClass::Sealed => BranchValue::Unavailable,
        SyncClass::Everywhere | SyncClass::Encrypted => {
            BranchValue::Available(stored.unwrap_or_default())
        }
    }
}

/// The explanation key a screen shows for an unavailable home-only field.
pub const HOME_ONLY_EXPLANATION: &str = concat!("error.", "sealed_fields.home_only");

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use spot_protected_keys::{
        Hub, KeyEpoch, MemoryRootKeyProvider, NodeId, NodeWrapKey, TenantId, Tier, TierTable,
    };

    fn group(name: &str) -> GroupName {
        GroupName::new(name).expect("group")
    }

    fn tenant() -> TenantId {
        TenantId::new("acme").expect("tenant")
    }

    fn node() -> NodeId {
        NodeId::new("branch-1").expect("node")
    }

    /// A node holding the key for `bank_details` under a one-hour lease.
    fn held_node(now: MonoTime) -> (NodeKeyStore, GroupName, KeyEpoch) {
        let mut hub = Hub::new(MemoryRootKeyProvider::new(tenant()).expect("root"));
        let g = group("bank_details");
        let epoch = hub.create_group(&g, MonoTime::ZERO).expect("group");
        let n = node();
        let hub_copy = NodeWrapKey::generate(n.clone()).expect("wrap");
        let node_copy = hub_copy.duplicate().expect("duplicate");
        hub.register_node(&n, hub_copy).expect("register");
        let mut tiers = TierTable::new();
        tiers.grant(n.clone(), g.clone(), Tier::one());
        let release = hub.release(&n, &g, now, &tiers).expect("release");
        let mut store = NodeKeyStore::new(n, node_copy);
        store.accept(&release, now).expect("accept");
        (store, g, epoch)
    }

    fn ad(epoch: KeyEpoch) -> FieldAd {
        FieldAd::new(tenant(), "Bank Account", "iban", "BA-1", epoch)
    }

    #[test]
    fn an_encrypted_field_round_trips_under_a_live_lease() {
        let now = MonoTime::from_millis(1_000);
        let (mut keys, g, epoch) = held_node(now);
        let mut store = EncryptedStore::new(&mut keys);
        let field = store
            .write(&g, epoch, &ad(epoch), b"DE89", now)
            .expect("write");
        assert_ne!(field.ciphertext.as_slice(), b"DE89");
        let opened = store.read(&field, &ad(epoch), now).expect("read");
        assert_eq!(opened.expose(), b"DE89");
    }

    #[test]
    fn a_read_after_the_lease_fails_closed() {
        let now = MonoTime::from_millis(1_000);
        let (mut keys, g, epoch) = held_node(now);
        let mut store = EncryptedStore::new(&mut keys);
        let field = store
            .write(&g, epoch, &ad(epoch), b"DE89", now)
            .expect("write");
        let after = now.plus(spot_protected_keys::Duration::from_hours(2));
        let error = store.read(&field, &ad(epoch), after).expect_err("expired");
        assert_eq!(error.code(), crate::error::LEASE_EXPIRED);
    }

    #[test]
    fn a_moved_ciphertext_fails_authentication() {
        let now = MonoTime::from_millis(1_000);
        let (mut keys, g, epoch) = held_node(now);
        let mut store = EncryptedStore::new(&mut keys);
        let field = store
            .write(&g, epoch, &ad(epoch), b"DE89", now)
            .expect("write");
        let other = FieldAd::new(tenant(), "Bank Account", "iban", "BA-2", epoch);
        assert_eq!(
            store.read(&field, &other, now).expect_err("moved").code(),
            crate::error::TAMPERED
        );
    }

    #[test]
    fn home_only_never_appears_in_an_outgoing_batch() {
        let batch = vec![
            OutgoingField::plain("number", OutgoingValue::Plain(b"BA-1".to_vec())),
            OutgoingField::protected(
                "iban",
                group("bank_details"),
                SyncClass::HomeOnly,
                OutgoingValue::Plain(b"DE89".to_vec()),
            ),
            OutgoingField::protected(
                "salary",
                group("hr"),
                SyncClass::Encrypted,
                OutgoingValue::Encrypted(vec![1, 2, 3]),
            ),
            OutgoingField::protected(
                "notes",
                group("notes"),
                SyncClass::Everywhere,
                OutgoingValue::Plain(b"hi".to_vec()),
            ),
        ];
        let filtered = filter_outgoing_batch(batch, Destination::Branch);
        let names: Vec<&str> = filtered.iter().map(|f| f.field.as_str()).collect();
        assert_eq!(names, vec!["number", "salary", "notes"]);
        assert!(!names.contains(&"iban"));
    }

    #[test]
    fn a_sealed_field_is_not_sent_to_a_branch() {
        let batch = vec![OutgoingField::protected(
            "iban",
            group("bank_details"),
            SyncClass::Sealed,
            OutgoingValue::Sealed(vec![9, 9, 9]),
        )];
        assert!(filter_outgoing_batch(batch.clone(), Destination::Branch).is_empty());
        assert_eq!(filter_outgoing_batch(batch, Destination::Hub).len(), 1);
    }

    #[test]
    fn the_policy_answers_by_group_and_destination() {
        let mut table = SyncClassTable::new();
        table.set(group("bank_details"), SyncClass::HomeOnly);
        table.set(group("hr"), SyncClass::Sealed);
        let mut tags = BTreeMap::new();
        tags.insert(group("notes"), Vec::new());
        let policy = SyncPolicy::new(table, tags);
        assert_eq!(
            policy
                .outgoing_filter(&group("bank_details"), Destination::Branch)
                .expect("class"),
            Emission::Drop
        );
        assert_eq!(
            policy
                .outgoing_filter(&group("hr"), Destination::Branch)
                .expect("class"),
            Emission::Drop
        );
        assert_eq!(
            policy
                .outgoing_filter(&group("hr"), Destination::Hub)
                .expect("class"),
            Emission::Emit
        );
        assert_eq!(
            policy
                .outgoing_filter(&group("notes"), Destination::Branch)
                .expect("default"),
            Emission::Emit
        );
        assert_eq!(
            policy
                .read_at_branch(&group("bank_details"), Some(b"DE89".to_vec()))
                .expect("class"),
            BranchValue::Unavailable
        );
    }

    #[test]
    fn the_policy_refuses_a_tagged_group_with_no_class() {
        let mut tags = BTreeMap::new();
        tags.insert(group("bank_details"), vec![Tag("secret".to_owned())]);
        let policy = SyncPolicy::new(SyncClassTable::new(), tags);
        assert_eq!(
            policy
                .outgoing_filter(&group("bank_details"), Destination::Branch)
                .expect_err("unset")
                .code(),
            crate::error::CLASS_UNSET
        );
    }

    #[test]
    fn a_branch_shows_home_only_as_unavailable() {
        assert_eq!(
            read_at_branch(SyncClass::HomeOnly, Some(b"DE89".to_vec())),
            BranchValue::Unavailable
        );
        assert_eq!(
            read_at_branch(SyncClass::Sealed, Some(b"x".to_vec())),
            BranchValue::Unavailable
        );
        assert!(read_at_branch(SyncClass::Everywhere, Some(b"x".to_vec())).is_available());
    }

    #[test]
    fn a_grouped_field_with_no_class_is_dropped_from_a_batch() {
        let orphan = OutgoingField {
            field: "iban".to_owned(),
            group: Some(group("bank_details")),
            class: None,
            value: OutgoingValue::Plain(b"DE89".to_vec()),
        };
        assert!(filter_outgoing_batch(vec![orphan], Destination::Branch).is_empty());
    }
}
