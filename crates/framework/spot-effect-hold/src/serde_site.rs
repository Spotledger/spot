//! serde for [`SiteId`](spot_core::SiteId), which the contract crate does not serialise itself.
//!
//! A site id is its canonical host name; on the wire and in a ledger row it is that string.

use serde::{Deserialize, Deserializer, Serializer};
use spot_core::SiteId;

/// Serialises a site id as its host name.
///
/// # Errors
/// When the serializer refuses the string.
pub fn serialize<S: Serializer>(site: &SiteId, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(site.as_str())
}

/// Deserialises a site id from its host name, validating it.
///
/// # Errors
/// When the string is not a valid site id, or the deserializer fails.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SiteId, D::Error> {
    let text = String::deserialize(deserializer)?;
    SiteId::new(text).map_err(serde::de::Error::custom)
}
