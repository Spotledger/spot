//! `spot types generate|check` (WP-EXT-09.T2): the TypeScript declarations of a site's effective
//! metadata, one command away. Guide: `docs/running-spot/types.md`.
//!
//! - `generate --site S [--app A ...] [--doctype D ...] [--out DIR] [--json]` writes
//!   `DIR/spot.d.ts` and `DIR/tsconfig.spot.json`, each atomically (a temporary file in the same
//!   directory, then a rename) and only when its content changed, so an unchanged run does not
//!   touch the files' modification times.
//! - `check ...` (the same selection) exits 0 when the files in `DIR` equal what would be
//!   generated and 2 when they are missing or stale, naming the DocTypes whose declarations
//!   changed and the command to run. It is the CI gate.
//!
//! # Where the metadata comes from
//!
//! The host's own loader: `services::metadata::effective` (the apps the site has installed with
//! every recorded runtime change, custom fields included, applied by `spot-meta`). It reads the
//! site's files (`sites/<site>/metadata.json`, the record the API appends to when a Custom Field
//! is added), never a database and never a document, so a field added through the API appears in
//! the next `generate` without a restart. No second parser exists.
//!
//! # Exit codes
//!
//! 0 done (or up to date), 1 usage (no such site, app or DocType), 2 refused (stale types in
//! `check`, a symlink at the output), 3 failed (a name that cannot be declared, metadata that
//! cannot be loaded, an output that cannot be written). A failed run writes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_dts::Generated;

use super::{CommandCtx, Exit};

pub const NAME: &str = "types";

/// The declaration file's name inside the output directory.
const TYPES_FILE: &str = "spot.d.ts";
/// The editor configuration's name inside the output directory.
const TSCONFIG_FILE: &str = "tsconfig.spot.json";
/// The most an existing output file is read for the comparison. Larger is simply "different".
const MAX_EXISTING_BYTES: u64 = 32 * 1024 * 1024;

/// Distinguishes the temporary files of runs in one process (the process id covers the rest).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn selection(command: Command) -> Command {
    command
        .arg(
            Arg::new("site")
                .long("site")
                .value_name("SITE")
                .required(true)
                .help("The site whose effective metadata is declared"),
        )
        .arg(
            Arg::new("app")
                .long("app")
                .value_name("APP")
                .action(ArgAction::Append)
                .help("Declare this app's DocTypes and what they reference (repeatable)"),
        )
        .arg(
            Arg::new("doctype")
                .long("doctype")
                .value_name("DOCTYPE")
                .action(ArgAction::Append)
                .help("Declare this DocType (storage name) and what it references (repeatable)"),
        )
        .arg(
            Arg::new("out")
                .long("out")
                .value_name("DIR")
                .value_parser(clap::value_parser!(PathBuf))
                .help("The output directory (default: <app dir>/types for one app, else ./types)"),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .action(ArgAction::SetTrue)
                .help("Print machine-readable JSON"),
        )
}

pub fn command() -> Command {
    Command::new(NAME)
        .about("Generate and check the TypeScript declarations of a site's metadata")
        .after_help("Exit codes: 0 done, 1 usage, 2 refused (stale types, a symlink), 3 failed.")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(selection(Command::new("generate").about(
            "Write spot.d.ts and tsconfig.spot.json (only when the content changed)",
        )))
        .subcommand(selection(Command::new("check").about(
            "Exit 0 when the checked-in declarations are current, 2 when stale",
        )))
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let outcome = match matches.subcommand() {
        Some(("generate", sub)) => generate(sub, ctx),
        Some(("check", sub)) => check(sub, ctx),
        Some((other, _)) => Err(Failure::Usage(format!("unknown types command `{other}`"))),
        None => Err(Failure::Usage("name a types command".to_owned())),
    };
    match outcome {
        Ok(text) => {
            write!(ctx.out(), "{text}")?;
            Ok(Exit::Ok)
        }
        Err(Failure::Usage(message)) => {
            writeln!(ctx.err(), "spot types: {message}")?;
            Ok(Exit::Usage)
        }
        Err(Failure::Refused(text)) => {
            write!(ctx.err(), "{text}")?;
            Ok(Exit::Refused)
        }
        Err(Failure::RefusedJson(text)) => {
            write!(ctx.out(), "{text}")?;
            Ok(Exit::Refused)
        }
        Err(Failure::Failed(text)) => {
            write!(ctx.err(), "{text}")?;
            Ok(Exit::Failed)
        }
    }
}

/// How a run ended unhappily, before it becomes an exit and a message.
enum Failure {
    /// Wrong use: exit 1.
    Usage(String),
    /// A refusal, printed on standard error: exit 2.
    Refused(String),
    /// A refusal for `--json`, printed on standard output: exit 2.
    RefusedJson(String),
    /// Something could not be done: exit 3 (lines, each ending in a newline).
    Failed(String),
}

type Outcome = Result<String, Failure>;

/// What a run declares, and where.
struct Plan {
    site: String,
    apps: Vec<String>,
    doctypes: Vec<String>,
    /// The `--out` as typed, for the command a stale `check` prints.
    explicit_out: Option<PathBuf>,
    out: PathBuf,
    generated: Generated,
    tsconfig: String,
}

fn values(matches: &ArgMatches, name: &str) -> Vec<String> {
    matches
        .get_many::<String>(name)
        .map(|v| v.cloned().collect())
        .unwrap_or_default()
}

/// Loads the site's effective metadata through the host's loader and generates the declarations
/// the selection asks for. Writes nothing.
fn plan(matches: &ArgMatches, ctx: &CommandCtx) -> Result<Plan, Failure> {
    let site_name = matches
        .get_one::<String>("site")
        .cloned()
        .ok_or_else(|| Failure::Usage("--site is required".to_owned()))?;
    let apps = values(matches, "app");
    let doctypes = values(matches, "doctype");
    let explicit_out = matches.get_one::<PathBuf>("out").cloned();

    let root = ctx.root().to_path_buf();
    let loaded = spot_cli::config::load(&root, false)
        .map_err(|e| Failure::Failed(format!("spot types: {e}\n")))?;
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == site_name)
        .ok_or_else(|| Failure::Usage(format!("there is no site `{site_name}`")))?;
    let meta = spot_cli::services::metadata::effective(&root, &loaded, site)
        .map_err(|e| Failure::Failed(format!("spot types: {e}\n")))?;

    // The DocTypes named directly and those of the named apps.
    let mut roots: BTreeSet<String> = doctypes.iter().cloned().collect();
    let mut app_dir: Option<PathBuf> = None;
    if !apps.is_empty() {
        let catalogue = spot_cli::apps::catalogue(&root, &loaded)
            .map_err(|e| Failure::Failed(format!("spot types: {e}\n")))?;
        for app in &apps {
            if !site.apps.is_empty() && !site.apps.iter().any(|a| a == app) {
                return Err(Failure::Usage(format!(
                    "the app `{app}` is not installed on site `{site_name}`"
                )));
            }
            if let Some(source) = catalogue.iter().find(|a| &a.name == app) {
                roots.extend(source.definitions.iter().map(|d| d.name.clone()));
                app_dir = Some(source.dir.clone());
            } else if app == spot_cli::appdir::PLATFORM {
                let definitions = spot_cli::appdir::platform_definitions()
                    .map_err(|e| Failure::Failed(format!("spot types: {e}\n")))?;
                roots.extend(definitions.iter().map(|d| d.name.clone()));
            } else {
                return Err(Failure::Usage(format!(
                    "the app `{app}` is not in common.toml `apps`"
                )));
            }
        }
    }
    for name in &doctypes {
        if meta.definition(name).is_none() {
            return Err(Failure::Usage(format!(
                "the site has no DocType `{name}` (name it by its storage name)"
            )));
        }
    }

    let result = if roots.is_empty() {
        spot_dts::generate(meta.definitions())
    } else {
        let names: Vec<&str> = roots.iter().map(String::as_str).collect();
        spot_dts::generate_for(meta.definitions(), &names)
    };
    let generated = result.map_err(|errors| {
        let mut text = String::new();
        for error in &errors {
            match &error.doctype {
                Some(doctype) => text.push_str(&format!("{doctype}: {}\n", error.message)),
                None => text.push_str(&format!("{}\n", error.message)),
            }
        }
        text.push_str("nothing was written\n");
        Failure::Failed(text)
    })?;

    let out = match (&explicit_out, apps.as_slice(), app_dir) {
        (Some(out), _, _) => out.clone(),
        (None, [_], Some(dir)) => dir.join("types"),
        (None, [_], None) => {
            return Err(Failure::Usage(
                "that app has no directory on this host; name one with --out".to_owned(),
            ));
        }
        (None, _, _) => PathBuf::from("types"),
    };
    Ok(Plan {
        site: site_name,
        apps,
        doctypes,
        explicit_out,
        out,
        generated,
        tsconfig: spot_dts::tsconfig_snippet(TYPES_FILE),
    })
}

// --- generate -----------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Created,
    Updated,
    Unchanged,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Status::Created => "created",
            Status::Updated => "updated",
            Status::Unchanged => "unchanged",
        }
    }
}

fn generate(matches: &ArgMatches, ctx: &mut CommandCtx) -> Outcome {
    let json = matches.get_flag("json");
    let plan = plan(matches, ctx)?;
    prepare_dir(&plan.out)?;
    let types = write_if_changed(&plan.out, TYPES_FILE, &plan.generated.text)?;
    let tsconfig = write_if_changed(&plan.out, TSCONFIG_FILE, &plan.tsconfig)?;
    let interfaces = interface_blocks(&plan.generated.text).len();
    Ok(if json {
        let value = serde_json::json!({
            "out": plan.out.display().to_string(),
            "interfaces": interfaces,
            "fingerprint": plan.generated.fingerprint,
            "files": [
                { "file": TYPES_FILE, "status": types.word() },
                { "file": TSCONFIG_FILE, "status": tsconfig.word() },
            ],
        });
        format!("{value}\n")
    } else {
        format!(
            "{}: {} ({interfaces} interfaces, sha256 {})\n{}: {}\n",
            plan.out.join(TYPES_FILE).display(),
            types.word(),
            &plan.generated.fingerprint[..plan.generated.fingerprint.len().min(12)],
            plan.out.join(TSCONFIG_FILE).display(),
            tsconfig.word(),
        )
    })
}

/// Makes sure `dir` is a directory the command may write to: not a symlink, created when absent.
fn prepare_dir(dir: &Path) -> Result<(), Failure> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => Err(Failure::Refused(format!(
            "spot types: {} is a symlink; the command never writes through one\n",
            dir.display()
        ))),
        Ok(meta) if !meta.is_dir() => Err(Failure::Failed(format!(
            "spot types: {} is not a directory\n",
            dir.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).map_err(|e| {
                Failure::Failed(format!(
                    "spot types: {} cannot be created: {e}\n",
                    dir.display()
                ))
            })
        }
        Err(e) => Err(Failure::Failed(format!(
            "spot types: {} cannot be read: {e}\n",
            dir.display()
        ))),
    }
}

/// Writes `dir/name` with `text` unless it already holds exactly that. The write goes to a
/// temporary file in the same directory and is renamed over the target, so a reader (or a run
/// in parallel) sees a complete file, old or new. A symlink at the target is refused.
fn write_if_changed(dir: &Path, name: &str, text: &str) -> Result<Status, Failure> {
    let target = dir.join(name);
    let status = match std::fs::symlink_metadata(&target) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(Failure::Refused(format!(
                "spot types: {} is a symlink; the command never writes through one\n",
                target.display()
            )));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(Failure::Failed(format!(
                "spot types: {} is not a regular file\n",
                target.display()
            )));
        }
        Ok(meta) => {
            if meta.len() <= MAX_EXISTING_BYTES
                && std::fs::read_to_string(&target)
                    .is_ok_and(|old| spot_dts::fingerprint(&old) == spot_dts::fingerprint(text))
            {
                return Ok(Status::Unchanged);
            }
            Status::Updated
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Status::Created,
        Err(e) => {
            return Err(Failure::Failed(format!(
                "spot types: {} cannot be read: {e}\n",
                target.display()
            )));
        }
    };
    let temporary = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let failed = |e: std::io::Error| {
        Failure::Failed(format!(
            "spot types: {} cannot be written: {e}\n",
            target.display()
        ))
    };
    if let Err(e) =
        write_temporary(&temporary, text).and_then(|()| std::fs::rename(&temporary, &target))
    {
        let _ = std::fs::remove_file(&temporary);
        return Err(failed(e));
    }
    Ok(status)
}

/// Creates `path` (it must not exist) and writes `text` to it, flushed to disk.
fn write_temporary(path: &Path, text: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

// --- check --------------------------------------------------------------------------------------

/// What the files in the output directory hold, compared with what would be generated.
enum Found {
    Missing,
    Same,
    /// Differs; the DocType interfaces that differ (empty when only the rest of the file does).
    Differs(Vec<String>),
}

fn compare(dir: &Path, name: &str, wanted: &str) -> Result<Found, Failure> {
    let path = dir.join(name);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Found::Missing),
        Err(e) => {
            return Err(Failure::Failed(format!(
                "spot types: {} cannot be read: {e}\n",
                path.display()
            )));
        }
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(Failure::Refused(format!(
                "spot types: {} is not a regular file (a symlink is never followed)\n",
                path.display()
            )));
        }
        Ok(meta) if meta.len() > MAX_EXISTING_BYTES => return Ok(Found::Differs(Vec::new())),
        Ok(_) => {}
    }
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(Found::Differs(Vec::new()));
    };
    if spot_dts::fingerprint(&existing) == spot_dts::fingerprint(wanted) {
        return Ok(Found::Same);
    }
    Ok(Found::Differs(changed_interfaces(&existing, wanted)))
}

fn check(matches: &ArgMatches, ctx: &mut CommandCtx) -> Outcome {
    let json = matches.get_flag("json");
    let plan = plan(matches, ctx)?;
    let types = compare(&plan.out, TYPES_FILE, &plan.generated.text)?;
    let tsconfig = compare(&plan.out, TSCONFIG_FILE, &plan.tsconfig)?;
    let interfaces = interface_blocks(&plan.generated.text).len();
    if matches!((&types, &tsconfig), (Found::Same, Found::Same)) {
        return Ok(if json {
            format!(
                "{}\n",
                serde_json::json!({ "ok": true, "interfaces": interfaces })
            )
        } else {
            format!(
                "{} is current ({interfaces} interfaces)\n",
                plan.out.join(TYPES_FILE).display()
            )
        });
    }
    let command = regenerate_command(&plan);
    let mut lines = Vec::new();
    let mut changed: Vec<String> = Vec::new();
    for (name, found) in [(TYPES_FILE, &types), (TSCONFIG_FILE, &tsconfig)] {
        match found {
            Found::Same => {}
            Found::Missing => lines.push(format!("{name} is missing")),
            Found::Differs(names) if names.is_empty() => {
                lines.push(format!("{name} differs outside the DocType interfaces"));
            }
            Found::Differs(names) => {
                lines.push(format!(
                    "{name} differs; changed DocTypes: {}",
                    names.join(", ")
                ));
                changed.extend(names.iter().cloned());
            }
        }
    }
    if json {
        let value = serde_json::json!({
            "ok": false,
            "problems": lines,
            "changed_doctypes": changed,
            "command": command,
        });
        return Err(Failure::RefusedJson(format!("{value}\n")));
    }
    let mut text = format!("the types in {} are stale:\n", plan.out.display());
    for line in &lines {
        text.push_str(&format!("  {line}\n"));
    }
    text.push_str(&format!("run: {command}\n"));
    Err(Failure::Refused(text))
}

/// The command that brings the files up to date, as the developer would type it.
fn regenerate_command(plan: &Plan) -> String {
    let mut words = vec![
        "spot".to_owned(),
        "types".to_owned(),
        "generate".to_owned(),
        "--site".to_owned(),
        shell_word(&plan.site),
    ];
    for app in &plan.apps {
        words.push("--app".to_owned());
        words.push(shell_word(app));
    }
    for doctype in &plan.doctypes {
        words.push("--doctype".to_owned());
        words.push(shell_word(doctype));
    }
    if let Some(out) = &plan.explicit_out {
        words.push("--out".to_owned());
        words.push(shell_word(&out.display().to_string()));
    }
    words.join(" ")
}

/// `word`, single-quoted unless it is made of plainly safe characters.
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b':'));
    if plain {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

// --- reading the generated text -----------------------------------------------------------------

/// The `interface Name { ... }` blocks of a generated file, by interface name. Lines inside a
/// block comment are never read as code, so a label that spans lines and imitates a declaration
/// cannot invent a name.
fn interface_blocks(text: &str) -> BTreeMap<String, String> {
    let mut blocks = BTreeMap::new();
    let mut in_comment = false;
    let mut current: Option<(String, String)> = None;
    for line in text.lines() {
        let started_in_comment = in_comment;
        in_comment = update_comment_state(line, in_comment);
        if started_in_comment {
            if let Some((_, body)) = current.as_mut() {
                body.push_str(line);
                body.push('\n');
            }
            continue;
        }
        if let Some((name, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
            if line == "}" {
                blocks.insert(std::mem::take(name), std::mem::take(body));
                current = None;
            }
            continue;
        }
        let declaration = line
            .strip_prefix("export interface ")
            .or_else(|| line.strip_prefix("interface "));
        if let Some(rest) = declaration {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect();
            if !name.is_empty() {
                current = Some((name, format!("{line}\n")));
            }
        }
    }
    blocks
}

/// Whether a line ends inside a `/* ... */` comment, given whether it started in one.
fn update_comment_state(line: &str, started_inside: bool) -> bool {
    let mut inside = started_inside;
    let mut rest = line;
    loop {
        if inside {
            match rest.find("*/") {
                Some(at) => {
                    inside = false;
                    rest = &rest[at + 2..];
                }
                None => return true,
            }
        } else {
            match rest.find("/*") {
                Some(at) => {
                    inside = true;
                    rest = &rest[at + 2..];
                }
                None => return false,
            }
        }
    }
}

/// The interface names whose declarations differ between two generated files, or that exist in
/// only one of them, sorted. A line-level comparison of names, not of content.
fn changed_interfaces(existing: &str, wanted: &str) -> Vec<String> {
    let old = interface_blocks(existing);
    let new = interface_blocks(wanted);
    let mut names: BTreeSet<&String> = BTreeSet::new();
    for (name, block) in &new {
        if old.get(name) != Some(block) {
            names.insert(name);
        }
    }
    for name in old.keys() {
        if !new.contains_key(name) {
            names.insert(name);
        }
    }
    names.into_iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multi_line_label_cannot_invent_an_interface() {
        let text =
            "/** x\nexport interface Evil {\n */\nexport interface Real {\n  a?: string;\n}\n";
        let names: Vec<String> = interface_blocks(text).into_keys().collect();
        assert_eq!(names, ["Real"]);
    }

    #[test]
    fn changed_names_are_sorted_and_cover_additions_and_removals() {
        let old = "export interface A {\n  x?: string;\n}\nexport interface B {\n}\n";
        let new = "export interface A {\n  x?: number;\n}\nexport interface C {\n}\n";
        assert_eq!(changed_interfaces(old, new), ["A", "B", "C"]);
    }

    #[test]
    fn words_are_quoted_when_unsafe() {
        assert_eq!(shell_word("site-1"), "site-1");
        assert_eq!(shell_word("a b"), "'a b'");
        assert_eq!(shell_word("it's"), "'it'\\''s'");
    }
}
