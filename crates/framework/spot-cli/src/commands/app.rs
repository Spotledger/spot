//! `spot app new|check|pack|verify|inspect`: the developer commands of the `spot` binary
//! (WP-DX-01.T3). `new` forwards to `spot-scaffold`, the other four to `spot-pack`; the sibling
//! files `doctype.rs`, `controller.rs` and `script.rs` forward the rest of the scaffolder here
//! through [`forward_scaffold`]. Guide: `docs/running-spot/developer_commands.md`.
//!
//! # Where the arguments come from
//!
//! The scaffolder's command line is a `clap` derive in `spot_scaffold::cli`; its subcommands are
//! added to the `spot` command with the library's own `Subcommand::augment_subcommands`, and
//! parsed back with its own `FromArgMatches`, so a flag added to the scaffolder appears here
//! without an edit. `spot-pack`'s command line lives in its binary (`main.rs`), which a library
//! cannot reach: the four pack subcommands are declared here with the same names, flags and
//! meanings (the one place an argument is described twice; see the report's open questions).
//!
//! # The layout
//!
//! `app new` writes the layout `spot install-app` reads: `app/app.toml` (never `manifest.toml`),
//! `app/doctypes/`, `app/permissions/`. `spot-pack` knows the manifest as `manifest.toml`, so
//! `check` and `pack` read an app directory that has `app.toml` through a private copy under
//! that name (see "the installable layout" below); the bundle carries `manifest.toml` as ever.
//!
//! # Exit codes
//!
//! The registry's numbers (0 done, 1 usage, 2 refused, 3 failed) apply. The scaffolder's and the
//! packer's own numbers map as the guide's table says:
//!
//! | source | meaning | `spot` |
//! |---|---|---|
//! | scaffold 0, pack 0 | done | 0 |
//! | scaffold 2 (`scaffold.usage`), pack 2 for a wrong argument | usage | 1 |
//! | scaffold 3 (any other `scaffold.*`), pack 1 (problems, a refused signature) | refused | 2 |
//! | `scaffold.io`, pack 2 for a file that cannot be read or written | failed | 3 |
//!
//! `clap`'s own parse errors (an unknown flag, a missing argument) keep `clap`'s exit code 2,
//! as for every command of the binary.
//!
//! No command here needs a `common.toml`, a site or a database; none runs a process or touches
//! the network. Output paths are relative, free of `..` and of symlinks, and nothing is
//! replaced without `--force`. A signing key file is never printed, in output or in an error.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use clap::{Arg, ArgAction, ArgMatches, Command, FromArgMatches, Subcommand};
use spot_bundle_signing::{
    BundleSignature, CertificateChain, PublicKey, RevocationList, SignedCertificate, TrustAnchor,
    TrustedRoot, VerifyMode, VerifyPolicy,
};
use spot_pack::report;
use spot_pack::signing::{FileKeyProvider, install_policy, sign_packed, verify_packed};
use spot_pack::{PackOptions, PackProblem};
use spot_scaffold::cli::{
    AppCommand, Command as ScaffoldCommand, Common, ControllerCommand, DoctypeCommand,
    ScriptCommand,
};
use spot_scaffold::generate::app::AppSpec;
use spot_scaffold::generate::controller::ControllerSpec;
use spot_scaffold::generate::doctype::DoctypeSpec;
use spot_scaffold::generate::script::ScriptSpec;
use spot_scaffold::{AppName, DocTypeName, FileTree, ScaffoldError, WritePolicy};

use super::{CommandCtx, Exit};

pub const NAME: &str = "app";

/// The most a sidecar file (signature, certificate, trust roots, revocation list) may weigh.
const SIDECAR_MAX_BYTES: u64 = 1024 * 1024;

pub fn command() -> Command {
    let command = Command::new(NAME)
        .about("Create, check, pack, verify and inspect an app")
        .after_help(
            "Exit codes: 0 done, 1 usage, 2 refused (problems, nothing changed), 3 failed. \
             Needs no site and no database.",
        )
        .subcommand_required(true)
        .arg_required_else_help(true);
    // `new` comes from the scaffolder's own derive.
    AppCommand::augment_subcommands(command)
        .subcommand(check_command())
        .subcommand(pack_command())
        .subcommand(verify_command())
        .subcommand(inspect_command())
}

fn json_arg() -> Arg {
    Arg::new("json")
        .long("json")
        .action(ArgAction::SetTrue)
        .help("Print machine-readable JSON instead of text")
}

fn path_arg(name: &'static str, long: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(long)
        .value_name("FILE")
        .value_parser(clap::value_parser!(PathBuf))
        .help(help)
}

fn check_command() -> Command {
    Command::new("check")
        .about("Check an app directory and report every problem at once")
        .arg(
            Arg::new("dir")
                .value_name("DIR")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("The app directory"),
        )
        .arg(
            Arg::new("native")
                .long("native")
                .value_name("NAMES")
                .help("Native controller names this build provides, comma-separated"),
        )
        .arg(json_arg())
}

fn pack_command() -> Command {
    Command::new("pack")
        .about("Pack a checked app into one deterministic bundle")
        .arg(
            Arg::new("dir")
                .value_name("DIR")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("The app directory"),
        )
        .arg(
            Arg::new("out")
                .short('o')
                .long("out")
                .value_name("FILE")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("Where the bundle is written (relative, no `..`, no symlink)"),
        )
        .arg(path_arg(
            "sign",
            "sign",
            "The key file to sign with (32 seed bytes, base64; a regular file, mode 0600 or tighter)",
        ))
        .arg(
            path_arg("certificate", "certificate", "The publisher certificate file, with --sign")
                .requires("sign"),
        )
        .arg(
            Arg::new("signed-at")
                .long("signed-at")
                .value_name("SECONDS")
                .value_parser(clap::value_parser!(i64))
                .requires("sign")
                .help("The signing time as UTC seconds (default: the clock's now)"),
        )
        .arg(
            Arg::new("force")
                .long("force")
                .action(ArgAction::SetTrue)
                .help("Replace an existing bundle or signature file"),
        )
        .arg(json_arg())
}

fn verify_command() -> Command {
    Command::new("verify")
        .about("Verify a bundle against its signature, certificate, trust roots and revocations")
        .arg(
            Arg::new("file")
                .value_name("BUNDLE")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("The bundle file"),
        )
        .arg(path_arg(
            "signature",
            "signature",
            "The signature file `app pack --sign` wrote",
        ))
        .arg(path_arg(
            "certificate",
            "certificate",
            "The publisher certificate file",
        ))
        .arg(path_arg(
            "trust",
            "trust",
            "The trust-roots file: one root public key per line (base64)",
        ))
        .arg(path_arg(
            "revocations",
            "revocations",
            "The revocation list file",
        ))
        .arg(
            Arg::new("now")
                .long("now")
                .value_name("SECONDS")
                .value_parser(clap::value_parser!(i64))
                .allow_negative_numbers(true)
                .help("The check time as UTC seconds (default: the clock's now)"),
        )
        .arg(
            Arg::new("mode")
                .long("mode")
                .value_name("MODE")
                .default_value("install")
                .help("`install` (default) or `startup`"),
        )
        .arg(
            Arg::new("allow-unsigned-development")
                .long("allow-unsigned-development")
                .action(ArgAction::SetTrue)
                .help(
                    "Open a bundle that has no signature, on a development machine only: it \
                     proves nothing about who made it",
                ),
        )
        .arg(json_arg())
}

fn inspect_command() -> Command {
    Command::new("inspect")
        .about("Show a bundle: manifest identity, digest, entries, capabilities, publisher")
        .arg(
            Arg::new("file")
                .value_name("BUNDLE")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf))
                .help("The bundle file"),
        )
        .arg(path_arg(
            "signature",
            "signature",
            "The signature file, to show the publisher",
        ))
        .arg(path_arg(
            "certificate",
            "certificate",
            "The publisher certificate file",
        ))
        .arg(json_arg())
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let now = ctx.clock().now_utc().unix_timestamp();
    let outcome = match matches.subcommand() {
        Some(("new", _)) => {
            let action = AppCommand::from_arg_matches(matches)
                .map_err(|e| spot_cli::Error(e.to_string()))?;
            let wrapped = ScaffoldCommand::App { command: action };
            return forward_scaffold(&wrapped, ctx);
        }
        Some(("check", sub)) => check(sub),
        Some(("pack", sub)) => pack(sub, now),
        Some(("verify", sub)) => verify(sub, now),
        Some(("inspect", sub)) => inspect(sub),
        Some((other, _)) => Err(Failure::Usage(format!("unknown app command `{other}`"))),
        None => Err(Failure::Usage("name an app command".to_owned())),
    };
    conclude(outcome, ctx)
}

// --- the scaffolder -----------------------------------------------------------------------------

/// Runs one scaffolder command (`app new`, `doctype add`, `controller add`, `script add`) with its
/// output on the context's writers and the exit-code mapping of this file's table.
pub fn forward_scaffold(action: &ScaffoldCommand, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let common = match action {
        ScaffoldCommand::App {
            command: AppCommand::New(args),
        } => &args.common,
        ScaffoldCommand::Doctype {
            command: DoctypeCommand::Add(args),
        } => &args.common,
        ScaffoldCommand::Controller {
            command: ControllerCommand::Add(args),
        } => &args.common,
        ScaffoldCommand::Script {
            command: ScriptCommand::Add(args),
        } => &args.common,
    };
    if let Err((code, message)) = check_output_path(&common.out) {
        writeln!(ctx.err(), "{code}: {message}")?;
        return Ok(Exit::Refused);
    }
    if let Some(message) = populated_app_root(action) {
        writeln!(ctx.err(), "{message}")?;
        return Ok(Exit::Refused);
    }
    let staged = match stage_project(action, common) {
        Ok(staged) => staged,
        Err(error) => return scaffold_failure(&error, ctx),
    };
    let project = staged
        .as_ref()
        .map_or(common.out.as_path(), |stage| stage.root.as_path());
    let tree = match scaffold_tree(action, project) {
        Ok(tree) => tree,
        Err(error) => return scaffold_failure(&error, ctx),
    };
    // The generators speak the packer's file name (`manifest.toml`); what lands on disk is the
    // layout `spot install-app` reads (`app.toml`). A project that still has the old name on
    // disk keeps it, so nothing appears twice.
    let installable = matches!(
        action,
        ScaffoldCommand::App {
            command: AppCommand::New(_)
        }
    ) || staged.is_some();
    let tree = if installable {
        if staged.as_ref().is_some_and(|s| s.controller_key)
            && tree.get(GENERATED_MANIFEST).is_some()
        {
            let error = ScaffoldError::new(
                spot_scaffold::codes::MANIFEST,
                "app.toml carries a host-level `controller` key, which the scaffolder cannot \
                 rewrite without dropping it: add this row to app.toml by hand",
            );
            return scaffold_failure(&error, ctx);
        }
        match to_install_layout(&tree) {
            Ok(tree) => tree,
            Err(error) => return scaffold_failure(&error, ctx),
        }
    } else {
        tree
    };
    match scaffold_finish(&tree, common, ctx) {
        Ok(exit) => Ok(exit),
        Err(ScaffoldOrIo::Scaffold(error)) => scaffold_failure(&error, ctx),
        Err(ScaffoldOrIo::Io(error)) => Err(error.into()),
    }
}

/// The tree the command generates; the same calls `spot_scaffold::cli::run` makes. `project` is
/// the directory the add commands read the existing app from (the output directory, or its
/// staged copy, see [`stage_project`]).
fn scaffold_tree(action: &ScaffoldCommand, project: &Path) -> spot_scaffold::Result<FileTree> {
    match action {
        ScaffoldCommand::App {
            command: AppCommand::New(args),
        } => {
            let mut spec = AppSpec::new(AppName::parse(&args.name)?);
            if let Some(title) = &args.title {
                spec.title.clone_from(title);
            }
            if let Some(version) = &args.version {
                spec.version.clone_from(version);
            }
            if let Some(author) = &args.author {
                spec.author.clone_from(author);
            }
            if let Some(licence) = &args.licence {
                spec.licence = Some(licence.clone());
            }
            spec.with_controller = args.with_controller;
            spec.with_script = args.with_script;
            spec.example_fields = args.example_fields;
            spot_scaffold::generate::app::generate(&spec)
        }
        ScaffoldCommand::Doctype {
            command: DoctypeCommand::Add(args),
        } => {
            let spec = DoctypeSpec {
                app: AppName::parse(&args.app)?,
                label: args.label.clone(),
                name: args.name.clone(),
                example_fields: args.example_fields,
                role: args.role.clone(),
            };
            spot_scaffold::generate::doctype::generate(project, &spec)
        }
        ScaffoldCommand::Controller {
            command: ControllerCommand::Add(args),
        } => {
            let spec = ControllerSpec {
                app: AppName::parse(&args.app)?,
                doctype: DocTypeName::parse(&args.doctype)?,
            };
            spot_scaffold::generate::controller::generate(project, &spec)
        }
        ScaffoldCommand::Script {
            command: ScriptCommand::Add(args),
        } => {
            let spec = ScriptSpec {
                app: AppName::parse(&args.app)?,
                name: args.name.clone(),
            };
            spot_scaffold::generate::script::generate(project, &spec)
        }
    }
}

/// A scaffolder error, or a failure to write to the context's own writers.
enum ScaffoldOrIo {
    Scaffold(ScaffoldError),
    Io(std::io::Error),
}

impl From<std::io::Error> for ScaffoldOrIo {
    fn from(error: std::io::Error) -> Self {
        ScaffoldOrIo::Io(error)
    }
}

impl From<ScaffoldError> for ScaffoldOrIo {
    fn from(error: ScaffoldError) -> Self {
        ScaffoldOrIo::Scaffold(error)
    }
}

/// Prints a dry run, or writes the tree and prints what was written and the next command.
fn scaffold_finish(
    tree: &FileTree,
    common: &Common,
    ctx: &mut CommandCtx,
) -> Result<Exit, ScaffoldOrIo> {
    if common.dry_run {
        if common.json {
            writeln!(ctx.out(), "{}", tree.to_json())?;
        } else {
            write!(ctx.out(), "{}", tree.dry_run())?;
        }
        return Ok(Exit::Ok);
    }
    let policy = if common.force {
        WritePolicy::force()
    } else {
        WritePolicy::new()
    };
    let report = tree.write_to_disk(&common.out, &policy)?;
    let app_root = spot_scaffold::cli::app_root(&common.out);
    // `./app` for the default output directory: the command the developer types names `app`.
    let app_root = app_root
        .strip_prefix(".")
        .map_or(app_root.clone(), std::path::Path::to_path_buf);
    if common.json {
        // The scaffolder's own keys; `next` names the `spot` command the developer has.
        let value = serde_json::json!({
            "files": report.files,
            "bytes": report.bytes,
            "next": format!("spot app check {}", app_root.display()),
        });
        writeln!(ctx.out(), "{value}")?;
    } else {
        writeln!(
            ctx.out(),
            "wrote {} files ({} bytes)",
            report.files.len(),
            report.bytes
        )?;
        for file in &report.files {
            writeln!(ctx.out(), "  {file}")?;
        }
        writeln!(ctx.out(), "now run `spot app check {}`", app_root.display())?;
    }
    Ok(Exit::Ok)
}

/// The exit of a scaffolder error: usage 1, an I/O failure 3, every other refusal 2.
fn scaffold_failure(error: &ScaffoldError, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    writeln!(ctx.err(), "{error}")?;
    Ok(scaffold_exit(error.code))
}

/// The mapping of a scaffolder code onto the registry's exits (the guide's table).
pub fn scaffold_exit(code: &str) -> Exit {
    if code == spot_scaffold::codes::USAGE {
        Exit::Usage
    } else if code == spot_scaffold::codes::IO {
        Exit::Failed
    } else {
        Exit::Refused
    }
}

/// Refuses an output path that is absolute, has a `..`, or has a symlink on it (the final
/// component included). Returns the stable code and the reason.
///
/// # Errors
/// `scaffold.path_escape` or `scaffold.symlink`, with a message naming the path (never a key or
/// file content).
pub fn check_output_path(path: &Path) -> Result<(), (&'static str, String)> {
    if path.is_absolute() || path.has_root() {
        return Err((
            spot_scaffold::codes::PATH_ESCAPE,
            format!(
                "{} is absolute; an output path is relative to the current directory",
                path.display()
            ),
        ));
    }
    let mut here = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err((
                    spot_scaffold::codes::PATH_ESCAPE,
                    format!("{} leaves the current directory", path.display()),
                ));
            }
            Component::Normal(part) => {
                here.push(part);
                if std::fs::symlink_metadata(&here).is_ok_and(|m| m.file_type().is_symlink()) {
                    return Err((
                        spot_scaffold::codes::SYMLINK,
                        format!(
                            "{} is a symlink; a command never writes through one",
                            here.display()
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

// --- the installable layout ---------------------------------------------------------------------
//
// `spot install-app` reads an app directory with `app.toml` at its root (`appdir.rs`); `spot-pack`
// and the bundle know the same file as `manifest.toml`, and so do the scaffold generators, which
// are another crate's. The commands here are where the two meet: what `app new` writes is the
// installable layout, and `check` and `pack` read it through a private copy under the packer's
// name. Nothing is written back into the app directory.

/// The manifest's name in the layout `spot install-app` reads.
const INSTALL_MANIFEST: &str = "app.toml";
/// The manifest's name in the layout `spot-pack` reads and the bundle carries.
const PACK_MANIFEST: &str = "manifest.toml";
/// The manifest's path in a generated tree.
const GENERATED_MANIFEST: &str = "app/manifest.toml";
/// The README line that names the manifest file.
const README_MANIFEST_NAME: &str = "`app/manifest.toml`";

/// `tree` with the manifest at `app/app.toml` (and the README naming it), nothing else changed.
fn to_install_layout(tree: &FileTree) -> spot_scaffold::Result<FileTree> {
    let mut out = FileTree::new();
    for path in tree.paths() {
        let Some(bytes) = tree.get(path) else {
            continue;
        };
        if path == GENERATED_MANIFEST {
            out.insert("app/app.toml", bytes.to_vec())?;
        } else if path == "README.md" {
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    out.insert_text(path, &text.replace(README_MANIFEST_NAME, "`app/app.toml`"))?
                }
                Err(_) => out.insert(path, bytes.to_vec())?,
            }
        } else {
            out.insert(path, bytes.to_vec())?;
        }
    }
    Ok(out)
}

/// `app new` writes into `<out>/app`: a directory that already holds files is refused without
/// `--force`, so a new app never lands among unrelated files.
fn populated_app_root(action: &ScaffoldCommand) -> Option<String> {
    let ScaffoldCommand::App {
        command: AppCommand::New(args),
    } = action
    else {
        return None;
    };
    if args.common.dry_run || args.common.force {
        return None;
    }
    let root = spot_scaffold::cli::app_root(&args.common.out);
    let mut entries = std::fs::read_dir(&root).ok()?;
    entries.next().map(|_| {
        format!(
            "{}: {} already exists and is not empty; pass --force to write into it",
            spot_scaffold::codes::EXISTS,
            root.display()
        )
    })
}

/// A private copy of an app directory in the packer's layout, removed when dropped.
struct Stage {
    root: PathBuf,
    /// Whether `app.toml` had a host-level `[app] controller` key (the manifest parser does not
    /// know it, so the copy leaves it out).
    controller_key: bool,
}

static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Stage {
    /// A new, empty directory only this user can enter. The name is the process id and a
    /// counter; an existing path (even a planted link) is never reused.
    fn create() -> std::io::Result<Stage> {
        for _ in 0..32 {
            let n = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("spot-app-stage-{}-{n}", std::process::id()));
            match private_dir_builder().create(&root) {
                Ok(()) => {
                    return Ok(Stage {
                        root,
                        controller_key: false,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "no free staging directory",
        ))
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[cfg(unix)]
fn private_dir_builder() -> std::fs::DirBuilder {
    let mut builder = std::fs::DirBuilder::new();
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
}

#[cfg(not(unix))]
fn private_dir_builder() -> std::fs::DirBuilder {
    std::fs::DirBuilder::new()
}

/// Why a copy into a [`Stage`] stopped.
enum StageFault {
    /// A symlink at this app-relative path: an app carries its own bytes.
    Symlink(String),
    /// More bytes than the packer's total limit.
    TooLarge,
    /// Anything else: the path and the reason.
    Other(String, String),
}

impl StageFault {
    fn other(path: &str, error: impl std::fmt::Display) -> StageFault {
        StageFault::Other(path.to_owned(), error.to_string())
    }

    /// The scaffolder's form of this fault.
    fn into_scaffold(self) -> ScaffoldError {
        match self {
            StageFault::Symlink(path) => ScaffoldError::symlink(format!(
                "`{path}` is a symlink; a command never reads through one"
            )),
            StageFault::TooLarge => {
                ScaffoldError::io("the app is larger than the packer's size limit")
            }
            StageFault::Other(path, why) => {
                ScaffoldError::io(format!("`{path}` could not be copied: {why}"))
            }
        }
    }

    /// The packer's problem for this fault, in the shape `check` reports.
    fn into_problem(self) -> PackProblem {
        match self {
            StageFault::Symlink(path) => PackProblem::about(
                spot_pack::error::PATH_ESCAPE,
                path,
                "a symlink escapes the app directory; an app must carry its own bytes",
            ),
            StageFault::TooLarge => PackProblem::new(
                spot_pack::error::OVERSIZED,
                "the app's files are larger than the packer's size limit",
            ),
            StageFault::Other(path, why) => PackProblem::about(
                spot_pack::error::IO,
                path,
                format!("the file could not be copied: {why}"),
            ),
        }
    }
}

/// The manifest text the packer reads, from the text of `app.toml`: the host-level
/// `[app] controller` key (which `spot-manifest` refuses) is left out. The flag says whether it
/// was there. Text without that key is returned as written, comments and all.
fn manifest_for_pack(text: &str) -> (String, bool) {
    let Ok(mut table) = text.parse::<toml::Table>() else {
        return (text.to_owned(), false);
    };
    let had = table
        .get_mut("app")
        .and_then(toml::Value::as_table_mut)
        .is_some_and(|app| app.remove("controller").is_some());
    if !had {
        return (text.to_owned(), false);
    }
    match toml::to_string(&table) {
        Ok(rewritten) => (rewritten, true),
        Err(_) => (text.to_owned(), false),
    }
}

/// Copies the directory `from` into the existing directory `to`. At the top (`rel` empty)
/// `app.toml` becomes `manifest.toml`. With `names_only` the files are created empty (the
/// scaffolder only asks whether they exist). `budget` is the bytes still allowed.
fn copy_into(
    from: &Path,
    to: &Path,
    rel: &str,
    budget: &mut u64,
    names_only: bool,
    controller_key: &mut bool,
) -> Result<(), StageFault> {
    let listing = std::fs::read_dir(from).map_err(|e| StageFault::other(rel, e))?;
    let mut entries: Vec<_> = listing.filter_map(Result::ok).collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let source = entry.path();
        let meta = std::fs::symlink_metadata(&source).map_err(|e| StageFault::other(&child, e))?;
        if meta.file_type().is_symlink() {
            return Err(StageFault::Symlink(child));
        }
        if meta.is_dir() {
            let target = to.join(&name);
            std::fs::create_dir(&target).map_err(|e| StageFault::other(&child, e))?;
            copy_into(&source, &target, &child, budget, names_only, controller_key)?;
        } else if meta.is_file() {
            *budget = budget.checked_sub(meta.len()).ok_or(StageFault::TooLarge)?;
            if names_only {
                std::fs::File::create(to.join(&name)).map_err(|e| StageFault::other(&child, e))?;
            } else if rel.is_empty() && name == INSTALL_MANIFEST {
                let text =
                    std::fs::read_to_string(&source).map_err(|e| StageFault::other(&child, e))?;
                let (text, had) = manifest_for_pack(&text);
                *controller_key = had;
                std::fs::write(to.join(PACK_MANIFEST), text)
                    .map_err(|e| StageFault::other(&child, e))?;
            } else {
                std::fs::copy(&source, to.join(&name)).map_err(|e| StageFault::other(&child, e))?;
            }
        } else {
            return Err(StageFault::other(&child, "not a regular file"));
        }
    }
    Ok(())
}

/// The packer's view of an app directory that has the installable layout, or `None` for one
/// that does not (a directory without `app.toml` is handed to the packer as it is).
fn stage_for_pack(dir: &Path, json: bool) -> Result<Option<Stage>, Failure> {
    if std::fs::symlink_metadata(dir.join(INSTALL_MANIFEST)).is_err() {
        return Ok(None);
    }
    if std::fs::symlink_metadata(dir.join(PACK_MANIFEST)).is_ok() {
        return Err(check_failure(
            json,
            &spot_pack::CheckReport {
                manifest: None,
                problems: vec![PackProblem::about(
                    spot_pack::error::LAYOUT,
                    PACK_MANIFEST,
                    "the directory has app.toml (the file `spot install-app` reads) and \
                     manifest.toml: keep app.toml and remove manifest.toml",
                )],
            },
        ));
    }
    let mut stage = Stage::create()
        .map_err(|e| Failure::Failed(format!("a staging directory could not be made: {e}")))?;
    let mut budget = PackOptions::default().limits.max_total_bytes;
    let mut controller_key = false;
    copy_into(
        dir,
        &stage.root,
        "",
        &mut budget,
        false,
        &mut controller_key,
    )
    .map_err(|fault| {
        check_failure(
            json,
            &spot_pack::CheckReport {
                manifest: None,
                problems: vec![fault.into_problem()],
            },
        )
    })?;
    stage.controller_key = controller_key;
    Ok(Some(stage))
}

/// For an add command on a project that has `app/app.toml`: a private copy of the project the
/// generators can read (they look for `app/manifest.toml`), with the names of the files in
/// `src/` so their "already exists" checks still work. `None` for `app new`, for a project
/// without `app/app.toml`, and for one that still has the old file name (the generators read
/// that one as it is).
fn stage_project(
    action: &ScaffoldCommand,
    common: &Common,
) -> spot_scaffold::Result<Option<Stage>> {
    if matches!(
        action,
        ScaffoldCommand::App {
            command: AppCommand::New(_)
        }
    ) {
        return Ok(None);
    }
    let app = common.out.join("app");
    if std::fs::symlink_metadata(app.join(INSTALL_MANIFEST)).is_err()
        || std::fs::symlink_metadata(app.join(PACK_MANIFEST)).is_ok()
    {
        return Ok(None);
    }
    let mut stage = Stage::create()
        .map_err(|e| ScaffoldError::io(format!("a staging directory could not be made: {e}")))?;
    let mut budget = PackOptions::default().limits.max_total_bytes;
    let mut controller_key = false;
    let staged_app = stage.root.join("app");
    std::fs::create_dir(&staged_app)
        .map_err(|e| ScaffoldError::io(format!("a staging directory could not be made: {e}")))?;
    copy_into(
        &app,
        &staged_app,
        "",
        &mut budget,
        false,
        &mut controller_key,
    )
    .map_err(StageFault::into_scaffold)?;
    let src = common.out.join("src");
    if std::fs::symlink_metadata(&src).is_ok_and(|m| m.is_dir()) {
        let staged_src = stage.root.join("src");
        std::fs::create_dir(&staged_src).map_err(|e| {
            ScaffoldError::io(format!("a staging directory could not be made: {e}"))
        })?;
        copy_into(
            &src,
            &staged_src,
            "src",
            &mut budget,
            true,
            &mut controller_key,
        )
        .map_err(StageFault::into_scaffold)?;
    }
    stage.controller_key = controller_key;
    Ok(Some(stage))
}

// --- the packer ---------------------------------------------------------------------------------

/// How one pack command ended unhappily, before it becomes an exit and a message.
enum Failure {
    /// A wrong use of the command: exit 1.
    Usage(String),
    /// A policy refusal with nothing changed (an unsafe path, an unsigned bundle): exit 2.
    Refused(String),
    /// The app or signature has problems: exit 2, JSON on standard output when `--json`.
    Problems { text: String, json: bool },
    /// A file could not be read or written: exit 3.
    Failed(String),
}

type Outcome = Result<String, Failure>;

/// Writes the outcome on the context's writers and picks the exit.
fn conclude(outcome: Outcome, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    match outcome {
        Ok(text) => {
            write!(ctx.out(), "{text}")?;
            Ok(Exit::Ok)
        }
        Err(Failure::Usage(message)) => {
            writeln!(ctx.err(), "spot app: {message}")?;
            Ok(Exit::Usage)
        }
        Err(Failure::Refused(message)) => {
            writeln!(ctx.err(), "spot app: {message}")?;
            Ok(Exit::Refused)
        }
        Err(Failure::Problems { text, json }) => {
            if json {
                write!(ctx.out(), "{text}")?;
            } else {
                write!(ctx.err(), "{text}")?;
            }
            Ok(Exit::Refused)
        }
        Err(Failure::Failed(message)) => {
            writeln!(ctx.err(), "spot app: {message}")?;
            Ok(Exit::Failed)
        }
    }
}

fn required_path(matches: &ArgMatches, name: &str) -> Result<PathBuf, Failure> {
    matches
        .get_one::<PathBuf>(name)
        .cloned()
        .ok_or_else(|| Failure::Usage(format!("--{name} is required")))
}

fn check(matches: &ArgMatches) -> Outcome {
    let json = matches.get_flag("json");
    let dir = required_path(matches, "dir")?;
    if !dir.is_dir() {
        return Err(Failure::Usage(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    let names = matches.get_one::<String>("native").map(|text| {
        text.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect::<BTreeSet<String>>()
    });
    let staged = stage_for_pack(&dir, json)?;
    let target = staged.as_ref().map_or(dir.as_path(), |s| s.root.as_path());
    let report = spot_pack::check_app(target, names.as_ref());
    if report.is_ok() {
        Ok(if json {
            format!("{}\n", report::check_json(&report).unwrap_or_default())
        } else {
            format!("{}\n", report::check_text(&report).trim_end())
        })
    } else {
        Err(check_failure(json, &report))
    }
}

fn pack(matches: &ArgMatches, now: i64) -> Outcome {
    let json = matches.get_flag("json");
    let force = matches.get_flag("force");
    let dir = required_path(matches, "dir")?;
    let out = required_path(matches, "out")?;
    if !dir.is_dir() {
        return Err(Failure::Usage(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    check_output_path(&out)
        .map_err(|(code, message)| Failure::Refused(format!("{code}: {message}")))?;
    let signing = matches.get_one::<PathBuf>("sign").cloned();
    let signature_file = signing.as_ref().map(|_| signature_path_for(&out));
    for target in std::iter::once(&out).chain(signature_file.as_ref()) {
        refuse_existing(target, force)?;
    }
    if let Some(key) = &signing {
        check_key_file(key)?;
    }
    let certificate_path = match &signing {
        Some(_) => Some(matches.get_one::<PathBuf>("certificate").ok_or_else(|| {
            Failure::Usage("--sign needs --certificate (the publisher certificate)".to_owned())
        })?),
        None => None,
    };
    let options = PackOptions::default();
    let staged = stage_for_pack(&dir, json)?;
    let target = staged.as_ref().map_or(dir.as_path(), |s| s.root.as_path());
    let (_report, packed) = match spot_pack::pack_app(target, &options) {
        Ok(pair) => pair,
        Err(report) => return Err(check_failure(json, &report)),
    };
    write_file(&out, &packed.bytes, force)?;
    let mut signed_note = None;
    if let (Some(key), Some(signature_file), Some(certificate_path)) =
        (&signing, &signature_file, certificate_path)
    {
        let signed_at = matches.get_one::<i64>("signed-at").copied().unwrap_or(now);
        let bundle = packed
            .open(&options)
            .map_err(|errors| bundle_failure(json, &errors))?;
        let manifest = bundle.manifest();
        let certificate = read_certificate(certificate_path)?;
        let signature = sign_packed(
            &bundle,
            manifest.app.name.as_str(),
            &manifest.app.version.to_string(),
            &certificate,
            &FileKeyProvider::new(key),
            signed_at,
        )
        .map_err(|error| Failure::Usage(format!("signing failed: {error}")))?;
        write_file(
            signature_file,
            format!("{}\n", signature.to_json()).as_bytes(),
            force,
        )?;
        signed_note = Some((signature_file.clone(), signature.key_id.as_str().to_owned()));
    }
    Ok(pack_text(
        json,
        &out,
        &packed.digest.to_hex(),
        signed_note.as_ref(),
    ))
}

fn pack_text(json: bool, out: &Path, digest: &str, signed: Option<&(PathBuf, String)>) -> String {
    if json {
        let mut value = serde_json::json!({
            "ok": true,
            "out": out.display().to_string(),
            "digest": digest,
        });
        if let (Some((path, key_id)), Some(map)) = (signed, value.as_object_mut()) {
            map.insert("signed".to_owned(), serde_json::Value::Bool(true));
            map.insert(
                "signature".to_owned(),
                serde_json::Value::String(path.display().to_string()),
            );
            map.insert(
                "key_id".to_owned(),
                serde_json::Value::String(key_id.clone()),
            );
        }
        format!("{value}\n")
    } else {
        let mut text = format!("packed {} ({digest})\n", out.display());
        if let Some((path, key_id)) = signed {
            text.push_str(&format!("signed as {} (key {key_id})\n", path.display()));
        }
        text
    }
}

/// Where `pack --sign` writes the signature: next to the bundle, `app.sig`, the name the
/// installer looks for.
fn signature_path_for(out: &Path) -> PathBuf {
    let mut path = out.to_path_buf();
    path.set_file_name(spot_cli::bundle_install::SIGNATURE_FILE);
    path
}

/// Refuses a target that is a symlink, or exists without `--force`.
fn refuse_existing(target: &Path, force: bool) -> Result<(), Failure> {
    match std::fs::symlink_metadata(target) {
        Ok(meta) if meta.file_type().is_symlink() => Err(Failure::Refused(format!(
            "{}: {} is a symlink; a command never writes through one",
            spot_scaffold::codes::SYMLINK,
            target.display()
        ))),
        Ok(_) if !force => Err(Failure::Refused(format!(
            "{}: {} already exists; pass --force to replace it",
            spot_scaffold::codes::EXISTS,
            target.display()
        ))),
        _ => Ok(()),
    }
}

/// Writes `bytes` to `path`: a new file, or with `--force` a replaced one.
fn write_file(path: &Path, bytes: &[u8], force: bool) -> Result<(), Failure> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if force {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    let failed = |e: std::io::Error| {
        Failure::Failed(format!("{} could not be written: {e}", path.display()))
    };
    let mut file = options.open(path).map_err(failed)?;
    file.write_all(bytes).map_err(failed)
}

/// A signing key file must be a regular file only its owner can read (mode 0600 or tighter on
/// Unix). Windows has no mode to check here; the guide names the ACL rule. The content is never
/// read here and never put in a message.
fn check_key_file(path: &Path) -> Result<(), Failure> {
    let meta = std::fs::metadata(path)
        .map_err(|_| Failure::Failed(format!("the key file {} cannot be read", path.display())))?;
    if !meta.is_file() {
        return Err(Failure::Refused(format!(
            "the key file {} must be a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(Failure::Refused(format!(
                "the key file {} must be readable only by its owner (mode 0600 or tighter)",
                path.display()
            )));
        }
    }
    Ok(())
}

fn verify(matches: &ArgMatches, now: i64) -> Outcome {
    let json = matches.get_flag("json");
    let file = required_path(matches, "file")?;
    let mode = matches
        .get_one::<String>("mode")
        .map_or("install", String::as_str);
    let policy = match mode {
        "install" => install_policy(),
        "startup" => VerifyPolicy {
            mode: VerifyMode::Startup,
            allow_expired_certificate_at_startup: true,
            held_revocation_generation: 0,
        },
        other => {
            return Err(Failure::Usage(format!(
                "--mode is `install` or `startup`, not `{other}`"
            )));
        }
    };
    let options = PackOptions::default();
    let bundle = packed_from_file(&file, &options)?
        .open(&options)
        .map_err(|errors| bundle_failure(json, &errors))?;
    let digest = bundle.digest();
    let manifest = bundle.manifest();
    let Some(signature_path) = matches.get_one::<PathBuf>("signature") else {
        if matches.get_flag("allow-unsigned-development") {
            let (name, version) = (
                manifest.app.name.as_str().to_owned(),
                manifest.app.version.to_string(),
            );
            return Ok(if json {
                format!(
                    "{}\n",
                    serde_json::json!({
                        "ok": true,
                        "signed": false,
                        "development": true,
                        "app": name,
                        "version": version,
                        "digest": digest.to_hex(),
                    })
                )
            } else {
                format!(
                    "development: UNSIGNED bundle {name} {version} ({}); nothing is proven about who made it\n",
                    digest.to_hex()
                )
            });
        }
        return Err(Failure::Refused(
            "refused: the bundle has no signature (--signature). A development machine can open an \
             unsigned bundle with --allow-unsigned-development"
                .to_owned(),
        ));
    };
    let certificate_path = required_path(matches, "certificate")?;
    let trust_path = required_path(matches, "trust")?;
    let revocations_path = required_path(matches, "revocations")?;
    let now = matches.get_one::<i64>("now").copied().unwrap_or(now);
    let signature = read_signature(signature_path)?;
    let certificate = read_certificate(&certificate_path)?;
    let roots = read_trust_roots(&trust_path)?;
    let list = read_revocations(&revocations_path)?;
    let proof = match verify_packed(
        &bundle,
        &signature,
        &CertificateChain::new(certificate),
        &TrustAnchor::new(roots),
        &list,
        now,
        &policy,
    ) {
        Ok(proof) => proof,
        Err(refusal) => {
            return Err(Failure::Problems {
                text: if json {
                    format!("{}\n", report::refusal_json(&refusal).unwrap_or_default())
                } else {
                    format!("refused: {}\n", refusal.0)
                },
                json,
            });
        }
    };
    Ok(if json {
        format!("{}\n", report::proof_json(&proof).unwrap_or_default())
    } else {
        format!(
            "verified: publisher {} signed at {} over {}\n",
            proof.publisher_id, proof.signed_at, proof.digest
        )
    })
}

fn inspect(matches: &ArgMatches) -> Outcome {
    let json = matches.get_flag("json");
    let file = required_path(matches, "file")?;
    let options = PackOptions::default();
    let bytes = read_bundle_bytes(&file, &options)?;
    let publisher = match matches.get_one::<PathBuf>("signature") {
        Some(path) => Some(read_signature(path)?.publisher_id),
        None => None,
    };
    let inspected =
        spot_pack::inspect::inspect_bytes(&bytes, &options.limits, &options.host, publisher)
            .map_err(|errors| bundle_failure(json, &errors))?;
    Ok(if json {
        format!("{}\n", report::inspect_json(&inspected).unwrap_or_default())
    } else {
        report::inspect_text(&inspected)
    })
}

// --- shared helpers -----------------------------------------------------------------------------

/// The refusal form of a check report: its problems, as text or JSON.
fn check_failure(json: bool, report: &spot_pack::CheckReport) -> Failure {
    Failure::Problems {
        text: if json {
            format!("{}\n", report::check_json(report).unwrap_or_default())
        } else {
            report::check_text(report)
        },
        json,
    }
}

/// The refusal form of the bundle reader's own problems.
fn bundle_failure<E: std::fmt::Display>(json: bool, errors: &[E]) -> Failure {
    let problems: Vec<PackProblem> = errors
        .iter()
        .map(|e| PackProblem::new(spot_pack::error::WASM, e.to_string()))
        .collect();
    check_failure(
        json,
        &spot_pack::CheckReport {
            manifest: None,
            problems,
        },
    )
}

/// Reads a bundle file, within the bundle size limit.
fn read_bundle_bytes(path: &Path, options: &PackOptions) -> Result<Vec<u8>, Failure> {
    let size = std::fs::metadata(path)
        .map_err(|e| Failure::Failed(format!("{} could not be read: {e}", path.display())))?
        .len();
    if size > options.limits.max_compressed_bytes {
        return Err(Failure::Refused(format!(
            "{} is larger than the bundle limit",
            path.display()
        )));
    }
    std::fs::read(path)
        .map_err(|e| Failure::Failed(format!("{} could not be read: {e}", path.display())))
}

/// The bytes of a bundle file as a `PackedBundle`, so the caller opens it as the installer does
/// (path rules, limits, manifest, host version) without naming the bundle crate's types.
fn packed_from_file(
    path: &Path,
    options: &PackOptions,
) -> Result<spot_pack::PackedBundle, Failure> {
    let bytes = read_bundle_bytes(path, options)?;
    // `open` never reads the digest field; a parsed all-zero value fills it (the technique of
    // `spot_cli::bundle_install`).
    let Ok(unused) = "0".repeat(64).parse() else {
        return Err(Failure::Failed("the bundle cannot be opened".to_owned()));
    };
    Ok(spot_pack::PackedBundle {
        bytes,
        digest: unused,
    })
}

/// Reads a small sidecar file (signature, certificate, trust roots, revocations).
fn read_sidecar(path: &Path) -> Result<Vec<u8>, Failure> {
    let meta = std::fs::metadata(path)
        .map_err(|e| Failure::Failed(format!("{} could not be read: {e}", path.display())))?;
    if !meta.is_file() || meta.len() > SIDECAR_MAX_BYTES {
        return Err(Failure::Failed(format!(
            "{} is not a regular file within the size limit",
            path.display()
        )));
    }
    std::fs::read(path)
        .map_err(|e| Failure::Failed(format!("{} could not be read: {e}", path.display())))
}

fn read_signature(path: &Path) -> Result<BundleSignature, Failure> {
    BundleSignature::from_json(&read_sidecar(path)?).map_err(|_| {
        Failure::Failed(format!(
            "the signature file {} is malformed",
            path.display()
        ))
    })
}

fn read_certificate(path: &Path) -> Result<SignedCertificate, Failure> {
    SignedCertificate::from_json(&read_sidecar(path)?).map_err(|_| {
        Failure::Failed(format!(
            "the certificate file {} is malformed",
            path.display()
        ))
    })
}

/// The trust-roots file: one root public key per line (base64, `#` comments allowed).
fn read_trust_roots(path: &Path) -> Result<Vec<TrustedRoot>, Failure> {
    let text = String::from_utf8(read_sidecar(path)?)
        .map_err(|_| Failure::Failed(format!("{} is not text", path.display())))?;
    let mut roots = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let key = spot_bundle_signing::canonical::unb64(line)
            .and_then(|bytes| PublicKey::from_bytes(&bytes).ok())
            .ok_or_else(|| {
                Failure::Failed(format!(
                    "{} line {}: not a base64 public key",
                    path.display(),
                    n + 1
                ))
            })?;
        roots.push(TrustedRoot::new(key, 0, None));
    }
    if roots.is_empty() {
        return Err(Failure::Failed(format!("{} names no root", path.display())));
    }
    Ok(roots)
}

fn read_revocations(path: &Path) -> Result<RevocationList, Failure> {
    RevocationList::from_json(&read_sidecar(path)?).map_err(|_| {
        Failure::Failed(format!(
            "the revocation list {} is malformed",
            path.display()
        ))
    })
}
