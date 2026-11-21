#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The conformance corpus runner (WP-UI-12.T1).
//!
//! Every file in `conformance/forms/` is a self-contained case: a metadata fragment, a document,
//! an event, TypeScript source, and the effects, diff, problems and final state the event must
//! answer. The cases run on the **real** script engine (QuickJS), so the corpus is the executable
//! specification the C# client (WP-UI-12.T2) reads.
//!
//! Each file opens with shared `[meta]`, `[children]` and `[doc]` fragments; a case inherits them
//! and may override `meta`, add a child DocType or add document members. A case is therefore
//! self-contained given its file, without repeating 30 lines of metadata 70 times.
//!
//! The `state` snapshot is generated from the case's expected effects and diff: it is what the
//! client must show. Regenerate it after changing a case with
//! `cargo test -p spot-form-effects --test conformance -- --ignored regenerate_states`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Map, Value as Json, json};
use spot_app_api::Document;
use spot_app_api::meta::{
    AttachFormat, DecimalFormat, DocTypeDef, FieldDef, FieldKind, IntFormat, JsonFormat,
    LongTextFormat, TextFormat,
};
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId};
use spot_form_effects::{
    DialogRegistry, Effect, EffectBatch, EffectProblem, FormEvent, FormMeta, FormScript,
    FormScriptRunner, FormScriptSet, FormSecurity, FormState, MapMeta, RunRequest, StaticDialogs,
    StaticSecurity, document_for, id_for,
};
use spot_permissions::{DocFacts, Principal};

// --------------------------------------------------------------------------------------------
// The corpus's shape

#[derive(Deserialize)]
struct CorpusFile {
    /// The file's shared metadata fragment; a case may override it.
    #[serde(default)]
    meta: Option<MetaCase>,
    /// The file's shared child DocTypes; a case may add or override one.
    #[serde(default)]
    children: BTreeMap<String, MetaCase>,
    /// The file's shared document; a case's members override its members.
    #[serde(default)]
    doc: BTreeMap<String, Json>,
    #[serde(default)]
    case: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    id: String,
    #[serde(default)]
    description: String,
    /// Overrides the file's metadata fragment when present.
    #[serde(default)]
    meta: Option<MetaCase>,
    /// Adds to, or overrides, the file's child DocTypes.
    #[serde(default)]
    children: BTreeMap<String, MetaCase>,
    #[serde(default)]
    doc: BTreeMap<String, Json>,
    event: EventCase,
    #[serde(default)]
    script: Vec<ScriptCase>,
    #[serde(default)]
    security: SecurityCase,
    #[serde(default)]
    dialogs: Vec<String>,
    #[serde(default)]
    server_patch: Json,
    #[serde(default)]
    expect: Expect,
}

#[derive(Clone, Deserialize)]
struct MetaCase {
    name: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    is_posted: bool,
    #[serde(default)]
    fields: Vec<FieldCase>,
}

#[derive(Clone, Deserialize)]
struct FieldCase {
    name: String,
    kind: String,
    #[serde(default)]
    scale: u8,
    #[serde(default)]
    precision: u8,
    #[serde(default)]
    max_chars: Option<u32>,
    #[serde(default)]
    values: Vec<String>,
    #[serde(default)]
    doctype: Option<String>,
    #[serde(default)]
    type_field: Option<String>,
    #[serde(default)]
    link_field: Option<String>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    multiple: bool,
}

#[derive(Deserialize)]
struct EventCase {
    kind: String,
    #[serde(default)]
    field: Option<String>,
    #[serde(default)]
    action: Option<String>,
}

#[derive(Deserialize)]
struct ScriptCase {
    name: String,
    /// The script's registration; defaults to the case's own event.
    #[serde(default)]
    event: Option<EventCase>,
    #[serde(default)]
    priority: i64,
    #[serde(default)]
    timeout_ms: u64,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
    source: String,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Deserialize, Default)]
struct SecurityCase {
    #[serde(default)]
    hidden: Vec<String>,
    #[serde(default)]
    read_only: Vec<String>,
}

#[derive(Deserialize, Default)]
struct Expect {
    #[serde(default)]
    effects: Vec<Effect>,
    #[serde(default)]
    patch: Json,
    #[serde(default)]
    problem: Vec<ProblemCase>,
    #[serde(default)]
    errors: Vec<String>,
    #[serde(default)]
    state: FormState,
}

#[derive(Deserialize)]
struct ProblemCase {
    code: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    index: Option<usize>,
}

// --------------------------------------------------------------------------------------------
// Building a case

/// The built inputs of one case.
struct Fixture {
    def: Arc<DocTypeDef>,
    meta: Arc<MapMeta>,
    doc: Document,
    scripts: FormScriptSet,
    security: Arc<StaticSecurity>,
    dialogs: Arc<StaticDialogs>,
}

fn build(file: &CorpusFile, case: &Case) -> Fixture {
    let meta_case = case
        .meta
        .as_ref()
        .or(file.meta.as_ref())
        .expect("the file or the case provides metadata");
    let def = Arc::new(doctype_of(meta_case, false));
    let mut meta = MapMeta::new();
    meta.insert_shared(Arc::clone(&def));
    let mut children = file.children.clone();
    children.extend(case.children.clone());
    for (name, child) in &children {
        let mut child = doctype_of(child, true);
        if child.name.as_str() != name {
            child = DocTypeDef::new(
                Name::new(name).expect("a child name"),
                child.label.clone(),
                child.is_posted,
                true,
                child.title_field.clone(),
                child.fields.clone(),
            );
        }
        meta.insert(child);
    }
    let mut doc_values = file.doc.clone();
    doc_values.extend(case.doc.clone());
    let values = Json::Object(doc_values.into_iter().collect());
    let doc = document_for(&def, &values, id_for(&case.id)).expect("the corpus document fits");
    let scripts = FormScriptSet::new(case.script.iter().map(|script| {
        let event = script
            .event
            .as_ref()
            .map_or_else(|| event_of(&case.event), event_of);
        FormScript::new(&script.name, def.name.as_str(), event, &script.source)
            .with_priority(script.priority)
            .with_timeout(script.timeout_ms)
            .enabled(script.enabled)
    }))
    .expect("the corpus scripts are valid");
    let mut security = StaticSecurity::new();
    security.hide_all(case.security.hidden.iter().cloned());
    security.read_only_all(case.security.read_only.iter().cloned());
    Fixture {
        def,
        meta: Arc::new(meta),
        doc,
        scripts,
        security: Arc::new(security),
        dialogs: Arc::new(StaticDialogs::new(case.dialogs.iter().cloned())),
    }
}

fn doctype_of(case: &MetaCase, is_child: bool) -> DocTypeDef {
    DocTypeDef::new(
        Name::new(&case.name).expect("a DocType name"),
        case.label.clone().unwrap_or_else(|| case.name.clone()),
        case.is_posted,
        is_child,
        None,
        case.fields.iter().map(field_of).collect(),
    )
}

fn field_of(case: &FieldCase) -> FieldDef {
    let name = Name::new(&case.name).expect("a field name");
    let kind = match case.kind.as_str() {
        "bool" => FieldKind::Bool,
        "int" => FieldKind::Int {
            format: IntFormat::Plain,
        },
        "decimal" => FieldKind::Decimal {
            scale: if case.scale == 0 { 2 } else { case.scale },
            precision: if case.precision == 0 {
                18
            } else {
                case.precision
            },
            format: DecimalFormat::Plain,
        },
        "text" => FieldKind::Text {
            max_chars: case.max_chars,
            format: TextFormat::Plain,
        },
        "long_text" => FieldKind::LongText {
            format: LongTextFormat::Plain,
        },
        "select" => FieldKind::Select {
            values: case.values.clone(),
        },
        "date" => FieldKind::Date,
        "time" => FieldKind::Time,
        "datetime" => FieldKind::DateTime,
        "link" => FieldKind::Link {
            doctype: Name::new(case.doctype.as_deref().unwrap_or("link")).expect("a link DocType"),
        },
        "dynamic_link" => FieldKind::DynamicLink {
            type_field: Name::new(case.type_field.as_deref().unwrap_or("type"))
                .expect("a type field"),
        },
        "json" => FieldKind::Json {
            format: JsonFormat::Plain,
        },
        "attach" => FieldKind::Attach {
            format: AttachFormat::File,
        },
        "secret" => FieldKind::Secret,
        "table" => FieldKind::Table {
            doctype: Name::new(case.doctype.as_deref().unwrap_or("row")).expect("a table DocType"),
        },
        "table_multiselect" => FieldKind::TableMultiselect {
            doctype: Name::new(case.doctype.as_deref().unwrap_or("row")).expect("a table DocType"),
            link_field: Name::new(case.link_field.as_deref().unwrap_or("link"))
                .expect("a link field"),
        },
        other => panic!("unknown corpus field kind `{other}`"),
    };
    let mut field = FieldDef::new(name, kind, case.required, false);
    field.read_only = case.read_only;
    field.multiple = case.multiple;
    field
}

fn event_of(case: &EventCase) -> FormEvent {
    match case.kind.as_str() {
        "load" => FormEvent::Load,
        "change" => FormEvent::Change {
            field: case.field.clone().expect("a change case names its field"),
        },
        "save" => FormEvent::Save,
        "action" => FormEvent::Action {
            action: case
                .action
                .clone()
                .expect("an action case names its action"),
        },
        other => panic!("unknown corpus event `{other}`"),
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("a fixed instant")
}

/// The state the case's expected effects and diff must produce.
fn expected_state(file: &CorpusFile, case: &Case) -> FormState {
    let fixture = build(file, case);
    let mut state = FormState::from_document(&fixture.doc);
    state.apply_round(
        &merge(&patch_of(&case.server_patch), &case.expect.patch),
        &case.expect.effects,
    );
    state
}

fn patch_of(patch: &Json) -> Json {
    if patch.is_null() {
        Json::Object(Map::new())
    } else {
        patch.clone()
    }
}

fn effect_kinds(effects: &[Effect]) -> BTreeSet<&'static str> {
    let mut kinds = BTreeSet::new();
    for effect in effects {
        match effect.flatten() {
            Some(flat) => {
                for nested in flat {
                    kinds.insert(nested.kind());
                }
            }
            None => {
                kinds.insert(effect.kind());
            }
        }
    }
    kinds
}

fn problems_match(expected: &[ProblemCase], actual: &[EffectProblem]) -> bool {
    expected.len() == actual.len()
        && expected.iter().zip(actual).all(|(want, got)| {
            want.code == got.code
                && want.path == got.path
                && want.index.is_none_or(|index| got.index == Some(index))
        })
}

fn merge(server: &Json, script: &Json) -> Json {
    let mut merged = server.as_object().cloned().unwrap_or_default();
    if let Some(object) = script.as_object() {
        for (key, value) in object {
            merged.insert(key.clone(), value.clone());
        }
    }
    Json::Object(merged)
}

// --------------------------------------------------------------------------------------------
// Running the corpus

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("conformance/forms")
}

fn corpus_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(corpus_dir())
        .unwrap_or_else(|e| panic!("{}: {e}", corpus_dir().display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();
    files
}

fn load() -> Vec<(String, CorpusFile)> {
    corpus_files()
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let file: CorpusFile =
                toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            (
                path.file_name()
                    .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
                file,
            )
        })
        .collect()
}

async fn run_case(file: &CorpusFile, case: &Case) -> spot_form_effects::FormOutcome {
    let fixture = build(file, case);
    let event = event_of(&case.event);
    let runner = FormScriptRunner::new(
        fixture.meta.clone() as Arc<dyn FormMeta>,
        fixture.security.clone() as Arc<dyn FormSecurity>,
        fixture.dialogs.clone() as Arc<dyn DialogRegistry>,
    );
    let principal = Principal::system();
    let facts = DocFacts {
        doctype: fixture.def.name.as_str().to_owned(),
        ..DocFacts::default()
    };
    let request = RunRequest {
        def: Arc::clone(&fixture.def),
        document: &fixture.doc,
        event: &event,
        scripts: &fixture.scripts,
        principal: &principal,
        facts: &facts,
        server_patch: &patch_of(&case.server_patch),
        now: now(),
    };
    runner.run(&request).await
}

fn check(file: &CorpusFile, case: &Case, outcome: &spot_form_effects::FormOutcome) -> Vec<String> {
    let mut failures = Vec::new();
    let expected = EffectBatch::new(case.expect.effects.clone());
    if outcome.batch != expected {
        failures.push(format!(
            "effects differ\n  want {}\n  got  {}",
            serde_json::to_string(&expected.effects).unwrap_or_default(),
            serde_json::to_string(&outcome.batch.effects).unwrap_or_default()
        ));
    }
    if outcome.diff != patch_of(&case.expect.patch) {
        failures.push(format!(
            "diff differs\n  want {}\n  got  {}",
            patch_of(&case.expect.patch),
            outcome.diff
        ));
    }
    let merged = merge(&patch_of(&case.server_patch), &case.expect.patch);
    if outcome.merged_diff != merged {
        failures.push(format!(
            "merged diff differs\n  want {merged}\n  got  {}",
            outcome.merged_diff
        ));
    }
    if !problems_match(&case.expect.problem, &outcome.problems) {
        failures.push(format!(
            "problems differ\n  want {:?}\n  got  {:?}",
            case.expect
                .problem
                .iter()
                .map(|p| (&p.code, &p.path, p.index))
                .collect::<Vec<_>>(),
            outcome
                .problems
                .iter()
                .map(|p| (&p.code, &p.path, p.index))
                .collect::<Vec<_>>()
        ));
    }
    let errors: Vec<String> = outcome.errors.iter().map(|e| e.code.clone()).collect();
    if errors != case.expect.errors {
        failures.push(format!(
            "errors differ\n  want {:?}\n  got  {errors:?}",
            case.expect.errors
        ));
    }
    let want_state = expected_state(file, case);
    if want_state != case.expect.state {
        failures.push(format!(
            "the checked-in state snapshot is stale: run the `regenerate_states` ignored test\n  want {}\n  checked in {}",
            toml::to_string(&StateOnly { state: &want_state }).unwrap_or_default(),
            toml::to_string(&StateOnly {
                state: &case.expect.state
            })
            .unwrap_or_default()
        ));
    }
    let fixture = build(file, case);
    let mut actual_state = FormState::from_document(&fixture.doc);
    actual_state.apply_round(&outcome.merged_diff, &outcome.batch.effects);
    if actual_state != case.expect.state {
        failures.push(format!(
            "applying the actual diff and effects gives a different state\n  want {}\n  got  {}",
            toml::to_string(&StateOnly {
                state: &case.expect.state
            })
            .unwrap_or_default(),
            toml::to_string(&StateOnly {
                state: &actual_state
            })
            .unwrap_or_default()
        ));
    }
    failures
}

#[tokio::test]
async fn every_case_of_the_corpus_passes() {
    let mut failures = Vec::new();
    let mut total = 0;
    for (file, corpus) in load() {
        for case in &corpus.case {
            total += 1;
            let outcome = run_case(&corpus, case).await;
            for why in check(&corpus, case, &outcome) {
                failures.push(format!("{file} {}: {why}", case.id));
            }
        }
    }
    assert!(total >= 40, "the corpus has only {total} cases");
    assert!(
        failures.is_empty(),
        "{} case(s) failed:\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[tokio::test]
async fn the_corpus_is_deterministic() {
    for (file, corpus) in load() {
        for case in &corpus.case {
            let first = run_case(&corpus, case).await;
            let second = run_case(&corpus, case).await;
            assert_eq!(first, second, "{file} {}: two runs differ", case.id);
        }
    }
}

#[test]
fn the_corpus_is_well_formed() {
    let mut ids = BTreeSet::new();
    let mut kinds = BTreeSet::new();
    let mut uses_batch = false;
    let mut total = 0;
    for (file, corpus) in load() {
        for case in &corpus.case {
            total += 1;
            assert!(
                ids.insert(case.id.clone()),
                "{file}: duplicate case id {}",
                case.id
            );
            assert!(
                !case.description.trim().is_empty(),
                "{file} {}: needs a description",
                case.id
            );
            assert!(
                !case.script.is_empty(),
                "{file} {}: needs at least one script",
                case.id
            );
            kinds.extend(effect_kinds(&case.expect.effects));
            uses_batch |= case.script.iter().any(|s| s.source.contains(".batch("));
            for script in &case.script {
                assert!(
                    script.source.contains("export function on_"),
                    "{file} {}: script {} exports no handler",
                    case.id,
                    script.name
                );
            }
        }
    }
    assert!(total >= 40, "the corpus has only {total} cases");
    assert!(uses_batch, "no case exercises the batch effect");
    for kind in Effect::ALL_KINDS {
        if kind == "batch" {
            continue;
        }
        assert!(kinds.contains(kind), "no case expects a `{kind}` effect");
    }
}

// --------------------------------------------------------------------------------------------
// Regenerating the state snapshots

#[derive(serde::Serialize)]
struct StateOnly<'a> {
    state: &'a FormState,
}

#[test]
#[ignore = "rewrites every case's state snapshot"]
fn regenerate_states() {
    for path in corpus_files() {
        let text = std::fs::read_to_string(&path).expect("readable");
        let corpus: CorpusFile = toml::from_str(&text).expect("parses");
        let mut parts: Vec<String> = text.split("[[case]]").map(str::to_owned).collect();
        assert_eq!(parts.len(), corpus.case.len() + 1, "{}", path.display());
        for (index, case) in corpus.case.iter().enumerate() {
            let block = &mut parts[index + 1];
            // The state section is always written last; drop an older one.
            if let Some(at) = block.find("\n[case.expect.state") {
                block.truncate(at + 1);
            }
            let state = expected_state(&corpus, case);
            let snippet = toml::to_string(&StateOnly { state: &state }).expect("serialises");
            let snippet = snippet.replace("[state", "[case.expect.state");
            if !block.ends_with('\n') {
                block.push('\n');
            }
            block.push('\n');
            block.push_str(&snippet);
        }
        std::fs::write(&path, parts.join("[[case]]")).expect("writable");
    }
}

/// Keeps the imports honest: these are the types the corpus builds.
#[allow(dead_code)]
fn type_check(record: RecordId, document: &Document, meta: &dyn FormMeta) -> Json {
    let _ = (record, document, meta);
    json!({})
}
