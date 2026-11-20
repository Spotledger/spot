//! The `API Script` and `Scheduled Script` records: read from documents with total validation.
//!
//! Every refusal is a `serverscript.record_invalid` field error naming the field. No message
//! carries script text, arguments or any other value the author typed (only names the author
//! chose for the script's own address are echoed, and only as a name, never a source line).

use serde_json::Value as Json;
use spot_app_api::error::FieldPath;
use spot_app_api::prelude::{RecordId, Value};
use spot_app_api::{AppError, AppResult, Document, ErrorClass};
use spot_functions::{HttpVerb, is_storage_name};
use spot_manifest::{Cron, Queue};
use spot_scheduler::TimeZone;

use crate::codes;
use crate::limits::ScriptLimits;
use crate::record::{
    FIELD_COMPILED_HASH, FIELD_DISABLED_REASON, FIELD_ENABLED, FIELD_NAME, FIELD_SCRIPT,
    FIELD_TIMEOUT_MS,
};

/// The app the two DocTypes (and every API script's function key) live in.
pub const SCRIPTING_APP: &str = "scripting";
/// The DocType of API scripts.
pub const API_SCRIPT_DOCTYPE: &str = "api_script";
/// The DocType of scheduled scripts.
pub const SCHEDULED_SCRIPT_DOCTYPE: &str = "scheduled_script";

/// The storage field holding an API script's module.
pub const FIELD_MODULE: &str = "module";
/// The storage field holding an API script's function name.
pub const FIELD_FUNCTION: &str = "function";
/// The storage field holding the verbs an API script answers.
pub const FIELD_VERBS: &str = "verbs";
/// The storage field holding an API script's argument schema (JSON text).
pub const FIELD_ARGUMENTS_SCHEMA: &str = "arguments_schema";
/// The storage field holding a scheduled script's cron expression.
pub const FIELD_CRON: &str = "cron";
/// The storage field holding a scheduled script's time zone.
pub const FIELD_TIME_ZONE: &str = "time_zone";
/// The storage field holding a scheduled script's missed-run policy.
pub const FIELD_MISSED_RUN: &str = "missed_run";
/// The storage field holding a scheduled script's queue.
pub const FIELD_QUEUE: &str = "queue";
/// The storage field holding the user a scheduled script runs as.
pub const FIELD_RUN_AS: &str = "run_as";

/// The verbs an API script answers, as the DocType's select offers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiVerbs {
    /// `GET` only.
    Get,
    /// `POST` only.
    Post,
    /// Both `GET` and `POST`.
    GetAndPost,
}

impl ApiVerbs {
    /// Reads the select option.
    pub fn parse(text: &str) -> Option<ApiVerbs> {
        match text {
            "GET" => Some(ApiVerbs::Get),
            "POST" => Some(ApiVerbs::Post),
            "GET and POST" => Some(ApiVerbs::GetAndPost),
            _ => None,
        }
    }

    /// The HTTP verbs the function declares.
    pub fn verbs(self) -> Vec<HttpVerb> {
        match self {
            ApiVerbs::Get => vec![HttpVerb::Get],
            ApiVerbs::Post => vec![HttpVerb::Post],
            ApiVerbs::GetAndPost => vec![HttpVerb::Get, HttpVerb::Post],
        }
    }
}

/// What a scheduled script does with runs missed while the scheduler was down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissedRun {
    /// Missed runs are skipped.
    Skip,
    /// Any number of missed runs collapse into one.
    RunOnce,
}

impl MissedRun {
    /// Reads the select option.
    pub fn parse(text: &str) -> Option<MissedRun> {
        match text {
            "skip" => Some(MissedRun::Skip),
            "run_once" => Some(MissedRun::RunOnce),
            _ => None,
        }
    }
}

/// One API script, read from its document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiScript {
    /// The record id, when the script came from a stored document.
    pub record_id: Option<RecordId>,
    /// The script's name (the document's `name`).
    pub name: String,
    /// The module part of the function key (a storage name).
    pub module: String,
    /// The function part of the function key (a storage name).
    pub function: String,
    /// The verbs the function answers.
    pub verbs: ApiVerbs,
    /// The argument schema, when the script declares one.
    pub arguments_schema: Option<Json>,
    /// The TypeScript source.
    pub source: String,
    /// The per-script timeout in milliseconds (0 means the default).
    pub timeout_ms: u64,
    /// Whether the script is served.
    pub enabled: bool,
    /// The compiled source hash, when the script has been saved once.
    pub compiled_hash: Option<String>,
    /// Why the script was disabled, when it was.
    pub disabled_reason: Option<String>,
    /// The elevation flag, when the script is elevated (WP-EXT-06.T8).
    pub elevation: Option<crate::elevation::ElevationFlag>,
}

impl crate::elevation::Elevatable for ApiScript {
    fn elevation(&self) -> Option<&crate::elevation::ElevationFlag> {
        self.elevation.as_ref()
    }
}

impl ApiScript {
    /// The function key `scripting/<module>/<function>`.
    pub fn key(&self) -> String {
        format!("{SCRIPTING_APP}/{}/{}", self.module, self.function)
    }

    /// Reads a script from an `API Script` document, under the default [`ScriptLimits`].
    ///
    /// # Errors
    ///
    /// `serverscript.record_invalid`, as field errors, for every field that is not usable.
    pub fn from_document(doc: &Document) -> AppResult<ApiScript> {
        ApiScript::from_document_with(doc, &ScriptLimits::default())
    }

    /// [`ApiScript::from_document`] under the site's `limits` (the timeout may not exceed their
    /// maximum).
    ///
    /// # Errors
    ///
    /// As [`ApiScript::from_document`].
    pub fn from_document_with(doc: &Document, limits: &ScriptLimits) -> AppResult<ApiScript> {
        let mut problems = Problems::default();
        let name = text(doc, FIELD_NAME).unwrap_or_default().to_owned();
        let module = text(doc, FIELD_MODULE).unwrap_or_default().to_owned();
        let function = text(doc, FIELD_FUNCTION).unwrap_or_default().to_owned();
        let verbs_text = text(doc, FIELD_VERBS).unwrap_or_default();
        let source = text(doc, FIELD_SCRIPT).unwrap_or_default().to_owned();

        if name.trim().is_empty() {
            problems.add(FIELD_NAME, "An API Script needs a name.");
        }
        if !is_storage_name(&module) {
            problems.add(FIELD_MODULE, "The module must be a storage name.");
        }
        if !is_storage_name(&function) {
            problems.add(FIELD_FUNCTION, "The function must be a storage name.");
        }
        let verbs = ApiVerbs::parse(verbs_text);
        if verbs.is_none() {
            problems.add(
                FIELD_VERBS,
                "The verbs must be one of GET, POST, or GET and POST.",
            );
        }
        let arguments_schema = match text(doc, FIELD_ARGUMENTS_SCHEMA) {
            Some(raw) if !raw.trim().is_empty() => match serde_json::from_str::<Json>(raw) {
                Ok(schema) => match schema_shape(&schema) {
                    Ok(()) => Some(schema),
                    Err(why) => {
                        problems.add(FIELD_ARGUMENTS_SCHEMA, why);
                        None
                    }
                },
                Err(_) => {
                    problems.add(FIELD_ARGUMENTS_SCHEMA, "The argument schema is not JSON.");
                    None
                }
            },
            _ => None,
        };
        if source.trim().is_empty() {
            problems.add(FIELD_SCRIPT, "An API Script needs TypeScript source.");
        }
        let timeout_ms = timeout(doc, limits, &mut problems);
        problems.finish()?;
        let elevation = crate::elevation::read_flag(doc)?;

        Ok(ApiScript {
            record_id: doc.version().map(|_| doc.id()),
            name,
            module,
            function,
            verbs: verbs.unwrap_or(ApiVerbs::Post),
            arguments_schema,
            source,
            timeout_ms,
            enabled: matches!(doc.get(FIELD_ENABLED), Value::Bool(true)),
            compiled_hash: text(doc, FIELD_COMPILED_HASH).map(str::to_owned),
            disabled_reason: text(doc, FIELD_DISABLED_REASON).map(str::to_owned),
            elevation,
        })
    }
}

/// One scheduled script, read from its document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledScript {
    /// The record id, when the script came from a stored document.
    pub record_id: Option<RecordId>,
    /// The script's name (the document's `name`); also the schedule's name.
    pub name: String,
    /// When it fires: six fields, seconds first.
    pub cron: Cron,
    /// The IANA zone name the cron fields are read in.
    pub time_zone: String,
    /// What to do with missed runs.
    pub missed_run: MissedRun,
    /// The queue the run goes on.
    pub queue: Queue,
    /// The user the script runs as.
    pub run_as: String,
    /// The TypeScript source.
    pub source: String,
    /// The per-script timeout in milliseconds (0 means the default).
    pub timeout_ms: u64,
    /// Whether the script is scheduled.
    pub enabled: bool,
    /// The compiled source hash, when the script has been saved once.
    pub compiled_hash: Option<String>,
    /// Why the script was disabled, when it was.
    pub disabled_reason: Option<String>,
}

impl ScheduledScript {
    /// Reads a script from a `Scheduled Script` document, under the default [`ScriptLimits`].
    ///
    /// # Errors
    ///
    /// `serverscript.record_invalid`, as field errors, for every field that is not usable.
    pub fn from_document(doc: &Document) -> AppResult<ScheduledScript> {
        ScheduledScript::from_document_with(doc, &ScriptLimits::default())
    }

    /// [`ScheduledScript::from_document`] under the site's `limits`.
    ///
    /// # Errors
    ///
    /// As [`ScheduledScript::from_document`].
    pub fn from_document_with(doc: &Document, limits: &ScriptLimits) -> AppResult<ScheduledScript> {
        let mut problems = Problems::default();
        let name = text(doc, FIELD_NAME).unwrap_or_default().to_owned();
        let cron_text = text(doc, FIELD_CRON).unwrap_or_default();
        let zone = non_empty(text(doc, FIELD_TIME_ZONE))
            .unwrap_or("UTC")
            .to_owned();
        let missed_text = non_empty(text(doc, FIELD_MISSED_RUN)).unwrap_or("skip");
        let queue_text = non_empty(text(doc, FIELD_QUEUE)).unwrap_or("default");
        let run_as = text(doc, FIELD_RUN_AS)
            .unwrap_or_default()
            .trim()
            .to_owned();
        let source = text(doc, FIELD_SCRIPT).unwrap_or_default().to_owned();

        if name.trim().is_empty() {
            problems.add(FIELD_NAME, "A Scheduled Script needs a name.");
        }
        let cron = match Cron::parse(cron_text) {
            Ok(cron) => Some(cron),
            Err(_) => {
                problems.add(
                    FIELD_CRON,
                    "The cron expression needs six fields, seconds first, in the usual syntax.",
                );
                None
            }
        };
        if TimeZone::parse(&zone).is_err() {
            problems.add(
                FIELD_TIME_ZONE,
                "The time zone is not a known IANA zone name.",
            );
        }
        let missed_run = MissedRun::parse(missed_text);
        if missed_run.is_none() {
            problems.add(
                FIELD_MISSED_RUN,
                "The missed-run policy must be skip or run_once.",
            );
        }
        let queue = match queue_text {
            "default" => Some(Queue::Default),
            "short" => Some(Queue::Short),
            "long" => Some(Queue::Long),
            _ => {
                problems.add(FIELD_QUEUE, "The queue must be default, short or long.");
                None
            }
        };
        if run_as.is_empty() {
            problems.add(
                FIELD_RUN_AS,
                "A Scheduled Script runs as a named user; set run_as.",
            );
        }
        if source.trim().is_empty() {
            problems.add(FIELD_SCRIPT, "A Scheduled Script needs TypeScript source.");
        }
        let timeout_ms = timeout(doc, limits, &mut problems);
        if matches!(doc.get(crate::elevation::FIELD_ELEVATED), Value::Bool(true)) {
            problems.add_with(
                crate::elevation::FIELD_ELEVATED,
                codes::ELEVATION_NOT_ALLOWED,
                "A scheduled script runs as its run_as user and cannot be elevated.",
            );
        }
        problems.finish()?;

        // Every `Option` below was checked above: a `None` here would already have produced a
        // problem, so the fallbacks are never taken.
        let (Some(cron), Some(missed_run), Some(queue)) = (cron, missed_run, queue) else {
            return Err(AppError::new(
                ErrorClass::Internal,
                codes::RECORD_INVALID,
                "the record was checked above",
            ));
        };
        Ok(ScheduledScript {
            record_id: doc.version().map(|_| doc.id()),
            name,
            cron,
            time_zone: zone,
            missed_run,
            queue,
            run_as,
            source,
            timeout_ms,
            enabled: matches!(doc.get(FIELD_ENABLED), Value::Bool(true)),
            compiled_hash: text(doc, FIELD_COMPILED_HASH).map(str::to_owned),
            disabled_reason: text(doc, FIELD_DISABLED_REASON).map(str::to_owned),
        })
    }
}

/// The accumulated field problems of one record, reported together.
#[derive(Default)]
struct Problems(Vec<(&'static str, &'static str, String)>);

impl Problems {
    fn add(&mut self, field: &'static str, message: impl Into<String>) {
        self.0.push((field, codes::RECORD_INVALID, message.into()));
    }

    fn add_with(&mut self, field: &'static str, code: &'static str, message: impl Into<String>) {
        self.0.push((field, code, message.into()));
    }

    fn finish(self) -> AppResult<()> {
        let mut rest = self.0.into_iter();
        let Some((field, code, message)) = rest.next() else {
            return Ok(());
        };
        let mut error = AppError::field(FieldPath::field(field), code, message);
        for (field, code, message) in rest {
            error = error.with_field(FieldPath::field(field), code, message);
        }
        Err(error)
    }
}

fn timeout(doc: &Document, limits: &ScriptLimits, problems: &mut Problems) -> u64 {
    match doc.get(FIELD_TIMEOUT_MS) {
        Value::Int(ms) if *ms < 0 => {
            problems.add(FIELD_TIMEOUT_MS, "The timeout cannot be negative.");
            0
        }
        Value::Int(ms) => {
            let ms = u64::try_from(*ms).unwrap_or(0);
            if ms > limits.max_timeout_ms {
                problems.add(
                    FIELD_TIMEOUT_MS,
                    format!(
                        "The timeout may not exceed the site maximum of {} ms.",
                        limits.max_timeout_ms
                    ),
                );
            }
            ms
        }
        _ => limits.default_timeout_ms,
    }
}

/// Checks that a schema is in the subset `spot_functions::validate_arguments` understands:
/// objects with `type` (a known name), `required` (names) and `properties` (schemas).
fn schema_shape(schema: &Json) -> Result<(), String> {
    let Some(object) = schema.as_object() else {
        return Err("A schema node must be a JSON object.".to_owned());
    };
    if let Some(kind) = object.get("type") {
        let known = [
            "object", "array", "string", "integer", "number", "boolean", "null",
        ];
        match kind.as_str() {
            Some(name) if known.contains(&name) => {}
            _ => return Err("A schema `type` must be one of the known type names.".to_owned()),
        }
    }
    if let Some(required) = object.get("required")
        && !required
            .as_array()
            .is_some_and(|names| names.iter().all(Json::is_string))
    {
        return Err("A schema `required` must be a list of property names.".to_owned());
    }
    if let Some(properties) = object.get("properties") {
        let Some(properties) = properties.as_object() else {
            return Err("A schema `properties` must be an object.".to_owned());
        };
        for node in properties.values() {
            schema_shape(node)?;
        }
    }
    Ok(())
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
}

pub(crate) fn text<'a>(doc: &'a Document, field: &str) -> Option<&'a str> {
    match doc.get(field) {
        Value::Text(t) => Some(t.as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_app_api::prelude::{Name, Version};
    use std::collections::BTreeMap;

    fn doc_with(doctype: &str, fields: &[(&str, Value)]) -> Document {
        let map: BTreeMap<Name, Value> = fields
            .iter()
            .map(|(k, v)| (Name::new(k).expect("a name"), v.clone()))
            .collect();
        Document::loaded(
            Name::new(doctype).expect("a name"),
            RecordId::new_v7(),
            Version::FIRST,
            None,
            map,
            BTreeMap::new(),
        )
    }

    fn t(s: &str) -> Value {
        Value::Text(s.to_owned())
    }

    fn api_fields() -> Vec<(&'static str, Value)> {
        vec![
            (FIELD_NAME, t("double")),
            (FIELD_MODULE, t("maths")),
            (FIELD_FUNCTION, t("double")),
            (FIELD_VERBS, t("GET and POST")),
            (FIELD_SCRIPT, t("export function on_call() { return {}; }")),
            (FIELD_ENABLED, Value::Bool(true)),
            (FIELD_TIMEOUT_MS, Value::Int(300)),
        ]
    }

    fn with(
        base: Vec<(&'static str, Value)>,
        field: &'static str,
        value: Value,
    ) -> Vec<(&'static str, Value)> {
        let mut out: Vec<_> = base.into_iter().filter(|(k, _)| *k != field).collect();
        out.push((field, value));
        out
    }

    fn scheduled_fields() -> Vec<(&'static str, Value)> {
        vec![
            (FIELD_NAME, t("nightly")),
            (FIELD_CRON, t("0 0 2 * * *")),
            (FIELD_TIME_ZONE, t("Asia/Karachi")),
            (FIELD_MISSED_RUN, t("run_once")),
            (FIELD_QUEUE, t("long")),
            (FIELD_RUN_AS, t("batch_user")),
            (FIELD_SCRIPT, t("export function on_run() {}")),
            (FIELD_ENABLED, Value::Bool(true)),
        ]
    }

    #[test]
    fn an_api_record_reads_back() {
        let doc = doc_with(API_SCRIPT_DOCTYPE, &api_fields());
        let script = ApiScript::from_document(&doc).expect("valid");
        assert_eq!(script.key(), "scripting/maths/double");
        assert_eq!(script.verbs, ApiVerbs::GetAndPost);
        assert_eq!(script.verbs.verbs(), vec![HttpVerb::Get, HttpVerb::Post]);
        assert_eq!(script.timeout_ms, 300);
        assert!(script.enabled);
        assert!(script.arguments_schema.is_none());
    }

    #[test]
    fn an_api_record_with_a_schema_reads_it() {
        let fields = with(
            api_fields(),
            FIELD_ARGUMENTS_SCHEMA,
            t(r#"{"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}}"#),
        );
        let script = ApiScript::from_document(&doc_with(API_SCRIPT_DOCTYPE, &fields)).unwrap();
        assert!(script.arguments_schema.is_some());
    }

    #[test]
    fn every_api_validation_failure_names_its_field() {
        let cases: Vec<(&'static str, Value, &'static str)> = vec![
            (FIELD_NAME, t("  "), FIELD_NAME),
            (FIELD_MODULE, t("Bad Module"), FIELD_MODULE),
            (FIELD_FUNCTION, t(""), FIELD_FUNCTION),
            (FIELD_VERBS, t("PUT"), FIELD_VERBS),
            (
                FIELD_ARGUMENTS_SCHEMA,
                t("not json"),
                FIELD_ARGUMENTS_SCHEMA,
            ),
            (
                FIELD_ARGUMENTS_SCHEMA,
                t(r#"{"type":"banana"}"#),
                FIELD_ARGUMENTS_SCHEMA,
            ),
            (
                FIELD_ARGUMENTS_SCHEMA,
                t(r#"{"required":"n"}"#),
                FIELD_ARGUMENTS_SCHEMA,
            ),
            (FIELD_ARGUMENTS_SCHEMA, t("[1]"), FIELD_ARGUMENTS_SCHEMA),
            (FIELD_SCRIPT, t(" "), FIELD_SCRIPT),
            (FIELD_TIMEOUT_MS, Value::Int(5_000), FIELD_TIMEOUT_MS),
            (FIELD_TIMEOUT_MS, Value::Int(-1), FIELD_TIMEOUT_MS),
        ];
        for (field, value, expected) in cases {
            let fields = with(api_fields(), field, value);
            let error = ApiScript::from_document(&doc_with(API_SCRIPT_DOCTYPE, &fields))
                .expect_err("invalid");
            assert_eq!(error.fields.len(), 1, "{field}");
            assert_eq!(error.fields[0].code, codes::RECORD_INVALID);
            assert_eq!(error.fields[0].field.to_string(), expected);
        }
    }

    #[test]
    fn an_api_error_never_carries_the_source() {
        let fields = with(api_fields(), FIELD_ARGUMENTS_SCHEMA, t("SECRET-TEXT {{"));
        let error = ApiScript::from_document(&doc_with(API_SCRIPT_DOCTYPE, &fields)).unwrap_err();
        let rendered = format!("{error:?}");
        assert!(!rendered.contains("SECRET-TEXT"));
    }

    #[test]
    fn a_scheduled_record_reads_back() {
        let doc = doc_with(SCHEDULED_SCRIPT_DOCTYPE, &scheduled_fields());
        let script = ScheduledScript::from_document(&doc).expect("valid");
        assert_eq!(script.cron.as_str(), "0 0 2 * * *");
        assert_eq!(script.time_zone, "Asia/Karachi");
        assert_eq!(script.missed_run, MissedRun::RunOnce);
        assert_eq!(script.queue, Queue::Long);
        assert_eq!(script.run_as, "batch_user");
    }

    #[test]
    fn a_scheduled_record_defaults_zone_policy_and_queue() {
        let fields: Vec<_> = scheduled_fields()
            .into_iter()
            .filter(|(k, _)| ![FIELD_TIME_ZONE, FIELD_MISSED_RUN, FIELD_QUEUE].contains(k))
            .collect();
        let script =
            ScheduledScript::from_document(&doc_with(SCHEDULED_SCRIPT_DOCTYPE, &fields)).unwrap();
        assert_eq!(script.time_zone, "UTC");
        assert_eq!(script.missed_run, MissedRun::Skip);
        assert_eq!(script.queue, Queue::Default);
    }

    #[test]
    fn every_scheduled_validation_failure_names_its_field() {
        let cases: Vec<(&'static str, Value)> = vec![
            (FIELD_NAME, t("")),
            (FIELD_CRON, t("* * * *")),
            (FIELD_CRON, t("")),
            (FIELD_TIME_ZONE, t("Mars/Olympus")),
            (FIELD_MISSED_RUN, t("catch_up")),
            (FIELD_QUEUE, t("urgent")),
            (FIELD_RUN_AS, t("   ")),
            (FIELD_SCRIPT, t("")),
            (FIELD_TIMEOUT_MS, Value::Int(10_000)),
        ];
        for (field, value) in cases {
            let fields = with(scheduled_fields(), field, value);
            let error =
                ScheduledScript::from_document(&doc_with(SCHEDULED_SCRIPT_DOCTYPE, &fields))
                    .expect_err("invalid");
            assert_eq!(error.fields.len(), 1, "{field}");
            assert_eq!(error.fields[0].code, codes::RECORD_INVALID);
            assert_eq!(error.fields[0].field.to_string(), field);
        }
    }

    #[test]
    fn a_broken_record_reports_every_problem_at_once() {
        let doc = doc_with(SCHEDULED_SCRIPT_DOCTYPE, &[(FIELD_NAME, t(""))]);
        let error = ScheduledScript::from_document(&doc).expect_err("invalid");
        assert!(error.fields.len() >= 3);
    }
}
