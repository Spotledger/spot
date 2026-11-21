//! The component step: run the project's build command, then validate and hash the rebuilt
//! WebAssembly component through the `spot-wasm` host.
//!
//! A Rust or WASM change cannot be hot-swapped in process — the artefact has to be rebuilt. The
//! step therefore runs the configured [`BuildCommand`] through the [`BuildRunner`] seam (so a
//! test never spawns a process), and hands the rebuilt `*.wasm` to the [`ComponentHost`] seam.
//! The default host is [`WasmHostComponentHost`], which compiles the artefact with
//! [`spot_wasm::WasmHost`] — the same pooled engine the runtime serves calls from — so an
//! invalid component is rejected before it can reach a request, and the content hash is recorded
//! for the app-swap the host performs.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use camino::{Utf8Path, Utf8PathBuf};

use super::{ReloadContext, Step, StepName, StepOutcome};
use crate::error::{DevError, DevResult};

/// The project build command a component change triggers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildCommand {
    /// The program to run (`cargo`, `dotnet`, a script).
    pub program: String,
    /// Its arguments, in order.
    pub args: Vec<String>,
    /// The working directory.
    pub cwd: Utf8PathBuf,
}

impl BuildCommand {
    /// A command with no arguments in `cwd`.
    #[must_use]
    pub fn new(program: impl Into<String>, cwd: impl Into<Utf8PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
        }
    }

    /// Adds one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// The command rendered for a log line or an error message.
    #[must_use]
    pub fn display(&self) -> String {
        if self.args.is_empty() {
            self.program.clone()
        } else {
            format!("{} {}", self.program, self.args.join(" "))
        }
    }
}

/// What a build command produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildOutput {
    /// Whether the command exited successfully.
    pub ok: bool,
    /// The tail of standard output (the last lines), for the developer.
    pub stdout_tail: String,
    /// The tail of standard error (the last lines), for the developer.
    pub stderr_tail: String,
}

/// Runs a [`BuildCommand`].
pub trait BuildRunner: Send + Sync {
    /// Runs `command` and returns its outcome. A failure to launch is a [`DevError`]; a command
    /// that ran and exited non-zero is a [`BuildOutput`] with `ok == false`.
    ///
    /// # Errors
    ///
    /// [`DevError::Build`] when the program cannot be launched.
    fn run(&self, command: &BuildCommand) -> DevResult<BuildOutput>;
}

/// The real runner: `std::process::Command`, output tails kept.
#[derive(Clone, Copy, Debug, Default)]
pub struct CommandBuildRunner;

impl BuildRunner for CommandBuildRunner {
    fn run(&self, command: &BuildCommand) -> DevResult<BuildOutput> {
        let output = std::process::Command::new(&command.program)
            .args(&command.args)
            .current_dir(command.cwd.as_std_path())
            .output()
            .map_err(|error| DevError::Build {
                command: command.display(),
                message: error.to_string(),
            })?;
        Ok(BuildOutput {
            ok: output.status.success(),
            stdout_tail: tail(&String::from_utf8_lossy(&output.stdout), 20),
            stderr_tail: tail(&String::from_utf8_lossy(&output.stderr), 20),
        })
    }
}

/// The last `lines` lines of `text`, joined by newlines.
#[must_use]
pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Loads a rebuilt component into the runtime and returns its content hash.
pub trait ComponentHost: Send + Sync {
    /// Validates `artifact` for the app `app` and returns the content hash the host swapped in.
    ///
    /// # Errors
    ///
    /// [`DevError::Component`] when the artefact is not a loadable component.
    fn reload(&self, app: &str, artifact: &Utf8Path) -> DevResult<String>;
}

/// A host that records the artefact's SHA-256-like identity, for tests and for a build-only run.
#[derive(Default)]
pub struct InMemoryComponentHost {
    components: Mutex<BTreeMap<String, String>>,
}

impl InMemoryComponentHost {
    /// An empty host.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The hash last recorded for `app`.
    #[must_use]
    pub fn get(&self, app: &str) -> Option<String> {
        self.lock().get(app).cloned()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, String>> {
        self.components
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ComponentHost for InMemoryComponentHost {
    fn reload(&self, app: &str, artifact: &Utf8Path) -> DevResult<String> {
        let bytes = std::fs::read(artifact)
            .map_err(|error| DevError::io_message(artifact, error.to_string()))?;
        let hash = content_hash(&bytes);
        self.lock().insert(app.to_owned(), hash.clone());
        Ok(hash)
    }
}

/// The real host: validates and hashes the component with the `spot-wasm` engine (the same
/// pooling allocator the runtime serves calls from).
pub struct WasmHostComponentHost {
    host: Arc<spot_wasm::WasmHost>,
}

impl WasmHostComponentHost {
    /// Builds the `spot-wasm` host with `config`.
    ///
    /// # Errors
    ///
    /// [`DevError::Component`] when the engine cannot be built.
    pub fn new(config: spot_wasm::HostConfig) -> DevResult<Self> {
        let host = spot_wasm::WasmHost::new(config).map_err(|error| DevError::Component {
            message: error.to_string(),
        })?;
        Ok(Self {
            host: Arc::new(host),
        })
    }

    /// The host's pool size: how many calls may start at once.
    #[must_use]
    pub fn slots(&self) -> usize {
        self.host.slots()
    }
}

impl ComponentHost for WasmHostComponentHost {
    fn reload(&self, _app: &str, artifact: &Utf8Path) -> DevResult<String> {
        let bytes = std::fs::read(artifact)
            .map_err(|error| DevError::io_message(artifact, error.to_string()))?;
        let component = self
            .host
            .compile(&bytes)
            .map_err(|error| DevError::Component {
                message: error.to_string(),
            })?;
        Ok(component.content_hash().to_owned())
    }
}

/// A content hash of some bytes, used by the in-memory host (the real host hashes with
/// `spot-wasm`).
#[must_use]
pub fn content_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The component reload step.
pub struct ComponentStep {
    app: String,
    command: Option<BuildCommand>,
    runner: Arc<dyn BuildRunner>,
    host: Arc<dyn ComponentHost>,
    artifact: Utf8PathBuf,
}

impl ComponentStep {
    /// A step that builds `command` and loads `artifact` as `app`.
    #[must_use]
    pub fn new(
        app: impl Into<String>,
        command: Option<BuildCommand>,
        runner: Arc<dyn BuildRunner>,
        host: Arc<dyn ComponentHost>,
        artifact: impl Into<Utf8PathBuf>,
    ) -> Self {
        Self {
            app: app.into(),
            command,
            runner,
            host,
            artifact: artifact.into(),
        }
    }

    /// The artefact the step loads.
    #[must_use]
    pub fn artifact(&self) -> &Utf8Path {
        &self.artifact
    }
}

impl Step for ComponentStep {
    fn name(&self) -> StepName {
        StepName::Components
    }

    fn reload(&self, _ctx: &ReloadContext<'_>) -> StepOutcome {
        let Some(command) = &self.command else {
            return StepOutcome::failed(
                "no build command is configured; set --build so a component change can rebuild",
                vec![
                    "devserver.build: no build command is configured for the component step"
                        .to_owned(),
                ],
            );
        };
        let output = match self.runner.run(command) {
            Ok(output) => output,
            Err(error) => return StepOutcome::failed(error.to_string(), vec![error.to_string()]),
        };
        if !output.ok {
            let message = if output.stderr_tail.is_empty() {
                output.stdout_tail.clone()
            } else {
                output.stderr_tail.clone()
            };
            return StepOutcome::failed(
                format!(
                    "`{}` failed; the running component is unchanged",
                    command.display()
                ),
                vec![format!(
                    "devserver.build: `{}` failed:\n{message}",
                    command.display()
                )],
            );
        }
        match self.host.reload(&self.app, &self.artifact) {
            Ok(hash) => {
                StepOutcome::ok(format!("{} reloaded ({hash})", self.app)).with_artifact(hash)
            }
            Err(error) => StepOutcome::failed(error.to_string(), vec![error.to_string()]),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_lines() {
        let text = "1\n2\n3\n4";
        assert_eq!(tail(text, 2), "3\n4");
        assert_eq!(tail(text, 9), "1\n2\n3\n4");
    }

    #[test]
    fn content_hash_is_stable_and_distinct() {
        assert_eq!(content_hash(b"abc"), content_hash(b"abc"));
        assert_ne!(content_hash(b"abc"), content_hash(b"abd"));
    }
}
