//! Unit tests for `spot-settings`.
//!
//! Split by concern; the golden screen fixture lives in `tests/fixtures/`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_mgmt_ops::{ParamType, ParamValue, Risk};

use crate::builder::{RegistryBuilder, SettingsRegistry};
use crate::model::{Restart, Scope};
use crate::spec::SettingSpec;

mod builder;
mod catalogue;
mod changes;
mod diff;
mod resolution;
mod screen;
mod search;

/// A minimal, valid spec with the given value type, default and scopes.
pub(crate) fn spec(
    key: &str,
    value: ParamType,
    default: Option<ParamValue>,
    scopes: Vec<Scope>,
) -> SettingSpec {
    SettingSpec {
        key: key.to_owned(),
        title: title_of(key),
        summary: format!("Summary of {key}."),
        description: None,
        value,
        default,
        scopes,
        restart: Restart::None,
        risk: Risk::Low,
        secret: false,
        read_only: false,
        since: "0.1.0".to_owned(),
        deprecated: None,
        tags: Vec::new(),
        depends_on: None,
        help_url: None,
        group: None,
        order: None,
    }
}

/// A bounded integer spec.
pub(crate) fn int_spec(
    key: &str,
    min: i64,
    max: i64,
    default: i64,
    scopes: Vec<Scope>,
) -> SettingSpec {
    spec(
        key,
        ParamType::Int { min, max },
        Some(ParamValue::Int(default)),
        scopes,
    )
}

/// A text spec.
pub(crate) fn text_spec(key: &str, default: &str, scopes: Vec<Scope>) -> SettingSpec {
    spec(
        key,
        ParamType::Text {
            max_len: 64,
            pattern: None,
        },
        Some(ParamValue::Text(default.to_owned())),
        scopes,
    )
}

/// Builds a registry, panicking on a validation error (tests that expect success).
pub(crate) fn build(specs: impl IntoIterator<Item = SettingSpec>) -> SettingsRegistry {
    match RegistryBuilder::new().register_all(specs).build() {
        Ok(registry) => registry,
        Err(report) => panic!("expected a valid registry, got:\n{report}"),
    }
}

/// The codes in a build or validation report, for assertions.
pub(crate) fn codes(report: &crate::error::SettingsErrorReport) -> Vec<&'static str> {
    report.errors().iter().map(|error| error.code()).collect()
}

/// A humanised title from a key's last segment.
fn title_of(key: &str) -> String {
    key.rsplit('.')
        .next()
        .unwrap_or(key)
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
