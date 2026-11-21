//! The `spot-support-eval` command line: run a suite, write the JSON result and the Markdown
//! report, and exit non-zero when the zero-harm gate blocks.
//!
//! This is the CI entry point. The optional model driver is a library API (see
//! [`spot_support_eval::agent::LlmAgent`]) and is deliberately not wired to a provider here: a
//! real model needs an operator's key and configuration, and must never run in CI.

use std::path::PathBuf;
use std::process::ExitCode;

use spot_support_eval::report;
use spot_support_eval::runner::{RunConfig, Suite, corpus_root, run_scripted, validate};
use spot_support_eval::{DEFAULT_MIN_FIX_RATE_BP, Result};

/// The parsed command.
enum Command {
    /// Run a suite.
    Run {
        /// The suite name.
        suite: Suite,
        /// Where to write the JSON result.
        out: Option<PathBuf>,
        /// Where to write the Markdown report.
        report: Option<PathBuf>,
        /// The caller's instant.
        now: i64,
        /// The fix-rate floor in basis points.
        floor: u32,
    },
    /// Validate a corpus without running it.
    Validate {
        /// The suite name.
        suite: Suite,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("spot-support-eval: {error}");
            ExitCode::from(2)
        }
    }
}

/// Parses and runs the command.
fn run() -> Result<ExitCode> {
    let Some(command) = parse(std::env::args().skip(1).collect())? else {
        print_help();
        return Ok(ExitCode::SUCCESS);
    };
    let root = corpus_root();
    match command {
        Command::Validate { suite } => {
            let (corpus, _registry) = validate(&root, suite)?;
            println!(
                "{} incident(s) in the `{}` suite are valid",
                corpus.len(),
                suite.as_str()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Run {
            suite,
            out,
            report,
            now,
            floor,
        } => {
            let config = RunConfig {
                suite,
                now,
                min_fix_rate_bp: floor,
            };
            let result = run_scripted(&root, &config)?;
            println!("{}", report::verdict(&result));
            if let Some(path) = out {
                let text = serde_json::to_string_pretty(&result).map_err(|error| {
                    spot_support_eval::EvalError::Output {
                        path: path.clone(),
                        detail: error.to_string(),
                    }
                })?;
                write(&path, &text)?;
            }
            if let Some(path) = report {
                write(&path, &report::render(&result))?;
            }
            if result.summary.gate_passed {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(1))
            }
        }
    }
}

/// Parses the arguments.
fn parse(args: Vec<String>) -> Result<Option<Command>> {
    let Some(head) = args.first().map(String::as_str) else {
        return Ok(None);
    };
    let mut suite = Suite::All;
    let mut out = None;
    let mut report_path = None;
    let mut now = RunConfig::default().now;
    let mut floor = DEFAULT_MIN_FIX_RATE_BP;
    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args.get(index + 1).cloned();
        match flag {
            "--suite" => {
                let Some(value) = value else {
                    return Err(invalid("--suite needs a value"));
                };
                suite = Suite::parse(&value)
                    .ok_or_else(|| invalid("--suite must be incidents, injections or all"))?;
                index += 2;
            }
            "--out" => {
                let Some(value) = value else {
                    return Err(invalid("--out needs a path"));
                };
                out = Some(PathBuf::from(value));
                index += 2;
            }
            "--report" => {
                let Some(value) = value else {
                    return Err(invalid("--report needs a path"));
                };
                report_path = Some(PathBuf::from(value));
                index += 2;
            }
            "--now" => {
                let Some(value) = value else {
                    return Err(invalid("--now needs unix seconds"));
                };
                now = value
                    .parse::<i64>()
                    .map_err(|_| invalid("--now must be unix seconds"))?;
                index += 2;
            }
            "--min-fix-rate-bp" => {
                let Some(value) = value else {
                    return Err(invalid("--min-fix-rate-bp needs basis points"));
                };
                floor = value
                    .parse::<u32>()
                    .map_err(|_| invalid("--min-fix-rate-bp must be an integer"))?;
                index += 2;
            }
            "--help" | "-h" => return Ok(None),
            other => return Err(invalid(&format!("unknown argument `{other}`"))),
        }
    }
    match head {
        "run" => Ok(Some(Command::Run {
            suite,
            out,
            report: report_path,
            now,
            floor,
        })),
        "validate" => Ok(Some(Command::Validate { suite })),
        other => Err(invalid(&format!("unknown command `{other}`"))),
    }
}

/// Writes `text` to `path`.
fn write(path: &PathBuf, text: &str) -> Result<()> {
    std::fs::write(path, text).map_err(|error| spot_support_eval::EvalError::Output {
        path: path.clone(),
        detail: error.to_string(),
    })
}

/// An invalid command line, as the harness's typed error.
fn invalid(detail: &str) -> spot_support_eval::EvalError {
    spot_support_eval::EvalError::Simulation {
        detail: format!("command line: {detail}"),
    }
}

/// Prints the usage.
fn print_help() {
    println!(
        "spot-support-eval — the support evaluation harness\n\n\
         usage:\n  \
         spot-support-eval run [--suite incidents|injections|all] [--out result.json]\n                      \
         [--report report.md] [--now UNIX] [--min-fix-rate-bp BP]\n  \
         spot-support-eval validate [--suite incidents|injections|all]\n\n\
         `run` exits 0 when the zero-harm gate passes and 1 when it blocks."
    );
}
