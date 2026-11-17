//! Command line: `enrol`, `run`, `status`, `rotate-key`, `version`.

use std::path::PathBuf;

use crate::error::AgentError;
use crate::keystore::KeyStoreKind;

/// A parsed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `enrol --gateway URL --token TOKEN [--ca FILE]`.
    Enrol {
        /// Gateway base URL.
        gateway: String,
        /// The token text.
        token: String,
        /// CA PEM path.
        ca: Option<String>,
        /// Key store.
        key_store: KeyStoreKind,
    },
    /// `rotate-key --token TOKEN`.
    RotateKey {
        /// The fresh one-time token.
        token: String,
        /// Key store.
        key_store: KeyStoreKind,
    },
    /// `run`.
    Run,
    /// `status`.
    Status,
    /// `version`.
    Version,
}

/// The parsed command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cli {
    /// `--state-dir` override.
    pub state_dir: Option<PathBuf>,
    /// The command.
    pub command: Command,
}

/// The usage text.
pub const USAGE: &str = "usage: spot-agent [--state-dir DIR] <command>\n  enrol --gateway URL (--token TOKEN | --token-file FILE) [--ca FILE] [--key-store file|tpm]\n  run\n  status\n  rotate-key (--token TOKEN | --token-file FILE) [--key-store file|tpm]\n  version";

fn usage(msg: &str) -> AgentError {
    AgentError::Usage(msg.to_owned())
}

/// Parses `args` (without the program name). `read_file` reads a token file.
///
/// # Errors
/// [`AgentError::Usage`].
pub fn parse(
    args: &[String],
    read_file: &dyn Fn(&str) -> Option<String>,
) -> Result<Cli, AgentError> {
    let mut state_dir = None;
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.peek() {
        if a.as_str() == "--state-dir" {
            iter.next();
            state_dir = Some(PathBuf::from(
                iter.next()
                    .ok_or_else(|| usage("--state-dir needs a value"))?,
            ));
        } else {
            break;
        }
    }
    let name = iter
        .next()
        .ok_or_else(|| usage("a command is required"))?
        .clone();
    let rest: Vec<&String> = iter.collect();
    let mut gateway = None;
    let mut token = None;
    let mut ca = None;
    let mut key_store = KeyStoreKind::File;
    let mut i = 0;
    while i < rest.len() {
        let flag = rest[i].as_str();
        let value = rest.get(i + 1).map(|s| s.as_str());
        match (flag, value) {
            ("--gateway", Some(v)) => gateway = Some(v.to_owned()),
            ("--token", Some(v)) => token = Some(v.to_owned()),
            ("--token-file", Some(v)) => {
                token = Some(
                    read_file(v)
                        .map(|t| t.trim().to_owned())
                        .ok_or_else(|| usage("cannot read the token file"))?,
                );
            }
            ("--ca", Some(v)) => ca = Some(v.to_owned()),
            ("--key-store", Some(v)) => key_store = KeyStoreKind::parse(v)?,
            _ => return Err(usage("unknown or incomplete option")),
        }
        i += 2;
    }
    let no_options = gateway.is_none() && token.is_none() && ca.is_none();
    let command = match name.as_str() {
        "enrol" => Command::Enrol {
            gateway: gateway.ok_or_else(|| usage("--gateway is required"))?,
            token: token.ok_or_else(|| usage("--token is required"))?,
            ca,
            key_store,
        },
        "rotate-key" => {
            if gateway.is_some() || ca.is_some() {
                return Err(usage("rotate-key takes only --token"));
            }
            Command::RotateKey {
                token: token.ok_or_else(|| usage("--token is required"))?,
                key_store,
            }
        }
        "run" if no_options => Command::Run,
        "status" if no_options => Command::Status,
        "version" if no_options => Command::Version,
        _ => return Err(usage("unknown command or options")),
    };
    Ok(Cli { state_dir, command })
}
