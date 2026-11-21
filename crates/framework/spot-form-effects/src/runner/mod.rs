//! Running a form's TypeScript scripts for one form event, and turning what they emit into a
//! validated effect batch and a document diff.
//!
//! # Where this sits
//!
//! The form-event endpoint (`spot-api-services`, WP-UI-01.T1) is a pure derivation: the client
//! sends a draft and the field the user changed, and the engine's `field_changed` steps answer a
//! diff. This runner is the next step in that same request. The endpoint computes its diff first
//! and passes it to [`FormScriptRunner::run`] as `server_patch`; the runner applies it, runs the
//! form scripts on the result, and returns the scripts' own diff and effects.
//!
//! # Ordering
//!
//! 1. The client's instant field rules (the shared expression language) ran locally before it
//!    sent the event; they are not repeated here.
//! 2. The endpoint's `field_changed` diff (`server_patch`) is applied to the draft first.
//! 3. The event's scripts run in deterministic order: priority, then name. Each sees the document
//!    with the earlier scripts' patches applied.
//! 4. A script's document patch is applied immediately, so the next script sees it. Its effects
//!    are collected in emission order, then any effects it returned explicitly.
//! 5. Effects are validated against the final document and the user's rights, de-duplicated, and
//!    only then returned. A client applies them in the order given.
//! 6. The server's decision wins on a conflict: the returned `merged_diff` is `server_patch`
//!    overridden by the scripts' diff, and these effects are the last word on presentation.
//!
//! # Idempotency
//!
//! [`FormOutcome::idempotency_key`] is a deterministic hash of the DocType, the event (with its
//! target), the draft plus the endpoint's patch, and the scripts that ran (name and source). The
//! same request on the same scripts gives the same key; a retry can therefore be recognised. It
//! never reads a clock or a random source.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Map, Value as Json, json};
use spot_app_api::Document;
use spot_app_api::document::ChildRow;
use spot_app_api::meta::{DocTypeDef, FieldKind};
use spot_app_api::prelude::{OffsetDateTime, RecordId};
use spot_permissions::{DocFacts, Principal};
use spot_server_scripts::ScriptLimits;
use spot_server_scripts::{patch_from_json, value_json};
use spot_ts::{
    EngineConfig, HostBridge, HostFn, HostSurface, ScriptEngine, ScriptError, ScriptId, Shape,
};

use crate::codes;
use crate::effect::{Effect, EffectBatch, MessageLevel};
use crate::problem::{EffectProblem, ScriptFailure};
use crate::record::{FORM_SCRIPT_DOCTYPE, FormEvent, FormScript, FormScriptSet};
use crate::security::{DialogRegistry, FormMeta, FormSecurity};
use crate::validate::{EffectContext, EffectLimits, Validator};

mod bridge;
mod document;
mod ids;
mod redaction;

use bridge::{EffectBridge, harness, host_surface, lifecycle_record};
use document::{
    apply_patch, apply_script_patch, document_diff, merge_patches, patch_is_writable, path_visible,
    visible_document,
};
pub use ids::{document_for, id_for};
use ids::{fnv1a, idempotency_key, now_millis, seed_for};
use redaction::redactor_for;

/// The host functions a form script may call: emits one effect, or logs one line. There is no
/// `fetch`, no document read or write and no other document's data: a form event is a pure
/// derivation on the draft the user already has open.
const HOST_EMIT: &str = "emit";
/// The host function carrying a script's log line to the server log.
const HOST_LOG: &str = "log";

/// The runner's bounds: the engine's per-script limits and the protocol's effect caps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunnerLimits {
    /// The engine and per-script limits (`spot-server-scripts`).
    pub scripts: ScriptLimits,
    /// The effect caps.
    pub effects: EffectLimits,
}

/// One form event to run.
pub struct RunRequest<'a> {
    /// The DocType's effective definition.
    pub def: Arc<DocTypeDef>,
    /// The draft the client sent, before the endpoint's diff.
    pub document: &'a Document,
    /// What the user did.
    pub event: &'a FormEvent,
    /// The registered form scripts.
    pub scripts: &'a FormScriptSet,
    /// The triggering user.
    pub principal: &'a Principal,
    /// The document facts the permission decision uses.
    pub facts: &'a DocFacts,
    /// The endpoint's own diff (the engine's `field_changed` steps), applied before the scripts.
    /// `null` or `{}` when the endpoint derived nothing.
    pub server_patch: &'a Json,
    /// The invocation's time; the only clock a script sees.
    pub now: OffsetDateTime,
}

/// What one form event answers.
#[derive(Clone, Debug, PartialEq)]
pub struct FormOutcome {
    /// The validated effects, in application order.
    pub batch: EffectBatch,
    /// The effects dropped on the way, with why.
    pub problems: Vec<EffectProblem>,
    /// The scripts' own document diff (a merge patch).
    pub diff: Json,
    /// The endpoint's diff with the scripts' diff on top: the server's final decision.
    pub merged_diff: Json,
    /// The document after the endpoint's patch and the scripts' patches.
    pub document: Document,
    /// The deterministic idempotency key of the request.
    pub idempotency_key: String,
    /// The scripts that ran, in order.
    pub ran: Vec<String>,
    /// Server-side failures; never sent to a client.
    pub errors: Vec<ScriptFailure>,
    /// The lines the scripts logged, `script: message`; for the server log.
    pub logs: Vec<String>,
}

/// A prepared script: the JavaScript the engine compiles, and its source map.
struct Prepared {
    js: String,
    source_map: String,
}

/// The form-script runner. Build it once per host and share it; its compile cache is safe to use
/// from many requests.
pub struct FormScriptRunner {
    engine: ScriptEngine,
    limits: RunnerLimits,
    meta: Arc<dyn FormMeta>,
    security: Arc<dyn FormSecurity>,
    dialogs: Arc<dyn DialogRegistry>,
    prepared: Mutex<BTreeMap<String, Arc<Prepared>>>,
}

impl FormScriptRunner {
    /// A runner over these authorities, with the default limits and a fresh engine.
    pub fn new(
        meta: Arc<dyn FormMeta>,
        security: Arc<dyn FormSecurity>,
        dialogs: Arc<dyn DialogRegistry>,
    ) -> FormScriptRunner {
        FormScriptRunner {
            engine: ScriptEngine::new(EngineConfig {
                surface: host_surface(),
                ..EngineConfig::default()
            }),
            limits: RunnerLimits::default(),
            meta,
            security,
            dialogs,
            prepared: Mutex::new(BTreeMap::new()),
        }
    }

    /// The same runner with the host's bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: RunnerLimits) -> FormScriptRunner {
        self.limits = limits;
        self
    }

    /// Runs `request` and answers its effects and diff. A script failure never fails the form
    /// event: it becomes one safe message and a server-side [`ScriptFailure`].
    pub async fn run(&self, request: &RunRequest<'_>) -> FormOutcome {
        let def = Arc::clone(&request.def);
        let base = match apply_patch(request.document, &def, request.server_patch) {
            Ok(document) => document,
            Err(error) => {
                // The endpoint computed this patch; if it does not fit, the form event cannot
                // be served safely. The empty outcome carries the reason for the server log.
                return FormOutcome {
                    batch: EffectBatch::empty(),
                    problems: Vec::new(),
                    diff: Json::Object(Map::new()),
                    merged_diff: Json::Object(Map::new()),
                    document: request.document.clone(),
                    idempotency_key: idempotency_key(&def, request, &[]),
                    ran: Vec::new(),
                    errors: vec![ScriptFailure::new(
                        "form_event",
                        codes::PATCH_INVALID,
                        format!("the server patch does not fit the DocType: {error}"),
                    )],
                    logs: Vec::new(),
                };
            }
        };
        let mut doc = base.clone();
        let matching = request.scripts.matching(def.name.as_str(), request.event);
        let mut raw: Vec<Json> = Vec::new();
        let mut errors = Vec::new();
        let mut logs = Vec::new();
        let mut ran = Vec::new();
        let mut failed = false;
        let mut log_truncated = false;
        let mut extra_problems: Vec<EffectProblem> = Vec::new();
        for script in &matching {
            ran.push(script.name.clone());
            let prepared = match self.prepare(script) {
                Ok(prepared) => prepared,
                Err(error) => {
                    errors.push(error);
                    failed = true;
                    continue;
                }
            };
            let compiled = match self.engine.compile(
                &prepared.js,
                ScriptId::new(
                    FORM_SCRIPT_DOCTYPE,
                    &script.name,
                    fnv1a(script.source.as_bytes()),
                ),
                Some(&prepared.source_map),
            ) {
                Ok(compiled) => compiled,
                Err(error) => {
                    errors.push(ScriptFailure::new(
                        &script.name,
                        codes::COMPILE_FAILED,
                        error.to_string(),
                    ));
                    failed = true;
                    continue;
                }
            };
            let args = json!({
                "doc": visible_document(
                    &def,
                    &doc,
                    self.meta.as_ref(),
                    self.security.as_ref(),
                    request.principal,
                    request.facts,
                ),
                "changed": request.event.field(),
                "event": request.event.as_str(),
                "doctype": def.name.as_str(),
                "id": doc.id().to_string(),
                "now": now_millis(request.now),
            });
            let redactor = redactor_for(
                &def,
                &doc,
                self.meta.as_ref(),
                self.security.as_ref(),
                request.principal,
                request.facts,
            );
            let bridge = Arc::new(EffectBridge::new(
                self.limits.effects,
                redactor,
                self.limits.effects.max_log_lines.saturating_sub(logs.len()),
                now_millis(request.now),
                seed_for(&script.name, request.event, &doc),
            ));
            let result = self
                .engine
                .run(
                    &compiled,
                    "__spot_form_run",
                    &args,
                    Arc::clone(&bridge) as Arc<dyn HostBridge>,
                    self.limits.scripts.engine_limits(&lifecycle_record(script)),
                )
                .await;
            if bridge.log_overflowed() && !log_truncated {
                log_truncated = true;
                errors.push(ScriptFailure::new(
                    &script.name,
                    codes::LOG_TRUNCATED,
                    "the scripts logged more lines than the per-event cap; the surplus was dropped",
                ));
            }
            logs.extend(
                bridge
                    .take_logs()
                    .into_iter()
                    .map(|line| format!("{}: {line}", script.name)),
            );
            let mut keep_effects = true;
            let mut returned_effects: Vec<Json> = Vec::new();
            match result {
                Ok(value) if !value.is_null() && !value.is_object() => {
                    errors.push(ScriptFailure::new(
                        &script.name,
                        codes::EFFECT_INVALID,
                        "a form script returns null or an object with `patch` and `effects`",
                    ));
                    failed = true;
                    keep_effects = false;
                }
                Ok(value) => {
                    let mut returned = value.as_object().cloned().unwrap_or_default();
                    if let Some(patch) = returned.remove("patch")
                        && !patch.as_object().is_none_or(Map::is_empty)
                    {
                        if !patch_is_writable(
                            &def,
                            &doc,
                            self.meta.as_ref(),
                            self.security.as_ref(),
                            request.principal,
                            request.facts,
                            &patch,
                        ) {
                            // The whole patch goes; nothing of it is applied, and the problem
                            // names no field, so it cannot reveal a hidden one.
                            extra_problems.push(EffectProblem {
                                code: codes::FIELD_UNAVAILABLE.to_owned(),
                                index: None,
                                path: None,
                                message: "A form script changed a field the user may not change."
                                    .to_owned(),
                            });
                            errors.push(ScriptFailure::new(
                                &script.name,
                                codes::FIELD_UNAVAILABLE,
                                "the returned patch names a field the user may not change, \
                                 cannot see, or that the form does not have",
                            ));
                            failed = true;
                            keep_effects = false;
                        } else {
                            match apply_script_patch(&doc, &def, &patch, &script.name) {
                                Ok(updated) => doc = updated,
                                Err(error) => {
                                    errors.push(ScriptFailure::new(
                                        &script.name,
                                        codes::PATCH_INVALID,
                                        error,
                                    ));
                                    failed = true;
                                    keep_effects = false;
                                }
                            }
                        }
                    }
                    if let Some(effects) = returned.remove("effects") {
                        match effects {
                            Json::Array(items) => returned_effects.extend(items),
                            Json::Null => {}
                            _ => {
                                errors.push(ScriptFailure::new(
                                    &script.name,
                                    codes::EFFECT_INVALID,
                                    "`effects` must be an array".to_owned(),
                                ));
                                failed = true;
                                keep_effects = false;
                            }
                        }
                    }
                }
                Err(ScriptError::Validation {
                    code,
                    message,
                    fields,
                }) if code != codes::MISSING_HANDLER => {
                    // A validation throw is intentional: its text is written for the user, but
                    // the state it emitted before throwing is not applied.
                    keep_effects = false;
                    if fields.is_empty() {
                        raw.push(
                            serde_json::to_value(Effect::ShowMessage {
                                level: MessageLevel::Error,
                                text: Some(message),
                                text_key: None,
                                field: None,
                            })
                            .unwrap_or(Json::Null),
                        );
                    } else {
                        for field in fields {
                            if path_visible(
                                &def,
                                &doc,
                                self.meta.as_ref(),
                                self.security.as_ref(),
                                request.principal,
                                request.facts,
                                &field.field,
                            ) {
                                raw.push(
                                    serde_json::to_value(Effect::ShowMessage {
                                        level: MessageLevel::Error,
                                        text: Some(field.message),
                                        text_key: None,
                                        field: Some(field.field),
                                    })
                                    .unwrap_or(Json::Null),
                                );
                            } else {
                                errors.push(ScriptFailure::new(
                                    &script.name,
                                    codes::SCRIPT_FAILED,
                                    "a validation message named a field the user cannot see",
                                ));
                                failed = true;
                            }
                        }
                    }
                }
                // The cap already has its own failure below; an uncaught limit error would
                // only repeat it.
                Err(_) if bridge.overflowed() => {
                    keep_effects = false;
                }
                Err(error) => {
                    keep_effects = false;
                    let (line, column) = match &error {
                        ScriptError::Thrown {
                            position: Some(position),
                            ..
                        } => (Some(position.line), Some(position.column)),
                        _ => (None, None),
                    };
                    let code = match &error {
                        ScriptError::MissingEntry(_) => codes::MISSING_HANDLER,
                        ScriptError::Validation { code, .. } if code == codes::MISSING_HANDLER => {
                            codes::MISSING_HANDLER
                        }
                        _ => codes::SCRIPT_FAILED,
                    };
                    errors.push(
                        ScriptFailure::new(&script.name, code, error.to_string()).at(line, column),
                    );
                    failed = true;
                }
            }
            let emitted = bridge.take_effects();
            if bridge.overflowed() {
                errors.push(ScriptFailure::new(
                    &script.name,
                    codes::LIMIT_EXCEEDED,
                    "the script emitted more effects, or larger ones, than the caps allow",
                ));
                failed = true;
                keep_effects = false;
            }
            if keep_effects {
                raw.extend(emitted);
                raw.extend(returned_effects);
            }
        }

        if failed {
            // One safe message, whatever went wrong and however many scripts failed. The
            // details live in `errors`, which never reach the client.
            raw.push(
                serde_json::to_value(Effect::ShowMessage {
                    level: MessageLevel::Error,
                    text: None,
                    text_key: Some(format!("error.{}", codes::SCRIPT_FAILED)),
                    field: None,
                })
                .unwrap_or(Json::Null),
            );
        }

        let validator = Validator::new(
            self.meta.as_ref(),
            self.security.as_ref(),
            self.dialogs.as_ref(),
            self.limits.effects,
        );
        let validator_ctx = EffectContext {
            def: &def,
            document: &doc,
            principal: request.principal,
            facts: request.facts,
        };
        let mut validated = validator.validate_raw(&raw, &validator_ctx);
        validated.problems.extend(extra_problems);
        let diff = document_diff(
            &def,
            &base,
            &doc,
            self.meta.as_ref(),
            self.security.as_ref(),
            request.principal,
            request.facts,
        );
        let merged = merge_patches(request.server_patch, &diff);
        FormOutcome {
            batch: validated.batch,
            problems: validated.problems,
            diff,
            merged_diff: merged,
            document: doc,
            idempotency_key: idempotency_key(&def, request, &ran),
            ran,
            errors,
            logs,
        }
    }

    /// Transpiles a script once per distinct source, appending the small harness that turns
    /// `ctx.effects` into host calls and calls the script's exported handler.
    fn prepare(&self, script: &FormScript) -> Result<Arc<Prepared>, ScriptFailure> {
        let key = format!("{}:{}", script.name, fnv1a(script.source.as_bytes()));
        if let Some(found) = self.prepared().get(&key) {
            return Ok(Arc::clone(found));
        }
        let options = spot_ts_transpile::Options {
            allow_imports: Vec::new(),
            ..spot_ts_transpile::Options::default()
        };
        let file = format!("{}.ts", script.name);
        let transpiled = spot_ts_transpile::transpile(&script.source, &file, &options).map_err(
            |diagnostics| {
                let message = diagnostics
                    .first()
                    .map(|d| format!("line {}, column {}: {}", d.line, d.column, d.message))
                    .unwrap_or_else(|| "the source did not transpile".to_owned());
                ScriptFailure::new(&script.name, codes::COMPILE_FAILED, message)
            },
        )?;
        let prepared = Arc::new(Prepared {
            js: format!("{}\n{}", transpiled.js, harness(script.entry())),
            source_map: transpiled.source_map,
        });
        self.prepared().insert(key, Arc::clone(&prepared));
        Ok(prepared)
    }

    fn prepared(&self) -> MutexGuard<'_, BTreeMap<String, Arc<Prepared>>> {
        self.prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::security::{MapMeta, StaticDialogs, StaticSecurity};
    use spot_app_api::meta::{DecimalFormat, FieldDef, TextFormat};
    use spot_app_api::prelude::Name;
    use spot_permissions::Principal;

    fn meta() -> (Arc<DocTypeDef>, Document) {
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
                    max_chars: Some(20),
                    format: TextFormat::Plain,
                },
                false,
                false,
            ),
        ];
        let def = Arc::new(DocTypeDef::new(
            Name::new("order").unwrap(),
            "Order".into(),
            false,
            false,
            None,
            fields,
        ));
        let doc = document_for(&def, &json!({"qty": 2, "title": "t"}), RecordId::new_v7()).unwrap();
        (def, doc)
    }

    fn runner(def: &Arc<DocTypeDef>, security: Arc<dyn FormSecurity>) -> FormScriptRunner {
        let mut meta = MapMeta::new();
        meta.insert_shared(Arc::clone(def));
        FormScriptRunner::new(Arc::new(meta), security, Arc::new(StaticDialogs::none()))
    }

    async fn run_one(source: &str, event: FormEvent, changed: bool) -> FormOutcome {
        let (def, doc) = meta();
        let mut scripts = FormScriptSet::default();
        scripts
            .insert(FormScript::new("totals", "order", event.clone(), source))
            .unwrap();
        let runner = runner(&def, Arc::new(StaticSecurity::new()));
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
        let _ = changed;
        runner.run(&request).await
    }

    #[tokio::test]
    async fn a_script_emits_effects_through_the_builder() {
        let out = run_one(
            r#"
export function on_change(doc, changed, ctx) {
  ctx.effects.setValue("total", "12.50");
  ctx.effects.focus("total");
}
"#,
            FormEvent::Change {
                field: "qty".into(),
            },
            true,
        )
        .await;
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.problems, vec![]);
        assert_eq!(
            out.batch.effects,
            vec![
                Effect::SetValue {
                    path: "total".into(),
                    value: json!("12.50")
                },
                Effect::Focus {
                    path: "total".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn a_patch_changes_the_document_and_the_diff() {
        let out = run_one(
            r#"
export function on_change(doc, changed, ctx) {
  return { patch: { title: "new" } };
}
"#,
            FormEvent::Change {
                field: "qty".into(),
            },
            true,
        )
        .await;
        assert_eq!(out.diff, json!({"title": "new"}));
        assert_eq!(out.merged_diff, json!({"title": "new"}));
    }

    #[tokio::test]
    async fn an_uncaught_error_becomes_one_safe_message() {
        let out = run_one(
            r#"
export function on_change(doc, changed, ctx) {
  throw new Error("secret detail");
}
"#,
            FormEvent::Change {
                field: "qty".into(),
            },
            true,
        )
        .await;
        assert_eq!(out.errors.len(), 1);
        assert_eq!(out.batch.effects.len(), 1);
        match &out.batch.effects[0] {
            Effect::ShowMessage {
                level,
                text,
                text_key,
                ..
            } => {
                assert_eq!(*level, MessageLevel::Error);
                assert_eq!(text.as_deref(), None);
                assert_eq!(
                    text_key.as_deref(),
                    Some("error.form_effects.script_failed")
                );
            }
            other => panic!("{other:?}"),
        }
        assert!(!out.batch.effects.iter().any(|e| {
            serde_json::to_string(e)
                .unwrap_or_default()
                .contains("secret detail")
        }));
    }

    #[tokio::test]
    async fn log_lines_are_capped_per_run_and_reported_once() {
        let (def, doc) = meta();
        let event = FormEvent::Change {
            field: "qty".into(),
        };
        let mut scripts = FormScriptSet::default();
        scripts
            .insert(FormScript::new(
                "totals",
                "order",
                event.clone(),
                r#"
export function on_change(doc, changed, ctx) {
  for (let i = 0; i < 50; i++) { host.log({ message: "line " + i }); }
}
"#,
            ))
            .unwrap();
        let mut limits = RunnerLimits::default();
        limits.effects.max_log_lines = 10;
        let runner = runner(&def, Arc::new(StaticSecurity::new())).with_limits(limits);
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
        assert_eq!(out.logs.len(), 10, "the cap is the constructor argument");
        assert_eq!(out.logs[0], "totals: line 0");
        let notices = out
            .errors
            .iter()
            .filter(|e| e.code == codes::LOG_TRUNCATED)
            .count();
        assert_eq!(notices, 1, "one notice, not one per line: {:?}", out.errors);
        // A noisy script is not a failing one: no safe error message reaches the client.
        assert!(out.batch.effects.is_empty(), "{:?}", out.batch.effects);
        assert_eq!(RunnerLimits::default().effects.max_log_lines, 100);
    }

    #[tokio::test]
    async fn a_long_log_line_is_cut_and_marked() {
        let out = run_one(
            r#"
export function on_change(doc, changed, ctx) {
  host.log({ message: "x".repeat(5000) });
}
"#,
            FormEvent::Change {
                field: "qty".into(),
            },
            true,
        )
        .await;
        assert_eq!(out.logs.len(), 1);
        assert!(out.logs[0].ends_with("...[truncated]"));
        assert!(out.logs[0].len() < 1_100, "{}", out.logs[0].len());
        assert!(out.errors.is_empty(), "{:?}", out.errors);
    }

    #[tokio::test]
    async fn a_patch_naming_a_read_only_field_is_rejected_whole() {
        let (def, doc) = meta();
        let mut scripts = FormScriptSet::default();
        scripts
            .insert(FormScript::new(
                "p",
                "order",
                FormEvent::Change {
                    field: "qty".into(),
                },
                r#"export function on_change(doc, changed, ctx) { return { patch: { title: "ok", total: "1.00" } }; }"#,
            ))
            .unwrap();
        let mut security = StaticSecurity::new();
        security.read_only("total");
        let runner = runner(&def, Arc::new(security));
        let event = FormEvent::Change {
            field: "qty".into(),
        };
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
        assert_eq!(out.diff, json!({}), "nothing of the patch was applied");
        let [problem] = out.problems.as_slice() else {
            panic!("{:?}", out.problems);
        };
        assert_eq!(problem.code, codes::FIELD_UNAVAILABLE);
        assert_eq!(problem.path, None);
        assert!(!problem.message.contains("total"), "{}", problem.message);
        assert!(out.errors.iter().all(|e| !e.message.contains("total")));
    }

    #[tokio::test]
    async fn a_typed_validation_throw_shows_its_field_message() {
        let out = run_one(
            r#"
export function on_save(doc, changed, ctx) {
  throw { code: "order.qty_bad", message: "Qty is odd.", fields: [{ field: "qty", code: "order.qty_bad", message: "Qty is odd." }] };
}
"#,
            FormEvent::Save,
            false,
        )
        .await;
        assert_eq!(
            out.batch.effects,
            vec![Effect::ShowMessage {
                level: MessageLevel::Error,
                text: Some("Qty is odd.".into()),
                text_key: None,
                field: Some("qty".into()),
            }]
        );
    }

    #[tokio::test]
    async fn a_missing_handler_is_one_safe_message() {
        let out = run_one(
            r#"export function on_load(doc, changed, ctx) { ctx.effects.focus("qty"); }"#,
            FormEvent::Change {
                field: "qty".into(),
            },
            true,
        )
        .await;
        assert_eq!(out.errors.len(), 1);
        assert_eq!(out.batch.effects.len(), 1);
    }

    #[tokio::test]
    async fn scripts_run_in_priority_order_and_see_earlier_patches() {
        let (def, doc) = meta();
        let mut scripts = FormScriptSet::default();
        scripts
            .insert(
                FormScript::new(
                    "second",
                    "order",
                    FormEvent::Save,
                    r#"export function on_save(doc, changed, ctx) { ctx.effects.setLabel("qty", doc.title); }"#,
                )
                .with_priority(2),
            )
            .unwrap();
        scripts
            .insert(
                FormScript::new(
                    "first",
                    "order",
                    FormEvent::Save,
                    r#"export function on_save(doc, changed, ctx) { return { patch: { title: "from first" } }; }"#,
                )
                .with_priority(1),
            )
            .unwrap();
        let runner = runner(&def, Arc::new(StaticSecurity::new()));
        let event = FormEvent::Save;
        let out = runner
            .run(&RunRequest {
                def: Arc::clone(&def),
                document: &doc,
                event: &event,
                scripts: &scripts,
                principal: &Principal::system(),
                facts: &DocFacts::default(),
                server_patch: &Json::Null,
                now: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            })
            .await;
        assert_eq!(out.ran, vec!["first", "second"]);
        assert_eq!(
            out.batch.effects,
            vec![Effect::SetLabel {
                path: "qty".into(),
                label: "from first".into()
            }]
        );
    }

    #[tokio::test]
    async fn a_script_that_emits_too_many_effects_fails_safely() {
        let (def, doc) = meta();
        let mut scripts = FormScriptSet::default();
        scripts
            .insert(FormScript::new(
                "flood",
                "order",
                FormEvent::Change {
                    field: "qty".into(),
                },
                r#"
export function on_change(doc, changed, ctx) {
  ctx.effects.focus("qty");
  ctx.effects.focus("title");
  ctx.effects.focus("amount");
}
"#,
            ))
            .unwrap();
        let mut meta_map = MapMeta::new();
        meta_map.insert_shared(Arc::clone(&def));
        let runner = FormScriptRunner::new(
            Arc::new(meta_map),
            Arc::new(StaticSecurity::new()),
            Arc::new(StaticDialogs::none()),
        )
        .with_limits(RunnerLimits {
            effects: EffectLimits {
                max_effects: 2,
                ..EffectLimits::default()
            },
            ..RunnerLimits::default()
        });
        let event = FormEvent::Change {
            field: "qty".into(),
        };
        let out = runner
            .run(&RunRequest {
                def: Arc::clone(&def),
                document: &doc,
                event: &event,
                scripts: &scripts,
                principal: &Principal::system(),
                facts: &DocFacts::default(),
                server_patch: &Json::Null,
                now: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            })
            .await;
        assert_eq!(out.errors.len(), 1);
        assert_eq!(out.errors[0].code, codes::LIMIT_EXCEEDED);
        assert_eq!(out.batch.effects.len(), 1);
        assert!(matches!(
            &out.batch.effects[0],
            Effect::ShowMessage {
                level: MessageLevel::Error,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn the_script_view_hides_a_field_the_user_cannot_see() {
        let (def, doc) = meta();
        let mut scripts = FormScriptSet::default();
        scripts
            .insert(FormScript::new(
                "view",
                "order",
                FormEvent::Change {
                    field: "qty".into(),
                },
                r#"
export function on_change(doc, changed, ctx) {
  ctx.effects.setLabel("qty", doc.note === undefined ? "hidden" : "visible");
}
"#,
            ))
            .unwrap();
        let mut meta_map = MapMeta::new();
        meta_map.insert_shared(Arc::clone(&def));
        let mut security = StaticSecurity::new();
        security.hide("note");
        let runner = FormScriptRunner::new(
            Arc::new(meta_map),
            Arc::new(security),
            Arc::new(StaticDialogs::none()),
        );
        let event = FormEvent::Change {
            field: "qty".into(),
        };
        let out = runner
            .run(&RunRequest {
                def: Arc::clone(&def),
                document: &doc,
                event: &event,
                scripts: &scripts,
                principal: &Principal::system(),
                facts: &DocFacts::default(),
                server_patch: &Json::Null,
                now: OffsetDateTime::from_unix_timestamp(0).unwrap(),
            })
            .await;
        assert_eq!(
            out.batch.effects,
            vec![Effect::SetLabel {
                path: "qty".into(),
                label: "hidden".into()
            }]
        );
    }

    #[tokio::test]
    async fn the_idempotency_key_is_deterministic_and_event_specific() {
        let (def, doc) = meta();
        let scripts = FormScriptSet::default();
        let runner = runner(&def, Arc::new(StaticSecurity::new()));
        let change = FormEvent::Change {
            field: "qty".into(),
        };
        let other = FormEvent::Change {
            field: "title".into(),
        };
        let request = RunRequest {
            def: Arc::clone(&def),
            document: &doc,
            event: &change,
            scripts: &scripts,
            principal: &Principal::system(),
            facts: &DocFacts::default(),
            server_patch: &Json::Null,
            now: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        };
        let first = runner.run(&request).await;
        let again = runner.run(&request).await;
        assert_eq!(first.idempotency_key, again.idempotency_key);
        let mut second_request = RunRequest {
            def: Arc::clone(&def),
            document: &doc,
            event: &other,
            scripts: &scripts,
            principal: &Principal::system(),
            facts: &DocFacts::default(),
            server_patch: &Json::Null,
            now: OffsetDateTime::from_unix_timestamp(0).unwrap(),
        };
        second_request.event = &other;
        assert_ne!(
            first.idempotency_key,
            runner.run(&second_request).await.idempotency_key
        );
    }
}
