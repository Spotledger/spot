//! Catalog-free validation of a [`PresetPolicy`]: the rules that need no metadata.
//!
//! Every problem is reported at once, each with a stable code, a plain reason and a suggested
//! fix. The catalog-wide lints L1 to L7 of ADR-063 section 6 are WP-SYN-01.T2.

use std::collections::BTreeSet;

use spot_app_api::{AppError, ErrorClass};

use super::{Obligation, Preset, PresetPolicy, Selector};

/// The stable dotted codes of the preset validation. Each is catalogued in `sync_policy.toml`.
pub mod codes {
    /// `relay` is set on a preset other than `record`.
    pub const PRESET_RELAY_NOT_RECORD: &str = "sync_policy.preset_relay_not_record";
    /// `adopt` is set on a preset other than `handover`.
    pub const PRESET_ADOPT_NOT_HANDOVER: &str = "sync_policy.preset_adopt_not_handover";
    /// `owner_key` is set on a preset other than `record` or `handover`.
    pub const PRESET_OWNER_KEY_NOT_OWNED: &str = "sync_policy.preset_owner_key_not_owned";
    /// `scope` is set on a `local` policy.
    pub const PRESET_SCOPE_ON_LOCAL: &str = "sync_policy.preset_scope_on_local";
    /// A field appears more than once in `excluded_fields`.
    pub const PRESET_EXCLUDED_DUPLICATE: &str = "sync_policy.preset_excluded_duplicate";
    /// `effective_from_field` is set but the obligation is not `mandatory`.
    pub const PRESET_EFFECTIVE_NEEDS_MANDATORY: &str =
        "sync_policy.preset_effective_needs_mandatory";
    /// A `local` policy carries a setting other than the preset.
    pub const PRESET_LOCAL_HAS_SETTINGS: &str = "sync_policy.preset_local_has_settings";
    /// A group tag selector has an empty or control-character tag.
    pub const PRESET_BAD_GROUP: &str = "sync_policy.preset_bad_group";
    /// The closure overrides contradict each other or are zero.
    pub const PRESET_CLOSURE_BOUNDS: &str = "sync_policy.preset_closure_bounds";
}

/// One problem found in a [`PresetPolicy`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresetIssue {
    /// The stable `sync_policy.preset_*` code.
    pub code: &'static str,
    /// What is wrong, in plain words.
    pub reason: String,
    /// What to do about it.
    pub fix: String,
}

impl PresetIssue {
    fn new(code: &'static str, reason: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
            fix: fix.into(),
        }
    }

    /// The full message: the code, the reason and the fix.
    #[must_use]
    pub fn message(&self) -> String {
        format!("{}: {} ({})", self.code, self.reason, self.fix)
    }

    /// The platform error, classed [`ErrorClass::Validation`].
    #[must_use]
    pub fn to_error(&self) -> AppError {
        AppError::new(ErrorClass::Validation, self.code, self.message())
    }
}

impl PresetPolicy {
    /// Checks the rules that need no catalog and returns every problem at once.
    ///
    /// # Errors
    ///
    /// The list of [`PresetIssue`]s, in a fixed order, when the policy is inconsistent.
    pub fn validate(&self) -> Result<(), Vec<PresetIssue>> {
        let mut issues = Vec::new();
        let preset = self.preset;

        if self.relay.is_some() && preset != Preset::Record {
            issues.push(PresetIssue::new(
                codes::PRESET_RELAY_NOT_RECORD,
                format!("`relay` is only for a `record` policy, but the preset is `{preset}`"),
                "remove `relay`, or change the preset to `record`",
            ));
        }
        if self.adopt.is_some() && preset != Preset::Handover {
            issues.push(PresetIssue::new(
                codes::PRESET_ADOPT_NOT_HANDOVER,
                format!("`adopt` is only for a `handover` policy, but the preset is `{preset}`"),
                "remove `adopt`, or change the preset to `handover`",
            ));
        }
        if self.owner_key.is_some() && !matches!(preset, Preset::Record | Preset::Handover) {
            issues.push(PresetIssue::new(
                codes::PRESET_OWNER_KEY_NOT_OWNED,
                format!(
                    "`owner_key` is only for `record` and `handover`, but the preset is `{preset}`"
                ),
                "remove `owner_key`, or use a preset that a node creates",
            ));
        }
        if self.scope.is_some() && preset == Preset::Local {
            issues.push(PresetIssue::new(
                codes::PRESET_SCOPE_ON_LOCAL,
                "`scope` is set on a `local` policy, which is never sent anywhere",
                "remove `scope`, or choose a preset that syncs",
            ));
        }
        self.duplicate_exclusions(&mut issues);
        if self.effective_from_field.is_some()
            && preset != Preset::Local
            && self.obligation_class() != Obligation::Mandatory
        {
            issues.push(PresetIssue::new(
                codes::PRESET_EFFECTIVE_NEEDS_MANDATORY,
                "`effective_from_field` needs a delivery obligation, but the obligation is normal",
                "set `obligation = \"mandatory\"`, or remove `effective_from_field`",
            ));
        }
        self.local_settings(&mut issues);
        self.selectors(&mut issues);
        self.closure_bounds(&mut issues);

        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }

    fn duplicate_exclusions(&self, issues: &mut Vec<PresetIssue>) {
        let mut seen = BTreeSet::new();
        let mut reported = BTreeSet::new();
        for field in self.excluded_fields.iter_all() {
            if !seen.insert(field) && reported.insert(field) {
                issues.push(PresetIssue::new(
                    codes::PRESET_EXCLUDED_DUPLICATE,
                    format!("field `{field}` is listed more than once in `excluded_fields`"),
                    "list each field in exactly one of local, down or up",
                ));
            }
        }
    }

    /// A `local` policy carries no setting but the preset (and `scope`, which has its own code).
    fn local_settings(&self, issues: &mut Vec<PresetIssue>) {
        if self.preset != Preset::Local {
            return;
        }
        let mut set = Vec::new();
        if self.owner_key.is_some() {
            set.push("owner_key");
        }
        if self.relay.is_some() {
            set.push("relay");
        }
        if !self.excluded_fields.is_empty() {
            set.push("excluded_fields");
        }
        if self.adopt.is_some() {
            set.push("adopt");
        }
        if self.when_empty != super::WhenEmpty::None {
            set.push("when_empty");
        }
        if self.obligation.is_some() {
            set.push("obligation");
        }
        if self.effective_from_field.is_some() {
            set.push("effective_from_field");
        }
        if self.stale_policy != super::StalePolicy::AllowStamp {
            set.push("stale_policy");
        }
        if !self.closure.is_empty() {
            set.push("closure");
        }
        if !set.is_empty() {
            issues.push(PresetIssue::new(
                codes::PRESET_LOCAL_HAS_SETTINGS,
                format!(
                    "a `local` policy carries no other setting, but these are set: {}",
                    set.join(", ")
                ),
                "remove them, or choose a preset that syncs",
            ));
        }
    }

    fn selectors(&self, issues: &mut Vec<PresetIssue>) {
        for (name, selector) in [("scope", &self.scope), ("relay", &self.relay)] {
            if let Some(Selector::Group(tag)) = selector
                && (tag.is_empty() || tag.chars().any(char::is_control))
            {
                issues.push(PresetIssue::new(
                    codes::PRESET_BAD_GROUP,
                    format!("the group tag in `{name}` is empty or holds a control character"),
                    "name a group tag registered in the node registry",
                ));
            }
        }
    }

    fn closure_bounds(&self, issues: &mut Vec<PresetIssue>) {
        let c = &self.closure;
        let zero =
            c.max_records == Some(0) || c.max_bytes == Some(0) || c.hard_limit_records == Some(0);
        let inverted = matches!(
            (c.max_records, c.hard_limit_records),
            (Some(cap), Some(hard)) if cap > hard
        );
        if zero || inverted {
            issues.push(PresetIssue::new(
                codes::PRESET_CLOSURE_BOUNDS,
                if zero {
                    "a closure bound of zero would split every bundle"
                } else {
                    "`max_records` is larger than `hard_limit_records`"
                },
                "use positive bounds with `max_records` not above `hard_limit_records`",
            ));
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::super::{
        AdoptMode, ClosureOverrides, KeyRef, Obligation, Preset, PresetPolicy, Selector,
        StalePolicy, WhenEmpty,
    };
    use super::codes;
    use crate::policy::{FieldName, ScopeKeyType};

    fn field(name: &str) -> FieldName {
        FieldName::new(name).expect("field")
    }

    fn key() -> KeyRef {
        KeyRef {
            field: field("warehouse"),
            key_type: ScopeKeyType::new("warehouse").expect("key type"),
        }
    }

    fn codes_of(p: &PresetPolicy) -> Vec<&'static str> {
        p.validate()
            .err()
            .unwrap_or_default()
            .into_iter()
            .map(|i| i.code)
            .collect()
    }

    #[test]
    fn every_default_policy_is_valid() {
        for preset in Preset::ALL {
            assert_eq!(PresetPolicy::new(preset).validate(), Ok(()), "{preset}");
        }
    }

    #[test]
    fn relay_only_on_record() {
        let mut p = PresetPolicy::new(Preset::Record);
        p.relay = Some(Selector::All);
        assert_eq!(p.validate(), Ok(()));
        for preset in [Preset::Master, Preset::Handover] {
            let mut p = PresetPolicy::new(preset);
            p.relay = Some(Selector::All);
            assert_eq!(codes_of(&p), vec![codes::PRESET_RELAY_NOT_RECORD]);
        }
    }

    #[test]
    fn adopt_only_on_handover() {
        let mut p = PresetPolicy::new(Preset::Handover);
        p.adopt = Some(AdoptMode::Auto);
        assert_eq!(p.validate(), Ok(()));
        for preset in [Preset::Master, Preset::Record] {
            let mut p = PresetPolicy::new(preset);
            p.adopt = Some(AdoptMode::Never);
            assert_eq!(codes_of(&p), vec![codes::PRESET_ADOPT_NOT_HANDOVER]);
        }
    }

    #[test]
    fn owner_key_only_on_record_and_handover() {
        for preset in [Preset::Record, Preset::Handover] {
            let mut p = PresetPolicy::new(preset);
            p.owner_key = Some(key());
            assert_eq!(p.validate(), Ok(()), "{preset}");
        }
        let mut p = PresetPolicy::new(Preset::Master);
        p.owner_key = Some(key());
        assert_eq!(codes_of(&p), vec![codes::PRESET_OWNER_KEY_NOT_OWNED]);
    }

    #[test]
    fn scope_not_on_local() {
        let mut p = PresetPolicy::local();
        p.scope = Some(Selector::All);
        assert_eq!(codes_of(&p), vec![codes::PRESET_SCOPE_ON_LOCAL]);
        let mut p = PresetPolicy::new(Preset::Handover);
        p.scope = Some(Selector::Group("kiosks".to_owned()));
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn excluded_fields_are_unique_across_the_three_lists() {
        let mut p = PresetPolicy::new(Preset::Master);
        p.excluded_fields.local = vec![field("cost_price")];
        p.excluded_fields.down = vec![field("margin")];
        p.excluded_fields.up = vec![field("notes")];
        assert_eq!(p.validate(), Ok(()));

        p.excluded_fields.down.push(field("cost_price"));
        p.excluded_fields.up.push(field("cost_price"));
        // reported once however many times it repeats
        assert_eq!(codes_of(&p), vec![codes::PRESET_EXCLUDED_DUPLICATE]);

        let mut q = PresetPolicy::new(Preset::Master);
        q.excluded_fields.local = vec![field("a"), field("a")];
        assert_eq!(codes_of(&q), vec![codes::PRESET_EXCLUDED_DUPLICATE]);
    }

    #[test]
    fn effective_from_field_implies_mandatory() {
        // master defaults to normal: rejected until the obligation is raised
        let mut p = PresetPolicy::new(Preset::Master);
        p.effective_from_field = Some(field("valid_from"));
        assert_eq!(codes_of(&p), vec![codes::PRESET_EFFECTIVE_NEEDS_MANDATORY]);
        p.obligation = Some(Obligation::Normal);
        assert_eq!(codes_of(&p), vec![codes::PRESET_EFFECTIVE_NEEDS_MANDATORY]);
        p.obligation = Some(Obligation::Mandatory);
        assert_eq!(p.validate(), Ok(()));
        // record is mandatory by default
        let mut r = PresetPolicy::new(Preset::Record);
        r.effective_from_field = Some(field("valid_from"));
        assert_eq!(r.validate(), Ok(()));
    }

    #[test]
    fn local_carries_no_other_setting() {
        let mut p = PresetPolicy::local();
        p.obligation = Some(Obligation::Mandatory);
        p.stale_policy = StalePolicy::Block;
        p.when_empty = WhenEmpty::All;
        p.excluded_fields.local = vec![field("secret")];
        p.closure.max_records = Some(10);
        assert_eq!(codes_of(&p), vec![codes::PRESET_LOCAL_HAS_SETTINGS]);
        let issues = p.validate().expect_err("issues");
        assert!(issues[0].reason.contains("obligation"));
        assert!(issues[0].reason.contains("closure"));
        assert!(!issues[0].fix.is_empty());
    }

    #[test]
    fn group_tag_must_be_printable_and_nonempty() {
        let mut p = PresetPolicy::new(Preset::Master);
        p.scope = Some(Selector::Group(String::new()));
        assert_eq!(codes_of(&p), vec![codes::PRESET_BAD_GROUP]);
        p.scope = Some(Selector::Group("kiosks".to_owned()));
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn closure_bounds_must_be_positive_and_ordered() {
        let mut p = PresetPolicy::new(Preset::Master);
        p.closure = ClosureOverrides {
            max_records: Some(500),
            max_bytes: Some(4_000_000),
            hard_limit_records: Some(5_000),
        };
        assert_eq!(p.validate(), Ok(()));
        p.closure.max_records = Some(6_000);
        assert_eq!(codes_of(&p), vec![codes::PRESET_CLOSURE_BOUNDS]);
        p.closure.max_records = Some(0);
        assert_eq!(codes_of(&p), vec![codes::PRESET_CLOSURE_BOUNDS]);
    }

    #[test]
    fn all_problems_are_reported_at_once_with_a_fix() {
        let mut p = PresetPolicy::new(Preset::Master);
        p.relay = Some(Selector::All);
        p.adopt = Some(AdoptMode::Auto);
        p.owner_key = Some(key());
        let issues = p.validate().expect_err("issues");
        assert_eq!(issues.len(), 3);
        for issue in issues {
            assert!(issue.code.starts_with("sync_policy.preset_"));
            assert!(!issue.reason.is_empty() && !issue.fix.is_empty());
            assert_eq!(issue.to_error().code, issue.code);
        }
    }
}
