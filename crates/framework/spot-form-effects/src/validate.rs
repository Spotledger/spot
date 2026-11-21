//! Judging raw effects against the form's metadata, the triggering user's rights and the form's
//! registered dialogs: an effect that fails any check is **dropped**, never applied, and leaves
//! one [`EffectProblem`] in the response.
//!
//! The validator is the server's half of the protocol's guarantee: a client may apply whatever
//! the server sent without asking anyone. Order is preserved, state-setting duplicates collapse
//! to their last occurrence, and the result is bounded.

use std::collections::BTreeSet;

use serde_json::Value as Json;
use spot_app_api::Document;
use spot_app_api::document::ChildRow;
use spot_app_api::meta::{DocTypeDef, FieldDef, FieldKind};
use spot_app_api::prelude::Value;
use spot_permissions::{DocFacts, Principal};
use spot_server_scripts::{json_to_value, value_json};

use crate::codes;
use crate::effect::{Effect, EffectBatch};
use crate::path::TargetPath;
use crate::problem::EffectProblem;
use crate::security::{DialogRegistry, FormMeta, FormSecurity};

/// The caps every response is bounded by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectLimits {
    /// The most effects one form event may return.
    pub max_effects: usize,
    /// The largest serialised size of one effect.
    pub max_effect_bytes: usize,
    /// The largest serialised size of the whole batch.
    pub max_batch_bytes: usize,
    /// The most candidate values one `set_options` may carry.
    pub max_options: usize,
    /// The longest message text.
    pub max_text_chars: usize,
    /// The longest label or option label.
    pub max_label_chars: usize,
    /// The largest serialised dialog parameter object.
    pub max_params_bytes: usize,
    /// The most log lines the scripts of one form event may write; the surplus is dropped and
    /// reported once.
    pub max_log_lines: usize,
    /// The longest log line in bytes; a longer line is cut and marked.
    pub max_log_line_bytes: usize,
}

impl Default for EffectLimits {
    fn default() -> Self {
        EffectLimits {
            max_effects: 200,
            max_effect_bytes: 16 * 1024,
            max_batch_bytes: 64 * 1024,
            max_options: 500,
            max_text_chars: 4_000,
            max_label_chars: 200,
            max_params_bytes: 8 * 1024,
            max_log_lines: 100,
            max_log_line_bytes: 1024,
        }
    }
}

/// The form an effect is judged against.
pub struct EffectContext<'a> {
    /// The DocType's effective definition.
    pub def: &'a DocTypeDef,
    /// The document as it will be after the server patch, so row paths resolve.
    pub document: &'a Document,
    /// The triggering user.
    pub principal: &'a Principal,
    /// The document facts the permission decision uses.
    pub facts: &'a DocFacts,
}

/// The validated effects and every problem met on the way.
#[derive(Clone, Debug, PartialEq)]
pub struct Validated {
    /// The effects a client may apply, in order.
    pub batch: EffectBatch,
    /// The effects that were dropped, with why.
    pub problems: Vec<EffectProblem>,
}

/// The validator.
pub struct Validator<'a> {
    meta: &'a dyn FormMeta,
    security: &'a dyn FormSecurity,
    dialogs: &'a dyn DialogRegistry,
    limits: EffectLimits,
}

impl<'a> Validator<'a> {
    /// A validator over these authorities and caps.
    #[must_use]
    pub fn new(
        meta: &'a dyn FormMeta,
        security: &'a dyn FormSecurity,
        dialogs: &'a dyn DialogRegistry,
        limits: EffectLimits,
    ) -> Validator<'a> {
        Validator {
            meta,
            security,
            dialogs,
            limits,
        }
    }

    /// Parses and validates the raw effects a script emitted (JSON, in emission order).
    pub fn validate_raw(&self, raw: &[Json], ctx: &EffectContext<'_>) -> Validated {
        let mut parsed: Vec<(usize, Effect)> = Vec::new();
        let mut problems = Vec::new();
        if raw.len() > self.limits.max_effects {
            problems.push(EffectProblem::at(
                0,
                codes::LIMIT_EXCEEDED,
                format!(
                    "A form event returns at most {} effects.",
                    self.limits.max_effects
                ),
            ));
        }
        for (index, value) in raw.iter().take(self.limits.max_effects).enumerate() {
            match parse_raw(value) {
                Ok(effect) => parsed.push((index, effect)),
                Err(problem) => problems.push(problem.with_index(index)),
            }
        }
        self.validate_parsed(parsed, problems, ctx)
    }

    /// Validates effects that are already typed (the conformance corpus's and a host's path).
    pub fn validate_effects(&self, effects: &[Effect], ctx: &EffectContext<'_>) -> Validated {
        let parsed: Vec<(usize, Effect)> = effects
            .iter()
            .take(self.limits.max_effects)
            .cloned()
            .enumerate()
            .collect();
        self.validate_parsed(parsed, Vec::new(), ctx)
    }

    fn validate_parsed(
        &self,
        parsed: Vec<(usize, Effect)>,
        mut problems: Vec<EffectProblem>,
        ctx: &EffectContext<'_>,
    ) -> Validated {
        // Flatten nested batches first: the result is what a client applies.
        let mut flat: Vec<(usize, Effect)> = Vec::new();
        for (index, effect) in parsed {
            match flatten_one(&effect) {
                Some(effects) => {
                    for nested in effects {
                        flat.push((index, nested.clone()));
                    }
                }
                None => problems.push(EffectProblem::at(
                    index,
                    codes::LIMIT_EXCEEDED,
                    format!(
                        "A nested batch may be at most {} levels deep.",
                        crate::effect::MAX_BATCH_DEPTH
                    ),
                )),
            }
        }

        let mut kept: Vec<Effect> = Vec::new();
        let mut bytes: usize = 0;
        for (index, effect) in flat {
            let size = serde_json::to_vec(&effect).map_or(usize::MAX, |bytes| bytes.len());
            if size > self.limits.max_effect_bytes {
                problems.push(EffectProblem::at(
                    index,
                    codes::LIMIT_EXCEEDED,
                    format!(
                        "One effect may be at most {} bytes.",
                        self.limits.max_effect_bytes
                    ),
                ));
                continue;
            }
            if bytes.saturating_add(size) > self.limits.max_batch_bytes {
                problems.push(EffectProblem::at(
                    index,
                    codes::LIMIT_EXCEEDED,
                    format!(
                        "A form event returns at most {} bytes of effects.",
                        self.limits.max_batch_bytes
                    ),
                ));
                continue;
            }
            match self.validate_one(&effect, index, ctx) {
                Ok(effect) => {
                    bytes = bytes.saturating_add(size);
                    kept.push(effect);
                }
                Err(problem) => problems.push(problem),
            }
        }

        Validated {
            batch: EffectBatch::new(dedupe(kept)),
            problems,
        }
    }

    /// Validates one (already flattened) effect, returning the canonical form to send.
    fn validate_one(
        &self,
        effect: &Effect,
        index: usize,
        ctx: &EffectContext<'_>,
    ) -> Result<Effect, EffectProblem> {
        let bad = |code: &str, message: String| EffectProblem::at(index, code, message);
        match effect {
            Effect::SetValue { path, value } => {
                let target = self.resolve(path, ctx).map_err(|p| p.with_index(index))?;
                if !target.writable || target.field.read_only {
                    return Err(bad(
                        codes::FIELD_NOT_WRITABLE,
                        "The user may not write this field.".to_owned(),
                    )
                    .with_path(target.path));
                }
                let value = canonical_value(&target.field, value)
                    .map_err(|why| bad(codes::VALUE_INVALID, why).with_path(target.path.clone()))?;
                Ok(Effect::SetValue {
                    path: target.path,
                    value,
                })
            }
            Effect::SetReadOnly { path, read_only } => {
                let target = self.resolve(path, ctx).map_err(|p| p.with_index(index))?;
                // Narrowing (read-only, hidden) is always allowed; widening or changing a
                // field's state needs the right to write it.
                if !*read_only && (!target.writable || target.field.read_only) {
                    return Err(bad(
                        codes::FIELD_NOT_WRITABLE,
                        "The user may not change this field's state.".to_owned(),
                    )
                    .with_path(target.path));
                }
                Ok(Effect::SetReadOnly {
                    path: target.path,
                    read_only: *read_only,
                })
            }
            Effect::SetHidden { path, hidden } => {
                let target = self.resolve(path, ctx).map_err(|p| p.with_index(index))?;
                // Narrowing (read-only, hidden) is always allowed; widening or changing a
                // field's state needs the right to write it.
                if !*hidden && !target.writable {
                    return Err(bad(
                        codes::FIELD_NOT_WRITABLE,
                        "The user may not change this field's state.".to_owned(),
                    )
                    .with_path(target.path));
                }
                Ok(Effect::SetHidden {
                    path: target.path,
                    hidden: *hidden,
                })
            }
            Effect::SetRequired { path, required } => {
                let target = self.resolve(path, ctx).map_err(|p| p.with_index(index))?;
                // Narrowing (read-only, hidden) is always allowed; widening or changing a
                // field's state needs the right to write it.
                if !target.writable {
                    return Err(bad(
                        codes::FIELD_NOT_WRITABLE,
                        "The user may not change this field's state.".to_owned(),
                    )
                    .with_path(target.path));
                }
                Ok(Effect::SetRequired {
                    path: target.path,
                    required: *required,
                })
            }
            Effect::SetOptions { path, options } => {
                let target = self.resolve(path, ctx).map_err(|p| p.with_index(index))?;
                if !matches!(
                    target.field.kind,
                    FieldKind::Select { .. }
                        | FieldKind::Link { .. }
                        | FieldKind::DynamicLink { .. }
                ) {
                    return Err(bad(
                        codes::OPTION_INVALID,
                        "Candidate values apply to a select or link field only.".to_owned(),
                    )
                    .with_path(target.path));
                }
                if options.len() > self.limits.max_options {
                    return Err(bad(
                        codes::OPTION_INVALID,
                        format!(
                            "At most {} candidate values are allowed.",
                            self.limits.max_options
                        ),
                    )
                    .with_path(target.path));
                }
                let mut seen = BTreeSet::new();
                let mut clean = Vec::with_capacity(options.len());
                for option in options {
                    if option.value.is_empty() {
                        return Err(bad(
                            codes::OPTION_INVALID,
                            "A candidate value is empty.".to_owned(),
                        )
                        .with_path(target.path));
                    }
                    if !seen.insert(option.value.clone()) {
                        return Err(bad(
                            codes::OPTION_INVALID,
                            "A candidate value appears twice.".to_owned(),
                        )
                        .with_path(target.path));
                    }
                    if let Some(label) = &option.label
                        && (label.trim().is_empty()
                            || label.chars().count() > self.limits.max_label_chars)
                    {
                        return Err(bad(
                            codes::OPTION_INVALID,
                            format!(
                                "A candidate label is empty or longer than {} characters.",
                                self.limits.max_label_chars
                            ),
                        )
                        .with_path(target.path));
                    }
                    if let FieldKind::Select { values } = &target.field.kind
                        && !values.contains(&option.value)
                    {
                        return Err(bad(
                            codes::OPTION_INVALID,
                            "A candidate value is not one of the select's values.".to_owned(),
                        )
                        .with_path(target.path));
                    }
                    clean.push(option.clone());
                }
                Ok(Effect::SetOptions {
                    path: target.path,
                    options: clean,
                })
            }
            Effect::SetLabel { path, label } => {
                let target = self.resolve(path, ctx).map_err(|p| p.with_index(index))?;
                if label.trim().is_empty() || label.chars().count() > self.limits.max_label_chars {
                    return Err(bad(
                        codes::LABEL_INVALID,
                        format!(
                            "A label is non-empty and at most {} characters.",
                            self.limits.max_label_chars
                        ),
                    )
                    .with_path(target.path));
                }
                Ok(Effect::SetLabel {
                    path: target.path,
                    label: label.clone(),
                })
            }
            Effect::AddAction { id, label, effect } => {
                if !crate::record::is_id(id) {
                    return Err(bad(
                        codes::ACTION_INVALID,
                        format!("`{id}` is not an action id."),
                    ));
                }
                if label.trim().is_empty() || label.chars().count() > self.limits.max_label_chars {
                    return Err(bad(
                        codes::ACTION_INVALID,
                        "An action label is non-empty and short.".to_owned(),
                    ));
                }
                // The nested effect is not applied now, but it must be applicable when it is.
                let nested = self
                    .validate_one(effect, index, ctx)
                    .map_err(|p| EffectProblem {
                        code: p.code,
                        index: p.index,
                        path: p.path,
                        message: format!("the action's effect: {}", p.message),
                    })?;
                Ok(Effect::AddAction {
                    id: id.clone(),
                    label: label.clone(),
                    effect: Box::new(nested),
                })
            }
            Effect::RemoveAction { id } => {
                if !crate::record::is_id(id) {
                    return Err(bad(
                        codes::ACTION_INVALID,
                        format!("`{id}` is not an action id."),
                    ));
                }
                Ok(Effect::RemoveAction { id: id.clone() })
            }
            Effect::ShowMessage {
                level,
                text,
                text_key,
                field,
            } => {
                let text = text.clone().filter(|t| !t.is_empty());
                let text_key = text_key.clone().filter(|k| !k.is_empty());
                if text.is_none() && text_key.is_none() {
                    return Err(bad(
                        codes::MESSAGE_INVALID,
                        "A message needs text or a catalogue key.".to_owned(),
                    ));
                }
                if let Some(text) = &text
                    && text.chars().count() > self.limits.max_text_chars
                {
                    return Err(bad(
                        codes::MESSAGE_INVALID,
                        format!(
                            "A message is at most {} characters.",
                            self.limits.max_text_chars
                        ),
                    ));
                }
                if let Some(key) = &text_key
                    && !is_text_key(key)
                {
                    return Err(bad(
                        codes::MESSAGE_INVALID,
                        "A catalogue key is dotted lower-case.".to_owned(),
                    ));
                }
                let field = match field {
                    None => None,
                    Some(path) => {
                        let target = self.resolve(path, ctx).map_err(|p| p.with_index(index))?;
                        Some(target.path)
                    }
                };
                Ok(Effect::ShowMessage {
                    level: *level,
                    text,
                    text_key,
                    field,
                })
            }
            Effect::OpenDialog { dialog_id, params } => {
                if !self.dialogs.is_registered(dialog_id) {
                    return Err(bad(
                        codes::DIALOG_UNKNOWN,
                        format!("`{dialog_id}` is not a registered dialog."),
                    ));
                }
                let params = params.clone();
                if !params.is_null() && !params.is_object() {
                    return Err(bad(
                        codes::EFFECT_INVALID,
                        "Dialog parameters are an object.".to_owned(),
                    ));
                }
                let size = serde_json::to_vec(&params).map_or(usize::MAX, |bytes| bytes.len());
                if size > self.limits.max_params_bytes {
                    return Err(bad(
                        codes::LIMIT_EXCEEDED,
                        format!(
                            "Dialog parameters may be at most {} bytes.",
                            self.limits.max_params_bytes
                        ),
                    ));
                }
                Ok(Effect::OpenDialog {
                    dialog_id: dialog_id.clone(),
                    params,
                })
            }
            Effect::Focus { path } => {
                let target = self.resolve(path, ctx).map_err(|p| p.with_index(index))?;
                Ok(Effect::Focus { path: target.path })
            }
            Effect::SetFilter { link_path, filter } => {
                let target = self
                    .resolve(link_path, ctx)
                    .map_err(|p| p.with_index(index))?;
                if !matches!(
                    target.field.kind,
                    FieldKind::Link { .. } | FieldKind::DynamicLink { .. }
                ) {
                    return Err(bad(
                        codes::FILTER_INVALID,
                        "A filter applies to a link field only.".to_owned(),
                    )
                    .with_path(target.path));
                }
                if filter.chars().count() > 2_000 {
                    return Err(bad(
                        codes::FILTER_INVALID,
                        "A filter is at most 2000 characters.".to_owned(),
                    )
                    .with_path(target.path));
                }
                if let Err(error) = spot_expr::parse(filter) {
                    return Err(bad(
                        codes::FILTER_INVALID,
                        format!("The filter does not parse: {}", error.code),
                    )
                    .with_path(target.path));
                }
                Ok(Effect::SetFilter {
                    link_path: target.path,
                    filter: filter.clone(),
                })
            }
            // A batch was flattened before validation.
            Effect::Batch { .. } => Err(bad(
                codes::EFFECT_INVALID,
                "A batch is flattened before validation.".to_owned(),
            )),
        }
    }

    /// Resolves a path against the metadata, the document and the user's rights.
    ///
    /// A field the user may not see answers [`codes::PATH_UNAVAILABLE`] **without** the path, so
    /// a problem never tells the client that a hidden field exists.
    fn resolve(&self, text: &str, ctx: &EffectContext<'_>) -> Result<Resolved, EffectProblem> {
        let path = TargetPath::parse(text).map_err(|_| EffectProblem {
            code: codes::PATH_INVALID.to_owned(),
            index: None,
            path: None,
            message: "An effect path must be `field` or `table[idx].field` with idx from 1."
                .to_owned(),
        })?;
        match (&path.table, path.index) {
            (None, _) => {
                let field = ctx.def.field(&path.field).ok_or_else(unavailable)?;
                let access = self.security.access(
                    ctx.def.name.as_str(),
                    &path.field,
                    ctx.principal,
                    ctx.facts,
                );
                if !access.visible {
                    return Err(unavailable());
                }
                Ok(Resolved {
                    path: path.wire(),
                    field: field.clone(),
                    writable: access.writable,
                })
            }
            (Some(table), Some(index)) => {
                let table_field = ctx.def.field(table).ok_or_else(unavailable)?;
                let table_access =
                    self.security
                        .access(ctx.def.name.as_str(), table, ctx.principal, ctx.facts);
                if !table_access.visible {
                    return Err(unavailable());
                }
                let child = match &table_field.kind {
                    FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. } => {
                        doctype
                    }
                    _ => return Err(unknown(text)),
                };
                let child_def = self.meta.doctype(child.as_str()).ok_or_else(unavailable)?;
                let field = child_def.field(&path.field).ok_or_else(unavailable)?;
                let Some(row) = ctx.document.rows(table).get(index - 1) else {
                    return Err(unknown(text));
                };
                let facts = row_facts(child.as_str(), row);
                let access =
                    self.security
                        .access(child.as_str(), &path.field, ctx.principal, &facts);
                if !access.visible {
                    return Err(unavailable());
                }
                Ok(Resolved {
                    path: path.wire(),
                    field: field.clone(),
                    writable: access.writable,
                })
            }
            (Some(_), None) => Err(unknown(text)),
        }
    }
}

/// One resolved target field.
struct Resolved {
    /// The wire path, canonical.
    path: String,
    /// The field's definition (the parent's or the child's).
    field: FieldDef,
    /// The user may write it.
    writable: bool,
}

/// The facts of one child row: its own values, so a user permission on a link in the row judges
/// that row.
fn row_facts(child: &str, row: &ChildRow) -> DocFacts {
    DocFacts {
        doctype: child.to_owned(),
        values: row
            .fields
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
            .collect(),
        ..DocFacts::default()
    }
}

/// A field the DocType, table, row or column does not have.
fn unknown(text: &str) -> EffectProblem {
    EffectProblem {
        code: codes::PATH_UNKNOWN.to_owned(),
        index: None,
        path: Some(text.to_owned()),
        message: format!("The form has no field at `{text}`."),
    }
}

/// A field the user may not see, or one the form does not have: the two answer identically
/// (same code, same text, no path), so a script cannot probe for hidden fields.
fn unavailable() -> EffectProblem {
    EffectProblem {
        code: codes::PATH_UNAVAILABLE.to_owned(),
        index: None,
        path: None,
        message: "An effect targets a field the user may not see.".to_owned(),
    }
}

/// The canonical wire value for `field`, or why it does not fit.
fn canonical_value(field: &FieldDef, value: &Json) -> Result<Json, String> {
    if field.multiple
        && let FieldKind::Select { values } = &field.kind
    {
        let Json::Array(items) = value else {
            return Err("a multiple select takes an array of values".to_owned());
        };
        for item in items {
            let Some(text) = item.as_str() else {
                return Err("a multiple select takes an array of strings".to_owned());
            };
            if !values.iter().any(|v| v == text) {
                return Err("a value is not one of the select's values".to_owned());
            }
        }
        return Ok(value.clone());
    }
    if matches!(
        field.kind,
        FieldKind::Table { .. } | FieldKind::TableMultiselect { .. }
    ) {
        return Err("a child table is not set by one effect".to_owned());
    }
    let converted = json_to_value(&field.kind, value)?;
    if !field.kind.accepts(&converted) {
        return Err(format!(
            "`{value}` does not fit a `{}` field",
            kind_name(&field.kind)
        ));
    }
    if let Value::Decimal(decimal) = &converted
        && let FieldKind::Decimal { scale, .. } = &field.kind
        && decimal.scale() > u32::from(*scale)
    {
        return Err(format!("the value has more than {scale} decimal places"));
    }
    if let Value::Text(text) = &converted {
        if let FieldKind::Text {
            max_chars: Some(max),
            ..
        } = &field.kind
            && text.chars().count() > *max as usize
        {
            return Err(format!("the text is longer than {max} characters"));
        }
        if let FieldKind::Select { values } = &field.kind
            && !values.iter().any(|v| v.as_str() == text.as_str())
        {
            return Err("the value is not one of the select's values".to_owned());
        }
    }
    Ok(value_json(&converted))
}

/// A short name for a kind, for a problem message.
fn kind_name(kind: &FieldKind) -> &'static str {
    match kind {
        FieldKind::Bool => "bool",
        FieldKind::Int { .. } => "int",
        FieldKind::Decimal { .. } => "decimal",
        FieldKind::Text { .. } => "text",
        FieldKind::LongText { .. } => "long text",
        FieldKind::Select { .. } => "select",
        FieldKind::Date => "date",
        FieldKind::Time => "time",
        FieldKind::DateTime => "date-time",
        FieldKind::Link { .. } => "link",
        FieldKind::DynamicLink { .. } => "dynamic link",
        FieldKind::Json { .. } => "json",
        FieldKind::Bytes => "bytes",
        FieldKind::Attach { .. } => "attach",
        FieldKind::Secret => "secret",
        FieldKind::Table { .. } => "table",
        FieldKind::TableMultiselect { .. } => "table",
        _ => "field",
    }
}

/// Parses one raw effect, keeping unknown kinds apart from malformed ones.
fn parse_raw(value: &Json) -> Result<Effect, EffectProblem> {
    let kind = value
        .as_object()
        .and_then(|object| object.get("kind"))
        .and_then(Json::as_str);
    let Some(kind) = kind else {
        return Err(EffectProblem {
            code: codes::EFFECT_INVALID.to_owned(),
            index: None,
            path: None,
            message: "An effect is an object with a `kind`.".to_owned(),
        });
    };
    if !Effect::ALL_KINDS.contains(&kind) {
        return Err(EffectProblem {
            code: codes::EFFECT_UNKNOWN.to_owned(),
            index: None,
            path: None,
            message: format!("`{kind}` is not an effect this protocol version knows."),
        });
    }
    serde_json::from_value(value.clone()).map_err(|_| EffectProblem {
        code: codes::EFFECT_INVALID.to_owned(),
        index: None,
        path: None,
        message: format!("The effect does not have the shape `{kind}` requires."),
    })
}

/// Flattens one effect, or says the nesting is too deep.
fn flatten_one(effect: &Effect) -> Option<Vec<&Effect>> {
    effect.flatten()
}

/// Collapses state-setting duplicates to their last occurrence and drops exact repeats of
/// ordering effects, preserving order.
fn dedupe(effects: Vec<Effect>) -> Vec<Effect> {
    let mut keep = vec![true; effects.len()];
    let mut keyed: std::collections::BTreeMap<(&'static str, String), usize> =
        std::collections::BTreeMap::new();
    let mut exact: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (index, effect) in effects.iter().enumerate() {
        if let Some(key) = effect.state_key() {
            if let Some(previous) = keyed.insert(key, index) {
                keep[previous] = false;
            }
        } else {
            let canonical = serde_json::to_string(effect).unwrap_or_default();
            if exact.insert(canonical, index).is_some() {
                keep[index] = false;
            }
        }
    }
    effects
        .into_iter()
        .zip(keep)
        .filter_map(|(effect, keep)| keep.then_some(effect))
        .collect()
}

/// A message catalogue key: dotted lower-case, dashes and underscores allowed.
fn is_text_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::runner::document_for;
    use crate::security::{StaticDialogs, StaticSecurity};
    use serde_json::json;
    use spot_app_api::meta::{DecimalFormat, FieldDef, TextFormat};
    use spot_app_api::prelude::Name;
    use spot_app_api::prelude::RecordId;
    use spot_permissions::Principal;

    fn meta() -> (DocTypeDef, Document) {
        let fields = vec![
            FieldDef::new(
                Name::new("qty").unwrap(),
                FieldKind::Int {
                    format: spot_app_api::meta::IntFormat::Plain,
                },
                false,
                false,
            ),
            FieldDef::new(
                Name::new("total").unwrap(),
                FieldKind::Decimal {
                    scale: 2,
                    precision: 18,
                    format: DecimalFormat::Plain,
                },
                false,
                false,
            ),
            FieldDef::new(
                Name::new("title").unwrap(),
                FieldKind::Text {
                    max_chars: Some(5),
                    format: TextFormat::Plain,
                },
                false,
                false,
            ),
            FieldDef::new(
                Name::new("status").unwrap(),
                FieldKind::Select {
                    values: vec!["draft".into(), "open".into()],
                },
                false,
                false,
            ),
            FieldDef::new(
                Name::new("customer").unwrap(),
                FieldKind::Link {
                    doctype: Name::new("customer").unwrap(),
                },
                false,
                false,
            ),
        ];
        let def = DocTypeDef::new(
            Name::new("order").unwrap(),
            "Order".into(),
            false,
            false,
            None,
            fields,
        );
        let doc = document_for(
            &def,
            &json!({"qty": 2, "title": "abc", "status": "draft"}),
            RecordId::new_v7(),
        )
        .unwrap();
        (def, doc)
    }

    fn run(raw: Vec<Json>) -> Validated {
        let (def, doc) = meta();
        let mut static_meta = crate::security::MapMeta::new();
        static_meta.insert(def.clone());
        let security = StaticSecurity::new();
        let dialogs = StaticDialogs::new(["pick".to_owned()]);
        let principal = Principal::system();
        let facts = DocFacts::default();
        let validator = Validator::new(&static_meta, &security, &dialogs, EffectLimits::default());
        validator.validate_raw(
            &raw,
            &EffectContext {
                def: &def,
                document: &doc,
                principal: &principal,
                facts: &facts,
            },
        )
    }

    #[test]
    fn a_valid_value_is_canonicalised() {
        let out = run(vec![json!({"kind":"set_value","path":"total","value":3})]);
        assert_eq!(out.problems, vec![]);
        assert_eq!(
            out.batch.effects,
            vec![Effect::SetValue {
                path: "total".into(),
                value: json!("3"),
            }]
        );
    }

    #[test]
    fn a_decimal_with_too_many_places_is_refused() {
        let out = run(vec![
            json!({"kind":"set_value","path":"total","value":"1.234"}),
        ]);
        assert_eq!(out.problems[0].code, codes::VALUE_INVALID);
        assert!(out.batch.is_empty());
    }

    #[test]
    fn a_select_value_must_be_listed() {
        let out = run(vec![
            json!({"kind":"set_value","path":"status","value":"closed"}),
        ]);
        assert_eq!(out.problems[0].code, codes::VALUE_INVALID);
    }

    #[test]
    fn a_hidden_field_is_dropped_without_naming_it() {
        let (def, doc) = meta();
        let mut static_meta = crate::security::MapMeta::new();
        static_meta.insert(def.clone());
        let mut security = StaticSecurity::new();
        security.hide("customer");
        let dialogs = StaticDialogs::none();
        let validator = Validator::new(&static_meta, &security, &dialogs, EffectLimits::default());
        let principal = Principal::system();
        let facts = DocFacts::default();
        let out = validator.validate_raw(
            &[json!({"kind":"set_value","path":"customer","value":"x"})],
            &EffectContext {
                def: &def,
                document: &doc,
                principal: &principal,
                facts: &facts,
            },
        );
        assert_eq!(out.problems[0].code, codes::PATH_UNAVAILABLE);
        assert_eq!(out.problems[0].path, None);
    }

    /// Runs `raw` with `title` hidden and `total` read-only for the user.
    fn run_restricted(raw: Vec<Json>) -> Validated {
        let (def, doc) = meta();
        let mut static_meta = crate::security::MapMeta::new();
        static_meta.insert(def.clone());
        let mut security = StaticSecurity::new();
        security.hide("title");
        security.read_only("total");
        let dialogs = StaticDialogs::none();
        let principal = Principal::system();
        let facts = DocFacts::default();
        let validator = Validator::new(&static_meta, &security, &dialogs, EffectLimits::default());
        validator.validate_raw(
            &raw,
            &EffectContext {
                def: &def,
                document: &doc,
                principal: &principal,
                facts: &facts,
            },
        )
    }

    #[test]
    fn a_hidden_field_and_a_nonexistent_one_answer_byte_for_byte_alike() {
        for kind in [
            json!({"kind":"set_value","path":"{}","value":1}),
            json!({"kind":"set_read_only","path":"{}","read_only":false}),
            json!({"kind":"set_label","path":"{}","label":"x"}),
            json!({"kind":"focus","path":"{}"}),
        ] {
            let with = |name: &str| {
                let mut effect = kind.clone();
                effect["path"] = json!(name);
                serde_json::to_vec(&run_restricted(vec![effect]).problems).unwrap()
            };
            let hidden = with("title");
            let missing = with("no_such_field");
            assert!(!hidden.is_empty());
            assert_eq!(hidden, missing, "{kind}");
            let text = String::from_utf8(hidden).unwrap();
            assert!(text.contains(codes::PATH_UNAVAILABLE), "{text}");
            assert!(
                !text.contains("title") && !text.contains("no_such_field"),
                "{text}"
            );
        }
    }

    #[test]
    fn widening_a_field_needs_the_right_to_write_it_but_narrowing_never_does() {
        // `total` is read-only for this user.
        for effect in [
            json!({"kind":"set_read_only","path":"total","read_only":false}),
            json!({"kind":"set_hidden","path":"total","hidden":false}),
            json!({"kind":"set_required","path":"total","required":true}),
            json!({"kind":"set_required","path":"total","required":false}),
        ] {
            let out = run_restricted(vec![effect.clone()]);
            assert!(out.batch.is_empty(), "{effect}");
            assert_eq!(out.problems.len(), 1, "{effect}");
            assert_eq!(out.problems[0].code, codes::FIELD_NOT_WRITABLE, "{effect}");
        }
        for effect in [
            json!({"kind":"set_read_only","path":"total","read_only":true}),
            json!({"kind":"set_hidden","path":"total","hidden":true}),
        ] {
            let out = run_restricted(vec![effect.clone()]);
            assert_eq!(out.problems, vec![], "{effect}");
            assert_eq!(out.batch.effects.len(), 1, "{effect}");
        }
        // A writable field may be toggled either way.
        let out = run_restricted(vec![
            json!({"kind":"set_read_only","path":"qty","read_only":false}),
            json!({"kind":"set_required","path":"qty","required":true}),
        ]);
        assert_eq!(out.problems, vec![]);
        assert_eq!(out.batch.effects.len(), 2);
    }

    #[test]
    fn duplicates_collapse_to_the_last_state_and_keep_order() {
        let out = run(vec![
            json!({"kind":"set_value","path":"qty","value":1}),
            json!({"kind":"set_hidden","path":"title","hidden":true}),
            json!({"kind":"set_value","path":"qty","value":2}),
        ]);
        assert_eq!(
            out.batch.effects,
            vec![
                Effect::SetHidden {
                    path: "title".into(),
                    hidden: true
                },
                Effect::SetValue {
                    path: "qty".into(),
                    value: json!(2)
                },
            ]
        );
    }

    #[test]
    fn an_unknown_kind_is_unknown_and_a_bad_shape_is_invalid() {
        let unknown = run(vec![json!({"kind":"sparkle","path":"qty"})]);
        assert_eq!(unknown.problems[0].code, codes::EFFECT_UNKNOWN);
        let invalid = run(vec![json!({"kind":"set_value","path":"qty"})]);
        assert_eq!(invalid.problems[0].code, codes::EFFECT_INVALID);
    }

    #[test]
    fn an_unregistered_dialog_is_dropped() {
        let out = run(vec![
            json!({"kind":"open_dialog","dialog_id":"nope","params":{}}),
        ]);
        assert_eq!(out.problems[0].code, codes::DIALOG_UNKNOWN);
    }

    #[test]
    fn text_length_is_checked() {
        let out = run(vec![
            json!({"kind":"set_value","path":"title","value":"too long"}),
        ]);
        assert_eq!(out.problems[0].code, codes::VALUE_INVALID);
    }
}
