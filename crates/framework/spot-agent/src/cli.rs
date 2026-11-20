//! Command line: `enrol`, `run`, `status`, `probe`, `rotate-key`, `version`.

use std::path::PathBuf;

use crate::error::AgentError;
use crate::keystore::KeyStoreKind;
use crate::service_cmd::{ServiceRequest, parse_service_request};

/// A parsed command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// `enrol --gateway URL --token TOKEN [--ca FILE] [--allow-software-keys]`.
    Enrol {
        /// Gateway base URL.
        gateway: String,
        /// The token text.
        token: String,
        /// CA PEM path.
        ca: Option<String>,
        /// Key store.
        key_store: KeyStoreKind,
        /// Accept software administrator keys on a host without hardware-backed keys.
        allow_software_keys: bool,
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
    /// `probe`: the health run of a new version during a self-update.
    Probe,
    /// `service install|uninstall|start|stop|status|run`.
    Service(ServiceRequest),
    /// `audit-verify [--file PATH] [--agent-id ID]`: checks the local audit log's chain.
    AuditVerify {
        /// The log file; the state directory's current log when `None`.
        file: Option<String>,
        /// The agent id the chain starts from; the enrolled agent's when `None`.
        agent_id: Option<String>,
    },
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
pub const USAGE: &str = "usage: spot-agent [--state-dir DIR] <command>\n  enrol --gateway URL (--token TOKEN | --token-file FILE) [--ca FILE] [--key-store file|tpm] [--allow-software-keys]\n  run\n  status\n  probe\n  audit-verify [--file PATH] [--agent-id ID]\n  service install|uninstall|start|stop|status [--print] (install: [--binary PATH] [--user NAME] [--privileged-helper])\n  rotate-key (--token TOKEN | --token-file FILE) [--key-store file|tpm]\n  version";

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
    if name == "service" {
        let words: Vec<String> = iter.cloned().collect();
        return Ok(Cli {
            state_dir,
            command: Command::Service(parse_service_request(&words)?),
        });
    }
    let rest: Vec<&String> = iter.collect();
    let mut gateway = None;
    let mut token = None;
    let mut ca = None;
    let mut key_store = KeyStoreKind::File;
    let mut allow_software_keys = false;
    let mut file = None;
    let mut agent_id = None;
    let mut i = 0;
    while i < rest.len() {
        let flag = rest[i].as_str();
        if flag == "--allow-software-keys" {
            // A switch without a value; the policy is chosen at enrolment only.
            allow_software_keys = true;
            i += 1;
            continue;
        }
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
            ("--file", Some(v)) => file = Some(v.to_owned()),
            ("--agent-id", Some(v)) => agent_id = Some(v.to_owned()),
            _ => return Err(usage("unknown or incomplete option")),
        }
        i += 2;
    }
    let no_options = gateway.is_none() && token.is_none() && ca.is_none() && !allow_software_keys;
    // `--file` and `--agent-id` belong to `audit-verify` alone.
    let bare = no_options && file.is_none() && agent_id.is_none();
    let command = match name.as_str() {
        "enrol" => Command::Enrol {
            gateway: gateway.ok_or_else(|| usage("--gateway is required"))?,
            token: token.ok_or_else(|| usage("--token is required"))?,
            ca,
            key_store,
            allow_software_keys,
        },
        "rotate-key" => {
            if gateway.is_some() || ca.is_some() || allow_software_keys {
                return Err(usage("rotate-key takes only --token"));
            }
            Command::RotateKey {
                token: token.ok_or_else(|| usage("--token is required"))?,
                key_store,
            }
        }
        "run" if bare => Command::Run,
        "status" if bare => Command::Status,
        "probe" if bare => Command::Probe,
        "audit-verify" if no_options => Command::AuditVerify { file, agent_id },
        "version" if bare => Command::Version,
        _ => return Err(usage("unknown command or options")),
    };
    Ok(Cli { state_dir, command })
}
