//! The command registry: a subcommand is one file `src/commands/<name>.rs` that registers itself.
//!
//! A command file declares, at column 0:
//!
//! - `pub const NAME: &str = "backup";`
//! - `pub fn command() -> clap::Command`: its arguments and help;
//! - `pub async fn run(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit>`.
//!
//! `build.rs` turns the files into `pub mod` lines and [`COMMANDS`]; adding a command edits no
//! shared file. The derive commands of `main.rs` stay where they are; [`augment`] adds the
//! registered ones to the same `clap::Command`, so `--help` lists everything.
//!
//! This module uses only `clap` and the public API of `spot_cli`, so `tests/commands_registry.rs`
//! includes it by path and drives it without a process.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use spot_cli::config::{Common, Loaded, Site};
use spot_core::Clock;
use spot_postgres::PgBackend;

/// How a command ended. The numbers are the convention operators and scripts rely on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Done.
    Ok,
    /// Wrong usage: unknown command, missing or invalid arguments.
    Usage,
    /// Refused by policy or permission; nothing was changed.
    Refused,
    /// Failed while running (an `Err` from the command lands here).
    Failed,
}

impl Exit {
    /// The process exit code: 0, 1, 2 or 3.
    pub fn code(self) -> u8 {
        match self {
            Exit::Ok => 0,
            Exit::Usage => 1,
            Exit::Refused => 2,
            Exit::Failed => 3,
        }
    }
}

impl From<Exit> for std::process::ExitCode {
    fn from(exit: Exit) -> Self {
        std::process::ExitCode::from(exit.code())
    }
}

/// What a command's `run` returns; boxed so the registry can hold `async fn`s.
pub type CommandFuture<'a> = Pin<Box<dyn Future<Output = spot_cli::Result<Exit>> + Send + 'a>>;

/// One registered command. `build.rs` fills [`COMMANDS`]; tests build their own.
#[derive(Clone, Copy)]
pub struct Registration {
    /// The subcommand name (equal to the `clap::Command`'s name).
    pub name: &'static str,
    /// Its arguments and help.
    pub command: fn() -> clap::Command,
    /// Runs it with its parsed arguments.
    pub run: for<'a> fn(&'a clap::ArgMatches, &'a mut CommandCtx) -> CommandFuture<'a>,
}

/// What every command receives: the loaded `common.toml` and sites, a PostgreSQL backend per site
/// on demand, the clock, and the output writers, so a command is testable without a process.
///
/// There is deliberately no raw SQL helper and no administrative connection: a command reaches
/// data only through a site's backend and the same document engine, actor and permission path as
/// the server.
pub struct CommandCtx {
    root: PathBuf,
    clock: Arc<dyn Clock>,
    out: Box<dyn Write + Send>,
    err: Box<dyn Write + Send>,
    loaded: Option<Loaded>,
    backends: BTreeMap<String, Arc<PgBackend>>,
}

impl CommandCtx {
    /// A context over the host `root` directory (`--root`), with the clock and writers a command
    /// uses. Nothing is read until a command asks.
    pub fn new(
        root: PathBuf,
        clock: Arc<dyn Clock>,
        out: Box<dyn Write + Send>,
        err: Box<dyn Write + Send>,
    ) -> Self {
        CommandCtx {
            root,
            clock,
            out,
            err,
            loaded: None,
            backends: BTreeMap::new(),
        }
    }

    /// The host root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The clock. Time is an argument: a command never reads the wall clock itself.
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// Standard output.
    pub fn out(&mut self) -> &mut (dyn Write + Send) {
        &mut *self.out
    }

    /// Standard error.
    pub fn err(&mut self) -> &mut (dyn Write + Send) {
        &mut *self.err
    }

    fn load(&mut self) -> spot_cli::Result<&Loaded> {
        if self.loaded.is_none() {
            self.loaded = Some(spot_cli::config::load(&self.root, false)?);
        }
        self.loaded
            .as_ref()
            .ok_or_else(|| spot_cli::Error("the host configuration did not load".into()))
    }

    /// The loaded `common.toml` (read and validated on first use).
    pub fn common(&mut self) -> spot_cli::Result<&Common> {
        Ok(&self.load()?.common)
    }

    /// The configured sites, sorted by directory name.
    pub fn sites(&mut self) -> spot_cli::Result<&[Site]> {
        Ok(&self.load()?.sites)
    }

    /// The site called `name`.
    pub fn site(&mut self, name: &str) -> spot_cli::Result<&Site> {
        self.load()?
            .sites
            .iter()
            .find(|s| s.site == name)
            .ok_or_else(|| spot_cli::Error(format!("there is no site `{name}`")))
    }

    /// The pooled backend on `name`'s database, connected on first use. Connecting creates and
    /// changes nothing.
    pub async fn backend(&mut self, name: &str) -> spot_cli::Result<Arc<PgBackend>> {
        if let Some(backend) = self.backends.get(name) {
            return Ok(backend.clone());
        }
        self.site(name)?;
        let loaded = self.load()?;
        let site = loaded
            .sites
            .iter()
            .find(|s| s.site == name)
            .ok_or_else(|| spot_cli::Error(format!("there is no site `{name}`")))?;
        let backend = Arc::new(spot_cli::schema::backend(loaded, site).await?);
        self.backends.insert(name.to_owned(), backend.clone());
        Ok(backend)
    }
}

// The module list and `COMMANDS` are generated by `build.rs` from `src/commands/*.rs`.
include!(concat!(env!("OUT_DIR"), "/commands_registry.rs"));

/// Adds every registration of `registrations` to `base` as a subcommand, so `--help` lists the
/// derive commands and the registered ones together.
///
/// # Errors
///
/// A name registered twice or equal to a command `base` already has, or a command whose own name
/// differs from its registered one. The build script catches the first two for files; this
/// guards test registrations and `main.rs` edits.
pub fn augment(
    base: clap::Command,
    registrations: &[Registration],
) -> Result<clap::Command, String> {
    let mut command = base;
    for registration in registrations {
        let sub = (registration.command)();
        if sub.get_name() != registration.name {
            return Err(format!(
                "command `{}` builds a clap command named `{}`",
                registration.name,
                sub.get_name()
            ));
        }
        if command.find_subcommand(registration.name).is_some() {
            return Err(format!(
                "command `{}` is registered twice or is a built-in command",
                registration.name
            ));
        }
        command = command.subcommand(sub);
    }
    Ok(command)
}

/// The registration `matches` selected, if its subcommand is a registered one.
pub fn selected<'a>(
    registrations: &'a [Registration],
    matches: &clap::ArgMatches,
) -> Option<(&'a Registration, &'a str)> {
    let (name, _) = matches.subcommand()?;
    registrations
        .iter()
        .find(|r| r.name == name)
        .map(|r| (r, r.name))
}

/// Runs the registered command `matches` selected. `None` when the subcommand is not a
/// registered one (a derive command: the caller handles it). An `Err` from the command is
/// reported on the error writer and is [`Exit::Failed`]; the report is the error's own text,
/// which commands keep free of secrets.
pub async fn run_registered(
    registrations: &[Registration],
    matches: &clap::ArgMatches,
    ctx: &mut CommandCtx,
) -> Option<Exit> {
    let (registration, name) = selected(registrations, matches)?;
    let sub = matches.subcommand_matches(name)?;
    Some(match (registration.run)(sub, ctx).await {
        Ok(exit) => exit,
        Err(error) => {
            let _ = writeln!(ctx.err(), "{error}");
            Exit::Failed
        }
    })
}

/// The message for a parse failure that is an unknown subcommand: the name, then every command
/// there is. `None` for any other clap error (those keep clap's own handling).
pub fn unknown_command_message(command: &clap::Command, error: &clap::Error) -> Option<String> {
    if error.kind() != clap::error::ErrorKind::InvalidSubcommand {
        return None;
    }
    let mut names: Vec<&str> = command.get_subcommands().map(|c| c.get_name()).collect();
    names.sort_unstable();
    Some(format!(
        "{}\n\navailable commands: {}\n",
        error.render().to_string().trim_end(),
        names.join(", ")
    ))
}
