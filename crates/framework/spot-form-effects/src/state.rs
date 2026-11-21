//! The reference form state an effect batch produces, and the applier the conformance corpus
//! checks.
//!
//! This is deliberately small: it is the *server's* model of what a client shows, used to prove
//! that the corpus's expected effects and diff end in exactly the state it records. The client
//! has its own applier over its metadata runtime core (WP-UI-12.T2); the two agree because they
//! run the same corpus.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as Json};
use spot_app_api::Document;

use crate::effect::{Effect, MessageLevel, OptionItem};

/// One action on the form.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionState {
    /// The action's id.
    pub id: String,
    /// Its label.
    pub label: String,
}

/// One message the form shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageState {
    /// Its level.
    pub level: MessageLevel,
    /// Its text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Its catalogue key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_key: Option<String>,
    /// The field it is about.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

/// One dialog the form asks to open.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DialogState {
    /// The registered dialog id.
    pub dialog_id: String,
    /// Its parameters.
    #[serde(default)]
    pub params: Json,
}

/// What a form shows after a batch and a diff are applied.
///
/// `values` holds the document (tables included, as arrays of rows); the rest is presentation
/// state. Empty collections are omitted from the wire and from the corpus snapshot.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormState {
    /// The field values, by wire path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub values: BTreeMap<String, Json>,
    /// The fields made read-only.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub read_only: BTreeSet<String>,
    /// The fields made hidden.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub hidden: BTreeSet<String>,
    /// The fields made required.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub required: BTreeSet<String>,
    /// The candidate values, by path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, Vec<OptionItem>>,
    /// The labels, by path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// The actions, in display order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ActionState>,
    /// The messages, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<MessageState>,
    /// The dialogs to open, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dialogs: Vec<DialogState>,
    /// The focused path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
    /// The link filters, by path.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub filters: BTreeMap<String, String>,
}

impl FormState {
    /// The state of a freshly loaded document: its values, nothing toggled.
    #[must_use]
    pub fn from_document(doc: &Document) -> FormState {
        let mut state = FormState::default();
        for (name, value) in doc.fields() {
            state.values.insert(
                name.as_str().to_owned(),
                spot_server_scripts::value_json(value),
            );
        }
        for (table, rows) in doc.tables() {
            let encoded: Vec<Json> = rows
                .iter()
                .map(|row| {
                    let mut object = Map::new();
                    object.insert("id".to_owned(), Json::String(row.id.to_string()));
                    for (name, value) in &row.fields {
                        object.insert(
                            name.as_str().to_owned(),
                            spot_server_scripts::value_json(value),
                        );
                    }
                    Json::Object(object)
                })
                .collect();
            state
                .values
                .insert(table.as_str().to_owned(), Json::Array(encoded));
        }
        state
    }

    /// Applies a document diff (the shape `resource_update` takes): each top-level member replaces
    /// the value at that path.
    pub fn apply_patch(&mut self, patch: &Json) {
        let Some(object) = patch.as_object() else {
            return;
        };
        for (key, value) in object {
            if matches!(key.as_str(), "id" | "version" | "_state") {
                continue;
            }
            self.values.insert(key.clone(), value.clone());
        }
    }

    /// Applies one validated effect.
    pub fn apply(&mut self, effect: &Effect) {
        match effect {
            Effect::SetValue { path, value } => set_value(&mut self.values, path, value.clone()),
            Effect::SetReadOnly { path, read_only } => {
                set_flag(&mut self.read_only, path, *read_only)
            }
            Effect::SetHidden { path, hidden } => set_flag(&mut self.hidden, path, *hidden),
            Effect::SetRequired { path, required } => {
                set_flag(&mut self.required, path, *required);
            }
            Effect::SetOptions { path, options } => {
                self.options.insert(path.clone(), options.clone());
            }
            Effect::SetLabel { path, label } => {
                self.labels.insert(path.clone(), label.clone());
            }
            Effect::AddAction { id, label, .. } => {
                match self.actions.iter_mut().find(|action| action.id == *id) {
                    Some(action) => action.label.clone_from(label),
                    None => self.actions.push(ActionState {
                        id: id.clone(),
                        label: label.clone(),
                    }),
                }
            }
            Effect::RemoveAction { id } => {
                self.actions.retain(|action| action.id != *id);
            }
            Effect::ShowMessage {
                level,
                text,
                text_key,
                field,
            } => self.messages.push(MessageState {
                level: *level,
                text: text.clone(),
                text_key: text_key.clone(),
                field: field.clone(),
            }),
            Effect::OpenDialog { dialog_id, params } => self.dialogs.push(DialogState {
                dialog_id: dialog_id.clone(),
                params: params.clone(),
            }),
            Effect::Focus { path } => self.focus = Some(path.clone()),
            Effect::SetFilter { link_path, filter } => {
                self.filters.insert(link_path.clone(), filter.clone());
            }
            Effect::Batch { effects } => {
                for nested in effects {
                    self.apply(nested);
                }
            }
        }
    }

    /// Applies a whole batch in order.
    pub fn apply_batch(&mut self, effects: &[Effect]) {
        for effect in effects {
            self.apply(effect);
        }
    }

    /// Applies a server diff and then a batch: the diff is the server's document decision, the
    /// effects are applied on top in order (the server's later decision wins).
    pub fn apply_round(&mut self, patch: &Json, effects: &[Effect]) {
        self.apply_patch(patch);
        self.apply_batch(effects);
    }
}

/// Writes a value, keeping a child table's array coherent when the path names a row.
fn set_value(values: &mut BTreeMap<String, Json>, path: &str, value: Json) {
    if let Some((table, index, field)) = row_path(path)
        && let Some(Json::Array(rows)) = values.get_mut(table)
        && let Some(Json::Object(row)) = rows.get_mut(index)
    {
        row.insert(field.to_owned(), value);
        return;
    }
    values.insert(path.to_owned(), value);
}

/// Splits `table[idx].field` into its table, 0-based row position and field.
fn row_path(path: &str) -> Option<(&str, usize, &str)> {
    let (table, rest) = path.split_once('[')?;
    let (index, field) = rest.split_once("].")?;
    let index = index.parse::<usize>().ok()?.checked_sub(1)?;
    Some((table, index, field))
}

fn set_flag(set: &mut BTreeSet<String>, path: &str, on: bool) {
    if on {
        set.insert(path.to_owned());
    } else {
        set.remove(path);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use serde_json::json;

    #[test]
    fn a_batch_applies_in_order() {
        let mut state = FormState::default();
        state.apply_batch(&[
            Effect::SetValue {
                path: "qty".into(),
                value: json!(2),
            },
            Effect::SetReadOnly {
                path: "qty".into(),
                read_only: true,
            },
            Effect::SetReadOnly {
                path: "qty".into(),
                read_only: false,
            },
            Effect::SetHidden {
                path: "note".into(),
                hidden: true,
            },
            Effect::Focus { path: "qty".into() },
        ]);
        assert_eq!(state.values["qty"], json!(2));
        assert!(state.read_only.is_empty());
        assert!(state.hidden.contains("note"));
        assert_eq!(state.focus.as_deref(), Some("qty"));
    }

    #[test]
    fn actions_replaces_in_place_and_remove_takes_it_away() {
        let mut state = FormState::default();
        state.apply_batch(&[
            Effect::AddAction {
                id: "a".into(),
                label: "First".into(),
                effect: Box::new(Effect::Focus { path: "qty".into() }),
            },
            Effect::AddAction {
                id: "b".into(),
                label: "Second".into(),
                effect: Box::new(Effect::Focus { path: "qty".into() }),
            },
            Effect::AddAction {
                id: "a".into(),
                label: "Again".into(),
                effect: Box::new(Effect::Focus { path: "qty".into() }),
            },
        ]);
        assert_eq!(state.actions[0].label, "Again");
        assert_eq!(state.actions[1].id, "b");
        state.apply(&Effect::RemoveAction { id: "a".into() });
        assert_eq!(state.actions.len(), 1);
        assert_eq!(state.actions[0].id, "b");
    }

    #[test]
    fn a_patch_replaces_values_and_ignores_standard_fields() {
        let mut state = FormState::default();
        state.apply_patch(&json!({"qty": 3, "id": "x", "version": 4, "_state": "draft"}));
        assert_eq!(state.values["qty"], json!(3));
        assert!(!state.values.contains_key("id"));
    }
}
