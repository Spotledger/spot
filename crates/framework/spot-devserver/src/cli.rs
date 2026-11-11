//! The `spot dev` command: argument parsing, assembly of the pipeline, and the run loop.
//!
//! A clap-based CLI registers the command with [`register`] (`spot dev`), or embeds the
//! [`DevArgs`] struct directly. [`run`] assembles the metadata, view, script and component
//! steps, starts the status endpoint, watches the root and drives [`DevServer::poll_once`] from
//! a [`spot_core::SystemClock`].
//!
//! The realtime notification and the database-backed site are **not** built here: the running
//! server owns the bus and the tenant. The CLI's `spot dev` therefore constructs the pipeline
//! with the `InMemoryScriptHost`/`InMemoryComponentHost` seams (a `spot dev check` is a
//! build-and-validate run) and leaves the bus and `SiteControl` to the host that embeds it —
//! which is exactly the boundary `spot-cli` implements on the other side.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use camino::Utf8PathBuf;
use clap::{ArgMatches, Command, CommandFactory, FromArgMatches, Parser};
use spot_core::{SiteId, SystemClock};

use crate::classify::Classifier;
use crate::error::{DevError, DevResult};
use crate::reload::{
    BuildCommand, CommandBuildRunner, ComponentStep, DeclarationNameChecker, InMemoryComponentHost,
    InMemoryScriptHost, LiveDefinitions, LiveDtsProvider, MetaStep, Reloader, ScriptStep, Step,
    ViewStep,
};
use crate::server::DevServer;
use crate::status::{StatusServer, StatusStore};
use crate::watch::{NotifyEventSource, Watcher, WatcherConfig};

/// The `spot dev` command line.
#[derive(Clone, Debug, Parser)]
#[command(
    name = "dev",
    about = "Run the local development server with hot reload"
)]
pub struct DevArgs {
    /// The app root to watch (defaults to the current directory).
    #[arg(long, default_value = ".")]
    pub root: String,
    /// The development site name.
    #[arg(long, default_value = "dev")]
    pub site: String,
    /// The metadata directory, relative to the root unless absolute.
    #[arg(long, default_value = "meta")]
    pub meta_dir: String,
    /// The views directory.
    #[arg(long, default_value = "views")]
    pub views_dir: String,
    /// The TypeScript directory.
    #[arg(long, default_value = "ts")]
    pub scripts_dir: String,
    /// The debounce window, in milliseconds.
    #[arg(long, default_value_t = 200)]
    pub debounce_ms: u64,
    /// How long the run loop waits between polls, in milliseconds.
    #[arg(long, default_value_t = 250)]
    pub poll_ms: u64,
    /// The `host:port` the status endpoint binds.
    #[arg(long, default_value = "127.0.0.1:7311")]
    pub status_addr: String,
    /// The build program a component change runs (`cargo`, a script); unset disables component
    /// rebuilds.
    #[arg(long)]
    pub build: Option<String>,
    /// One argument of the build program; repeat for more.
    #[arg(long = "build-arg")]
    pub build_args: Vec<String>,
    /// The component artefact a successful build loads.
    #[arg(long, default_value = "target/wasm32-wasip1/release/app.wasm")]
    pub artifact: String,
    /// Run one poll and exit (the `spot dev check` mode).
    #[arg(long)]
    pub once: bool,
}

impl DevArgs {
    /// The parsed arguments as resolved options.
    ///
    /// # Errors
    ///
    /// [`DevError::Config`] when the site name or the status address is invalid.
    pub fn options(&self) -> DevResult<DevOptions> {
        let site = SiteId::new(self.site.clone()).map_err(|error| DevError::Config {
            message: format!("invalid site name `{}`: {error}", self.site),
        })?;
        let status_addr =
            self.status_addr
                .parse::<SocketAddr>()
                .map_err(|error| DevError::Config {
                    message: format!("invalid status address `{}`: {error}", self.status_addr),
                })?;
        let root = Utf8PathBuf::from(&self.root);
        let build = self.build.as_ref().map(|program| {
            let mut command = BuildCommand::new(program.clone(), root.clone());
            for arg in &self.build_args {
                command = command.arg(arg.clone());
            }
            command
        });
        Ok(DevOptions {
            root,
            site,
            meta_dir: Utf8PathBuf::from(&self.meta_dir),
            views_dir: Utf8PathBuf::from(&self.views_dir),
            scripts_dir: Utf8PathBuf::from(&self.scripts_dir),
            debounce: Duration::from_millis(self.debounce_ms),
            poll: Duration::from_millis(self.poll_ms),
            status_addr,
            build,
            artifact: Utf8PathBuf::from(&self.artifact),
            once: self.once,
        })
    }
}

/// Resolved [`DevArgs`] the run loop uses.
#[derive(Clone, Debug)]
pub struct DevOptions {
    /// The watched root.
    pub root: Utf8PathBuf,
    /// The development site id.
    pub site: SiteId,
    /// The metadata directory.
    pub meta_dir: Utf8PathBuf,
    /// The views directory.
    pub views_dir: Utf8PathBuf,
    /// The scripts directory.
    pub scripts_dir: Utf8PathBuf,
    /// The debounce window.
    pub debounce: Duration,
    /// The poll interval.
    pub poll: Duration,
    /// The status endpoint address.
    pub status_addr: SocketAddr,
    /// The component build command; `None` disables rebuilds.
    pub build: Option<BuildCommand>,
    /// The artefact a successful build loads.
    pub artifact: Utf8PathBuf,
    /// Run one poll and exit.
    pub once: bool,
}

/// The `dev` command for a clap-based CLI.
#[must_use]
pub fn command() -> Command {
    DevArgs::command()
}

/// Adds the `dev` subcommand to `parent`.
#[must_use]
pub fn register(parent: Command) -> Command {
    parent.subcommand(command())
}

/// Parses `matches` as [`DevArgs`] and runs the server.
///
/// # Errors
///
/// [`DevError::Config`] on bad arguments, and whatever [`run`] returns.
pub fn run_from_matches(matches: &ArgMatches) -> DevResult<()> {
    let args = DevArgs::from_arg_matches(matches).map_err(|error| DevError::Config {
        message: error.to_string(),
    })?;
    run(&args)
}

/// Resolves `args` and runs the development server. Returns when the server stops.
///
/// # Errors
///
/// [`DevError::Config`] on bad arguments, [`DevError::Metadata`] when the initial metadata does
/// not build, and [`DevError::Watch`] when the watcher cannot start.
pub fn run(args: &DevArgs) -> DevResult<()> {
    let options = args.options()?;
    run_options(&options)
}

/// Assembles and runs the server for resolved `options`.
///
/// # Errors
///
/// As [`run`].
pub fn run_options(options: &DevOptions) -> DevResult<()> {
    let meta_dir = options.root.join(&options.meta_dir);
    let meta = MetaStep::open(options.site.clone(), meta_dir)?;
    let live = meta.live().clone();
    let definitions = Arc::new(LiveDefinitions::new(live));

    let views = Arc::new(ViewStep::new(
        options.root.join(&options.views_dir),
        definitions.clone(),
    ));
    let scripts = Arc::new(ScriptStep::new(
        options.root.join(&options.scripts_dir),
        Arc::new(LiveDtsProvider::new(definitions.clone())),
        Arc::new(DeclarationNameChecker),
        Arc::new(InMemoryScriptHost::new()),
    ));
    let components = Arc::new(ComponentStep::new(
        options.site.as_str(),
        options.build.clone(),
        Arc::new(CommandBuildRunner),
        Arc::new(InMemoryComponentHost::new()),
        options.artifact.clone(),
    ));

    let reloader = Arc::new(
        Reloader::new(256)
            .with_step(Arc::new(meta) as Arc<dyn Step>)
            .with_step(views as Arc<dyn Step>)
            .with_step(scripts as Arc<dyn Step>)
            .with_step(components as Arc<dyn Step>),
    );
    let status = Arc::new(StatusStore::new(256));

    let mut server = DevServer::new(
        options.root.clone(),
        Classifier::new(),
        Watcher::new(
            NotifyEventSource::new()?,
            WatcherConfig {
                debounce: options.debounce,
                extra_ignored: Vec::new(),
            },
        ),
        reloader,
        status.clone(),
    );
    server.watch()?;

    let shutdown = Arc::new(AtomicBool::new(false));
    let server_thread = {
        let status = status.clone();
        let addr = options.status_addr;
        let shutdown = shutdown.clone();
        std::thread::spawn(move || {
            let endpoint = StatusServer::new(status, addr);
            if let Err(error) = endpoint.serve_until(&shutdown) {
                eprintln!("status endpoint stopped: {error}");
            }
        })
    };

    println!(
        "spot dev: watching {} (site `{}`), status on http://{}/dev/status",
        options.root, options.site, options.status_addr
    );
    let clock = SystemClock::new();
    loop {
        for record in server.poll_once(&clock) {
            if record.ok {
                println!(
                    "[{}] {} ok ({} ms){}",
                    record.step.as_str(),
                    record.paths.join(", "),
                    record.duration_ms,
                    record
                        .version
                        .map(|version| format!(", version {version}"))
                        .unwrap_or_default()
                );
            } else {
                eprintln!(
                    "[{}] {} failed: {}",
                    record.step.as_str(),
                    record.paths.join(", "),
                    record.detail
                );
                for error in &record.errors {
                    eprintln!("  {error}");
                }
            }
        }
        if options.once {
            break;
        }
        std::thread::sleep(options.poll);
    }

    shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = server_thread.join();
    Ok(())
}
