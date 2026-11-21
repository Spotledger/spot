//! `spot-sdk-conformance` — the guest-SDK contract kit (WP-STR-07.T1).
//!
//! The harness (`tools/sdk-conformance/run.py`) builds a guest component from a template, then
//! calls this binary with the component and the sample-app contract cases. This binary does the
//! part that must run against the real host:
//!
//! 1. **Parse and validate** the component the way `spot-wasm` does (`WasmHost::compile`).
//! 2. **Link it with the granted capabilities** (`WasmHost::instantiate`): a component that
//!    imports anything the host does not provide — any WASI interface, an unknown function — is
//!    refused with `wasm.capability_denied`, naming the import. This is the honest "cannot run
//!    here" result, recorded as `BLOCKED(instantiate, <reason>)`.
//! 3. **Run the contract cases** from a JSON file: each case is one `on-event` call, compared by
//!    the world's `result<string, error>` shape (an `ok` string, or the app's own error record).
//!
//! The result is one JSON document on stdout (and to `--out`), and the process exits 0 for every
//! component outcome — a blocked component is a result, not a failure. Exit 2 is reserved for a
//! usage error. The exit code never decides support: `run.py` reads the JSON.
//!
//! # The sample app
//!
//! The templates all implement the same small app over `spot-wasm`'s test world
//! (`crates/framework/spot-wasm/wit/test-world.wit`): one DocType (`time_entry`), one lifecycle
//! step (`sample:check_hours`, the 0.25–24 hours range) and one script-callable function
//! (`sample:double_hours`). The event string is a JSON envelope; the cases live in
//! `tools/sdk-conformance/cases.json` and are the single source of truth for every language.
//!
//! Time is never read from the wall clock: the runner's `Imports::now` returns a fixed
//! invocation time, exactly as the host passes one in.

use std::process::ExitCode;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spot_wasm::{GrantedCapabilities, HostConfig, Imports, Limits, LogLevel, WasmError, WasmHost};

/// The one export the world declares.
const EXPORT: &str = "on-event";

/// The invocation time every case sees, in microseconds since the Unix epoch. The host owns the
/// clock; the runner never reads the machine's.
const INVOCATION_NOW_MICROS: i64 = 1_700_000_000_000_000;

/// The DocType the sample app may read. `get-doc` needs read access to at least one DocType to be
/// linked at all; the sample app itself does not call it, but granting it lets a guest that does
/// import `get-doc` still link, so a language is not blocked for importing an allowed function.
const SAMPLE_DOCTYPE: &str = "time_entry";

/// One contract case, as it appears in `cases.json`.
#[derive(Debug, Clone, Deserialize)]
struct Case {
    /// A stable name for the case, shown in the matrix.
    name: String,
    /// The `event` string handed to `on-event`.
    event: String,
    /// What the world should do: `ok` (a returned string) or `err` (the app's error record).
    expect: Expect,
    /// For `ok`: a substring the returned string must contain (empty means any string).
    #[serde(default)]
    contains: Option<String>,
    /// For `err`: the `code` the error record must carry.
    #[serde(default)]
    code: Option<String>,
    /// For `err`: a substring the `message` must contain (empty means any message).
    #[serde(default)]
    message_contains: Option<String>,
}

/// The two outcomes of `on-event`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Expect {
    /// `ok(string)`.
    Ok,
    /// `err({code, message})`.
    Err,
}

/// What one case produced.
#[derive(Debug, Clone, Serialize)]
struct CaseResult {
    /// The case name.
    name: String,
    /// `ok` or `err`.
    expected: String,
    /// What actually happened: `ok`, `err`, or `call-failed`.
    actual: String,
    /// Whether the outcome matched the expectation.
    passed: bool,
    /// A human explanation of a mismatch (or the returned value on a pass).
    detail: String,
}

/// A blocked step: the language could not be run here, and why.
#[derive(Debug, Clone, Serialize)]
struct Blocked {
    /// The step that stopped: `parse`, `instantiate` or `call`.
    step: String,
    /// Why, in one sentence.
    reason: String,
}

/// The whole result document.
#[derive(Debug, Clone, Serialize)]
struct Report {
    /// The language the component was built from.
    language: String,
    /// Whether the component parsed and linked.
    instantiated: bool,
    /// The SHA-256 of the component bytes (`spot-wasm`'s content hash), when it parsed.
    component_sha256: Option<String>,
    /// The component size in bytes.
    binary_bytes: usize,
    /// The component's top-level imports, sorted, when it parsed.
    imports: Vec<String>,
    /// The component's top-level exports, sorted, when it parsed.
    exports: Vec<String>,
    /// The per-case results.
    cases: Vec<CaseResult>,
    /// Every case passed, or the blocked step when the component could not run.
    passed: bool,
    /// Why the component could not be run, when it could not.
    blocked: Option<Blocked>,
    /// Anything a reader needs to know (the granted capabilities, the import check).
    notes: Vec<String>,
}

/// Parses `--flag value` pairs; `--help` prints usage.
fn parse_args() -> Result<Args, String> {
    let mut component: Option<String> = None;
    let mut cases: Option<String> = None;
    let mut out: Option<String> = None;
    let mut language = "unknown".to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--component" => component = args.next(),
            "--cases" => cases = args.next(),
            "--out" => out = args.next(),
            "--language" => language = args.next().unwrap_or_else(|| "unknown".to_owned()),
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(Args {
        component: component.ok_or("--component is required")?,
        cases: cases.ok_or("--cases is required")?,
        out,
        language,
    })
}

/// The parsed command line.
struct Args {
    /// The component to test.
    component: String,
    /// The contract cases JSON.
    cases: String,
    /// Where to also write the report, when given.
    out: Option<String>,
    /// The language name for the report.
    language: String,
}

/// Prints usage.
fn print_usage() {
    println!(
        "spot-sdk-conformance --component <file.wasm> --cases <cases.json> \
         [--out <report.json>] [--language <name>]"
    );
}

/// The host side of the world. The sample app needs no imports; the runner answers the world's
/// three imports so a guest that does import them still links.
#[derive(Debug, Default)]
struct SampleImports;

impl Imports for SampleImports {
    fn log(&self, _level: LogLevel, _message: &str) {}

    fn now(&self) -> i64 {
        INVOCATION_NOW_MICROS
    }

    fn get_doc(&self, _doctype: &str, _id: &str) -> Option<String> {
        None
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("spot-sdk-conformance: {err}");
            print_usage();
            return ExitCode::from(2);
        }
    };
    match run(&args).await {
        Ok(report) => {
            let text = match serde_json::to_string_pretty(&report) {
                Ok(text) => text,
                Err(err) => {
                    eprintln!("spot-sdk-conformance: cannot render the report: {err}");
                    return ExitCode::FAILURE;
                }
            };
            if let Some(path) = &args.out
                && let Err(err) = std::fs::write(path, &text)
            {
                eprintln!("spot-sdk-conformance: cannot write {path}: {err}");
                return ExitCode::FAILURE;
            }
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("spot-sdk-conformance: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Runs the whole check, returning a report for every component outcome.
async fn run(args: &Args) -> Result<Report, String> {
    let bytes = std::fs::read(&args.component)
        .map_err(|e| format!("cannot read {}: {e}", args.component))?;
    let cases_text = std::fs::read_to_string(&args.cases)
        .map_err(|e| format!("cannot read {}: {e}", args.cases))?;
    let cases: Vec<Case> = serde_json::from_str(&cases_text)
        .map_err(|e| format!("cannot parse {}: {e}", args.cases))?;

    let mut notes = vec![
        format!(
            "granted: read {SAMPLE_DOCTYPE} (so `get-doc` links); log and now carry no authority"
        ),
        format!("invocation time fixed at {INVOCATION_NOW_MICROS} µs; no wall clock is read"),
    ];

    let host = WasmHost::new(HostConfig::default())
        .map_err(|e| format!("cannot build the wasm host: {e}"))?;

    // 1. Parse and validate.
    let compiled = match host.compile(&bytes) {
        Ok(compiled) => compiled,
        Err(err) => {
            return Ok(blocked_report(
                args,
                bytes.len(),
                "parse",
                &format!("not a WebAssembly component spot-wasm accepts: {err}"),
                notes,
            ));
        }
    };
    let imports: Vec<String> = compiled.imports().to_vec();
    let exports: Vec<String> = compiled.exports().to_vec();
    notes.push(format!("imports: {}", join_or_none(&imports)));
    notes.push(format!("exports: {}", join_or_none(&exports)));

    if !exports.iter().any(|e| e == EXPORT) {
        return Ok(blocked_report(
            args,
            bytes.len(),
            "export",
            &format!(
                "the component does not export `{EXPORT}` (exports: {})",
                join_or_none(&exports)
            ),
            notes,
        ));
    }

    // 2. Link with the granted capabilities: anything else is refused here, by name.
    let mut caps = GrantedCapabilities::default();
    caps.doctypes.read.push(SAMPLE_DOCTYPE.to_owned());
    let instance = match host.instantiate(&compiled, caps) {
        Ok(instance) => instance,
        Err(err) => {
            return Ok(blocked_report(
                args,
                bytes.len(),
                "instantiate",
                &format!("{err}"),
                notes,
            ));
        }
    };

    // 3. Run the cases.
    let imports_impl: Arc<dyn Imports> = Arc::new(SampleImports);
    let mut results = Vec::with_capacity(cases.len());
    let mut all_passed = true;
    for case in &cases {
        let result = run_case(&instance, case, Arc::clone(&imports_impl)).await;
        all_passed &= result.passed;
        results.push(result);
    }

    Ok(Report {
        language: args.language.clone(),
        instantiated: true,
        component_sha256: Some(compiled.content_hash().to_owned()),
        binary_bytes: bytes.len(),
        imports,
        exports,
        cases: results,
        passed: all_passed,
        blocked: None,
        notes,
    })
}

/// Runs one case and compares the outcome.
async fn run_case(
    instance: &spot_wasm::AppInstance,
    case: &Case,
    imports: Arc<dyn Imports>,
) -> CaseResult {
    let expected = match case.expect {
        Expect::Ok => "ok",
        Expect::Err => "err",
    };
    let outcome = instance
        .call(EXPORT, &json!([case.event]), Limits::default(), imports)
        .await;
    match (case.expect, outcome) {
        (Expect::Ok, Ok(Value::String(returned))) => {
            let want = case.contains.as_deref().unwrap_or("");
            let passed = returned.contains(want);
            CaseResult {
                name: case.name.clone(),
                expected: expected.to_owned(),
                actual: "ok".to_owned(),
                passed,
                detail: if passed {
                    format!("returned {returned}")
                } else {
                    format!("returned {returned}, which does not contain {want:?}")
                },
            }
        }
        (Expect::Ok, Ok(other)) => CaseResult {
            name: case.name.clone(),
            expected: expected.to_owned(),
            actual: "ok".to_owned(),
            passed: false,
            detail: format!("`{EXPORT}` returned {other}, not a string"),
        },
        (Expect::Err, Err(WasmError::App(error))) => {
            let code = error.get("code").and_then(Value::as_str).unwrap_or("");
            let message = error.get("message").and_then(Value::as_str).unwrap_or("");
            let want_code = case.code.as_deref().unwrap_or("");
            let want_message = case.message_contains.as_deref().unwrap_or("");
            let passed = code == want_code && message.contains(want_message);
            CaseResult {
                name: case.name.clone(),
                expected: expected.to_owned(),
                actual: "err".to_owned(),
                passed,
                detail: if passed {
                    format!("refused with {code}")
                } else {
                    format!("refused with {code} ({message}), wanted code {want_code:?}")
                },
            }
        }
        (expect, outcome) => {
            let actual = match &outcome {
                Ok(_) => "ok".to_owned(),
                Err(WasmError::App(_)) => "err".to_owned(),
                Err(err) => format!("call-failed: {err}"),
            };
            let want = match expect {
                Expect::Ok => "ok",
                Expect::Err => "err",
            };
            let detail = match outcome {
                Err(err) => format!("{err}"),
                Ok(value) => format!("returned {value}"),
            };
            CaseResult {
                name: case.name.clone(),
                expected: want.to_owned(),
                actual,
                passed: false,
                detail,
            }
        }
    }
}

/// A report for a component that parsed but could not be linked or does not carry the export.
fn blocked_report(
    args: &Args,
    binary_bytes: usize,
    step: &str,
    reason: &str,
    notes: Vec<String>,
) -> Report {
    Report {
        language: args.language.clone(),
        instantiated: false,
        component_sha256: None,
        binary_bytes,
        imports: Vec::new(),
        exports: Vec::new(),
        cases: Vec::new(),
        passed: false,
        blocked: Some(Blocked {
            step: step.to_owned(),
            reason: reason.to_owned(),
        }),
        notes,
    }
}

/// Joins names with commas, or `none`.
fn join_or_none(names: &[String]) -> String {
    if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    }
}
