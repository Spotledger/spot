//! The metadata text form of a [`PresetPolicy`]: one DocType property, [`SYNC_PRESET_KEY`], whose
//! value is TOML, written inline (`preset = "master", scope = "all"`) or over several lines, the
//! same shape as the EDG-08 `sync_policy` property in [`crate::text`]. It is a **separate**
//! property name, so the old property is untouched.
//!
//! | Setting | Text |
//! | --- | --- |
//! | `preset` | `local`, `master`, `record`, `handover` (required) |
//! | `scope`, `relay` | `all`, `group:<tag>` or `key:<field>@<key_type>`; `scope = "none"` on a master sends it nowhere |
//! | `owner_key` | `<field>@<key_type>` |
//! | `adopt` | `manual`, `auto`, `never` |
//! | `when_empty` | `all`, `none` |
//! | `obligation` | `normal`, `mandatory` |
//! | `effective_from_field` | a field name |
//! | `stale_policy` | `allow_stamp`, `warn`, `block` |
//! | `excluded_fields` | `{ local = [..], down = [..], up = [..] }` |
//! | `closure` | `{ max_records = n, max_bytes = n, hard_limit_records = n }` |
//!
//! [`PresetPolicy::to_text`] writes the canonical multi-line form; it writes only what differs
//! from the preset's defaults, so [`PresetPolicy::parse_text`] of it gives the same policy back.

use serde::Deserialize;

use super::{
    AdoptMode, ClosureOverrides, ExcludedFields, KeyRef, Obligation, Preset, PresetPolicy,
    Selector, StalePolicy, WhenEmpty,
};
use crate::error::PolicyError;
use crate::policy::{FieldName, ScopeKeyType};

/// The DocType property key that carries a preset policy.
pub const SYNC_PRESET_KEY: &str = "sync_preset";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExcludedText {
    #[serde(default)]
    local: Vec<String>,
    #[serde(default)]
    down: Vec<String>,
    #[serde(default)]
    up: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClosureText {
    max_records: Option<u32>,
    max_bytes: Option<u64>,
    hard_limit_records: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresetText {
    preset: String,
    scope: Option<String>,
    owner_key: Option<String>,
    relay: Option<String>,
    excluded_fields: Option<ExcludedText>,
    adopt: Option<String>,
    when_empty: Option<String>,
    obligation: Option<String>,
    effective_from_field: Option<String>,
    stale_policy: Option<String>,
    closure: Option<ClosureText>,
}

impl PresetPolicy {
    /// Parses the value of a [`SYNC_PRESET_KEY`] property.
    ///
    /// # Errors
    ///
    /// [`PolicyError`] with [`crate::codes::BAD_VALUE`] when the TOML is malformed, a setting is
    /// unknown, or a value does not fit its setting. Consistency between settings is
    /// [`PresetPolicy::validate`], not parsing.
    pub fn parse_text(value: &str) -> Result<Self, PolicyError> {
        let text: PresetText = if value.contains('\n') {
            toml::from_str(value)
        } else {
            #[derive(Deserialize)]
            struct Inline {
                sync_preset: PresetText,
            }
            toml::from_str::<Inline>(&format!("sync_preset = {{ {value} }}")).map(|i| i.sync_preset)
        }
        .map_err(|e| PolicyError::bad_value(SYNC_PRESET_KEY, first_line(&e.to_string())))?;

        let preset = enum_value(Preset::parse, Preset::TEXTS, "preset", &text.preset)?;
        let mut p = PresetPolicy::new(preset);
        if let Some(scope) = &text.scope {
            p.scope = if scope == "none" {
                None
            } else {
                Some(parse_selector("scope", scope)?)
            };
        }
        if let Some(relay) = &text.relay {
            p.relay = Some(parse_selector("relay", relay)?);
        }
        if let Some(owner_key) = &text.owner_key {
            p.owner_key = Some(parse_key_ref("owner_key", owner_key)?);
        }
        if let Some(x) = &text.adopt {
            p.adopt = Some(enum_value(AdoptMode::parse, AdoptMode::TEXTS, "adopt", x)?);
        }
        if let Some(x) = &text.when_empty {
            p.when_empty = enum_value(WhenEmpty::parse, WhenEmpty::TEXTS, "when_empty", x)?;
        }
        if let Some(x) = &text.obligation {
            p.obligation = Some(enum_value(
                Obligation::parse,
                Obligation::TEXTS,
                "obligation",
                x,
            )?);
        }
        if let Some(x) = &text.effective_from_field {
            p.effective_from_field = Some(FieldName::new(x)?);
        }
        if let Some(x) = &text.stale_policy {
            p.stale_policy = enum_value(StalePolicy::parse, StalePolicy::TEXTS, "stale_policy", x)?;
        }
        if let Some(x) = text.excluded_fields {
            p.excluded_fields = ExcludedFields {
                local: names(&x.local)?,
                down: names(&x.down)?,
                up: names(&x.up)?,
            };
        }
        if let Some(x) = text.closure {
            p.closure = ClosureOverrides {
                max_records: x.max_records,
                max_bytes: x.max_bytes,
                hard_limit_records: x.hard_limit_records,
            };
        }
        Ok(p)
    }

    /// Parses a property from its key and value, mirroring [`crate::PolicyProperty::parse`].
    ///
    /// # Errors
    ///
    /// [`crate::codes::UNKNOWN_PROPERTY`] for any key but [`SYNC_PRESET_KEY`], else as
    /// [`PresetPolicy::parse_text`].
    pub fn parse_property(key: &str, value: &str) -> Result<Self, PolicyError> {
        if key == SYNC_PRESET_KEY {
            Self::parse_text(value)
        } else {
            Err(PolicyError::unknown_property(key))
        }
    }

    /// The canonical multi-line text form: only what differs from the preset's defaults.
    #[must_use]
    pub fn to_text(&self) -> String {
        let defaults = PresetPolicy::new(self.preset);
        let mut lines = vec![format!("preset = {}", quote(self.preset.as_str()))];
        if self.scope != defaults.scope {
            let text = self
                .scope
                .as_ref()
                .map_or_else(|| "none".to_owned(), selector_text);
            lines.push(format!("scope = {}", quote(&text)));
        }
        if let Some(k) = &self.owner_key {
            lines.push(format!("owner_key = {}", quote(&key_text(k))));
        }
        if let Some(r) = &self.relay {
            lines.push(format!("relay = {}", quote(&selector_text(r))));
        }
        if let Some(a) = self.adopt {
            lines.push(format!("adopt = {}", quote(a.as_str())));
        }
        if self.when_empty != WhenEmpty::None {
            lines.push(format!("when_empty = {}", quote(self.when_empty.as_str())));
        }
        if let Some(o) = self.obligation {
            lines.push(format!("obligation = {}", quote(o.as_str())));
        }
        if let Some(f) = &self.effective_from_field {
            lines.push(format!("effective_from_field = {}", quote(f.as_str())));
        }
        if self.stale_policy != StalePolicy::AllowStamp {
            lines.push(format!(
                "stale_policy = {}",
                quote(self.stale_policy.as_str())
            ));
        }
        if !self.excluded_fields.is_empty() {
            let mut parts = Vec::new();
            for (name, list) in [
                ("local", &self.excluded_fields.local),
                ("down", &self.excluded_fields.down),
                ("up", &self.excluded_fields.up),
            ] {
                if !list.is_empty() {
                    let items: Vec<String> = list.iter().map(|f| quote(f.as_str())).collect();
                    parts.push(format!("{name} = [{}]", items.join(", ")));
                }
            }
            lines.push(format!("excluded_fields = {{ {} }}", parts.join(", ")));
        }
        if !self.closure.is_empty() {
            let mut parts = Vec::new();
            if let Some(n) = self.closure.max_records {
                parts.push(format!("max_records = {n}"));
            }
            if let Some(n) = self.closure.max_bytes {
                parts.push(format!("max_bytes = {n}"));
            }
            if let Some(n) = self.closure.hard_limit_records {
                parts.push(format!("hard_limit_records = {n}"));
            }
            lines.push(format!("closure = {{ {} }}", parts.join(", ")));
        }
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }
}

fn quote(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

fn selector_text(selector: &Selector) -> String {
    match selector {
        Selector::All => "all".to_owned(),
        Selector::Group(tag) => format!("group:{tag}"),
        Selector::Key { field, key_type } => format!("key:{field}@{key_type}"),
    }
}

fn key_text(key: &KeyRef) -> String {
    format!("{}@{}", key.field, key.key_type)
}

fn parse_selector(setting: &str, text: &str) -> Result<Selector, PolicyError> {
    if text == "all" {
        return Ok(Selector::All);
    }
    if let Some(tag) = text.strip_prefix("group:") {
        return Ok(Selector::Group(tag.to_owned()));
    }
    if let Some(rest) = text.strip_prefix("key:") {
        let key = parse_key_ref(setting, rest)?;
        return Ok(Selector::Key {
            field: key.field,
            key_type: key.key_type,
        });
    }
    Err(PolicyError::bad_value(
        SYNC_PRESET_KEY,
        format!("`{setting}` is `{text}`, not one of all, group:<tag>, key:<field>@<key_type>"),
    ))
}

fn parse_key_ref(setting: &str, text: &str) -> Result<KeyRef, PolicyError> {
    let Some((field, key_type)) = text.split_once('@') else {
        return Err(PolicyError::bad_value(
            SYNC_PRESET_KEY,
            format!("`{setting}` is `{text}`, not <field>@<key_type>"),
        ));
    };
    Ok(KeyRef {
        field: FieldName::new(field)?,
        key_type: ScopeKeyType::new(key_type)?,
    })
}

fn names(list: &[String]) -> Result<Vec<FieldName>, PolicyError> {
    list.iter().map(|n| FieldName::new(n)).collect()
}

fn enum_value<T>(
    parse: fn(&str) -> Option<T>,
    allowed: &[&str],
    setting: &str,
    value: &str,
) -> Result<T, PolicyError> {
    parse(value).ok_or_else(|| {
        PolicyError::bad_value(
            SYNC_PRESET_KEY,
            format!(
                "`{setting}` is `{value}`, not one of {}",
                allowed.join(", ")
            ),
        )
    })
}

fn first_line(text: &str) -> String {
    match text.split_once('\n') {
        Some((first, _)) => first.trim_end().to_owned(),
        None => text.trim_end().to_owned(),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    use crate::codes;

    fn field(name: &str) -> FieldName {
        FieldName::new(name).expect("field")
    }

    fn full() -> PresetPolicy {
        let mut p = PresetPolicy::new(Preset::Record);
        p.scope = Some(Selector::Group("north".to_owned()));
        p.owner_key = Some(KeyRef {
            field: field("warehouse"),
            key_type: ScopeKeyType::new("warehouse").expect("kt"),
        });
        p.relay = Some(Selector::Key {
            field: field("to_warehouse"),
            key_type: ScopeKeyType::new("warehouse").expect("kt"),
        });
        p.excluded_fields = ExcludedFields {
            local: vec![field("cost_price")],
            down: vec![field("margin")],
            up: vec![field("pin"), field("note")],
        };
        p.when_empty = WhenEmpty::All;
        p.obligation = Some(Obligation::Normal);
        p.effective_from_field = Some(field("valid_from"));
        p.stale_policy = StalePolicy::Block;
        p.closure = ClosureOverrides {
            max_records: Some(500),
            max_bytes: Some(4_194_304),
            hard_limit_records: Some(5_000),
        };
        p
    }

    #[test]
    fn golden_text_of_a_minimal_master() {
        let mut p = PresetPolicy::new(Preset::Master);
        p.excluded_fields.local = vec![field("cost_price")];
        assert_eq!(
            p.to_text(),
            "preset = \"master\"\nexcluded_fields = { local = [\"cost_price\"] }\n"
        );
        assert_eq!(PresetPolicy::local().to_text(), "preset = \"local\"\n");
    }

    #[test]
    fn golden_text_of_every_setting() {
        assert_eq!(
            full().to_text(),
            "preset = \"record\"\n\
             scope = \"group:north\"\n\
             owner_key = \"warehouse@warehouse\"\n\
             relay = \"key:to_warehouse@warehouse\"\n\
             when_empty = \"all\"\n\
             obligation = \"normal\"\n\
             effective_from_field = \"valid_from\"\n\
             stale_policy = \"block\"\n\
             excluded_fields = { local = [\"cost_price\"], down = [\"margin\"], up = [\"pin\", \"note\"] }\n\
             closure = { max_records = 500, max_bytes = 4194304, hard_limit_records = 5000 }\n"
        );
    }

    #[test]
    fn text_round_trips_for_every_preset_and_for_unusual_settings() {
        for preset in Preset::ALL {
            let p = PresetPolicy::new(preset);
            assert_eq!(PresetPolicy::parse_text(&p.to_text()), Ok(p));
        }
        assert_eq!(PresetPolicy::parse_text(&full().to_text()), Ok(full()));

        // a master sent nowhere, a handover with an explicit adopt, a group tag needing escapes
        let mut nowhere = PresetPolicy::new(Preset::Master);
        nowhere.scope = None;
        assert!(nowhere.to_text().contains("scope = \"none\""));
        assert_eq!(PresetPolicy::parse_text(&nowhere.to_text()), Ok(nowhere));

        let mut h = PresetPolicy::new(Preset::Handover);
        h.adopt = Some(AdoptMode::Auto);
        h.scope = Some(Selector::Group("say \"hi\"".to_owned()));
        assert_eq!(PresetPolicy::parse_text(&h.to_text()), Ok(h));
    }

    #[test]
    fn inline_form_matches_multiline() {
        let inline = "preset = \"handover\", adopt = \"auto\", scope = \"all\"";
        let multi = "preset = \"handover\"\nadopt = \"auto\"\nscope = \"all\"\n";
        let a = PresetPolicy::parse_text(inline).expect("inline");
        assert_eq!(Ok(a.clone()), PresetPolicy::parse_text(multi));
        assert_eq!(a.adopt, Some(AdoptMode::Auto));
        assert_eq!(a.scope, Some(Selector::All));
    }

    #[test]
    fn bad_text_is_a_bad_value_that_names_the_setting() {
        for (text, needle) in [
            ("preset = \"shared\"", "preset"),
            ("preset = \"master\", scope = \"some\"", "scope"),
            (
                "preset = \"record\", owner_key = \"warehouse\"",
                "owner_key",
            ),
            ("preset = \"handover\", adopt = \"later\"", "adopt"),
            (
                "preset = \"master\", stale_policy = \"ignore\"",
                "stale_policy",
            ),
            ("preset = \"master\", bogus = 1", "sync_preset"),
            ("scope = \"all\"", "sync_preset"),
            (
                "preset = \"record\", effective_from_field = \"Bad Name\"",
                "field name",
            ),
        ] {
            let err = PresetPolicy::parse_text(text).expect_err(text);
            assert_eq!(err.code(), codes::BAD_VALUE, "{text}");
            assert!(err.detail().contains(needle), "{text}: {}", err.detail());
        }
    }

    #[test]
    fn property_key_is_separate_from_the_old_one() {
        assert_eq!(SYNC_PRESET_KEY, "sync_preset");
        assert!(!crate::text::KEYS.contains(&SYNC_PRESET_KEY));
        assert!(PresetPolicy::parse_property("sync_preset", "preset = \"local\"").is_ok());
        let err =
            PresetPolicy::parse_property("sync_policy", "class = \"local\"").expect_err("key");
        assert_eq!(err.code(), codes::UNKNOWN_PROPERTY);
    }
}
