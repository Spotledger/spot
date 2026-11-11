//! Values to columns and back. A document that does not decode is a store failure, never a
//! default: a corrupt row fails closed.

use serde::Serialize;
use serde::de::DeserializeOwned;
use spot_link_exchange::{Error, Result};

use crate::error::store_failed;

pub(crate) fn corrupt() -> Error {
    store_failed("a stored exchange row is not valid")
}

pub(crate) fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| store_failed("a time, sequence or version is out of range"))
}

pub(crate) fn to_u64(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| corrupt())
}

pub(crate) fn json<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|_| store_failed("an exchange value could not be encoded"))
}

pub(crate) fn from_json<T: DeserializeOwned>(text: &str) -> Result<T> {
    serde_json::from_str(text).map_err(|_| corrupt())
}
