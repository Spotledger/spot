//! The generated screen model: groups, rows and controls derived from the schema.

use serde::{Deserialize, Serialize};
use spot_mgmt_ops::{ParamType, ParamValue, Risk};

use crate::builder::SettingsRegistry;
use crate::diff::is_different;
use crate::model::{EffectiveValue, Origin, Resolved, Restart, ScopeRef};
use crate::resolution::{Chain, resolve};
use crate::spec::SettingSpec;
use crate::store::ValueStore;

/// The control a console renders for one setting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Control {
    /// A boolean toggle.
    Toggle,
    /// A bounded number input.
    Number {
        /// The smallest accepted value.
        min: i64,
        /// The largest accepted value.
        max: i64,
        /// The increment between steps.
        step: i64,
    },
    /// A text input.
    Text {
        /// The maximum length in characters.
        max_len: usize,
        /// An optional whole-value pattern.
        pattern: Option<String>,
    },
    /// A choice from a closed list.
    Choice {
        /// The accepted choices, in display order.
        options: Vec<String>,
    },
    /// A whole-second duration input.
    Duration,
    /// A list editor over an item control.
    ListEditor {
        /// The control for one list element.
        item: Box<Control>,
    },
    /// A secret input: the value is never shown, only "set" or "not set".
    SecretInput,
    /// A read-only value display.
    ReadOnlyValue,
}

impl Control {
    /// The control for a value type.
    #[must_use]
    pub fn from_param_type(value_type: &ParamType) -> Self {
        match value_type {
            ParamType::Bool => Control::Toggle,
            ParamType::Int { min, max } => Control::Number {
                min: *min,
                max: *max,
                step: 1,
            },
            ParamType::Text { max_len, pattern } => Control::Text {
                max_len: *max_len,
                pattern: pattern.clone(),
            },
            ParamType::Enum(options) => Control::Choice {
                options: options.clone(),
            },
            ParamType::Duration => Control::Duration,
            ParamType::List(inner) => Control::ListEditor {
                item: Box::new(Control::from_param_type(inner)),
            },
        }
    }
}

/// One setting row in a generated screen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    /// The setting key.
    pub key: String,
    /// The setting title.
    pub title: String,
    /// The setting summary.
    pub summary: String,
    /// The control to render.
    pub control: Control,
    /// The effective value (`Redacted` for a set secret).
    pub effective: EffectiveValue,
    /// An explanation of where the value came from, for example
    /// `set at group:emea, overridden at server:srv-17`.
    pub origin_text: String,
    /// The default value, `None` for a secret.
    pub default: Option<ParamValue>,
    /// Whether the effective value differs from the default.
    pub differs: bool,
    /// The restart badge.
    pub restart: Restart,
    /// The risk badge.
    pub risk: Risk,
    /// Whether the setting is deprecated.
    pub deprecated: bool,
    /// The deprecation banner text, when deprecated.
    pub deprecated_message: Option<String>,
    /// Whether the setting is currently visible (its `depends_on` is satisfied).
    pub visible: bool,
    /// Whether the asker may edit it at the editing scope.
    pub can_edit: bool,
}

/// A group of rows in a generated screen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// The group title.
    pub title: String,
    /// The rows in the group, in screen order.
    pub settings: Vec<Row>,
}

/// A screen: a title and its groups.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Screen {
    /// The screen title.
    pub title: String,
    /// The groups.
    pub groups: Vec<Group>,
}

/// The generated screen plus the context it was generated for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenModel {
    /// The screen.
    pub screen: Screen,
    /// The area or key path this screen is for.
    pub area_or_path: String,
    /// The scope chain used to resolve values.
    pub chain: Vec<ScopeRef>,
}

/// Generates the screen model for an area (or an exact key).
///
/// The selection is the spec whose key is exactly `area_or_path`, or every spec in that area
/// or under that key path. Grouping comes from the `group` hint when present, otherwise from
/// the key path (`runtime.http.*` groups under `runtime > http`). Rows sort by the `order`
/// hint (unset last) and then by key; groups sort by their smallest order and then by title.
///
/// `editing_scope` is the scope the asker is editing at; a row is `can_edit` when its spec
/// allows that scope and is not read-only.
#[must_use]
pub fn screen_for(
    area_or_path: &str,
    registry: &SettingsRegistry,
    chain: &Chain,
    store: &dyn ValueStore,
    editing_scope: Option<&ScopeRef>,
) -> ScreenModel {
    let selected = select_specs(area_or_path, registry);

    let mut groups: Vec<(String, i64, Vec<Row>)> = Vec::new();
    for spec in selected {
        let group_title = group_title(spec);
        let position = spec.order.unwrap_or(i64::MAX);
        let row = build_row(spec, chain, store, registry, editing_scope);
        match groups
            .iter_mut()
            .find(|(title, _, _)| *title == group_title)
        {
            Some((_, order, rows)) => {
                *order = (*order).min(position);
                rows.push(row);
            }
            None => groups.push((group_title, position, vec![row])),
        }
    }

    let mut groups: Vec<(i64, Group)> = groups
        .into_iter()
        .map(|(title, order, mut rows)| {
            rows.sort_by(|left, right| {
                let left_order = registry
                    .get(&left.key)
                    .and_then(|spec| spec.order)
                    .unwrap_or(i64::MAX);
                let right_order = registry
                    .get(&right.key)
                    .and_then(|spec| spec.order)
                    .unwrap_or(i64::MAX);
                left_order
                    .cmp(&right_order)
                    .then_with(|| left.key.cmp(&right.key))
            });
            (
                order,
                Group {
                    title,
                    settings: rows,
                },
            )
        })
        .collect();
    groups.sort_by(|left, right| {
        left.0
            .cmp(&right.0)
            .then_with(|| left.1.title.cmp(&right.1.title))
    });
    let groups: Vec<Group> = groups.into_iter().map(|(_, group)| group).collect();

    ScreenModel {
        screen: Screen {
            title: format!("Settings: {area_or_path}"),
            groups,
        },
        area_or_path: area_or_path.to_owned(),
        chain: chain.scopes().to_vec(),
    }
}

/// The specs a screen shows: the exact key, or everything in an area or under a path.
fn select_specs<'a>(area_or_path: &str, registry: &'a SettingsRegistry) -> Vec<&'a SettingSpec> {
    if let Some(spec) = registry.get(area_or_path) {
        return vec![spec];
    }
    let prefix = format!("{area_or_path}.");
    registry
        .specs()
        .filter(|spec| spec.key.starts_with(&prefix) || spec.area() == area_or_path)
        .collect()
}

/// The group title: the explicit hint, else the key path's first one or two segments.
fn group_title(spec: &SettingSpec) -> String {
    if let Some(group) = &spec.group {
        return group.clone();
    }
    let segments: Vec<&str> = spec.key.split('.').collect();
    if segments.len() >= 3 {
        format!("{} > {}", segments[0], segments[1])
    } else if segments.len() == 2 {
        segments[0].to_owned()
    } else {
        "other".to_owned()
    }
}

/// Builds one row from a spec and its resolution.
fn build_row(
    spec: &SettingSpec,
    chain: &Chain,
    store: &dyn ValueStore,
    registry: &SettingsRegistry,
    editing_scope: Option<&ScopeRef>,
) -> Row {
    let resolved = resolve(&spec.key, chain, store, registry).unwrap_or_else(|_| Resolved {
        value: crate::resolution::default_effective(spec),
        origin: Origin::Default,
        chain: Vec::new(),
    });

    let visible = match &spec.depends_on {
        Some(dep) => resolve(&dep.key, chain, store, registry)
            .is_ok_and(|other| other.value == EffectiveValue::Value(dep.equals.clone())),
        None => true,
    };

    let can_edit =
        editing_scope.is_some_and(|scope| spec.scopes.contains(&scope.scope()) && !spec.read_only);

    let control = if spec.read_only {
        Control::ReadOnlyValue
    } else if spec.secret {
        Control::SecretInput
    } else {
        Control::from_param_type(&spec.value)
    };

    Row {
        key: spec.key.clone(),
        title: spec.title.clone(),
        summary: spec.summary.clone(),
        control,
        effective: resolved.value.clone(),
        origin_text: origin_text(&resolved),
        default: spec.default.clone(),
        differs: is_different(spec, &resolved.value, &resolved.origin),
        restart: spec.restart.clone(),
        risk: spec.risk,
        deprecated: spec.deprecated.is_some(),
        deprecated_message: spec.deprecated.as_ref().map(|deprecated| {
            format!(
                "Deprecated since {}; use {} instead",
                deprecated.since, deprecated.use_instead
            )
        }),
        visible,
        can_edit,
    }
}

/// Explains an origin from every scope that held a value.
fn origin_text(resolved: &Resolved) -> String {
    let set: Vec<&ScopeRef> = resolved
        .chain
        .iter()
        .filter(|considered| considered.stored.is_some())
        .map(|considered| &considered.scope)
        .collect();
    match set.as_slice() {
        [] => "default".to_owned(),
        [only] => format!("set at {only}"),
        first => {
            let mut text = format!("set at {}", first[0]);
            for scope in &first[1..] {
                text.push_str(&format!(", overridden at {scope}"));
            }
            text
        }
    }
}
