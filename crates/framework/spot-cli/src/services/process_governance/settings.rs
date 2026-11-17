//! `[services.process_governance]`: the typed settings of the module.

use std::collections::BTreeSet;

use serde::Deserialize;
use spot_app_api::prelude::Name;
use spot_bpm_governance::{DEFAULT_REASON_MIN_CHARS, GovernanceMode};

/// The default administrator role names (the platform's own default grant role).
pub const DEFAULT_ADMIN_ROLE: &str = "System Manager";

/// The default key id the request grants are signed under.
pub const DEFAULT_KEY_ID: &str = "grant-1";

/// How long a grant lives at most, seconds (the request authority caps it at 900).
pub const DEFAULT_GRANT_TTL_SECS: u32 = 600;

/// How long an untouched draft is kept, days.
pub const DEFAULT_DRAFT_RETENTION_DAYS: u32 = 30;

/// One record type whose entry is governed.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GovernedEntry {
    /// The DocType (storage name).
    pub doctype: String,
    /// The company (organisation-unit path) the entry applies to and below; absent: the whole
    /// site.
    #[serde(default)]
    pub scope: Option<String>,
    /// `governed` or `process_only` (`free` is the default of everything not listed).
    pub mode: String,
}

/// The entry process of a record type, for `process_only` scopes.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryProcessEntry {
    /// The DocType.
    pub doctype: String,
    /// The scope the setting is set on; absent: the whole site.
    #[serde(default)]
    pub scope: Option<String>,
    /// The process to start. A record type with no entry gets the generated minimal process.
    pub process: String,
}

/// `[services.process_governance]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module does anything at all.
    pub enabled: bool,
    /// Whether an administrator may write a governed record outside a process. Off by default;
    /// every override is audited. Never applies to `process_only`.
    pub admin_override: bool,
    /// The role names that count as administrator for `admin_override`.
    pub admin_roles: Vec<String>,
    /// The ids of the keys request grants are signed under, the current one first. A grant
    /// signed under a listed older id still verifies, so a key can be rotated without breaking
    /// a step that is in flight.
    pub key_ids: Vec<String>,
    /// How long a grant lives, seconds (30 to 900).
    pub grant_ttl_secs: u32,
    /// How long a draft nobody touched is kept before the reaper removes it, days.
    pub draft_retention_days: u32,
    /// How often the lease holder refreshes the request table and reaps, seconds (5 to 3600).
    pub tick_secs: u64,
    /// The smallest accepted outside-process reason, characters.
    pub reason_min_chars: usize,
    /// The company paths a record may name besides the site itself. A claim outside this list
    /// (and the site) is refused in `process_only`.
    pub companies: Vec<String>,
    /// The governed record types. Empty: governance is off everywhere.
    pub governed: Vec<GovernedEntry>,
    /// The entry processes of `process_only` record types.
    pub entry: Vec<EntryProcessEntry>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            admin_override: false,
            admin_roles: vec![DEFAULT_ADMIN_ROLE.to_owned()],
            key_ids: vec![DEFAULT_KEY_ID.to_owned()],
            grant_ttl_secs: DEFAULT_GRANT_TTL_SECS,
            draft_retention_days: DEFAULT_DRAFT_RETENTION_DAYS,
            tick_secs: 30,
            reason_min_chars: DEFAULT_REASON_MIN_CHARS,
            companies: Vec::new(),
            governed: Vec::new(),
            entry: Vec::new(),
        }
    }
}

impl Settings {
    /// Checks every range and name; the error is the key that is wrong, never its value.
    ///
    /// # Errors
    /// The name of the offending key.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !(30..=900).contains(&self.grant_ttl_secs) {
            return Err("grant_ttl_secs");
        }
        if !(1..=3650).contains(&self.draft_retention_days) {
            return Err("draft_retention_days");
        }
        if !(5..=3600).contains(&self.tick_secs) {
            return Err("tick_secs");
        }
        if !(1..=1000).contains(&self.reason_min_chars) {
            return Err("reason_min_chars");
        }
        if self.admin_roles.is_empty() || self.admin_roles.iter().any(|r| r.trim().is_empty()) {
            return Err("admin_roles");
        }
        if self.key_ids.is_empty()
            || self.key_ids.len() > 4
            || !self.key_ids.iter().all(|id| valid_key_id(id))
            || self.key_ids.iter().collect::<BTreeSet<_>>().len() != self.key_ids.len()
        {
            return Err("key_ids");
        }
        for entry in &self.governed {
            if Name::new(&entry.doctype).is_err() {
                return Err("governed.doctype");
            }
            match GovernanceMode::parse(&entry.mode) {
                Some(GovernanceMode::Governed | GovernanceMode::ProcessOnly) => {}
                _ => return Err("governed.mode"),
            }
            if entry.scope.as_deref().is_some_and(invalid_scope) {
                return Err("governed.scope");
            }
        }
        for entry in &self.entry {
            if Name::new(&entry.doctype).is_err() {
                return Err("entry.doctype");
            }
            if entry.process.trim().is_empty() {
                return Err("entry.process");
            }
            if entry.scope.as_deref().is_some_and(invalid_scope) {
                return Err("entry.scope");
            }
        }
        if self.companies.iter().any(|c| invalid_scope(c)) {
            return Err("companies");
        }
        Ok(())
    }

    /// The DocTypes the settings govern, for the participant's fast path.
    #[must_use]
    pub fn covered(&self) -> BTreeSet<String> {
        self.governed.iter().map(|g| g.doctype.clone()).collect()
    }
}

/// A key id is 1 to 64 characters of `a-z`, `0-9`, `_` and `-`.
fn valid_key_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// A scope path is non-blank, has no empty unit and no whitespace or control character.
fn invalid_scope(scope: &str) -> bool {
    scope.is_empty()
        || scope.split('/').any(str::is_empty)
        || scope.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_valid() {
        let settings = Settings::default();
        assert!(settings.validate().is_ok());
        assert!(settings.governed.is_empty());
        assert!(!settings.admin_override);
        assert_eq!(settings.draft_retention_days, 30);
    }

    #[test]
    fn unknown_keys_and_bad_values_are_refused() {
        let bad: Result<Settings, _> = toml::from_str("admin_overide = true");
        assert!(bad.is_err());
        let mut s = Settings::default();
        s.governed.push(GovernedEntry {
            doctype: "sales_order".to_owned(),
            scope: None,
            mode: "free".to_owned(),
        });
        assert_eq!(s.validate(), Err("governed.mode"));
        s.governed[0].mode = "governed".to_owned();
        assert!(s.validate().is_ok());
        s.key_ids = vec!["Bad Key".to_owned()];
        assert_eq!(s.validate(), Err("key_ids"));
        s.key_ids = vec!["a".to_owned(), "a".to_owned()];
        assert_eq!(s.validate(), Err("key_ids"));
        let s = Settings {
            grant_ttl_secs: 901,
            ..Settings::default()
        };
        assert_eq!(s.validate(), Err("grant_ttl_secs"));
        let s = Settings {
            draft_retention_days: 0,
            ..Settings::default()
        };
        assert_eq!(s.validate(), Err("draft_retention_days"));
    }

    #[test]
    fn covered_lists_each_doctype_once() {
        let mut s = Settings::default();
        for scope in [None, Some("acme".to_owned())] {
            s.governed.push(GovernedEntry {
                doctype: "invoice".to_owned(),
                scope,
                mode: "governed".to_owned(),
            });
        }
        assert_eq!(s.covered().len(), 1);
    }
}
