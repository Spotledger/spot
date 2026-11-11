//! The setting specification: the typed schema for one setting.

use serde::{Deserialize, Serialize};
use spot_mgmt_ops::{ParamType, ParamValue, Risk};

use crate::model::{DependsOn, Deprecated, Restart, Scope};

/// The complete specification for one setting.
///
/// Serialises to and from TOML. The `key` is dotted lower-case,
/// `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$`, and its first segment is the owning area
/// (`os`, `database`, `runtime`, `tenant`, `company`, `app`).
///
/// The value type is a [`ParamType`] from `spot-mgmt-ops`, so the same setting can be set
/// through a management operation without translation and validated by the one shared check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingSpec {
    /// The dotted lower-case key, for example `runtime.http.listen_address`.
    pub key: String,

    /// A one-line human name, shown in lists and search results.
    pub title: String,

    /// One sentence, for search results and hover text.
    pub summary: String,

    /// Longer markdown documentation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// The value type and its constraints.
    pub value: ParamType,

    /// The default value, required for every non-secret setting and forbidden for a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<ParamValue>,

    /// The scopes at which the setting may be set, ordered broad to narrow. Never empty.
    pub scopes: Vec<Scope>,

    /// Whether changing the setting needs a restart.
    #[serde(default)]
    pub restart: Restart,

    /// The risk level of changing the setting.
    pub risk: Risk,

    /// Whether the value is a secret: never shown, logged or hashed.
    #[serde(default)]
    pub secret: bool,

    /// Whether the setting is read-only (observed, never set).
    #[serde(default)]
    pub read_only: bool,

    /// The version that introduced the setting.
    pub since: String,

    /// Deprecation metadata, when the setting is deprecated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deprecated: Option<Deprecated>,

    /// Classification tags, used by search.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// Conditional relevance: the setting is visible only while another key holds a value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<DependsOn>,

    /// A documentation URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help_url: Option<String>,

    /// An explicit screen-group title; when absent the group is derived from the key path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,

    /// A screen order hint; rows with a smaller order come first (ties break on the key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
}

impl SettingSpec {
    /// Whether two segments or more, each `[a-z][a-z0-9_]*`, separated by dots.
    ///
    /// # Errors
    ///
    /// Returns a sentence naming the first offending segment when the key does not match.
    pub fn validate_key(key: &str) -> Result<(), String> {
        let segments: Vec<&str> = key.split('.').collect();
        if segments.len() < 2 {
            return Err("key must have at least two segments (area.name)".to_owned());
        }
        for segment in &segments {
            if !is_segment(segment) {
                return Err(format!(
                    "segment `{segment}` must match [a-z][a-z0-9_]* (lower-case letters, digits, underscore)"
                ));
            }
        }
        Ok(())
    }

    /// The result of [`SettingSpec::validate_key`] on this spec's key.
    ///
    /// # Errors
    ///
    /// Returns the sentence from [`SettingSpec::validate_key`].
    pub fn key_error(&self) -> Result<(), String> {
        Self::validate_key(&self.key)
    }

    /// The owning area: the key's first segment.
    #[must_use]
    pub fn area(&self) -> &str {
        self.key.split('.').next().unwrap_or("")
    }
}

/// Whether one key segment matches `[a-z][a-z0-9_]*`.
fn is_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
