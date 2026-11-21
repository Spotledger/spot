//! The redaction every script log line passes through before it reaches the server log sink
//!.
//!
//! A script reads the document the triggering user may see, but its log line is retained and
//! shipped. So each line goes through `spot-redact` with the **hidden-value set of the current
//! form event** registered as known secrets: the values of fields the user may not see, of
//! `Secret` fields and of fields in a sealed protection group, wherever they occur in the line
//! (also base64 and URL-encoded). The default pattern rules (tokens, keys, connection strings,
//! card numbers) apply on top. Fail closed: a redactor that cannot be built replaces every line
//! by [`REDACTION_FAILED_MARKER`].

use std::collections::BTreeSet;

use spot_redact::{RedactionPolicy, Redactor};
use spot_secrets::SecretString;

use super::document::row_facts;
use super::*;
use spot_app_api::meta::SyncClassHint;

/// What a log line becomes when the redaction layer cannot run.
pub(super) const REDACTION_FAILED_MARKER: &str = "[redacted:log_line]";

/// A value shorter than this cannot be registered with the redaction layer (it would also
/// match inside ordinary words); it is skipped.
const MIN_HIDDEN_VALUE_BYTES: usize = 8;

/// Builds the redactor for one script run: the default policy plus every hidden value of
/// `doc`. `None` when the layer cannot be built; the caller then marks every line.
pub(super) fn redactor_for(
    def: &DocTypeDef,
    doc: &Document,
    meta: &dyn FormMeta,
    security: &dyn FormSecurity,
    principal: &Principal,
    facts: &DocFacts,
) -> Option<Arc<Redactor>> {
    let mut redactor = Redactor::from_policy(&RedactionPolicy::default()).ok()?;
    let mut values = BTreeSet::new();
    collect_hidden(def, doc, meta, security, principal, facts, &mut values);
    for (index, value) in values.iter().enumerate() {
        redactor
            .register_secret(
                &format!("hidden_{index}"),
                &SecretString::new(value.clone()),
            )
            .ok()?;
    }
    Some(Arc::new(redactor))
}

/// Redacts `line`, or returns the fixed marker when there is no redactor.
pub(super) fn redact_line(redactor: Option<&Redactor>, line: &str) -> String {
    match redactor {
        Some(redactor) => redactor.redact_str(line).into_owned(),
        None => REDACTION_FAILED_MARKER.to_owned(),
    }
}

/// Whether the field's value must never reach a log.
fn is_protected(def: &DocTypeDef, field: &spot_app_api::meta::FieldDef) -> bool {
    if matches!(field.kind, FieldKind::Secret) {
        return true;
    }
    field
        .security
        .protection_group
        .as_ref()
        .is_some_and(|group| {
            def.protection_groups
                .iter()
                .any(|g| &g.name == group && g.sync_class_hint == SyncClassHint::Sealed)
        })
}

fn collect_hidden(
    def: &DocTypeDef,
    doc: &Document,
    meta: &dyn FormMeta,
    security: &dyn FormSecurity,
    principal: &Principal,
    facts: &DocFacts,
    out: &mut BTreeSet<String>,
) {
    for field in &def.fields {
        let name = field.name.as_str();
        let hidden = !security
            .access(def.name.as_str(), name, principal, facts)
            .visible;
        match &field.kind {
            FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. } => {
                let Some(child) = meta.doctype(doctype.as_str()) else {
                    // Unknown child definition: every cell is treated as hidden.
                    for row in doc.rows(name) {
                        for value in row.fields.values() {
                            leaves(&value_json(value), out);
                        }
                    }
                    continue;
                };
                for row in doc.rows(name) {
                    let row_facts = row_facts(doctype.as_str(), row);
                    for column in &child.fields {
                        let cell_hidden = hidden
                            || !security
                                .access(
                                    doctype.as_str(),
                                    column.name.as_str(),
                                    principal,
                                    &row_facts,
                                )
                                .visible;
                        if (cell_hidden || is_protected(&child, column))
                            && let Some(value) = row.fields.get(&column.name)
                        {
                            leaves(&value_json(value), out);
                        }
                    }
                }
            }
            _ => {
                if hidden || is_protected(def, field) {
                    leaves(&value_json(doc.get(name)), out);
                }
            }
        }
    }
}

/// Every string and number in `value`, as the text a log line would print it.
fn leaves(value: &Json, out: &mut BTreeSet<String>) {
    match value {
        Json::String(text) if text.len() >= MIN_HIDDEN_VALUE_BYTES => {
            out.insert(text.clone());
        }
        Json::Number(number) => {
            let text = number.to_string();
            if text.len() >= MIN_HIDDEN_VALUE_BYTES {
                out.insert(text);
            }
        }
        Json::Array(items) => items.iter().for_each(|item| leaves(item, out)),
        Json::Object(map) => map.values().for_each(|item| leaves(item, out)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::record::{FormEvent, FormScript, FormScriptSet};
    use crate::security::{MapMeta, StaticDialogs, StaticSecurity};
    use spot_app_api::meta::{FieldDef, GroupName, ProtectionGroupDef, TextFormat};
    use spot_app_api::prelude::{Name, OffsetDateTime, RecordId};

    const HIDDEN: &str = "HIDDEN-salary-4815162342";
    const SEALED: &str = "SEALED-bank-iban-2342";

    fn text(name: &str) -> FieldDef {
        FieldDef::new(
            Name::new(name).unwrap(),
            FieldKind::Text {
                max_chars: None,
                format: TextFormat::Plain,
            },
            false,
            false,
        )
    }

    fn def() -> Arc<DocTypeDef> {
        let mut sealed = text("bank");
        sealed.security.protection_group = Some(GroupName::new("bank_details").unwrap());
        let mut def = DocTypeDef::new(
            Name::new("order").unwrap(),
            "Order".into(),
            false,
            false,
            None,
            vec![text("title"), text("salary"), sealed],
        );
        let mut group = ProtectionGroupDef::new(
            GroupName::new("bank_details").unwrap(),
            "Bank details".into(),
        );
        group.sync_class_hint = SyncClassHint::Sealed;
        def.protection_groups.push(group);
        Arc::new(def)
    }

    async fn logs(source: &str) -> Vec<String> {
        let def = def();
        let doc = document_for(
            &def,
            &json!({"title": "an ordinary order", "salary": HIDDEN, "bank": SEALED}),
            RecordId::new_v7(),
        )
        .unwrap();
        let event = FormEvent::Change {
            field: "title".into(),
        };
        let mut scripts = FormScriptSet::default();
        scripts
            .insert(FormScript::new("s", "order", event.clone(), source))
            .unwrap();
        let mut meta = MapMeta::new();
        meta.insert_shared(Arc::clone(&def));
        let mut security = StaticSecurity::new();
        security.hide("salary");
        let runner = FormScriptRunner::new(
            Arc::new(meta),
            Arc::new(security),
            Arc::new(StaticDialogs::none()),
        );
        let request = RunRequest {
            def: Arc::clone(&def),
            document: &doc,
            event: &event,
            scripts: &scripts,
            principal: &Principal::system(),
            facts: &DocFacts::default(),
            server_patch: &Json::Null,
            now: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        };
        runner.run(&request).await.logs
    }

    #[tokio::test]
    async fn hidden_sealed_and_secret_values_and_token_shapes_never_reach_the_sink() {
        let source = format!(
            r#"
export function on_change(doc, changed, ctx) {{
  host.log({{ message: "salary is {HIDDEN} ok" }});
  host.log({{ message: "bank {SEALED}" }});
  host.log({{ message: "token ghp_{token}" }});
  host.log({{ message: "title " + doc.title }});
}}
"#,
            token = "a".repeat(36)
        );
        let lines = logs(&source).await;
        assert_eq!(lines.len(), 4, "{lines:?}");
        let all = lines.join("\n");
        for leak in [HIDDEN, SEALED, "ghp_aaaa"] {
            assert!(!all.contains(leak), "{leak} leaked: {all}");
        }
        assert!(lines[0].starts_with("s: salary is [redacted:"), "{lines:?}");
        assert!(lines[2].contains("[redacted:github_token]"), "{lines:?}");
        assert_eq!(
            lines[3], "s: title an ordinary order",
            "ordinary lines are unchanged"
        );
    }

    #[tokio::test]
    async fn the_cap_applies_after_redaction() {
        let source = format!(
            r#"
export function on_change(doc, changed, ctx) {{
  host.log({{ message: "{pad}{HIDDEN}" }});
}}
"#,
            pad = "x".repeat(1_010)
        );
        let lines = logs(&source).await;
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("HIDDEN-salary"), "{}", lines[0]);
        assert!(!lines[0].contains("4815"), "{}", lines[0]);
    }

    #[tokio::test]
    async fn a_line_logged_before_the_script_throws_is_redacted_too() {
        let source = format!(
            r#"
export function on_change(doc, changed, ctx) {{
  host.log({{ message: "about to fail on {HIDDEN}" }});
  throw new Error("boom");
}}
"#
        );
        let lines = logs(&source).await;
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(!lines[0].contains("4815162342"), "{lines:?}");
    }

    #[tokio::test]
    async fn a_hidden_child_table_cell_is_redacted() {
        let child = Arc::new(DocTypeDef::new(
            Name::new("line").unwrap(),
            "Line".into(),
            true,
            false,
            None,
            vec![text("title"), text("secret_note")],
        ));
        let def = Arc::new(DocTypeDef::new(
            Name::new("order").unwrap(),
            "Order".into(),
            false,
            false,
            None,
            vec![
                text("title"),
                FieldDef::new(
                    Name::new("lines").unwrap(),
                    FieldKind::Table {
                        doctype: Name::new("line").unwrap(),
                    },
                    false,
                    false,
                ),
            ],
        ));
        let cell_value = "CELL-hidden-value-9988";
        let doc = document_for(
            &def,
            &json!({"title": "t", "lines": [{"title": "row", "secret_note": cell_value}]}),
            RecordId::new_v7(),
        )
        .unwrap();
        let event = FormEvent::Change {
            field: "title".into(),
        };
        let mut scripts = FormScriptSet::default();
        scripts
            .insert(FormScript::new(
                "s",
                "order",
                event.clone(),
                &format!(
                    r#"export function on_change(doc, changed, ctx) {{ host.log({{ message: "n {cell_value}" }}); }}"#
                ),
            ))
            .unwrap();
        let mut meta = MapMeta::new();
        meta.insert_shared(Arc::clone(&def));
        meta.insert_shared(Arc::clone(&child));
        let mut security = StaticSecurity::new();
        security.hide("secret_note");
        let runner = FormScriptRunner::new(
            Arc::new(meta),
            Arc::new(security),
            Arc::new(StaticDialogs::none()),
        );
        let request = RunRequest {
            def: Arc::clone(&def),
            document: &doc,
            event: &event,
            scripts: &scripts,
            principal: &Principal::system(),
            facts: &DocFacts::default(),
            server_patch: &Json::Null,
            now: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        };
        let out = runner.run(&request).await;
        assert_eq!(out.logs.len(), 1, "{:?}", out.logs);
        assert!(!out.logs[0].contains("9988"), "{:?}", out.logs);
    }

    #[test]
    fn a_line_that_cannot_be_redacted_becomes_the_marker() {
        assert_eq!(
            redact_line(None, "anything at all"),
            REDACTION_FAILED_MARKER
        );
        let bridge = EffectBridge::new(Default::default(), None, 10, 0, 0);
        assert!(bridge.call(HOST_LOG, json!({"message": HIDDEN})).is_ok());
        assert_eq!(bridge.take_logs(), vec![REDACTION_FAILED_MARKER.to_owned()]);
    }
}
