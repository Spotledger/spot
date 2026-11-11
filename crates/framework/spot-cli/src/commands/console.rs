//! `spot console --site S --user EMAIL`: an administrator's interactive or scripted session over
//! one site's documents, as a named user (WP-PRD-01.T30).
//!
//! The console is an **ordinary client of the engine**, not a back door. It signs in through the
//! same sign-in service as `/auth/login`, then every statement runs as that user through the
//! native API's own router, in process: the permission engine, the field guard, owner scoping,
//! number series and the controllers apply exactly as for an HTTP caller. There is no system
//! actor, no `--as-system`, no raw SQL and no flag that skips a check.
//!
//! * **Maintenance gate.** A write (`create`, `update`, `delete`, `call`) is refused while the
//!   site's gate is closed, and also while it cannot be read (fail closed).
//! * **Audit.** Before a write, a report, and a read of a DocType with protected fields, one
//!   record goes to the site's hash-chained identity trail (channel `console`, the application
//!   user, the OS user). If it cannot be written the statement is refused and nothing runs. The
//!   outcome is recorded after it.
//! * **Output** never repeats a statement's values: an error prints its stable code only.
//!
//! The grammar and the bounds are in `docs/running-spot/console.md`.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::{Method, Request, header};
use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{Value as Json, json};
use spot_api_services::engine_resources::EngineResources;
use spot_api_services::{Delegates, FunctionDelegate, MetaResources, ReportDelegate};
use spot_app_api::context::Actor;
use spot_cli::audit::chain::ChainAudit;
use spot_cli::audit::{IdentityAudit, Recorder, redact_name};
use spot_cli::live::SiteLive;
use spot_cli::login::{Credentials, LoginStores, SiteAuth, SiteDirectory};
use spot_cli::permissions::GuardedEngine;
use spot_cli::services::{self, Services};
use spot_core::SiteId;
use spot_document::engine::{Engine, InProcessDelivery, V7Ids};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::lookup::{LookupPage, LookupRequest};
use spot_http::api::registry::Code;
use spot_http::api::router::{ApiServices, router};
use spot_http::api::service::{
    ApiError, BoxFuture, Credential, LoginBody, LookupService, RequestCtx,
};
use spot_http::request::RequestId;
use spot_http::site::SiteContext;
use spot_migration_apply::gate::GateStatus;
use spot_migration_apply_pg::PgAdmissionGate;
use spot_storage_contract::RecordId;
use tower::ServiceExt;

use super::{CommandCtx, Exit};

/// The subcommand name.
pub const NAME: &str = "console";

const CHANNEL: &str = "console";
const LIMIT_MAX: u32 = 1000;
const LIMIT_DEFAULT: u32 = 100;
const PAGE: u32 = 200; // rows asked of the API at a time (its page limit is 500)
const SCRIPT_MAX: usize = 10_000;
const STATEMENT_MAX: usize = 1 << 20;
const FILE_MAX: u64 = 16 << 20;
const RESPONSE_MAX: usize = 32 << 20;
const TIMEOUT_DEFAULT: u64 = 600;
const TIMEOUT_MAX: u64 = 86_400;
const STATEMENT_TIME: Duration = Duration::from_secs(300); // one interactive statement
const ATTEMPTS: usize = 3; // password tries at a terminal

/// Arguments and help.
pub fn command() -> Command {
    let opt = |id: &'static str, long: &'static str, help: &'static str| {
        Arg::new(id).long(long).help(help)
    };
    let flag = |id: &'static str, long: &'static str, help: &'static str| {
        opt(id, long, help).action(ArgAction::SetTrue)
    };
    Command::new(NAME)
        .about("Inspect and repair one site's documents as a named user (permissions and audit apply)")
        .long_about(
            "Signs in as the named user; every statement runs through the same permission, \
             field-guard and audit path as the native API. It is as powerful as that account: \
             use one with only the rights the work needs.",
        )
        .arg(opt("site", "site", "The site to open").required(true))
        .arg(opt("user", "user", "The user to sign in as (an e-mail)").required(true))
        .arg(opt(
            "password_file",
            "password-file",
            "Read the password from the first line of this file (mode 0600); else ask, echo off",
        ))
        .arg(
            opt("exec", "exec", "Run this statement and exit (repeatable)")
                .action(ArgAction::Append),
        )
        .arg(
            opt("file", "file", "Run the statements of a file, one per line, and exit")
                .conflicts_with("exec"),
        )
        .arg(flag("keep_going", "keep-going", "In a script, continue after a failed statement"))
        .arg(
            opt("format", "format", "Output: JSON lines or a table")
                .value_parser(["json", "table"])
                .default_value("json"),
        )
        .arg(flag("history", "history", "Keep a 0600 history file (statements can hold personal data)"))
        .arg(opt("timeout", "timeout", "Overall time limit of a script in seconds (default 600)"))
}

#[derive(Debug, PartialEq)]
enum Statement {
    Get(String, String),
    List { doctype: String, filter: Option<Json>, limit: u32, fields: Vec<String> },
    Create(String, Json),
    Update(String, String, Json),
    Delete(String, String),
    Call(String, Json),
    Report(String, Json),
    Whoami,
    Help,
    Exit,
}

/// A statement that does not parse. The text is fixed: it never echoes what was typed.
#[derive(Debug, PartialEq)]
struct Usage(&'static str);

const BAD_NAME: Usage = Usage("a name is lowercase letters, digits and `_`");
const BAD_ID: Usage = Usage("a record id is letters, digits, `-` and `_`");

fn name_of(text: Option<&&str>) -> Result<String, Usage> {
    let ok = |t: &str| (1..=64).contains(&t.len()) && t.bytes().all(is_name_byte);
    text.filter(|t| ok(t)).map(|t| (*t).to_owned()).ok_or(BAD_NAME)
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
}

fn id_of(text: Option<&&str>) -> Result<String, Usage> {
    let ok = |t: &str| (1..=64).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    text.filter(|t| ok(t)).map(|t| (*t).to_owned()).ok_or(BAD_ID)
}

fn parse(line: &str) -> Result<Statement, Usage> {
    let line = line.trim();
    let first = line.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    let (head, tail) = match line.find('{') {
        Some(at) if first != "list" => (&line[..at], Some(&line[at..])),
        _ => (line, None),
    };
    let body = match tail {
        None => None,
        Some(t) => match serde_json::from_str::<Json>(t) {
            Ok(v @ Json::Object(_)) => Some(v),
            _ => return Err(Usage("the JSON object could not be read")),
        },
    };
    let words: Vec<&str> = head.split_whitespace().collect();
    let word = |n: usize| words.get(n);
    let arity = |n: usize| match words.len() == n {
        true => Ok(()),
        false => Err(Usage("wrong number of arguments; see `help`")),
    };
    let need = || body.clone().ok_or(Usage("a JSON object is required"));
    let or_empty = || body.clone().unwrap_or_else(|| json!({}));
    match first.as_str() {
        "whoami" | "help" | "exit" | "quit" if words.len() == 1 => Ok(match first.as_str() {
            "whoami" => Statement::Whoami,
            "help" => Statement::Help,
            _ => Statement::Exit,
        }),
        "list" => parse_list(line),
        "get" | "delete" => {
            arity(3)?;
            let (doctype, id) = (name_of(word(1))?, id_of(word(2))?);
            Ok(match first.as_str() {
                "get" => Statement::Get(doctype, id),
                _ => Statement::Delete(doctype, id),
            })
        }
        "create" => arity(2).and_then(|()| Ok(Statement::Create(name_of(word(1))?, need()?))),
        "update" => {
            arity(3)?;
            Ok(Statement::Update(name_of(word(1))?, id_of(word(2))?, need()?))
        }
        "call" | "report" => {
            arity(2)?;
            let target = word(1).map(|t| (*t).to_owned()).unwrap_or_default();
            let parts: Vec<&str> = target.split(if first == "call" { '.' } else { '\u{0}' }).collect();
            let valid = |p: &str| !p.is_empty() && p.len() <= 64 && p.bytes().all(|b| is_name_byte(b) || (first == "report" && b == b'-'));
            match (first.as_str(), parts.len()) {
                ("call", 3) | ("report", 1) if parts.iter().all(|p| valid(p)) => Ok(match first.as_str() {
                    "call" => Statement::Call(target, or_empty()),
                    _ => Statement::Report(target, or_empty()),
                }),
                ("call", _) => Err(Usage("a function is APP.MODULE.FUNCTION")),
                _ => Err(Usage("a report name is lowercase letters, digits, `_` and `-`")),
            }
        }
        _ => Err(Usage("unknown statement; see `help`")),
    }
}

/// Words of a `list` statement; a double-quoted word is one JSON string (the flag says so).
fn words_of(line: &str) -> Result<Vec<(String, bool)>, Usage> {
    let (mut out, mut rest) = (Vec::new(), line.trim_start());
    while !rest.is_empty() {
        if rest.starts_with('"') {
            let mut escaped = false;
            let end = rest.char_indices().skip(1).find(|&(_, c)| match (escaped, c) {
                (true, _) => {
                    escaped = false;
                    false
                }
                (false, '\\') => {
                    escaped = true;
                    false
                }
                (false, c) => c == '"',
            });
            let end = end.ok_or(Usage("a quoted value is not closed"))?.0 + 1;
            let word = serde_json::from_str::<String>(&rest[..end]);
            out.push((word.map_err(|_| Usage("a quoted value could not be read"))?, true));
            rest = &rest[end..];
        } else {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            out.push((rest[..end].to_owned(), false));
            rest = &rest[end..];
        }
        rest = rest.trim_start();
    }
    Ok(out)
}

fn parse_list(line: &str) -> Result<Statement, Usage> {
    let all = words_of(line)?;
    let mut it = all.iter().skip(1).map(|(w, q)| (w.as_str(), *q));
    let doctype = name_of(it.next().map(|(w, _)| w).as_ref())?;
    let (mut filters, mut limit, mut fields) = (Vec::new(), LIMIT_DEFAULT, Vec::new());
    while let Some((word, _)) = it.next() {
        match word.to_ascii_lowercase().as_str() {
            "and" => {}
            "where" => {
                let field = name_of(it.next().map(|(w, _)| w).as_ref())?;
                let op = it.next().map(|(w, _)| w.to_ascii_lowercase());
                let op = match op.as_deref() {
                    Some("=" | "==") => "eq",
                    Some("!=" | "<>") => "ne",
                    Some("<") => "lt",
                    Some("<=") => "le",
                    Some(">") => "gt",
                    Some(">=") => "ge",
                    Some("prefix") => "prefix",
                    Some("contains") => "contains",
                    Some("suffix") => "suffix",
                    _ => return Err(Usage("an operator is = != < <= > >= prefix contains suffix")),
                };
                let (raw, quoted) = it.next().ok_or(Usage("where FIELD OP VALUE"))?;
                let as_json = serde_json::from_str::<Json>(raw).ok();
                let value = match as_json.filter(|v| !quoted && !v.is_array() && !v.is_object()) {
                    Some(v) => v,
                    None => Json::String(raw.to_owned()),
                };
                filters.push(json!({"field": field, "op": op, "value": value}));
            }
            "limit" => {
                let n = it.next().and_then(|(w, _)| w.parse::<u32>().ok());
                limit = n.filter(|n| (1..=LIMIT_MAX).contains(n)).ok_or(Usage("limit is 1 to 1000"))?;
            }
            "fields" => {
                let list = it.next().map(|(w, _)| w).ok_or(Usage("fields a,b"))?;
                fields = list.split(',').map(str::to_owned).collect();
                if !fields.iter().all(|f| name_of(Some(&f.as_str())).is_ok()) {
                    return Err(Usage("fields are names separated by commas"));
                }
            }
            _ => return Err(Usage("list DOCTYPE [where FIELD OP VALUE ...] [limit N] [fields a,b]")),
        }
    }
    let filter = match filters.len() {
        0 => None,
        1 => filters.pop(),
        _ => Some(json!({ "and": filters })),
    };
    Ok(Statement::List { doctype, filter, limit, fields })
}

const HELP: &str = "statements:\n  get DOCTYPE ID\n  list DOCTYPE [where FIELD OP VALUE ...] [limit N] [fields a,b]\n  create DOCTYPE {json}\n  update DOCTYPE ID {json}\n  delete DOCTYPE ID\n  call APP.MODULE.FUNCTION [{json}]\n  report NAME [{json}]\n  whoami | help | exit\noperators: = != < <= > >= prefix contains suffix; quote values with spaces; limit is at most 1000.";

fn cell(value: &Json) -> String {
    let text = match value {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        other => other.to_string(),
    };
    let text: String = text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    match text.chars().count() > 60 {
        true => format!("{}...", text.chars().take(57).collect::<String>()),
        false => text,
    }
}

fn table(header: &[String], rows: &[Vec<String>]) -> String {
    let width = |i: usize| {
        let cells = rows.iter().filter_map(|r| r.get(i));
        header.get(i).into_iter().chain(cells).map(|c| c.chars().count()).max().unwrap_or(0)
    };
    let widths: Vec<usize> = (0..header.len()).map(width).collect();
    let line = |cells: &[String]| {
        let padded: Vec<String> = cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        format!("{}\n", padded.join("  ").trim_end())
    };
    std::iter::once(header).chain(rows.iter().map(Vec::as_slice)).map(line).collect()
}

fn render(docs: &[Json], tabular: bool, out: &mut Vec<u8>) {
    if !tabular {
        docs.iter().for_each(|d| out.extend(format!("{d}\n").bytes()));
        return;
    }
    let mut columns: Vec<String> = Vec::new();
    for key in docs.iter().filter_map(Json::as_object).flat_map(|m| m.keys()) {
        if !columns.contains(key) {
            columns.push(key.clone());
        }
    }
    let rows: Vec<Vec<String>> = docs
        .iter()
        .map(|d| columns.iter().map(|c| cell(d.get(c).unwrap_or(&Json::Null))).collect())
        .collect();
    out.extend(table(&columns, &rows).bytes());
}

struct Fail(Exit, String);

fn refused(text: &str) -> Fail {
    Fail(Exit::Refused, text.to_owned())
}

struct Reply {
    status: u16,
    body: Json,
}

impl Reply {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("console.unexpected_reply")
    }
    /// The failure for a refused reply: the stable code and status, never a value.
    fn fail(&self) -> Fail {
        let code = self.code();
        let exit = match code.starts_with("permission.") || code.starts_with("auth.") || code == "server.maintenance" {
            true => Exit::Refused,
            false => Exit::Failed,
        };
        Fail(exit, format!("{code}: the statement failed (http {})", self.status))
    }
}

struct NoLookup;

impl LookupService for NoLookup {
    fn search<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a str,
        _: &'a LookupRequest,
    ) -> BoxFuture<'a, Result<LookupPage, ApiError>> {
        Box::pin(async { Err(ApiError::new(Code::ApiInvalidRequest, "No lookup provider is configured.")) })
    }
}

/// A signed-in user and the in-process API router they act through. Nothing here reaches the
/// database except through that router (and the gate and audit trail the console must consult).
struct Session {
    router: Router,
    site: SiteId,
    login: String,
    user: RecordId,
    token: String,
    os_user: String,
    recorder: Arc<Recorder>,
    gate: PgAdmissionGate,
    clock: Arc<dyn spot_core::Clock>,
    restricted: Vec<String>,
    tabular: bool,
    assembly: Option<Services>,
    missing: Vec<&'static str>,
}

impl Session {
    async fn send(&self, method: Method, path: &str, version: Option<&str>, body: Option<&Json>) -> Reply {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {}", self.token))
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(version) = version {
            request = request.header(header::IF_MATCH, format!("\"{version}\""));
        }
        let bytes = body.map(Json::to_string).unwrap_or_default();
        let Ok(mut request) = request.body(axum::body::Body::from(bytes)) else {
            return Reply { status: 500, body: Json::Null };
        };
        let request_id = RequestId::generate();
        request.extensions_mut().insert(request_id.clone());
        let site = SiteContext { site: self.site.clone(), request_id, peer: None, host: None };
        request.extensions_mut().insert(site);
        let response = match self.router.clone().oneshot(request).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
        let status = response.status().as_u16();
        let bytes = axum::body::to_bytes(response.into_body(), RESPONSE_MAX).await;
        let body = bytes.ok().and_then(|b| serde_json::from_slice(&b).ok());
        Reply { status, body: body.unwrap_or(Json::Null) }
    }

    /// One identity-trail record of channel `console`; `required` refuses the statement when it
    /// cannot be written.
    fn audit(&self, kind: &str, target: &str, required: bool) -> Result<(), ()> {
        let mut record = self.recorder.record(kind);
        record.user = Some(self.user.to_string());
        record.attempted = Some(redact_name(&self.os_user));
        record.path = Some(CHANNEL.to_owned());
        record.reason = Some(target.to_owned());
        match required {
            true => self.recorder.required(&record).map_err(|_| ()),
            false => {
                self.recorder.always(&record);
                Ok(())
            }
        }
    }

    /// The gate must be readable and open before anything changes.
    async fn gate_open(&self) -> Result<(), Fail> {
        let now = self.clock.now_utc().unix_timestamp();
        match self.gate.refresh(now).await {
            Ok(GateStatus::Open) => Ok(()),
            Ok(GateStatus::Closed { .. }) => Err(refused(
                "server.maintenance: the site is in maintenance; nothing was changed",
            )),
            Err(_) => Err(refused(
                "server.maintenance: the maintenance gate cannot be read, so writes are refused; nothing was changed",
            )),
        }
    }

    /// Runs one statement: the gate and the audit record first when it writes.
    async fn run(&self, statement: &Statement, out: &mut Vec<u8>) -> Result<(), Fail> {
        let (verb, target, writes, reads) = match statement {
            Statement::Get(d, _) | Statement::List { doctype: d, .. } => ("read", d.clone(), false, Some(d)),
            Statement::Create(d, _) => ("create", d.clone(), true, None),
            Statement::Update(d, ..) => ("update", d.clone(), true, None),
            Statement::Delete(d, _) => ("delete", d.clone(), true, None),
            Statement::Call(f, _) => ("call", f.clone(), true, None),
            Statement::Report(r, _) => ("report", r.clone(), false, None),
            Statement::Whoami => {
                let who = json!({"site": self.site.as_str(), "user": self.user.to_string(), "signed_in_as": self.login, "channel": CHANNEL, "os_user": self.os_user});
                out.extend(format!("{who}\n").bytes());
                return Ok(());
            }
            Statement::Help | Statement::Exit => {
                out.extend(format!("{HELP}\n").bytes());
                return Ok(());
            }
        };
        let restricted = reads.is_some_and(|d| self.restricted.contains(d));
        let audited = writes || verb == "report" || restricted;
        if writes {
            self.gate_open().await?;
        }
        if audited && self.audit(&format!("console_{verb}"), &target, true).is_err() {
            return Err(refused("console.audit_unavailable: the audit trail cannot be written; nothing was changed"));
        }
        let result = self.execute(statement, out).await;
        if audited {
            let outcome = result.as_ref().err().map_or("ok", |f| f.1.split(':').next().unwrap_or("failed"));
            let _ = self.audit(&format!("console_{verb}_outcome"), &format!("{target} {outcome}"), false);
        }
        result
    }

    /// The version a write must name. A document the user cannot see answers exactly as one that
    /// is not there.
    async fn version_of(&self, doctype: &str, id: &str) -> Result<String, Fail> {
        let reply = self.send(Method::GET, &format!("/api/v1/resource/{doctype}/{id}"), None, None).await;
        match reply.body["data"]["version"].as_u64() {
            Some(version) if reply.ok() => Ok(version.to_string()),
            _ if reply.status == 403 || reply.status == 404 => {
                Err(Fail(Exit::Failed, "document.not_found: no such document (http 404)".into()))
            }
            _ => Err(reply.fail()),
        }
    }

    async fn execute(&self, statement: &Statement, out: &mut Vec<u8>) -> Result<(), Fail> {
        let resource = |doctype: &str| format!("/api/v1/resource/{doctype}");
        let reply = match statement {
            Statement::Get(d, id) => self.send(Method::GET, &format!("{}/{id}", resource(d)), None, None).await,
            Statement::List { doctype, filter, limit, fields } => {
                return self.list(doctype, filter.as_ref(), *limit, fields, out).await;
            }
            Statement::Create(d, body) => self.send(Method::POST, &resource(d), None, Some(body)).await,
            Statement::Update(d, id, body) => {
                let version = self.version_of(d, id).await?;
                self.send(Method::PATCH, &format!("{}/{id}", resource(d)), Some(&version), Some(body)).await
            }
            Statement::Delete(d, id) => {
                let version = self.version_of(d, id).await?;
                let reply = self.send(Method::DELETE, &format!("{}/{id}", resource(d)), Some(&version), None).await;
                if reply.ok() {
                    out.extend(format!("{}\n", json!({"deleted": id})).bytes());
                    return Ok(());
                }
                reply
            }
            Statement::Call(path, args) => {
                self.need("function")?;
                self.send(Method::POST, &format!("/api/v1/method/{}", path.replace('.', "/")), None, Some(args)).await
            }
            Statement::Report(name, args) => {
                self.need("report")?;
                self.send(Method::POST, &format!("/api/v1/report/{name}/run"), None, Some(args)).await
            }
            Statement::Whoami | Statement::Help | Statement::Exit => return Ok(()),
        };
        if !reply.ok() {
            return Err(reply.fail());
        }
        let data = &reply.body["data"];
        match (data["columns"].as_array(), data["rows"].as_array()) {
            (Some(columns), Some(rows)) if self.tabular => {
                let header: Vec<String> = columns.iter().map(|c| cell(&c["name"])).collect();
                let rows: Vec<Vec<String>> = rows.iter().filter_map(Json::as_array).map(|r| r.iter().map(cell).collect()).collect();
                out.extend(table(&header, &rows).bytes());
            }
            _ if self.tabular && data.is_object() => {
                let rows: Vec<Vec<String>> = data.as_object().into_iter().flatten().map(|(k, v)| vec![k.clone(), cell(v)]).collect();
                out.extend(table(&["field".to_owned(), "value".to_owned()], &rows).bytes());
            }
            _ => out.extend(format!("{data}\n").bytes()),
        }
        Ok(())
    }

    fn need(&self, service: &'static str) -> Result<(), Fail> {
        match self.missing.contains(&service) {
            true => Err(Fail(Exit::Failed, format!("console.{service}_unavailable: that service did not start here"))),
            false => Ok(()),
        }
    }

    async fn list(&self, doctype: &str, filter: Option<&Json>, limit: u32, fields: &[String], out: &mut Vec<u8>) -> Result<(), Fail> {
        let (mut docs, mut cursor): (Vec<Json>, Option<String>) = (Vec::new(), None);
        while docs.len() < limit as usize {
            let remaining = limit.saturating_sub(u32::try_from(docs.len()).unwrap_or(limit));
            let mut body = json!({"limit": PAGE.min(remaining)});
            body["filter"] = filter.cloned().unwrap_or(Json::Null);
            body["cursor"] = cursor.take().map_or(Json::Null, Json::String);
            if !fields.is_empty() {
                body["fields"] = json!(fields);
            }
            let path = format!("/api/v1/resource/{doctype}/query");
            let reply = self.send(Method::POST, &path, None, Some(&body)).await;
            if !reply.ok() {
                return Err(reply.fail());
            }
            docs.extend(reply.body["data"].as_array().cloned().unwrap_or_default());
            cursor = reply.body["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        docs.truncate(limit as usize);
        render(&docs, self.tabular, out);
        Ok(())
    }
}

/// A line from the terminal with echo off (`stty`), never from an argument or the environment.
fn prompt(label: &str) -> Result<String, String> {
    #[cfg(unix)]
    {
        let none = || "no terminal; use --password-file".to_owned();
        let tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").map_err(|_| none())?;
        let reader = tty.try_clone().map_err(|_| none())?;
        let mut terminal = tty.try_clone().map_err(|_| none())?;
        let stty = |args: &[&str]| -> bool {
            let stdin = tty.try_clone().map_or(std::process::Stdio::null(), Into::into);
            let status = std::process::Command::new("stty").args(args).stdin(stdin).status();
            status.is_ok_and(|s| s.success())
        };
        write!(terminal, "{label}").and_then(|()| terminal.flush()).map_err(|_| none())?;
        if !stty(&["-echo"]) {
            return Err("cannot turn terminal echo off; use --password-file".to_owned());
        }
        let mut line = String::new();
        let read = std::io::BufReader::new(reader).read_line(&mut line);
        stty(&["echo"]);
        let _ = writeln!(terminal);
        read.map_err(|_| "the terminal could not be read".to_owned())?;
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }
    #[cfg(not(unix))]
    {
        let _ = label;
        Err("no echo-free terminal here; use --password-file".to_owned())
    }
}

fn wipe(secret: String) {
    let mut bytes = secret.into_bytes();
    bytes.fill(0);
    std::hint::black_box(&bytes);
}

fn read_password_file(path: &Path) -> Result<String, String> {
    let unreadable = || "the password file cannot be read".to_owned();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path).map_err(|_| unreadable())?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err("the password file must not be readable by group or others (chmod 600)".to_owned());
        }
    }
    let mut text = String::new();
    let file = std::fs::File::open(path).and_then(|f| f.take(4096).read_to_string(&mut text));
    file.map_err(|_| unreadable())?;
    let line = text.lines().next().unwrap_or("").to_owned();
    wipe(text);
    match line.is_empty() {
        true => Err("the password file is empty".to_owned()),
        false => Ok(line),
    }
}

/// Signs in through the site's own sign-in service. Every failure feeds that service's throttle
/// and its audit records, and a wrong password and an unknown user are answered the same.
async fn sign_in(auth: &SiteAuth, site: &SiteId, user: &str, file: Option<&Path>) -> Result<(String, RecordId), String> {
    let ctx = |credential: Option<Credential>| RequestCtx {
        site: site.clone(),
        credential,
        idempotency_key: None,
        language: None,
        request_id: RequestId::generate(),
        peer: None,
    };
    for _ in 0..if file.is_some() { 1 } else { ATTEMPTS } {
        let password = match file {
            Some(path) => read_password_file(path)?,
            None => prompt("Password: ")?,
        };
        let mut body = LoginBody { user: user.to_owned(), password, totp: None, recovery_code: None };
        let mut result = auth.login_with_source(&ctx(None), &body, "console").await;
        if result.as_ref().err().is_some_and(|e| e.code.as_str() == "auth.totp_required") {
            if file.is_some() {
                wipe(std::mem::take(&mut body.password));
                return Err("this user needs a second factor: run `spot console` at a terminal".to_owned());
            }
            body.totp = Some(prompt("One-time code: ")?);
            result = auth.login_with_source(&ctx(None), &body, "console").await;
        }
        wipe(std::mem::take(&mut body.password));
        match result {
            Ok((_, true)) => return Err("this user has not chosen a password yet; choose one on the web first".to_owned()),
            Ok((session, false)) => {
                let credential = Credential::new(session.token.clone());
                let principal = auth.authenticate(&ctx(Some(credential))).await;
                let principal = principal.map_err(|_| "the session could not be read".to_owned())?;
                return Ok((session.token, principal.user_id));
            }
            Err(error) if error.code.as_str().starts_with("auth.") => {}
            Err(_) => return Err("sign-in is unavailable".to_owned()),
        }
    }
    Err("sign-in failed".to_owned())
}

fn why(what: &'static str) -> impl Fn(spot_cli::Error) -> String {
    move |error| format!("{what}: {error}")
}

/// Builds the site's engine the way the host does (the same permission snapshot, controllers,
/// participants and secrets), without serving, listening or starting a job, then signs in.
async fn open(ctx: &mut CommandCtx, site_name: &str, user: &str, file: Option<&Path>, tabular: bool) -> Result<Session, String> {
    let root = ctx.root().to_path_buf();
    let clock = ctx.clock().clone();
    let loaded = spot_cli::config::load(&root, false).map_err(why("the host configuration did not load"))?;
    let site = loaded.sites.iter().find(|s| s.site == site_name).cloned();
    let site = site.ok_or_else(|| "there is no such site".to_owned())?;
    if site.partition.is_some() {
        return Err("this site is a partitioned node; the console does not open those".to_owned());
    }
    let permission = spot_cli::permissions::site(&root, &loaded, &site).await.map_err(why("the site cannot be opened"))?;
    let backend = spot_cli::schema::backend(&loaded, &site).await.map_err(why("the site's database cannot be reached"))?;
    let pending = spot_cli::schema::pending_platform_tables(&backend, &permission.catalog).await;
    if !pending.map_err(why("the database cannot be read"))?.is_empty() {
        return Err("the database is behind the installed apps; run `spot migrate --apply`".to_owned());
    }
    if spot_cli::schema::audit_protection_gap(&backend).await.map_err(why("the audit store cannot be read"))?.is_some() {
        return Err("the audit chain is not fully protected; run `spot migrate --apply`".to_owned());
    }
    let site_id = SiteId::new(&site.site).map_err(|_| "the site name is not valid".to_owned())?;
    // The identity trail: sign-ins and every console statement. Without it nothing runs.
    let mut pg = loaded.admin.clone().with_dbname(&site.database);
    pg.pool_max_size = loaded.common.postgres.pool_max_size.clamp(1, 4);
    let Ok((chain, _)) = ChainAudit::open(pg, &site.site, None).await.map_err(why("the audit trail cannot be opened"))? else {
        return Err("the audit store is not prepared; run `spot migrate --apply`".to_owned());
    };
    let sink: Arc<dyn IdentityAudit> = Arc::new(chain);
    let recorder = Arc::new(Recorder::new(&site.site, sink, clock.clone()));
    let stores = LoginStores::new(recorder.clone(), clock.clone());
    let credentials = Credentials::load(&root, &site, clock.clone()).map_err(why("the site's credentials cannot be read"))?;
    let credentials = Arc::new(credentials);
    let directory = SiteDirectory::new(backend.clone(), permission.catalog.clone(), credentials.clone());
    let auth = SiteAuth::new(spot_cli::login::default_login_config(), Arc::new(directory), credentials, Vec::new(), clock.clone(), &stores);
    let auth = Arc::new(auth.map_err(why("sign-in cannot start"))?);
    let (token, user_id) = sign_in(&auth, &site_id, user, file).await?;
    // The engine, as the host builds it.
    let apps = &permission.apps;
    let installed = services::controllers::build(&root, &loaded, &site, &permission.meta, apps).map_err(why("the controllers cannot be installed"))?;
    let secrets = services::secrets::open(&root, &loaded, &site).await.map_err(why("the secrets service cannot start"))?;
    if let Some(host) = &secrets {
        let checked = services::secrets::verify_values(host, &backend, &permission.catalog, &permission.meta).await;
        checked.map_err(why("the sealed values cannot be verified"))?;
    }
    let engine = Engine::new(backend.clone(), permission.meta.clone(), installed.seams.clone(), clock.clone(), Arc::new(V7Ids));
    let engine = engine.map_err(|e| format!("the engine cannot start: {e}"))?;
    let permitted = spot_app_permissions::SnapshotAppPermissions::new(permission.authorizer.clone());
    let engine = engine
        .with_delivery(Arc::new(InProcessDelivery))
        .with_authorizer(permission.authorizer.clone())
        .with_app_permissions(Arc::new(permitted))
        .with_controllers(installed.controllers.clone());
    let engine = match &secrets {
        Some(host) => engine.with_secrets(host.vault()),
        None => engine,
    };
    let generation = permission.meta.clone();
    let deps = services::participants::Deps {
        audit: None,
        controllers: Arc::new(installed.registry),
        partitions: None,
        meta: Arc::new(move || generation.clone()),
    };
    let participants = services::participants::build(&site_id, &site, &deps).map_err(why("the participants cannot be installed"))?;
    engine.set_participants(participants.participants);
    engine.prepare().await.map_err(|e| format!("the engine cannot prepare storage: {e}"))?;
    let engine = Arc::new(engine);
    let live = Arc::new(SiteLive::new());
    let (meta, model) = (permission.meta.clone(), permission.model.clone());
    let principals = permission.principals.clone();
    live.attach(meta, model, apps.clone(), permission.decision.clone(), engine.clone(), permission.authorizer.clone(), principals);
    live.set_auth(auth.clone());
    // Functions and reports: the same hosting modules, assembled but never serving.
    let grace = Duration::from_millis(loaded.common.services.shutdown_grace_ms);
    let extra = loaded.common.services.services_extra.clone();
    let env = services::SiteEnv { root: &root, loaded: &loaded, site: &site, permission: &permission, backend: &backend, live: &live };
    let mut hosting = services::SiteCtx::new(site.site.clone(), grace, clock.clone(), extra, true).with_env(env);
    let mut missing = Vec::new();
    hosting.as_module("function", services::function::ORDER);
    if services::function::register(&mut hosting).is_err() {
        missing.push("function");
    }
    hosting.as_module("report", services::report::ORDER);
    if services::report::register(&mut hosting).is_err() {
        missing.push("report");
    }
    let mut delegates = Delegates::none();
    if let Some(function) = hosting.port::<dyn FunctionDelegate>() {
        delegates = delegates.with_function(function);
    }
    if let Some(report) = hosting.port::<dyn ReportDelegate>() {
        delegates = delegates.with_report(report);
    }
    let mut assembly = hosting.finish(Router::new()).assembly;
    if let Some(running) = assembly.as_mut()
        && running.start().await.is_err()
    {
        missing.extend(["function", "report"]);
    }
    let guarded = Arc::new(GuardedEngine::new(engine, permission.authorizer.clone(), Arc::default()));
    let actor = Arc::new(move |_: &RequestCtx| Ok::<_, ApiError>(Actor::User { id: user_id, via: None }));
    let resources = EngineResources::new(guarded, permission.meta.clone(), clock.clone(), actor).with_delegates(delegates);
    let api = ApiServices::new(
        Arc::new(MetaResources::live(live.meta_source())),
        Arc::new(resources),
        Arc::new(NoLookup),
        auth.clone(),
        Arc::new(MemoryIdempotencyStore::new(clock.clone())),
    );
    let protected = permission.meta.field_protection();
    let restricted = protected.iter().filter(|p| !p.groups.is_empty() || !p.tags.is_empty());
    let restricted = restricted.filter_map(|p| p.path.split('.').next().map(str::to_owned)).collect();
    let gate = PgAdmissionGate::new(backend.pool(), site_id.clone(), clock.clone(), Duration::from_secs(1));
    Ok(Session {
        router: router(api),
        site: site_id,
        login: user.to_owned(),
        user: user_id,
        token,
        os_user: os_user(),
        recorder,
        gate,
        clock,
        restricted,
        tabular,
        assembly,
        missing,
    })
}

/// The OS account this process runs as: on Unix the real uid from the kernel (never `USER` or
/// `LOGNAME`, which the caller controls) named through `/etc/passwd`, else `uid:N`; elsewhere `USERNAME`.
fn os_user() -> String {
    #[cfg(unix)]
    {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let line = status.lines().find_map(|l| l.strip_prefix("Uid:"));
        let Some(uid) = line.and_then(|l| l.split_whitespace().next()).and_then(|u| u.parse::<u32>().ok()) else {
            return "unknown".to_owned();
        };
        let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
        let name = |l: &str| {
            let mut f = l.split(':');
            let name = f.next().filter(|n| !n.is_empty())?;
            (f.nth(1)?.parse::<u32>().ok()? == uid).then(|| name.to_owned())
        };
        passwd.lines().find_map(name).unwrap_or_else(|| format!("uid:{uid}"))
    }
    #[cfg(not(unix))]
    {
        std::env::var("USERNAME").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "unknown".to_owned())
    }
}

fn open_history() -> Result<std::fs::File, String> {
    let home = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("state"));
    let state = std::env::var_os("XDG_STATE_HOME").or_else(|| std::env::var_os("LOCALAPPDATA")).map(PathBuf::from);
    let dir = state.or(home).ok_or("there is no state directory for the history file")?.join("spot");
    std::fs::create_dir_all(&dir).map_err(|_| "the history directory cannot be created")?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(dir.join("console-history")).map_err(|_| "the history file cannot be opened".to_owned())
}

fn statements_of(matches: &ArgMatches) -> Result<Option<Vec<String>>, String> {
    if let Some(list) = matches.get_many::<String>("exec") {
        return Ok(Some(list.cloned().collect()));
    }
    let Some(path) = matches.get_one::<String>("file") else {
        return Ok(None);
    };
    let unreadable = || "the script cannot be read".to_owned();
    if std::fs::metadata(path).map_err(|_| unreadable())?.len() > FILE_MAX {
        return Err("the script is too large".to_owned());
    }
    let text = std::fs::read_to_string(path).map_err(|_| unreadable())?;
    let lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'));
    Ok(Some(lines.map(str::to_owned).collect()))
}

fn next_line() -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let read = std::io::stdin().lock().take(STATEMENT_MAX as u64 + 1).read_line(&mut line)?;
    Ok((read > 0).then_some(line))
}

/// Runs the command.
pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let (Some(site), Some(user)) = (matches.get_one::<String>("site"), matches.get_one::<String>("user")) else {
        writeln!(ctx.err(), "usage: spot console --site SITE --user EMAIL")?;
        return Ok(Exit::Usage);
    };
    let timeout = matches.get_one::<String>("timeout").map_or(Some(TIMEOUT_DEFAULT), |t| t.parse::<u64>().ok());
    let script = statements_of(matches);
    let mut early = None;
    match (&timeout, &script) {
        (Some(1..=TIMEOUT_MAX), Ok(s)) if s.as_ref().is_none_or(|s| s.len() <= SCRIPT_MAX) => {}
        (Some(1..=TIMEOUT_MAX), Ok(_)) => early = Some("a script runs at most 10000 statements".to_owned()),
        (_, Err(text)) => early = Some(text.clone()),
        _ => early = Some("--timeout is a number of seconds from 1 to 86400".to_owned()),
    }
    // The whole script is read before anything opens, so a statement that does not parse costs
    // nothing and changes nothing.
    let script: Option<Vec<(String, Statement)>> = match script {
        Ok(Some(lines)) if early.is_none() => {
            let parsed: Result<Vec<_>, Usage> = lines.into_iter().map(|l| parse(&l).map(|s| (l, s))).collect();
            match parsed {
                Ok(parsed) => Some(parsed),
                Err(Usage(text)) => {
                    early = Some(text.to_owned());
                    None
                }
            }
        }
        _ => None,
    };
    let mut history = None;
    if matches.get_flag("history") && early.is_none() {
        match open_history() {
            Ok(file) => history = Some(file),
            Err(text) => early = Some(text),
        }
    }
    if let Some(text) = early {
        writeln!(ctx.err(), "usage: {text}")?;
        return Ok(Exit::Usage);
    }
    let file = matches.get_one::<String>("password_file").map(PathBuf::from);
    let tabular = matches.get_one::<String>("format").is_some_and(|f| f == "table");
    let mut session = match open(ctx, site, user, file.as_deref(), tabular).await {
        Ok(session) => session,
        Err(text) => {
            writeln!(ctx.err(), "console: {text}")?;
            return Ok(Exit::Refused);
        }
    };
    let keep_going = matches.get_flag("keep_going");
    let interactive = script.is_none();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout.unwrap_or(TIMEOUT_DEFAULT));
    let mut queue = script.unwrap_or_default().into_iter();
    let mut first: Option<Exit> = None;
    loop {
        let (line, statement) = if interactive {
            if std::io::stdin().is_terminal() {
                write!(ctx.err(), "spot[{site}]> ")?;
                ctx.err().flush()?;
            }
            let Ok(Ok(Some(line))) = tokio::task::spawn_blocking(next_line).await else {
                break;
            };
            let parsed = if line.len() > STATEMENT_MAX {
                Err(Usage("a statement is at most 1 MiB"))
            } else if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            } else {
                parse(&line)
            };
            match parsed {
                Ok(statement) => (line, Some(statement)),
                Err(Usage(text)) => (text.to_owned(), None),
            }
        } else {
            match queue.next() {
                Some((line, statement)) => (line, Some(statement)),
                None => break,
            }
        };
        let Some(statement) = statement else {
            writeln!(ctx.err(), "usage: {line}")?;
            first.get_or_insert(Exit::Usage);
            continue;
        };
        if statement == Statement::Exit {
            break;
        }
        if let Some(file) = &mut history {
            let _ = writeln!(file, "{}", line.replace(['\r', '\n'], " "));
        }
        let limit = if interactive { STATEMENT_TIME } else { deadline.saturating_duration_since(tokio::time::Instant::now()) };
        let mut out = Vec::new();
        let result = tokio::time::timeout(limit, session.run(&statement, &mut out)).await;
        let result = result.unwrap_or_else(|_| Err(Fail(Exit::Failed, "console.timeout: the time limit passed; the statement may not have completed".into())));
        ctx.out().write_all(&out)?;
        if let Err(Fail(exit, text)) = result {
            writeln!(ctx.err(), "error: {text}")?;
            first.get_or_insert(exit);
            if !interactive && !keep_going {
                break;
            }
        }
    }
    if let Some(mut running) = session.assembly.take() {
        running.shutdown().await;
    }
    Ok(if interactive { Exit::Ok } else { first.unwrap_or(Exit::Ok) })
}
