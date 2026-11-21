//! Grants, directions, schema ranges, field lists and the two-sided offer match.
//!
//! A [`Grant`] is the unit of permission on a link: one topic, one kind (a record mirror or a
//! message), a direction, a schema range, the fields that may cross, an expiry, an optional
//! peer-region requirement and whether an arrival emits a delivery notice (ADR-058 rules 6, 8,
//! 9, 14).
//!
//! # Whose side a direction names
//!
//! A direction is always read **from the side that holds the grant**. `Send` on this side means
//! this side sends to the peer; the peer's mirrored grant for the same flow reads `Receive`.
//! [`grants_match`] converts between the two views and returns the agreement **from the local
//! side**, and [`grants_digest`] converts it once more to a party-order view so both sides
//! compute the same digest.
//!
//! `require_peer_region` and `notify` are deliberately **not** symmetric: each is this side's
//! own outbound policy and inbound preference, enforced by the side that holds it. They are
//! therefore excluded from the agreement digest; every other field is agreed by both sides.
//!
//! # Fields
//!
//! [`FieldList::filter`] drops everything not listed: the default is that nothing leaves. A
//! field classified personal or sensitive by metadata ([`FieldCatalog`], the
//! `spot-data-policy` classification) may only cross when it is **listed by name**;
//! [`FieldList::lint`] refuses a namespace rule that would sweep such a field along.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use spot_data_policy::{Classification, Region};

use crate::error::{Error, Result};
use crate::ids::{Digest, Topic};
use crate::model::Party;
use crate::region::RegionOracle;
use crate::util::{put_bool, put_field, put_u32, put_u64};

/// The kind of exchange topic (ADR-058 rule 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopicKind {
    /// The sender's record is mirrored into the receiver's schema, read-only (rule 5).
    Record,
    /// An immutable addressed message, stored in the receiver's framework inbox.
    Message,
}

impl TopicKind {
    /// The kind as metadata text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::Message => "message",
        }
    }
}

impl fmt::Display for TopicKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A direction of exchange, read from the side that holds the grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// This side sends to the peer.
    Send,
    /// This side receives from the peer.
    Receive,
    /// Both ways.
    Both,
}

const SEND_FLOW: u8 = 1;
const RECEIVE_FLOW: u8 = 2;

impl Direction {
    /// The direction as metadata text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Receive => "receive",
            Self::Both => "both",
        }
    }

    /// The same permission seen from the other side.
    #[must_use]
    pub fn flip(self) -> Self {
        match self {
            Self::Send => Self::Receive,
            Self::Receive => Self::Send,
            Self::Both => Self::Both,
        }
    }

    /// Whether this direction includes every flow of `other`.
    #[must_use]
    pub fn covers(self, other: Self) -> bool {
        self.flows() & other.flows() == other.flows()
    }

    fn flows(self) -> u8 {
        match self {
            Self::Send => SEND_FLOW,
            Self::Receive => RECEIVE_FLOW,
            Self::Both => SEND_FLOW | RECEIVE_FLOW,
        }
    }

    fn from_flows(flows: u8) -> Option<Self> {
        match flows {
            SEND_FLOW => Some(Self::Send),
            RECEIVE_FLOW => Some(Self::Receive),
            f if f == SEND_FLOW | RECEIVE_FLOW => Some(Self::Both),
            _ => None,
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An inclusive range of topic schema versions (ADR-058 rule 14).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaRange {
    /// The lowest accepted version, at least 1.
    pub min: u32,
    /// The highest accepted version, at least `min`.
    pub max: u32,
}

impl SchemaRange {
    /// A checked range.
    ///
    /// # Errors
    /// [`Error::InvalidSchemaRange`] when `min` is 0 or above `max`.
    pub fn new(min: u32, max: u32) -> Result<Self> {
        if min == 0 {
            return Err(Error::InvalidSchemaRange {
                reason: "the lowest schema version is at least 1",
            });
        }
        if min > max {
            return Err(Error::InvalidSchemaRange {
                reason: "the lowest schema version is above the highest",
            });
        }
        Ok(Self { min, max })
    }

    /// A single version.
    ///
    /// # Errors
    /// [`Error::InvalidSchemaRange`] when the version is 0.
    pub fn one(version: u32) -> Result<Self> {
        Self::new(version, version)
    }

    /// Whether `version` is inside the range.
    #[must_use]
    pub fn contains(&self, version: u32) -> bool {
        (self.min..=self.max).contains(&version)
    }

    /// The overlap of two ranges, if any.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Option<Self> {
        let min = self.min.max(other.min);
        let max = self.max.min(other.max);
        (min <= max).then_some(Self { min, max })
    }
}

/// A field name: one or more dotted segments of `[a-z][a-z0-9_]*`, at most 128 characters.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FieldName(String);

impl FieldName {
    /// Validates a field name.
    ///
    /// # Errors
    /// [`Error::InvalidField`] when the value is malformed.
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let segment_ok = |segment: &str| {
            let mut chars = segment.chars();
            chars.next().is_some_and(|c| c.is_ascii_lowercase())
                && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        };
        if value.is_empty() || value.len() > 128 {
            return Err(Error::InvalidField {
                reason: "a field name has 1 to 128 characters",
            });
        }
        if !value.split('.').all(segment_ok) {
            return Err(Error::InvalidField {
                reason: "a field name is dotted lower-case segments",
            });
        }
        Ok(Self(value))
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FieldName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for FieldName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FieldName({})", self.0)
    }
}

impl TryFrom<String> for FieldName {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        Self::new(value)
    }
}

impl From<FieldName> for String {
    fn from(value: FieldName) -> Self {
        value.0
    }
}

/// One field value that may cross a link. There is deliberately no float variant (ADR-12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldValue {
    /// Text.
    Text(String),
    /// A signed integer (money and quantities travel as minor units or text).
    Int(i64),
    /// A boolean.
    Bool(bool),
}

/// A record or message payload as field values.
///
/// Nothing here knows what the fields mean; apps declare topics and fields, the platform carries
/// them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExchangePayload {
    values: BTreeMap<FieldName, FieldValue>,
}

impl ExchangePayload {
    /// An empty payload.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces one field.
    pub fn insert(&mut self, field: FieldName, value: FieldValue) -> Option<FieldValue> {
        self.values.insert(field, value)
    }

    /// One field's value.
    #[must_use]
    pub fn get(&self, field: &FieldName) -> Option<&FieldValue> {
        self.values.get(field)
    }

    /// Every field, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&FieldName, &FieldValue)> {
        self.values.iter()
    }

    /// The number of fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the payload holds no field.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

/// What [`FieldList::filter`] let through, and the names it withheld.
///
/// The withheld list carries **names only**, never values: a dropped secret cannot leak through
/// a log line, a debug print or an error.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filtered {
    values: ExchangePayload,
    withheld: Vec<FieldName>,
}

impl Filtered {
    /// The values that may cross.
    #[must_use]
    pub fn values(&self) -> &ExchangePayload {
        &self.values
    }

    /// The names of the fields that did not cross.
    #[must_use]
    pub fn withheld(&self) -> &[FieldName] {
        &self.withheld
    }

    /// Consumes the result, returning the values that may cross.
    #[must_use]
    pub fn into_values(self) -> ExchangePayload {
        self.values
    }
}

/// The field metadata a topic declares, through which the classification is read.
///
/// An adapter over the app's manifest/metadata implements this; the crate itself never guesses a
/// classification. A field absent from the answer is treated as public for filtering (it still
/// has to be listed) but cannot satisfy the explicit-listing lint.
pub trait FieldCatalog {
    /// Every field of `topic` that the metadata classifies, with its classification.
    fn classified(&self, topic: &Topic, kind: TopicKind) -> Vec<(FieldName, Classification)>;
}

/// The names that may cross a link.
///
/// Default deny: a name crosses only when it is listed by name, or falls under a listed
/// namespace. A namespace (such as `party`) is shorthand for records with many public fields;
/// a personal or sensitive field under it must still be listed by name (see [`FieldList::lint`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldList {
    exact: BTreeSet<FieldName>,
    namespaces: BTreeSet<FieldName>,
}

impl FieldList {
    /// An empty list: nothing leaves.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// A list of fields named one by one.
    #[must_use]
    pub fn only(fields: impl IntoIterator<Item = FieldName>) -> Self {
        Self {
            exact: fields.into_iter().collect(),
            namespaces: BTreeSet::new(),
        }
    }

    /// This list plus a namespace rule covering `namespace` and everything below it.
    #[must_use]
    pub fn with_namespace(mut self, namespace: FieldName) -> Self {
        self.namespaces.insert(namespace);
        self
    }

    /// The fields listed by name.
    #[must_use]
    pub fn exact(&self) -> &BTreeSet<FieldName> {
        &self.exact
    }

    /// The namespace rules.
    #[must_use]
    pub fn namespaces(&self) -> &BTreeSet<FieldName> {
        &self.namespaces
    }

    /// Whether the list names no field and no namespace.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.namespaces.is_empty()
    }

    /// Whether `field` may cross under this list.
    #[must_use]
    pub fn includes(&self, field: &FieldName) -> bool {
        self.exact.contains(field)
            || self
                .namespaces
                .iter()
                .any(|namespace| under_namespace(field.as_str(), namespace.as_str()))
    }

    /// Drops every field this list does not name.
    #[must_use]
    pub fn filter(&self, payload: &ExchangePayload) -> Filtered {
        let mut values = ExchangePayload::new();
        let mut withheld = Vec::new();
        for (field, value) in payload.iter() {
            if self.includes(field) {
                values.insert(field.clone(), value.clone());
            } else {
                withheld.push(field.clone());
            }
        }
        Filtered { values, withheld }
    }

    /// The fields both lists name, and the namespaces both cover.
    #[must_use]
    pub fn intersect(&self, other: &Self) -> Self {
        let mut exact = BTreeSet::new();
        for field in self.exact.iter().chain(other.exact.iter()) {
            if self.includes(field) && other.includes(field) {
                exact.insert(field.clone());
            }
        }
        let mut namespaces = BTreeSet::new();
        for namespace in &self.namespaces {
            if other.covers_namespace(namespace) {
                namespaces.insert(namespace.clone());
            }
        }
        for namespace in &other.namespaces {
            if self.covers_namespace(namespace) {
                namespaces.insert(namespace.clone());
            }
        }
        Self { exact, namespaces }
    }

    /// Whether every field this list allows is also allowed by `other`.
    ///
    /// A namespace must be covered by a namespace of `other`; individual names can be covered by
    /// either a name or a namespace there.
    #[must_use]
    pub fn is_subset_of(&self, other: &Self) -> bool {
        self.exact.iter().all(|field| other.includes(field))
            && self
                .namespaces
                .iter()
                .all(|namespace| other.covers_namespace(namespace))
    }

    /// The lint of ADR-058 rule 6: a personal or sensitive field must be listed by name.
    ///
    /// # Errors
    /// [`Error::FieldClassification`] naming the first protected field that a namespace rule
    /// would carry without the field being listed explicitly.
    pub fn lint(&self, catalog: &dyn FieldCatalog, topic: &Topic, kind: TopicKind) -> Result<()> {
        for (field, classification) in catalog.classified(topic, kind) {
            if classification.is_personal() && self.includes(&field) && !self.exact.contains(&field)
            {
                return Err(Error::FieldClassification {
                    field: field.as_str().to_owned(),
                    class: class_name(classification),
                });
            }
        }
        Ok(())
    }

    fn covers_namespace(&self, namespace: &FieldName) -> bool {
        self.namespaces.iter().any(|candidate| {
            candidate == namespace
                || namespace
                    .as_str()
                    .strip_prefix(candidate.as_str())
                    .is_some_and(|rest| rest.starts_with('.'))
        })
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_u32(out, u32::try_from(self.exact.len()).unwrap_or(u32::MAX));
        for field in &self.exact {
            put_field(out, field.as_str().as_bytes());
        }
        put_u32(
            out,
            u32::try_from(self.namespaces.len()).unwrap_or(u32::MAX),
        );
        for namespace in &self.namespaces {
            put_field(out, namespace.as_str().as_bytes());
        }
    }
}

fn under_namespace(field: &str, namespace: &str) -> bool {
    field
        .strip_prefix(namespace)
        .is_some_and(|rest| rest.starts_with('.'))
}

fn class_name(classification: Classification) -> &'static str {
    match classification {
        Classification::Pii => "pii",
        Classification::Sensitive => "sensitive",
        Classification::Public | Classification::Internal => "public",
    }
}

/// One entry of a link: what may cross, how, until when and under which peer-region condition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// The exchange topic.
    pub topic: Topic,
    /// A record mirror or a message.
    pub kind: TopicKind,
    /// The direction, read from the side that holds this grant.
    pub direction: Direction,
    /// The schema versions this grant accepts.
    pub schema_range: SchemaRange,
    /// The fields that may cross; nothing else does.
    pub fields: FieldList,
    /// Unix milliseconds after which the grant is no longer valid.
    pub expires: u64,
    /// When set, this side sends only to a peer in this region (ADR-058 rule 6).
    pub require_peer_region: Option<Region>,
    /// Whether an arrival on this topic emits a delivery notice (ADR-058 rule 9).
    pub notify: bool,
}

impl Grant {
    /// The grant's shape and expiry, checked against the link's creation time.
    ///
    /// # Errors
    /// [`Error::InvalidExpiry`] when the grant expires at or before the link was created.
    pub fn validate(&self, created: u64) -> Result<()> {
        if self.expires <= created {
            return Err(Error::InvalidExpiry {
                reason: "a grant expires after the link was created",
            });
        }
        Ok(())
    }

    /// Whether this grant covers `direction` at `now`.
    #[must_use]
    pub fn covers(&self, direction: Direction, now: u64) -> bool {
        self.expires > now && self.direction.covers(direction)
    }

    /// Checks this side's peer-region requirement through the oracle.
    ///
    /// A refusal from the oracle keeps the policy clause it named
    /// (`spot-data-policy::Clause`), so the caller can show exactly which residency clause said
    /// no.
    ///
    /// # Errors
    /// [`Error::RegionRefused`] when the oracle refuses the peer's region or does not know it,
    /// [`Error::PeerRegionMismatch`] when it is not the region the grant requires.
    pub fn check_peer_region(&self, peer: &Party, oracle: &dyn RegionOracle) -> Result<()> {
        let Some(required) = &self.require_peer_region else {
            return Ok(());
        };
        let found = oracle
            .peer_region(peer)
            .map_err(|refusal| Error::RegionRefused {
                clause: refusal.clause.to_string(),
                region: refusal.region.to_string(),
            })?
            .ok_or_else(|| Error::RegionRefused {
                clause: spot_data_policy::Clause::UnknownResource.to_string(),
                region: "unknown".to_owned(),
            })?;
        if &found != required {
            return Err(Error::PeerRegionMismatch {
                required: required.to_string(),
                found: found.to_string(),
            });
        }
        Ok(())
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        put_field(out, self.topic.as_str().as_bytes());
        out.push(match self.kind {
            TopicKind::Record => 0,
            TopicKind::Message => 1,
        });
        out.push(match self.direction {
            Direction::Send => 0,
            Direction::Receive => 1,
            Direction::Both => 2,
        });
        put_u32(out, self.schema_range.min);
        put_u32(out, self.schema_range.max);
        self.fields.encode(out);
        put_u64(out, self.expires);
        match &self.require_peer_region {
            Some(region) => {
                out.push(1);
                put_field(out, region.as_str().as_bytes());
            }
            None => out.push(0),
        }
        put_bool(out, self.notify);
    }

    /// The grant as both sides must agree on it: in party order, without the two local-only
    /// fields (`require_peer_region`, `notify`).
    fn encode_agreed(&self, local: &Party, parties: &[Party; 2], out: &mut Vec<u8>) {
        let direction = if local == &parties[0] {
            self.direction
        } else {
            self.direction.flip()
        };
        put_field(out, self.topic.as_str().as_bytes());
        out.push(match self.kind {
            TopicKind::Record => 0,
            TopicKind::Message => 1,
        });
        out.push(match direction {
            Direction::Send => 0,
            Direction::Receive => 1,
            Direction::Both => 2,
        });
        put_u32(out, self.schema_range.min);
        put_u32(out, self.schema_range.max);
        self.fields.encode(out);
        put_u64(out, self.expires);
    }
}

/// One side's proposal of what may cross. Every grant's direction is read from `side`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offer {
    /// The side that made the offer.
    pub side: Party,
    /// The topics it offers.
    pub grants: Vec<Grant>,
}

impl Offer {
    /// An offer by `side`.
    ///
    /// # Errors
    /// [`Error::InvalidOffer`] when a topic appears twice.
    pub fn new(side: Party, grants: Vec<Grant>) -> Result<Self> {
        let offer = Self { side, grants };
        let mut seen = BTreeSet::new();
        for grant in &offer.grants {
            if !seen.insert(grant.topic.clone()) {
                return Err(Error::InvalidOffer {
                    reason: "an offer lists a topic only once",
                });
            }
        }
        Ok(offer)
    }

    /// The shape checks against the link that carries this offer.
    ///
    /// # Errors
    /// A malformed grant or expiry, or a record topic on an external carrier (ADR-058 rule 12).
    pub fn validate(&self, created: u64, carrier: crate::model::Carrier) -> Result<()> {
        for grant in &self.grants {
            grant.validate(created)?;
            if carrier == crate::model::Carrier::External && grant.kind == TopicKind::Record {
                return Err(Error::ExternalRecordTopic {
                    topic: grant.topic.as_str().to_owned(),
                });
            }
        }
        Ok(())
    }

    /// The grant for `topic`, if offered.
    #[must_use]
    pub fn grant(&self, topic: &Topic) -> Option<&Grant> {
        self.grants.iter().find(|grant| &grant.topic == topic)
    }
}

/// The topics both sides listed, agreed from the local side's point of view.
///
/// A topic only one side listed simply does not cross: the result is the intersection. A topic
/// both listed but on which they disagree (kind, direction, schema range) is an error, never a
/// guess.
///
/// # Errors
/// [`Error::InvalidOffer`] when both offers come from the same side;
/// [`Error::OfferConflict`] when a shared topic's kind, direction or schema range disagrees.
pub fn grants_match(local: &Offer, remote: &Offer) -> Result<Vec<Grant>> {
    if local.side == remote.side {
        return Err(Error::InvalidOffer {
            reason: "a link joins two different parties",
        });
    }
    let mut agreed = Vec::new();
    for mine in &local.grants {
        let Some(theirs) = remote.grant(&mine.topic) else {
            continue;
        };
        if mine.kind != theirs.kind {
            return Err(Error::OfferConflict {
                topic: mine.topic.as_str().to_owned(),
                reason: "kind",
            });
        }
        let their_flows = theirs.direction.flip().flows();
        let Some(direction) = Direction::from_flows(mine.direction.flows() & their_flows) else {
            return Err(Error::OfferConflict {
                topic: mine.topic.as_str().to_owned(),
                reason: "direction",
            });
        };
        let Some(schema_range) = mine.schema_range.intersect(&theirs.schema_range) else {
            return Err(Error::OfferConflict {
                topic: mine.topic.as_str().to_owned(),
                reason: "schema_range",
            });
        };
        agreed.push(Grant {
            topic: mine.topic.clone(),
            kind: mine.kind,
            direction,
            schema_range,
            fields: mine.fields.intersect(&theirs.fields),
            expires: mine.expires.min(theirs.expires),
            // This side's own outbound residency policy and inbound notice preference.
            require_peer_region: mine.require_peer_region.clone(),
            notify: mine.notify,
        });
    }
    Ok(agreed)
}

/// Whether `candidate` stays inside what both offers allow.
///
/// This is the check behind "a grant never exceeds the topics both sides listed": a grant wider
/// than either offer — a broader direction, schema range, field list or a later expiry — is
/// refused with the dimension named, never trimmed silently.
///
/// # Errors
/// [`Error::GrantWider`] naming `topic`, `kind`, `direction`, `schema_range`, `fields`, `expiry`
/// or `region`.
pub fn grant_allowed(candidate: &Grant, local: &Offer, remote: &Offer) -> Result<()> {
    let wider = |dimension: &'static str| {
        Err(Error::GrantWider {
            topic: candidate.topic.as_str().to_owned(),
            dimension,
        })
    };
    let Some(mine) = local.grant(&candidate.topic) else {
        return wider("topic");
    };
    let Some(theirs) = remote.grant(&candidate.topic) else {
        return wider("topic");
    };
    if candidate.kind != mine.kind || candidate.kind != theirs.kind {
        return wider("kind");
    }
    if !mine.direction.covers(candidate.direction)
        || !theirs.direction.flip().covers(candidate.direction)
    {
        return wider("direction");
    }
    if !schema_contains(&mine.schema_range, &candidate.schema_range)
        || !schema_contains(&theirs.schema_range, &candidate.schema_range)
    {
        return wider("schema_range");
    }
    if !candidate.fields.is_subset_of(&mine.fields)
        || !candidate.fields.is_subset_of(&theirs.fields)
    {
        return wider("fields");
    }
    if candidate.expires > mine.expires || candidate.expires > theirs.expires {
        return wider("expiry");
    }
    if candidate.require_peer_region != mine.require_peer_region {
        return wider("region");
    }
    if candidate.notify != mine.notify {
        return wider("notify");
    }
    Ok(())
}

fn schema_contains(outer: &SchemaRange, inner: &SchemaRange) -> bool {
    outer.min <= inner.min && outer.max >= inner.max
}

/// The digest both sides compute over the agreed grants, in party order.
///
/// `grants` are read from `local`; the direction is converted to the party-order view and the
/// two local-only fields are left out, so the two sides' copies produce one digest.
#[must_use]
pub fn grants_digest(grants: &[Grant], local: &Party, parties: &[Party; 2]) -> Digest {
    let mut sorted: Vec<&Grant> = grants.iter().collect();
    sorted.sort_by(|a, b| a.topic.cmp(&b.topic));
    let mut bytes = Vec::new();
    put_field(&mut bytes, b"spot.tenant-link.grants.v1");
    put_u32(&mut bytes, u32::try_from(sorted.len()).unwrap_or(u32::MAX));
    for grant in sorted {
        grant.encode_agreed(local, parties, &mut bytes);
    }
    Digest::of(&bytes)
}
