//! `spot rules check|eval|explain`: write a rule file, check it, try it and read its structure
//! before it is installed (WP-GRA-04.T3). Guide: `docs/running-spot/rules_command.md`.
//!
//! - `check FILE [--site S | --app DIR]` compiles the file against effective metadata and prints
//!   every problem as `FILE:LINE:COL: code: message`, sorted by position. With `--site` the
//!   metadata is the site's (apps, recorded runtime changes and the customisation records the
//!   database holds, read only); with `--app` it is the platform plus that app directory's DocType
//!   files (no database); with neither, the platform's.
//! - `eval FILE --facts FACTS.json --query 'predicate(a, _)'` evaluates over facts held in memory.
//!   It opens no database connection.
//! - `explain FILE` prints the derived predicates, their strata and what each depends on (through
//!   negation or aggregation marked), and the rules nothing query-able reaches. It opens no
//!   database connection either: with `--site` it reads the site's files only.
//!
//! A rule only derives and reads; nothing here writes anything. Rule text and fact values are never
//! echoed beyond the one source line a problem names (cut at 200 characters).
//!
//! # The fact file
//!
//! One JSON object: a predicate name to an array of tuples. An edge type (`employee.manager`) takes
//! `[holder, target]`; a field predicate (`employee.can_approve`) takes `[record, value]`.
//! A record is a string `"entity#N"` (a small test id, `N` from 0 to 65535) or `"entity:UUID"` (a
//! UUIDv7), or `{"record": "..."}`. Values: a string is text, a JSON integer an integer, `true` /
//! `false` a boolean, `null` null, `{"decimal": "12.50"}` an exact decimal. Facts for a predicate
//! the program does not read are ignored.
//!
//! Exit codes: 0 done; 1 usage; 2 refused (problems in the rule file, fuel exhausted, a fact file
//! over the limits or not valid); 3 failed (a file or the metadata cannot be read).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, Command};
use rust_decimal::Decimal;
use spot_customise::StorageRecordStore;
use spot_customise::records::{RecordState, RecordStore};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, Property, Registry};
use spot_rule_graph::ast::{HeadArg, Literal, Rule};
use spot_rule_graph::{
    Compiled, DEFAULT_FUEL, Datum, Engine, MemoryFacts, PredKind, Problem, Query, RuleGraphError,
    RuleSet,
};
use spot_storage_contract::{Collation, Name, NodeRef, RecordId, Value};

use super::{CommandCtx, Exit};

pub const NAME: &str = "rules";

/// The most a rule file may weigh.
const MAX_RULE_BYTES: u64 = 1024 * 1024;
/// The most a fact file may weigh, checked before it is read.
const MAX_FACT_BYTES: u64 = 50 * 1024 * 1024;
/// The most tuples a fact file may hold.
const MAX_FACT_TUPLES: usize = 1_000_000;
/// A source line is shown cut at this many characters.
const LINE_CUT: usize = 200;

fn target_args(command: Command) -> Command {
    command
        .arg(
            Arg::new("site")
                .long("site")
                .value_name("SITE")
                .conflicts_with("app")
                .help("Validate against this site's effective metadata"),
        )
        .arg(
            Arg::new("app")
                .long("app")
                .value_name("DIR")
                .value_parser(clap::value_parser!(PathBuf))
                .help("Validate against the platform plus this app directory's DocType files"),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .action(ArgAction::SetTrue)
                .help("Print machine-readable JSON"),
        )
}

fn file_arg() -> Arg {
    Arg::new("file")
        .value_name("FILE")
        .required(true)
        .value_parser(clap::value_parser!(PathBuf))
        .help("The rule file")
}

pub fn command() -> Command {
    Command::new(NAME)
        .about("Check, try and explain a rule file before it is installed")
        .after_help(
            "Exit codes: 0 done, 1 usage, 2 refused (problems, fuel), 3 failed (unreadable).",
        )
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(target_args(
            Command::new("check")
                .about("Compile a rule file and print every problem at its line and column")
                .arg(file_arg()),
        ))
        .subcommand(target_args(
            Command::new("eval")
                .about("Evaluate a rule over a JSON fact file, with no database")
                .arg(file_arg())
                .arg(
                    Arg::new("facts")
                        .long("facts")
                        .value_name("FACTS.json")
                        .required(true)
                        .value_parser(clap::value_parser!(PathBuf))
                        .help("The facts: a predicate name to an array of tuples"),
                )
                .arg(
                    Arg::new("query")
                        .long("query")
                        .value_name("QUERY")
                        .required(true)
                        .help("`predicate` or `predicate(a, _)`: `_` matches anything"),
                )
                .arg(
                    Arg::new("fuel")
                        .long("fuel")
                        .value_name("N")
                        .value_parser(clap::value_parser!(u64))
                        .help("Derived-tuple limit (default 1,000,000; at most ten times that)"),
                ),
        ))
        .subcommand(target_args(
            Command::new("explain")
                .about("Print the strata, derived predicates and dependencies of a rule file")
                .arg(file_arg()),
        ))
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let outcome = match matches.subcommand() {
        Some(("check", sub)) => check(sub, ctx).await,
        Some(("eval", sub)) => eval(sub, ctx).await,
        Some(("explain", sub)) => explain(sub, ctx).await,
        Some((other, _)) => Err(Fail::Usage(format!("unknown rules command `{other}`"))),
        None => Err(Fail::Usage(
            "name a rules command: check, eval or explain".to_owned(),
        )),
    };
    match outcome {
        Ok(text) => {
            write!(ctx.out(), "{text}")?;
            Ok(Exit::Ok)
        }
        Err(Fail::Usage(message)) => {
            writeln!(ctx.err(), "spot rules: {message}")?;
            Ok(Exit::Usage)
        }
        Err(Fail::Refused { text, json }) => {
            if json {
                write!(ctx.out(), "{text}")?;
            } else {
                write!(ctx.err(), "{text}")?;
            }
            Ok(Exit::Refused)
        }
        Err(Fail::Failed(message)) => {
            writeln!(ctx.err(), "spot rules: {message}")?;
            Ok(Exit::Failed)
        }
    }
}

/// How a subcommand ended unhappily.
enum Fail {
    Usage(String),
    /// Exit 2; the text is the whole report (standard output when `json`).
    Refused {
        text: String,
        json: bool,
    },
    Failed(String),
}

impl Fail {
    fn refused(message: impl Into<String>) -> Self {
        Fail::Refused {
            text: format!("{}\n", message.into()),
            json: false,
        }
    }
}

type Outcome = Result<String, Fail>;

// --- reading files ---------------------------------------------------------------------------------

/// A plain file's bytes, within `limit`. A bigger file is refused without being read.
fn read_limited(path: &Path, limit: u64, what: &str) -> Result<Vec<u8>, Fail> {
    let meta = std::fs::metadata(path)
        .map_err(|e| Fail::Failed(format!("{} cannot be read: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(Fail::Failed(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > limit {
        return Err(Fail::refused(format!(
            "{what} {} is larger than the limit of {limit} bytes; it was not read",
            path.display()
        )));
    }
    std::fs::read(path).map_err(|e| Fail::Failed(format!("{} cannot be read: {e}", path.display())))
}

/// Refuses a symlink whose target is outside the working tree.
fn refuse_outside_symlink(path: &Path) -> Result<(), Fail> {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !meta.file_type().is_symlink() {
        return Ok(());
    }
    let here = std::env::current_dir()
        .and_then(std::fs::canonicalize)
        .map_err(|e| Fail::Failed(format!("the working directory cannot be resolved: {e}")))?;
    let target = std::fs::canonicalize(path)
        .map_err(|e| Fail::Failed(format!("{} cannot be resolved: {e}", path.display())))?;
    if target.starts_with(&here) {
        Ok(())
    } else {
        Err(Fail::refused(format!(
            "{} is a symlink to a place outside the working tree; it was not read",
            path.display()
        )))
    }
}

fn rule_text(path: &Path) -> Result<String, Fail> {
    let bytes = read_limited(path, MAX_RULE_BYTES, "the rule file")?;
    String::from_utf8(bytes)
        .map_err(|_| Fail::refused(format!("{} is not UTF-8 text", path.display())))
}

fn file_of(matches: &ArgMatches) -> Result<PathBuf, Fail> {
    matches
        .get_one::<PathBuf>("file")
        .cloned()
        .ok_or_else(|| Fail::Usage("a rule file is required".to_owned()))
}

// --- metadata ----------------------------------------------------------------------------------------

fn meta_error(errors: &[spot_meta::MetaError]) -> Fail {
    Fail::Failed(
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn failed(error: &spot_cli::Error) -> Fail {
    Fail::Failed(error.to_string())
}

fn counts(state: RecordState) -> bool {
    matches!(state, RecordState::Pending | RecordState::Applied)
}

/// The effective metadata a rule file is checked against. `database` reads the site's
/// customisation records (read only); without it a site's files are all that is read.
async fn effective_meta(
    ctx: &mut CommandCtx,
    site: Option<&str>,
    app: Option<&Path>,
    database: bool,
) -> Result<Arc<EffectiveMeta>, Fail> {
    if let Some(site) = site {
        return site_meta(ctx, site, database).await;
    }
    let mut definitions = spot_cli::appdir::platform_definitions().map_err(|e| failed(&e))?;
    if let Some(dir) = app {
        let source = spot_cli::appdir::load(dir).map_err(|e| failed(&e))?;
        if source.name != spot_cli::appdir::PLATFORM {
            definitions.extend(source.definitions);
        }
    }
    Registry::new(definitions)
        .map(|r| r.effective())
        .map_err(|errors| meta_error(&errors))
}

/// A site's effective metadata, built the way the host builds it: the installed apps, the custom
/// DocType records, the changes recorded in `metadata.json`, then the custom field and property
/// records.
async fn site_meta(
    ctx: &mut CommandCtx,
    site_name: &str,
    database: bool,
) -> Result<Arc<EffectiveMeta>, Fail> {
    let root = ctx.root().to_path_buf();
    let loaded = spot_cli::config::load(&root, false).map_err(|e| failed(&e))?;
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == site_name)
        .ok_or_else(|| Fail::Failed(format!("there is no site `{site_name}`")))?;
    let (mut definitions, record) =
        spot_cli::services::metadata::source(&root, &loaded, site).map_err(|e| failed(&e))?;
    let records = if database {
        let backend = ctx.backend(site_name).await.map_err(|e| failed(&e))?;
        let collation = Collation::new("und").map_err(|e| Fail::Failed(e.to_string()))?;
        let catalog = StorageRecordStore::<spot_postgres::PgBackend>::catalog(collation)
            .map_err(Fail::Failed)?;
        let store = StorageRecordStore::new(backend, Arc::new(catalog));
        Some(store.load().await.map_err(|e| {
            Fail::Failed(format!(
                "the site's customisation records cannot be read: {e}"
            ))
        })?)
    } else {
        None
    };
    if let Some(records) = &records {
        let base: BTreeSet<String> = definitions.iter().map(|d| d.name.clone()).collect();
        for doctype in records.doctypes.iter().filter(|r| counts(r.state)) {
            let mut definition =
                Definition::parse(&format!("{}.toml", doctype.name), &doctype.definition)
                    .map_err(|e| Fail::Failed(format!("custom DocType `{}`: {e}", doctype.name)))?;
            if definition.name != doctype.name || base.contains(&doctype.name) {
                return Err(Fail::Failed(format!(
                    "custom DocType `{}` does not match its record or collides with an app DocType",
                    doctype.name
                )));
            }
            definition.module = Some(doctype.module.clone());
            if definition.app.is_none() {
                definition.app = Some(spot_customise::CUSTOM_MODULE.to_owned());
            }
            definitions.push(definition);
        }
    }
    let mut registry = Registry::new(definitions).map_err(|errors| meta_error(&errors))?;
    for change in &record.changes {
        match change {
            spot_cli::services::metadata::Change::AddCustomField {
                doctype,
                spec,
                insert_after,
            } => {
                registry
                    .add_custom_field(doctype, (**spec).clone(), insert_after.as_deref())
                    .map_err(|errors| meta_error(&errors))?;
            }
        }
    }
    if let Some(records) = &records {
        for field in records.fields.iter().filter(|r| counts(r.state)) {
            let spec: FieldSpec = toml::from_str(&field.spec).map_err(|e| {
                Fail::Failed(format!(
                    "custom field `{}.{}`: the spec is not valid: {}",
                    field.doctype,
                    field.name,
                    e.to_string().lines().next().unwrap_or_default()
                ))
            })?;
            registry
                .add_custom_field(&field.doctype, spec, field.insert_after.as_deref())
                .map_err(|errors| meta_error(&errors))?;
        }
        for property in records.properties.iter().filter(|r| counts(r.state)) {
            let parsed = Property::parse(&property.property, &property.value)
                .map_err(|e| Fail::Failed(format!("property override: {}", e.message)))?;
            registry
                .set_property(&property.doctype, property.field.as_deref(), parsed)
                .map_err(|errors| meta_error(&errors))?;
        }
    }
    Ok(registry.effective())
}

// --- compiling ---------------------------------------------------------------------------------------

/// Compiles `text`; every problem comes back sorted by position.
fn compile(text: &str, meta: &EffectiveMeta) -> Result<Compiled, Vec<Problem>> {
    match RuleSet::compile(text, meta) {
        Ok(compiled) => Ok(compiled),
        Err(RuleGraphError::Invalid(mut problems)) => {
            problems.sort_by(|a, b| {
                (a.line, a.column, a.code, &a.message).cmp(&(b.line, b.column, b.code, &b.message))
            });
            Err(problems)
        }
        Err(other) => Err(vec![Problem::plain(other.code(), other.to_string())]),
    }
}

/// The report of problems: `FILE:LINE:COL: code: message`, each followed (in text) by the one
/// source line it names, cut at 200 characters.
fn problems_report(file: &Path, text: &str, problems: &[Problem], json: bool) -> String {
    if json {
        let rows: Vec<serde_json::Value> = problems
            .iter()
            .map(|p| {
                serde_json::json!({
                    "line": p.line, "column": p.column, "code": p.code, "message": p.message,
                })
            })
            .collect();
        return format!(
            "{}\n",
            serde_json::json!({
                "ok": false, "file": file.display().to_string(), "problems": rows,
            })
        );
    }
    let mut out = String::new();
    for problem in problems {
        out.push_str(&format!(
            "{}:{}:{}: {}: {}\n",
            file.display(),
            problem.line,
            problem.column,
            problem.code,
            problem.message
        ));
        if let Some(line) = text.lines().nth((problem.line as usize).saturating_sub(1)) {
            let shown: String = line.chars().take(LINE_CUT).collect();
            out.push_str(&format!("    {shown}\n"));
        }
    }
    out.push_str(&format!(
        "{} problem{}\n",
        problems.len(),
        if problems.len() == 1 { "" } else { "s" }
    ));
    out
}

fn target_of(matches: &ArgMatches) -> (Option<String>, Option<PathBuf>, bool) {
    (
        matches.get_one::<String>("site").cloned(),
        matches.get_one::<PathBuf>("app").cloned(),
        matches.get_flag("json"),
    )
}

async fn check(matches: &ArgMatches, ctx: &mut CommandCtx) -> Outcome {
    let file = file_of(matches)?;
    let (site, app, json) = target_of(matches);
    let text = rule_text(&file)?;
    let meta = effective_meta(ctx, site.as_deref(), app.as_deref(), true).await?;
    match compile(&text, &meta) {
        Ok(compiled) => Ok(if json {
            format!(
                "{}\n",
                serde_json::json!({
                    "ok": true,
                    "file": file.display().to_string(),
                    "rules": compiled.rule_count(),
                    "strata": compiled.strata_count(),
                })
            )
        } else {
            format!(
                "{}: ok ({} rules, {} strata)\n",
                file.display(),
                compiled.rule_count(),
                compiled.strata_count()
            )
        }),
        Err(problems) => Err(Fail::Refused {
            text: problems_report(&file, &text, &problems, json),
            json,
        }),
    }
}

// --- facts -------------------------------------------------------------------------------------------

/// The test id `N`: a UUIDv7-shaped key that is a pure function of a small number.
fn small_id(n: u16) -> Result<RecordId, String> {
    let mut bytes = [0_u8; 16];
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[14..16].copy_from_slice(&n.to_be_bytes());
    RecordId::from_bytes(bytes).map_err(|e| e.to_string())
}

/// The number a [`small_id`] was made from, when `id` has that shape.
fn small_number(id: &RecordId) -> Option<u16> {
    let b = id.as_bytes();
    let shaped = b[..6].iter().all(|x| *x == 0)
        && b[6] == 0x70
        && b[7] == 0
        && b[8] == 0x80
        && b[9..14].iter().all(|x| *x == 0);
    shaped.then(|| u16::from_be_bytes([b[14], b[15]]))
}

fn parse_uuid(text: &str) -> Option<[u8; 16]> {
    let hex: String = text.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0_u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A record key: `entity#N` or `entity:UUID`.
fn parse_node(text: &str) -> Result<NodeRef, String> {
    // The text is never echoed: a fact value may be sensitive.
    let bad = || "a record key is `entity#N` or `entity:UUID`".to_owned();
    let (entity, id) = if let Some((entity, number)) = text.split_once('#') {
        let n: u16 = number.parse().map_err(|_| bad())?;
        (entity, small_id(n)?)
    } else if let Some((entity, uuid)) = text.split_once(':') {
        let bytes = parse_uuid(uuid).ok_or_else(bad)?;
        (
            entity,
            RecordId::from_bytes(bytes).map_err(|e| e.to_string())?,
        )
    } else {
        return Err(bad());
    };
    let entity = Name::new(entity).map_err(|_| bad())?;
    Ok(NodeRef { entity, id })
}

fn show_node(node: &NodeRef) -> String {
    match small_number(&node.id) {
        Some(n) => format!("{}#{n}", node.entity),
        None => format!("{}:{}", node.entity, node.id),
    }
}

fn node_of(value: &serde_json::Value) -> Result<NodeRef, String> {
    match value {
        serde_json::Value::String(text) => parse_node(text),
        serde_json::Value::Object(map) => match map.get("record") {
            Some(serde_json::Value::String(text)) if map.len() == 1 => parse_node(text),
            _ => Err("a record is `\"entity#N\"` or `{\"record\": \"entity#N\"}`".to_owned()),
        },
        _ => Err("a record is `\"entity#N\"` or `{\"record\": \"entity#N\"}`".to_owned()),
    }
}

fn storage_value(value: &serde_json::Value) -> Result<Value, String> {
    Ok(match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Number(n) => Value::Int(n.as_i64().ok_or_else(|| {
            "a number is an integer (write a decimal as {\"decimal\": \"1.5\"})".to_owned()
        })?),
        serde_json::Value::Object(map) => {
            if let (Some(serde_json::Value::String(d)), 1) = (map.get("decimal"), map.len()) {
                Value::Decimal(
                    d.parse::<Decimal>()
                        .map_err(|_| "a decimal is {\"decimal\": \"12.50\"}".to_owned())?,
                )
            } else {
                Value::Id(node_of(value)?.id)
            }
        }
        serde_json::Value::Array(_) => return Err("a value is a scalar or a record".to_owned()),
    })
}

/// Builds the facts of `compiled`'s base predicates from the JSON document. The document is
/// refused when it holds more than [`MAX_FACT_TUPLES`] tuples.
fn build_facts(document: &serde_json::Value, compiled: &Compiled) -> Result<MemoryFacts, String> {
    let serde_json::Value::Object(map) = document else {
        return Err(
            "the facts are one JSON object: a predicate name to an array of tuples".to_owned(),
        );
    };
    let total: usize = map.values().map(|v| v.as_array().map_or(0, Vec::len)).sum();
    if total > MAX_FACT_TUPLES {
        return Err(format!(
            "the facts hold {total} tuples; the limit is {MAX_FACT_TUPLES}"
        ));
    }
    let mut facts = MemoryFacts::new();
    for (name, tuples) in map {
        let serde_json::Value::Array(tuples) = tuples else {
            return Err(format!("`{name}` must be an array of tuples"));
        };
        let Some(predicate) = compiled.predicate(name) else {
            continue; // not read by this program
        };
        for (index, tuple) in tuples.iter().enumerate() {
            let pair = tuple
                .as_array()
                .filter(|t| t.len() == 2)
                .ok_or_else(|| format!("`{name}` tuple {index} must be a pair"))?;
            let at = |e: String| format!("`{name}` tuple {index}: {e}");
            match &predicate.kind {
                PredKind::Edge => {
                    facts.add_edge(
                        name,
                        node_of(&pair[0]).map_err(&at)?,
                        node_of(&pair[1]).map_err(&at)?,
                    );
                }
                PredKind::Field { doctype, field } => {
                    facts.add_field(
                        doctype,
                        field,
                        node_of(&pair[0]).map_err(&at)?,
                        storage_value(&pair[1]).map_err(&at)?,
                    );
                }
                PredKind::Derived => {
                    return Err(format!(
                        "`{name}` is derived by the rules: facts are for edge types and fields"
                    ));
                }
            }
        }
    }
    Ok(facts)
}

// --- queries and rows --------------------------------------------------------------------------------

/// One argument of a query: `_` or a constant.
#[derive(Debug, PartialEq)]
enum Arg1 {
    Any,
    Const(Datum),
}

/// `predicate` or `predicate(arg, ...)`.
fn parse_query(text: &str) -> Result<(String, Option<Vec<Arg1>>), String> {
    let text = text.trim();
    let Some(open) = text.find('(') else {
        return Ok((text.to_owned(), None));
    };
    let name = text[..open].trim().to_owned();
    let Some(inner) = text[open + 1..].trim_end().strip_suffix(')') else {
        return Err("a query is `predicate` or `predicate(a, _)`".to_owned());
    };
    let mut args = Vec::new();
    for part in split_args(inner)? {
        args.push(parse_query_arg(part.trim())?);
    }
    Ok((name, Some(args)))
}

/// Splits on commas outside double quotes.
fn split_args(inner: &str) -> Result<Vec<&str>, String> {
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut parts = Vec::new();
    let (mut start, mut quoted, mut escaped) = (0, false, false);
    for (i, c) in inner.char_indices() {
        match (c, quoted, escaped) {
            ('\\', true, false) => escaped = true,
            ('"', _, false) => quoted = !quoted,
            (',', false, _) => {
                parts.push(&inner[start..i]);
                start = i + 1;
            }
            _ => escaped = false,
        }
    }
    if quoted {
        return Err("a quoted text in the query is not closed".to_owned());
    }
    parts.push(&inner[start..]);
    Ok(parts)
}

fn parse_query_arg(text: &str) -> Result<Arg1, String> {
    if text == "_" {
        return Ok(Arg1::Any);
    }
    if text.starts_with('"') {
        return serde_json::from_str::<String>(text)
            .map(|s| Arg1::Const(Datum::Text(s)))
            .map_err(|_| format!("`{text}` is not a quoted text"));
    }
    match text {
        "true" => return Ok(Arg1::Const(Datum::Bool(true))),
        "false" => return Ok(Arg1::Const(Datum::Bool(false))),
        "null" => return Ok(Arg1::Const(Datum::Null)),
        _ => {}
    }
    if let Ok(n) = text.parse::<i64>() {
        return Ok(Arg1::Const(Datum::Int(n)));
    }
    if let Ok(d) = text.parse::<Decimal>() {
        return Ok(Arg1::Const(Datum::Decimal(d)));
    }
    parse_node(text)
        .map(|n| Arg1::Const(Datum::Record(n)))
        .map_err(|_| format!("`{text}` is not `_`, a number, a quoted text or a record key"))
}

fn matches_query(row: &[Datum], args: &[Arg1]) -> bool {
    row.iter().zip(args).all(|(datum, arg)| match arg {
        Arg1::Any => true,
        Arg1::Const(wanted) => datum == wanted,
    })
}

/// A datum as text: a record `entity#N`, a text in double quotes.
fn show_datum(datum: &Datum) -> String {
    match datum {
        Datum::Record(node) => show_node(node),
        Datum::Null => "null".to_owned(),
        Datum::Bool(b) => b.to_string(),
        Datum::Int(n) => n.to_string(),
        Datum::Decimal(d) => d.to_string(),
        Datum::Text(s) => serde_json::to_string(s).unwrap_or_default(),
        other => other.canonical(),
    }
}

fn datum_json(datum: &Datum) -> serde_json::Value {
    match datum {
        Datum::Record(node) => serde_json::json!({ "record": show_node(node) }),
        Datum::Null => serde_json::Value::Null,
        Datum::Bool(b) => serde_json::Value::Bool(*b),
        Datum::Int(n) => serde_json::json!(n),
        Datum::Decimal(d) => serde_json::json!({ "decimal": d.to_string() }),
        Datum::Text(s) => serde_json::Value::String(s.clone()),
        other => serde_json::json!({ "value": other.canonical() }),
    }
}

async fn eval(matches: &ArgMatches, ctx: &mut CommandCtx) -> Outcome {
    let file = file_of(matches)?;
    let (site, app, json) = target_of(matches);
    let facts_path = matches
        .get_one::<PathBuf>("facts")
        .cloned()
        .ok_or_else(|| Fail::Usage("--facts is required".to_owned()))?;
    let query_text = matches
        .get_one::<String>("query")
        .cloned()
        .ok_or_else(|| Fail::Usage("--query is required".to_owned()))?;
    let (predicate_name, query_args) = parse_query(&query_text).map_err(Fail::Usage)?;
    let fuel = matches
        .get_one::<u64>("fuel")
        .copied()
        .unwrap_or(DEFAULT_FUEL)
        .min(DEFAULT_FUEL.saturating_mul(10));
    if fuel == 0 {
        return Err(Fail::Usage("--fuel must be at least 1".to_owned()));
    }

    let text = rule_text(&file)?;
    // No database: a site's files only.
    let meta = effective_meta(ctx, site.as_deref(), app.as_deref(), false).await?;
    let compiled = compile(&text, &meta).map_err(|problems| Fail::Refused {
        text: problems_report(&file, &text, &problems, json),
        json,
    })?;
    let Some(predicate) = compiled.predicate(&predicate_name) else {
        return Err(Fail::Usage(format!(
            "the program does not define `{predicate_name}`"
        )));
    };
    if let Some(args) = &query_args
        && args.len() != predicate.arity
    {
        return Err(Fail::Usage(format!(
            "`{predicate_name}` takes {} arguments, the query has {}",
            predicate.arity,
            args.len()
        )));
    }

    refuse_outside_symlink(&facts_path)?;
    let bytes = read_limited(&facts_path, MAX_FACT_BYTES, "the fact file")?;
    let document: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| Fail::refused(format!("{} is not valid JSON", facts_path.display())))?;
    drop(bytes);
    let facts = build_facts(&document, &compiled)
        .map_err(|message| Fail::refused(format!("{}: {message}", facts_path.display())))?;
    drop(document);

    let rows = match Engine::with_fuel(fuel).evaluate(
        &compiled,
        &facts,
        &Query::new(&predicate_name),
    ) {
        Ok(rows) => rows,
        Err(RuleGraphError::FuelExhausted { limit }) => {
            return Err(Fail::refused(format!(
                "rule_graph.fuel_exhausted: the evaluation derived more than {limit} tuples; no rows"
            )));
        }
        Err(other) => {
            return Err(Fail::refused(format!("{}: {other}", other.code())));
        }
    };
    let rows: Vec<&Vec<Datum>> = rows
        .rows()
        .iter()
        .filter(|row| {
            query_args
                .as_ref()
                .is_none_or(|args| matches_query(row, args))
        })
        .collect();
    if json {
        let tuples: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| serde_json::Value::Array(row.iter().map(datum_json).collect()))
            .collect();
        return Ok(format!(
            "{}\n",
            serde_json::json!({
                "ok": true,
                "predicate": predicate_name,
                "rows": tuples,
                "fuel": { "limit": fuel, "exhausted": false },
            })
        ));
    }
    let mut out = String::new();
    for row in &rows {
        let cells: Vec<String> = row.iter().map(show_datum).collect();
        out.push_str(&cells.join("\t"));
        out.push('\n');
    }
    out.push_str(&format!(
        "# {} row{}, fuel limit {fuel}, exhausted: no\n",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    ));
    Ok(out)
}

// --- explain -------------------------------------------------------------------------------------------

/// What one derived predicate reads: `(predicate, how)` where `how` is `""`, `negated` or
/// `aggregate`.
fn dependencies(rules: &[Rule]) -> BTreeMap<String, BTreeSet<(String, &'static str)>> {
    let mut out: BTreeMap<String, BTreeSet<(String, &'static str)>> = BTreeMap::new();
    for rule in rules {
        let aggregate = rule
            .head
            .args
            .iter()
            .any(|a| matches!(a, HeadArg::Agg { .. }));
        let entry = out.entry(rule.head.predicate.clone()).or_default();
        for literal in &rule.body {
            match literal {
                Literal::Atom { predicate, .. } => {
                    entry.insert((predicate.clone(), if aggregate { "aggregate" } else { "" }));
                }
                Literal::Neg { predicate, .. } => {
                    entry.insert((predicate.clone(), "negated"));
                }
                Literal::Guard { .. } => {}
            }
        }
    }
    out
}

/// The derived predicates a query-able predicate (one no other derived predicate reads) reaches,
/// itself included.
fn reachable(deps: &BTreeMap<String, BTreeSet<(String, &'static str)>>) -> BTreeSet<String> {
    let mut read_by_others: BTreeSet<&str> = BTreeSet::new();
    for (head, reads) in deps {
        for (name, _) in reads {
            if name != head && deps.contains_key(name) {
                read_by_others.insert(name);
            }
        }
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<&str> = deps
        .keys()
        .map(String::as_str)
        .filter(|name| !read_by_others.contains(name))
        .collect();
    while let Some(name) = stack.pop() {
        if !seen.insert(name.to_owned()) {
            continue;
        }
        if let Some(reads) = deps.get(name) {
            for (read, _) in reads {
                if deps.contains_key(read) {
                    stack.push(read);
                }
            }
        }
    }
    seen
}

async fn explain(matches: &ArgMatches, ctx: &mut CommandCtx) -> Outcome {
    let file = file_of(matches)?;
    let (site, app, json) = target_of(matches);
    let text = rule_text(&file)?;
    // No database: a site's files only.
    let meta = effective_meta(ctx, site.as_deref(), app.as_deref(), false).await?;
    let compiled = compile(&text, &meta).map_err(|problems| Fail::Refused {
        text: problems_report(&file, &text, &problems, json),
        json,
    })?;
    let parsed = RuleSet::parse(&text).map_err(|e| Fail::refused(format!("{}: {e}", e.code())))?;
    let deps = dependencies(parsed.rules());
    let live = reachable(&deps);

    let mut derived: Vec<_> = compiled
        .predicates()
        .filter(|p| p.kind == PredKind::Derived)
        .collect();
    derived.sort_by(|a, b| (a.stratum, &a.name).cmp(&(b.stratum, &b.name)));
    let describe = |name: &str, how: &str| -> String {
        let (arity, kind) = match compiled.predicate(name) {
            Some(p) => (
                p.arity,
                match &p.kind {
                    PredKind::Edge => "edge",
                    PredKind::Field { .. } => "field",
                    PredKind::Derived => "derived",
                },
            ),
            None => (0, "?"),
        };
        let note = if how.is_empty() {
            String::new()
        } else {
            format!(", {how}")
        };
        format!("{name}/{arity} ({kind}{note})")
    };
    let mut warnings: Vec<String> = Vec::new();
    for rule in parsed.rules() {
        if !live.contains(&rule.head.predicate) {
            warnings.push(format!(
                "{}:{}:{}: the rule for `{}` is not reachable from any query-able predicate",
                file.display(),
                rule.head.span.line,
                rule.head.span.column,
                rule.head.predicate
            ));
        }
    }

    if json {
        let predicates: Vec<serde_json::Value> = derived
            .iter()
            .map(|p| {
                let reads: Vec<serde_json::Value> = deps
                    .get(&p.name)
                    .map(|set| {
                        set.iter()
                            .map(|(name, how)| serde_json::json!({ "predicate": name, "through": how }))
                            .collect()
                    })
                    .unwrap_or_default();
                serde_json::json!({
                    "predicate": p.name, "arity": p.arity, "stratum": p.stratum, "reads": reads,
                })
            })
            .collect();
        return Ok(format!(
            "{}\n",
            serde_json::json!({
                "ok": true,
                "file": file.display().to_string(),
                "strata": compiled.strata_count(),
                "predicates": predicates,
                "warnings": warnings,
            })
        ));
    }
    let mut out = format!(
        "{}: {} rules, {} strata\n",
        file.display(),
        compiled.rule_count(),
        compiled.strata_count()
    );
    let mut stratum = None;
    for predicate in &derived {
        if stratum != Some(predicate.stratum) {
            stratum = Some(predicate.stratum);
            out.push_str(&format!("stratum {}\n", predicate.stratum));
        }
        out.push_str(&format!("  {}/{}\n", predicate.name, predicate.arity));
        if let Some(reads) = deps.get(&predicate.name) {
            let list: Vec<String> = reads
                .iter()
                .map(|(name, how)| describe(name, how))
                .collect();
            if !list.is_empty() {
                out.push_str(&format!("    depends on: {}\n", list.join(", ")));
            }
        }
    }
    for warning in &warnings {
        out.push_str(&format!("warning: {warning}\n"));
    }
    Ok(out)
}
