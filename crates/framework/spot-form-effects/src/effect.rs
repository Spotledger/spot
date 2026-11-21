//! The UI effects protocol, v1: the closed set of declarative effects a server-side form script
//! may ask a client to apply.
//!
//! The client never runs app code (ADR-28). Imperative form logic runs here, on the server, as a
//! TypeScript form script; what comes back is this protocol: plain data the client applies in
//! order. The same bytes are applied by the desktop and the browser client, and the conformance
//! corpus (`conformance/forms/`) is the proof.
//!
//! # Versioning and evolution
//!
//! A batch is versioned ([`PROTOCOL_VERSION`]). Within v1:
//!
//! - **Additive only.** A new effect kind may be added in a new version; an existing kind never
//!   changes the meaning of a field, its type, or the order in which effects apply.
//! - **Unknown kinds are ignored by clients.** The server only ever emits kinds it knows, but a
//!   client that meets a newer kind must skip it and keep applying the rest, so a newer server
//!   never breaks an older client.
//! - **Unknown object members are ignored** (the Rust and C# models do not deny them), for the
//!   same reason.
//! - **A version a client does not know is not applied at all**, and the batch is reported as a
//!   protocol problem; applying part of a protocol one does not understand is worse than not
//!   applying it.
//!
//! # The closed set
//!
//! | Kind | Meaning |
//! | --- | --- |
//! | `set_value` | Put a value in a field, as if the user typed it |
//! | `set_read_only` | Toggle a field's read-only state |
//! | `set_hidden` | Toggle a field's visibility |
//! | `set_required` | Toggle a field's required marker |
//! | `set_options` | Replace a select's or link's candidate values |
//! | `set_label` | Replace a field's label |
//! | `add_action` | Add a form action, with the effect it applies when chosen |
//! | `remove_action` | Remove an action by id |
//! | `show_message` | Show an info, warning or error message |
//! | `open_dialog` | Ask the client to open a registered dialog |
//! | `focus` | Move the keyboard focus to a field |
//! | `set_filter` | Set the filter of a link field (a shared-expression-language expression) |
//! | `batch` | Apply a nested list in order (a transport convenience) |
//!
//! Every path is a field path: `field` or `table[idx].field` with `idx` from 1, the same form the
//! form-event endpoint takes. A value is JSON in the wire encoding the resource API uses: a
//! decimal is its exact string, never a float (ADR-12).

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::codes;
use crate::problem::EffectProblem;

/// The protocol version this crate speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// The largest nesting a `batch` effect may reach, counting the batch itself.
pub const MAX_BATCH_DEPTH: usize = 8;

fn version_one() -> u32 {
    PROTOCOL_VERSION
}

/// One effect a client applies to the form it is showing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    /// Put `value` in the field at `path`. The value is exactly the wire value of the field's
    /// type (a decimal string for money and quantities).
    SetValue {
        /// The target field path.
        path: String,
        /// The new value.
        value: Json,
    },
    /// Make the field read-only, or editable again.
    SetReadOnly {
        /// The target field path.
        path: String,
        /// `true` makes it read-only.
        read_only: bool,
    },
    /// Hide the field, or show it again.
    SetHidden {
        /// The target field path.
        path: String,
        /// `true` hides it.
        hidden: bool,
    },
    /// Mark the field required, or not.
    SetRequired {
        /// The target field path.
        path: String,
        /// `true` marks it required.
        required: bool,
    },
    /// Replace the candidate values of a select or link field.
    SetOptions {
        /// The target field path.
        path: String,
        /// The candidates, in display order.
        options: Vec<OptionItem>,
    },
    /// Replace the field's label.
    SetLabel {
        /// The target field path.
        path: String,
        /// The new label.
        label: String,
    },
    /// Add a form action. The nested effect is what the client applies when the action is chosen;
    /// it is data, never code.
    AddAction {
        /// The action's id, unique on the form.
        id: String,
        /// The label the client shows.
        label: String,
        /// What choosing the action does.
        effect: Box<Effect>,
    },
    /// Remove an action by id.
    RemoveAction {
        /// The action's id.
        id: String,
    },
    /// Show a message. At least one of `text` and `text_key` is present; a client uses `text`
    /// when it has one and otherwise looks `text_key` up in the message catalogue.
    ShowMessage {
        /// How important the message is.
        level: MessageLevel,
        /// The text, when the script supplies one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        /// The catalogue key, when the text is translated.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_key: Option<String>,
        /// The field the message is about, when it is about one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        field: Option<String>,
    },
    /// Ask the client to open a registered dialog.
    OpenDialog {
        /// The dialog's registered id.
        dialog_id: String,
        /// The dialog's parameters.
        #[serde(default)]
        params: Json,
    },
    /// Move the keyboard focus to a field.
    Focus {
        /// The target field path.
        path: String,
    },
    /// Set a link field's filter: a shared-expression-language expression evaluated against the
    /// form's document (`doc.customer`, …), exactly as the client evaluates field rules.
    SetFilter {
        /// The link field whose candidate list is filtered.
        link_path: String,
        /// The filter expression.
        filter: String,
    },
    /// Apply the nested effects in order. `batch` exists so one script can express a group; the
    /// server flattens it, and a client never needs to know it was there.
    Batch {
        /// The nested effects.
        effects: Vec<Effect>,
    },
}

impl Effect {
    /// Every kind name of v1, in the order the specification lists them.
    pub const ALL_KINDS: [&'static str; 13] = [
        "set_value",
        "set_read_only",
        "set_hidden",
        "set_required",
        "set_options",
        "set_label",
        "add_action",
        "remove_action",
        "show_message",
        "open_dialog",
        "focus",
        "set_filter",
        "batch",
    ];

    /// The effect's kind, as the wire names it.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Effect::SetValue { .. } => "set_value",
            Effect::SetReadOnly { .. } => "set_read_only",
            Effect::SetHidden { .. } => "set_hidden",
            Effect::SetRequired { .. } => "set_required",
            Effect::SetOptions { .. } => "set_options",
            Effect::SetLabel { .. } => "set_label",
            Effect::AddAction { .. } => "add_action",
            Effect::RemoveAction { .. } => "remove_action",
            Effect::ShowMessage { .. } => "show_message",
            Effect::OpenDialog { .. } => "open_dialog",
            Effect::Focus { .. } => "focus",
            Effect::SetFilter { .. } => "set_filter",
            Effect::Batch { .. } => "batch",
        }
    }

    /// The field path the effect targets, when it targets one directly.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Effect::SetValue { path, .. }
            | Effect::SetReadOnly { path, .. }
            | Effect::SetHidden { path, .. }
            | Effect::SetRequired { path, .. }
            | Effect::SetOptions { path, .. }
            | Effect::SetLabel { path, .. }
            | Effect::Focus { path } => Some(path),
            Effect::SetFilter { link_path, .. } => Some(link_path),
            Effect::ShowMessage { field, .. } => field.as_deref(),
            Effect::AddAction { .. }
            | Effect::RemoveAction { .. }
            | Effect::OpenDialog { .. }
            | Effect::Batch { .. } => None,
        }
    }

    /// The (group, key) that decides last-wins de-duplication, when the effect sets state rather
    /// than adding an event.
    ///
    /// Two effects with the same group and key target the same piece of form state, so only the
    /// last one can be observed; the earlier ones are dropped. Ordering effects (messages,
    /// dialogs, actions being added, nested batches) are never dropped this way.
    #[must_use]
    pub fn state_key(&self) -> Option<(&'static str, String)> {
        let keyed = |path: &str| Some(path.to_owned());
        match self {
            Effect::SetValue { path, .. } => keyed(path).map(|k| ("set_value", k)),
            Effect::SetReadOnly { path, .. } => keyed(path).map(|k| ("set_read_only", k)),
            Effect::SetHidden { path, .. } => keyed(path).map(|k| ("set_hidden", k)),
            Effect::SetRequired { path, .. } => keyed(path).map(|k| ("set_required", k)),
            Effect::SetOptions { path, .. } => keyed(path).map(|k| ("set_options", k)),
            Effect::SetLabel { path, .. } => keyed(path).map(|k| ("set_label", k)),
            // There is one keyboard focus, so every focus targets the same state.
            Effect::Focus { .. } => Some(("focus", String::new())),
            Effect::SetFilter { link_path, .. } => keyed(link_path).map(|k| ("set_filter", k)),
            Effect::RemoveAction { id } => Some(("remove_action", id.clone())),
            // An added action is keyed by its id: re-adding replaces the earlier definition.
            Effect::AddAction { id, .. } => Some(("add_action", id.clone())),
            Effect::ShowMessage { .. } | Effect::OpenDialog { .. } | Effect::Batch { .. } => None,
        }
    }

    /// The effect with nested batches flattened, in application order.
    ///
    /// The nesting depth is bounded by [`MAX_BATCH_DEPTH`]; `None` means the bound was
    /// exceeded. The result is the sequence a client applies.
    pub fn flatten(&self) -> Option<Vec<&Effect>> {
        fn walk<'a>(effect: &'a Effect, depth: usize, out: &mut Vec<&'a Effect>) -> bool {
            if depth > MAX_BATCH_DEPTH {
                return false;
            }
            match effect {
                Effect::Batch { effects } => {
                    effects.iter().all(|nested| walk(nested, depth + 1, out))
                }
                other => {
                    out.push(other);
                    true
                }
            }
        }
        let mut out = Vec::new();
        walk(self, 0, &mut out).then_some(out)
    }
}

/// One candidate value of a select or link field.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionItem {
    /// The stored value.
    pub value: String,
    /// The text the client shows; the value when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// How important a message is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageLevel {
    /// For information.
    Info,
    /// Something the user should check.
    Warning,
    /// A validation problem; the form should not be saved as it is.
    Error,
}

impl MessageLevel {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MessageLevel::Info => "info",
            MessageLevel::Warning => "warning",
            MessageLevel::Error => "error",
        }
    }

    /// Reads a level from its wire name.
    #[must_use]
    pub fn parse(text: &str) -> Option<MessageLevel> {
        match text {
            "info" => Some(MessageLevel::Info),
            "warning" => Some(MessageLevel::Warning),
            "error" => Some(MessageLevel::Error),
            _ => None,
        }
    }
}

/// The batch one form event answers: the effects a client applies, in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectBatch {
    /// The protocol version the batch is written in ([`PROTOCOL_VERSION`]).
    #[serde(default = "version_one")]
    pub version: u32,
    /// The effects, in application order.
    #[serde(default)]
    pub effects: Vec<Effect>,
}

impl EffectBatch {
    /// An empty v1 batch.
    #[must_use]
    pub fn empty() -> EffectBatch {
        EffectBatch {
            version: PROTOCOL_VERSION,
            effects: Vec::new(),
        }
    }

    /// A v1 batch of `effects`.
    #[must_use]
    pub fn new(effects: Vec<Effect>) -> EffectBatch {
        EffectBatch {
            version: PROTOCOL_VERSION,
            effects,
        }
    }

    /// Whether the batch has no effects.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty()
    }

    /// Reads a batch from the wire.
    ///
    /// A version this implementation does not speak is refused **whole** — applying part of a
    /// protocol one does not understand is worse than applying none of it. Within v1, every
    /// effect must have a shape its kind defines; an unknown kind is refused for a server-side
    /// batch (a client skips it instead, per the evolution rules).
    ///
    /// # Errors
    ///
    /// `form_effects.protocol_version` for an unknown or missing version, and
    /// `form_effects.effect_invalid` for a member that does not have its kind's shape.
    pub fn from_wire(value: &Json) -> Result<EffectBatch, EffectProblem> {
        let object = value.as_object().ok_or_else(|| EffectProblem {
            code: codes::EFFECT_INVALID.to_owned(),
            index: None,
            path: None,
            message: "An effects batch is an object.".to_owned(),
        })?;
        let version = object
            .get("version")
            .and_then(Json::as_u64)
            .ok_or_else(|| EffectProblem {
                code: codes::PROTOCOL_VERSION.to_owned(),
                index: None,
                path: None,
                message: "An effects batch names its protocol version.".to_owned(),
            })?;
        if version != u64::from(PROTOCOL_VERSION) {
            return Err(EffectProblem {
                code: codes::PROTOCOL_VERSION.to_owned(),
                index: None,
                path: None,
                message: format!(
                    "Protocol version {version} is not this server's {PROTOCOL_VERSION}."
                ),
            });
        }
        let items = object
            .get("effects")
            .and_then(Json::as_array)
            .ok_or_else(|| EffectProblem {
                code: codes::EFFECT_INVALID.to_owned(),
                index: None,
                path: None,
                message: "An effects batch carries an `effects` array.".to_owned(),
            })?;
        let mut effects = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let effect: Effect = serde_json::from_value(item.clone()).map_err(|_| {
                EffectProblem::at(
                    index,
                    codes::EFFECT_INVALID,
                    "The effect does not have the shape its kind requires.",
                )
            })?;
            effects.push(effect);
        }
        Ok(EffectBatch::new(effects))
    }
}

impl Default for EffectBatch {
    fn default() -> Self {
        EffectBatch::empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn every_kind_round_trips_through_its_wire_name() {
        let samples = vec![
            Effect::SetValue {
                path: "total".into(),
                value: json!("12.50"),
            },
            Effect::SetReadOnly {
                path: "total".into(),
                read_only: true,
            },
            Effect::SetHidden {
                path: "note".into(),
                hidden: false,
            },
            Effect::SetRequired {
                path: "qty".into(),
                required: true,
            },
            Effect::SetOptions {
                path: "status".into(),
                options: vec![OptionItem {
                    value: "open".into(),
                    label: None,
                }],
            },
            Effect::SetLabel {
                path: "qty".into(),
                label: "Quantity".into(),
            },
            Effect::AddAction {
                id: "pick".into(),
                label: "Pick".into(),
                effect: Box::new(Effect::Focus { path: "qty".into() }),
            },
            Effect::RemoveAction { id: "pick".into() },
            Effect::ShowMessage {
                level: MessageLevel::Warning,
                text: Some("Check it".into()),
                text_key: None,
                field: Some("qty".into()),
            },
            Effect::OpenDialog {
                dialog_id: "customer".into(),
                params: json!({ "search": "ac" }),
            },
            Effect::Focus { path: "qty".into() },
            Effect::SetFilter {
                link_path: "customer".into(),
                filter: "'open' == doc.status".into(),
            },
            Effect::Batch {
                effects: vec![Effect::Focus { path: "qty".into() }],
            },
        ];
        assert_eq!(samples.len(), Effect::ALL_KINDS.len());
        for (sample, kind) in samples.iter().zip(Effect::ALL_KINDS) {
            assert_eq!(sample.kind(), kind);
            let wire = serde_json::to_value(sample).unwrap();
            assert_eq!(wire["kind"], json!(kind));
            let back: Effect = serde_json::from_value(wire).unwrap();
            assert_eq!(&back, sample);
        }
    }

    #[test]
    fn a_batch_flattens_in_order_and_bounds_its_depth() {
        let nested = Effect::Batch {
            effects: vec![
                Effect::Focus { path: "a".into() },
                Effect::Batch {
                    effects: vec![Effect::Focus { path: "b".into() }],
                },
            ],
        };
        let flat: Vec<&str> = nested
            .flatten()
            .unwrap()
            .iter()
            .filter_map(|e| e.path())
            .collect();
        assert_eq!(flat, vec!["a", "b"]);

        let mut deep = Effect::Focus { path: "x".into() };
        for _ in 0..=MAX_BATCH_DEPTH {
            deep = Effect::Batch {
                effects: vec![deep],
            };
        }
        assert!(deep.flatten().is_none());
    }

    #[test]
    fn a_wire_batch_is_version_checked_and_parsed() {
        let wire = json!({
            "version": 1,
            "effects": [{ "kind": "focus", "path": "qty" }],
        });
        assert_eq!(
            EffectBatch::from_wire(&wire),
            Ok(EffectBatch::new(vec![Effect::Focus { path: "qty".into() }]))
        );
        let newer = json!({ "version": 2, "effects": [] });
        assert_eq!(
            EffectBatch::from_wire(&newer).unwrap_err().code,
            codes::PROTOCOL_VERSION
        );
        let missing = json!({ "effects": [] });
        assert_eq!(
            EffectBatch::from_wire(&missing).unwrap_err().code,
            codes::PROTOCOL_VERSION
        );
        let bad = json!({ "version": 1, "effects": [{ "kind": "focus" }] });
        assert_eq!(
            EffectBatch::from_wire(&bad).unwrap_err().code,
            codes::EFFECT_INVALID
        );
    }

    #[test]
    fn state_keys_name_the_state_an_effect_sets() {
        let first = Effect::SetValue {
            path: "total".into(),
            value: json!(1),
        };
        let second = Effect::SetValue {
            path: "total".into(),
            value: json!(2),
        };
        assert_eq!(first.state_key(), second.state_key());
        assert_eq!(
            Effect::ShowMessage {
                level: MessageLevel::Info,
                text: Some("a".into()),
                text_key: None,
                field: None,
            }
            .state_key(),
            None
        );
    }
}
