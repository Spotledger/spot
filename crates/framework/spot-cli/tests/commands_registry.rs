//! The command registry: registered commands appear in `--help`, receive parsed arguments and
//! writers, follow the exit-code convention, and an unknown name lists every command. Also the
//! build script's command scanner and the duplicate and built-in name checks.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

use std::io::Write;
use std::path::PathBuf;
use std::process::Command as Process;
use std::sync::{Arc, Mutex};

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_core::ManualClock;
use time::macros::datetime;

/// The registry module, driven without a process.
#[path = "../src/commands/mod.rs"]
mod commands;
/// The build script's scanner, tested without a build.
#[path = "../build.rs"]
mod scanner;

use commands::{CommandCtx, CommandFuture, Exit, Registration};

// --- a shared writer -------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Sink {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn ctx(root: &str) -> (CommandCtx, Sink, Sink) {
    let (out, err) = (Sink::default(), Sink::default());
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-04 09:00:00 UTC)));
    (
        CommandCtx::new(
            PathBuf::from(root),
            clock,
            Box::new(out.clone()),
            Box::new(err.clone()),
        ),
        out,
        err,
    )
}

// --- test-only commands (the technique of the services registry test) -----------------------

fn hello_command() -> Command {
    Command::new("hello-registry")
        .about("Greets, for the registry test")
        .arg(Arg::new("who").long("who").default_value("world"))
        .arg(Arg::new("shout").long("shout").action(ArgAction::SetTrue))
}
fn hello_run<'a>(matches: &'a ArgMatches, ctx: &'a mut CommandCtx) -> CommandFuture<'a> {
    Box::pin(async move {
        let who = matches
            .get_one::<String>("who")
            .cloned()
            .unwrap_or_default();
        let line = format!("hello {who} at {}", ctx.clock().now_utc());
        let line = if matches.get_flag("shout") {
            line.to_uppercase()
        } else {
            line
        };
        writeln!(ctx.out(), "{line}")?;
        Ok(Exit::Ok)
    })
}
const HELLO: Registration = Registration {
    name: "hello-registry",
    command: hello_command,
    run: hello_run,
};

fn refuse_command() -> Command {
    Command::new("refuse-registry")
}
fn refuse_run<'a>(_: &'a ArgMatches, ctx: &'a mut CommandCtx) -> CommandFuture<'a> {
    Box::pin(async move {
        writeln!(ctx.err(), "refused: the site operator must confirm")?;
        Ok(Exit::Refused)
    })
}
const REFUSE: Registration = Registration {
    name: "refuse-registry",
    command: refuse_command,
    run: refuse_run,
};

fn fail_command() -> Command {
    Command::new("fail-registry")
}
fn fail_run<'a>(_: &'a ArgMatches, ctx: &'a mut CommandCtx) -> CommandFuture<'a> {
    Box::pin(async move {
        let _ = ctx.sites()?;
        Ok(Exit::Ok)
    })
}
const FAIL: Registration = Registration {
    name: "fail-registry",
    command: fail_command,
    run: fail_run,
};

/// A stand-in for the derive command of `main.rs`.
fn base() -> Command {
    Command::new("spot")
        .arg(
            Arg::new("root")
                .long("root")
                .global(true)
                .default_value("."),
        )
        .subcommand_required(true)
        .subcommand(Command::new("check").about("a built-in"))
}

fn registered() -> Command {
    commands::augment(base(), &[HELLO, REFUSE, FAIL]).unwrap()
}

// --- help, unknown names ---------------------------------------------------------------------

#[test]
fn help_lists_the_built_in_and_the_registered_commands() {
    let help = registered().render_help().to_string();
    for name in [
        "check",
        "hello-registry",
        "refuse-registry",
        "fail-registry",
    ] {
        assert!(help.contains(name), "{name} is listed: {help}");
    }
    assert!(help.contains("Greets, for the registry test"));
}

#[test]
fn an_unknown_name_is_a_usage_failure_that_lists_every_command() {
    let command = registered();
    let error = command
        .clone()
        .try_get_matches_from(["spot", "nonesuch"])
        .unwrap_err();
    let message = commands::unknown_command_message(&command, &error).unwrap();
    assert!(message.contains("nonesuch"), "{message}");
    assert!(
        message
            .contains("available commands: check, fail-registry, hello-registry, refuse-registry"),
        "{message}"
    );
    // Any other parse error keeps clap's own handling.
    let other = command
        .clone()
        .try_get_matches_from(["spot", "hello-registry", "--nonesuch"])
        .unwrap_err();
    assert!(commands::unknown_command_message(&command, &other).is_none());
}

#[test]
fn the_binary_exits_1_for_an_unknown_name_and_lists_the_built_in_commands() {
    let output = Process::new(env!("CARGO_BIN_EXE_spot"))
        .arg("nonesuch")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("nonesuch"), "{stderr}");
    for name in ["new-site", "serve", "install-app", "migrate", "check"] {
        assert!(stderr.contains(name), "{name} is listed: {stderr}");
    }
    let help = Process::new(env!("CARGO_BIN_EXE_spot"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("install-app")
    );
}

// --- running ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_registered_command_gets_parsed_arguments_a_writer_and_the_clock() {
    let regs = [HELLO, REFUSE, FAIL];
    let matches = registered()
        .try_get_matches_from(["spot", "hello-registry", "--who", "alice", "--shout"])
        .unwrap();
    let (mut ctx, out, err) = ctx("/nonexistent");
    let exit = commands::run_registered(&regs, &matches, &mut ctx).await;
    assert_eq!(exit, Some(Exit::Ok));
    assert_eq!(
        out.text(),
        "HELLO ALICE AT 2026-11-04 9:00:00.0 +00:00:00\n"
    );
    assert_eq!(err.text(), "");
}

#[tokio::test]
async fn a_derive_command_is_not_taken_by_the_registry() {
    let matches = registered()
        .try_get_matches_from(["spot", "check"])
        .unwrap();
    let (mut ctx, out, _) = ctx("/nonexistent");
    assert_eq!(
        commands::run_registered(&[HELLO], &matches, &mut ctx).await,
        None
    );
    assert_eq!(out.text(), "");
}

#[tokio::test]
async fn refused_is_2_and_failed_is_3_and_ok_is_0() {
    let regs = [HELLO, REFUSE, FAIL];
    let (mut ctx, out, err) = ctx("/nonexistent-spot-root");

    let m = registered()
        .try_get_matches_from(["spot", "refuse-registry"])
        .unwrap();
    let exit = commands::run_registered(&regs, &m, &mut ctx).await.unwrap();
    assert_eq!(exit, Exit::Refused);
    assert_eq!(exit.code(), 2);
    assert!(err.text().contains("refused:"));

    // An error from the command is reported on the error writer and is 3; nothing is on stdout.
    let m = registered()
        .try_get_matches_from(["spot", "fail-registry"])
        .unwrap();
    let exit = commands::run_registered(&regs, &m, &mut ctx).await.unwrap();
    assert_eq!(exit, Exit::Failed);
    assert_eq!(exit.code(), 3);
    assert!(
        err.text().lines().count() >= 2,
        "the failure is reported: {}",
        err.text()
    );
    assert_eq!(out.text(), "");

    assert_eq!(Exit::Ok.code(), 0);
    assert_eq!(Exit::Usage.code(), 1);
}

#[tokio::test]
async fn the_context_loads_lazily_and_a_missing_root_is_an_error_not_a_panic() {
    let (mut ctx, _, _) = ctx("/nonexistent-spot-root");
    assert_eq!(ctx.root(), std::path::Path::new("/nonexistent-spot-root"));
    assert!(ctx.sites().is_err());
    assert!(ctx.common().is_err());
    assert!(ctx.backend("acme").await.is_err());
}

// --- names -----------------------------------------------------------------------------------

#[test]
fn a_name_registered_twice_or_equal_to_a_built_in_is_refused() {
    let twice = commands::augment(base(), &[HELLO, HELLO]).unwrap_err();
    assert!(twice.contains("hello-registry"), "{twice}");
    let clash = Registration {
        name: "check",
        command: || Command::new("check"),
        run: hello_run,
    };
    let err = commands::augment(base(), &[clash]).unwrap_err();
    assert!(err.contains("check") && err.contains("built-in"), "{err}");
    let liar = Registration {
        name: "other",
        command: hello_command,
        run: hello_run,
    };
    assert!(commands::augment(base(), &[liar]).is_err());
}

#[test]
fn the_build_fails_naming_both_files_for_a_duplicate_or_a_built_in_name() {
    use scanner::{CommandModule, check_command_names};
    let module = |file: &str, name: &str| CommandModule {
        module: file.trim_end_matches(".rs").to_owned(),
        path: format!("src/commands/{file}"),
        name: Some(name.to_owned()),
    };
    let builtins = vec!["new-site".to_owned(), "check".to_owned()];
    assert!(
        check_command_names(
            &[module("a.rs", "backup"), module("b.rs", "restore")],
            &builtins
        )
        .is_ok()
    );
    let dup = check_command_names(
        &[module("a.rs", "backup"), module("b.rs", "backup")],
        &builtins,
    )
    .unwrap_err();
    assert!(
        dup.contains("backup")
            && dup.contains("src/commands/a.rs")
            && dup.contains("src/commands/b.rs"),
        "{dup}"
    );
    let clash = check_command_names(&[module("c.rs", "new-site")], &builtins).unwrap_err();
    assert!(
        clash.contains("new-site")
            && clash.contains("src/commands/c.rs")
            && clash.contains("main.rs")
    );
}

#[test]
fn the_built_in_names_are_read_from_main_rs_as_clap_spells_them() {
    let main = include_str!("../src/main.rs");
    let names = scanner::builtin_commands(main);
    for expected in [
        "new-site",
        "serve",
        "install-app",
        "list-apps",
        "migrate",
        "add-user",
        "set-password",
        "reset-admin",
        "reload",
        "check",
    ] {
        assert!(names.iter().any(|n| n == expected), "{expected}: {names:?}");
    }
    assert_eq!(names.len(), 10, "{names:?}");
}

// --- the scanner -----------------------------------------------------------------------------

#[test]
fn a_command_file_declares_name_command_and_run_together_or_none() {
    use scanner::inspect_command;
    let full = "pub const NAME: &str = \"backup\";\npub fn command() -> clap::Command { todo!() }\npub async fn run(m: &ArgMatches, c: &mut CommandCtx) -> Result<Exit> { todo!() }\n";
    assert_eq!(inspect_command("a.rs", full), Ok(Some("backup".to_owned())));
    assert_eq!(inspect_command("a.rs", "fn helper() {}\n"), Ok(None));
    let partial = "pub const NAME: &str = \"backup\";\n";
    let err = inspect_command("a.rs", partial).unwrap_err();
    assert!(err.contains("command") && err.contains("run"), "{err}");
    // An indented declaration (inside an impl or a test module) does not count.
    assert_eq!(
        inspect_command("a.rs", "    pub async fn run() {}\n"),
        Ok(None)
    );
    let bad = full.replace("backup", "Back_Up");
    assert!(inspect_command("a.rs", &bad).is_err());
}

#[test]
fn command_names_are_checked() {
    for good in ["backup", "migrate-plan", "x2"] {
        assert!(scanner::valid_command_name(good), "{good}");
    }
    for bad in ["", "Backup", "2x", "a--b", "a-", "a_b", "a b"] {
        assert!(!scanner::valid_command_name(bad), "{bad:?}");
    }
}

#[test]
fn the_generated_source_wraps_each_run_and_lists_the_registered_modules() {
    use scanner::{CommandModule, generate_commands};
    let modules = vec![
        CommandModule {
            module: "backup".into(),
            path: "src/commands/backup.rs".into(),
            name: Some("backup".into()),
        },
        CommandModule {
            module: "helper".into(),
            path: "src/commands/helper.rs".into(),
            name: None,
        },
    ];
    let out = generate_commands("C:\\work\\spot-cli", &modules);
    assert!(out.contains("#[path = \"C:/work/spot-cli/src/commands/backup.rs\"]"));
    assert!(out.contains("pub mod backup;") && out.contains("pub mod helper;"));
    assert!(out.contains("fn __run_backup<'a>"));
    assert!(out.contains(
        "Registration { name: backup::NAME, command: backup::command, run: __run_backup }"
    ));
    assert!(!out.contains("__run_helper"));
}

#[test]
fn a_missing_commands_directory_is_an_empty_scan() {
    assert_eq!(
        scanner::scan_commands(std::path::Path::new("/nonexistent-spot-cli-dir")),
        Ok(Vec::new())
    );
}

#[test]
fn the_shipped_registry_matches_the_files_in_src_commands() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let on_disk: Vec<String> = scanner::scan_commands(dir)
        .unwrap()
        .into_iter()
        .filter_map(|m| m.name)
        .collect();
    let compiled: Vec<&str> = commands::COMMANDS.iter().map(|r| r.name).collect();
    assert_eq!(on_disk, compiled);
}
