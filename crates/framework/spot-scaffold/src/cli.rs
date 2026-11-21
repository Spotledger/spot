//! The `spot-scaffold` command line: `app new`, `doctype add`, `controller add`,
//! `script add`; `--dry-run`, `--force`, `--out`, `--json`; exit codes 0 ok / 2 usage /
//! 3 refused; no network and no child process.

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};

use crate::error::{Result, ScaffoldError};
use crate::generate::app::AppSpec;
use crate::generate::controller::ControllerSpec;
use crate::generate::doctype::DoctypeSpec;
use crate::generate::script::ScriptSpec;
use crate::names::{AppName, DocTypeName};
use crate::tree::{FileTree, WritePolicy};

/// The exit code for a usage problem.
pub const USAGE_EXIT: u8 = 2;

/// The command line.
#[derive(Debug, Parser)]
#[command(
    name = "spot-scaffold",
    version,
    about = "Generate a Spotledger app, DocType, controller or script from templates",
    after_help = "Exit codes: 0 ok, 2 usage, 3 refused."
)]
pub struct Cli {
    /// The subcommand.
    #[command(subcommand)]
    pub command: Command,
}

/// The top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create and change apps.
    App {
        /// What to do.
        #[command(subcommand)]
        command: AppCommand,
    },
    /// Add DocTypes.
    Doctype {
        /// What to do.
        #[command(subcommand)]
        command: DoctypeCommand,
    },
    /// Add native controllers.
    Controller {
        /// What to do.
        #[command(subcommand)]
        command: ControllerCommand,
    },
    /// Add TypeScript scripts.
    Script {
        /// What to do.
        #[command(subcommand)]
        command: ScriptCommand,
    },
}

/// `app ...`.
#[derive(Debug, Subcommand)]
pub enum AppCommand {
    /// Create a new app project.
    New(NewApp),
}

/// `doctype ...`.
#[derive(Debug, Subcommand)]
pub enum DoctypeCommand {
    /// Add a DocType, a starter grant and a load test.
    Add(AddDoctype),
}

/// `controller ...`.
#[derive(Debug, Subcommand)]
pub enum ControllerCommand {
    /// Add a native controller skeleton and its manifest rows.
    Add(AddController),
}

/// `script ...`.
#[derive(Debug, Subcommand)]
pub enum ScriptCommand {
    /// Add a TypeScript route handler and its manifest rows.
    Add(AddScript),
}

/// The options every leaf command shares.
#[derive(Debug, Args)]
pub struct Common {
    /// Print the tree and the byte count without writing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Replace existing files.
    #[arg(long)]
    pub force: bool,
    /// The project directory (the packable app lives in `<out>/app`).
    #[arg(long, default_value = ".")]
    pub out: PathBuf,
    /// Print the created files as JSON.
    #[arg(long)]
    pub json: bool,
}

/// `app new`.
#[derive(Debug, Args)]
pub struct NewApp {
    /// The app's storage name (lowercase letters, digits and `_`).
    pub name: String,
    /// The human title (default: the name).
    #[arg(long)]
    pub title: Option<String>,
    /// The semver version (default: 0.1.0).
    #[arg(long)]
    pub version: Option<String>,
    /// The author recorded in the README.
    #[arg(long)]
    pub author: Option<String>,
    /// The SPDX licence id recorded in the README (no default: without it the README says the
    /// author has not chosen a licence yet).
    #[arg(long)]
    pub licence: Option<String>,
    /// Add the native Rust controller crate skeleton.
    #[arg(long)]
    pub with_controller: bool,
    /// Add a TypeScript script skeleton.
    #[arg(long)]
    pub with_script: bool,
    /// Add one example field per common kind to the starter DocType.
    #[arg(long)]
    pub example_fields: bool,
    /// The shared options.
    #[command(flatten)]
    pub common: Common,
}

/// `doctype add`.
#[derive(Debug, Args)]
pub struct AddDoctype {
    /// The app the DocType belongs to.
    pub app: String,
    /// The human label, such as `Sales Invoice`.
    pub label: String,
    /// An explicit storage name (default: derived from the label).
    #[arg(long)]
    pub name: Option<String>,
    /// Add one example field per common kind.
    #[arg(long)]
    pub example_fields: bool,
    /// The starter role (default: `<label> Manager`).
    #[arg(long)]
    pub role: Option<String>,
    /// The shared options.
    #[command(flatten)]
    pub common: Common,
}

/// `controller add`.
#[derive(Debug, Args)]
pub struct AddController {
    /// The app the controller belongs to.
    pub app: String,
    /// The DocType the controller controls.
    pub doctype: String,
    /// The shared options.
    #[command(flatten)]
    pub common: Common,
}

/// `script add`.
#[derive(Debug, Args)]
pub struct AddScript {
    /// The app the script belongs to.
    pub app: String,
    /// The script's storage name (also its route and step).
    pub name: String,
    /// The shared options.
    #[command(flatten)]
    pub common: Common,
}

/// Runs the command line and returns the process exit code.
#[must_use]
pub fn run(cli: &Cli) -> u8 {
    match execute(cli) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("{error}");
            error.exit_code()
        }
    }
}

/// Executes the command, printing the outcome.
fn execute(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::App {
            command: AppCommand::New(args),
        } => new_app(args),
        Command::Doctype {
            command: DoctypeCommand::Add(args),
        } => add_doctype(args),
        Command::Controller {
            command: ControllerCommand::Add(args),
        } => add_controller(args),
        Command::Script {
            command: ScriptCommand::Add(args),
        } => add_script(args),
    }
}

/// `app new`.
fn new_app(args: &NewApp) -> Result<()> {
    let name = AppName::parse(&args.name)?;
    let mut spec = AppSpec::new(name);
    if let Some(title) = &args.title {
        spec.title = title.clone();
    }
    if let Some(version) = &args.version {
        spec.version = version.clone();
    }
    if let Some(author) = &args.author {
        spec.author = author.clone();
    }
    if let Some(licence) = &args.licence {
        spec.licence = Some(licence.clone());
    }
    spec.with_controller = args.with_controller;
    spec.with_script = args.with_script;
    spec.example_fields = args.example_fields;
    let tree = crate::generate::app::generate(&spec)?;
    finish(&tree, &args.common, "app")
}

/// `doctype add`.
fn add_doctype(args: &AddDoctype) -> Result<()> {
    let spec = DoctypeSpec {
        app: AppName::parse(&args.app)?,
        label: args.label.clone(),
        name: args.name.clone(),
        example_fields: args.example_fields,
        role: args.role.clone(),
    };
    let tree = crate::generate::doctype::generate(&args.common.out, &spec)?;
    finish(&tree, &args.common, "app")
}

/// `controller add`.
fn add_controller(args: &AddController) -> Result<()> {
    let spec = ControllerSpec {
        app: AppName::parse(&args.app)?,
        doctype: DocTypeName::parse(&args.doctype)?,
    };
    let tree = crate::generate::controller::generate(&args.common.out, &spec)?;
    finish(&tree, &args.common, "app")
}

/// `script add`.
fn add_script(args: &AddScript) -> Result<()> {
    let spec = ScriptSpec {
        app: AppName::parse(&args.app)?,
        name: args.name.clone(),
    };
    let tree = crate::generate::script::generate(&args.common.out, &spec)?;
    finish(&tree, &args.common, "app")
}

/// Prints a dry run, or writes the tree and prints the next command.
fn finish(tree: &FileTree, common: &Common, app_subdir: &str) -> Result<()> {
    if common.dry_run {
        if common.json {
            println!("{}", tree.to_json());
        } else {
            print!("{}", tree.dry_run());
        }
        return Ok(());
    }
    let policy = if common.force {
        WritePolicy::force()
    } else {
        WritePolicy::new()
    };
    let report = tree.write_to_disk(&common.out, &policy)?;
    let app_root = common.out.join(app_subdir);
    if common.json {
        let value = serde_json::json!({
            "files": report.files,
            "bytes": report.bytes,
            "next": format!("spot-pack check {}", app_root.display()),
        });
        println!("{value}");
    } else {
        println!(
            "wrote {} files ({} bytes)",
            report.files.len(),
            report.bytes
        );
        for file in &report.files {
            println!("  {file}");
        }
        println!("now run `spot-pack check {}`", app_root.display());
    }
    Ok(())
}

/// The app directory inside a project, for a caller that wants it.
#[must_use]
pub fn app_root(project: &Path) -> PathBuf {
    project.join(crate::generate::APP_DIR)
}

/// A usage error, for a caller that builds a [`Cli`] in code.
#[must_use]
pub fn usage(message: impl Into<String>) -> ScaffoldError {
    ScaffoldError::new(crate::codes::USAGE, message)
}
