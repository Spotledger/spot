//! Typed parameters in, CLI argument lists out. Each builder yields the argument list the
//! `spot` command takes and the equivalent line shown to the operator; they differ only in
//! that the line shows a secret as `<prompt>`.
//!
//! Every value rides as one `--flag=value` argument, so a value that begins with `-` (a free
//! text `reason`) can never be read as a flag, and nothing is ever passed through a shell.

use spot_mgmt_ops::{ParamValue, SECRET_PLACEHOLDER, Shell, Validated, quote};

/// The argument list and the equivalent line of one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliLine {
    /// Arguments for the CLI, without the program name and `--root`.
    pub argv: Vec<String>,
    /// The line to show: `spot ...`, secrets as `<prompt>`.
    pub display: String,
}

/// The text value of a parameter, when present.
#[must_use]
pub fn text<'a>(call: &'a Validated, name: &str) -> Option<&'a str> {
    call.params
        .iter()
        .find(|param| param.name == name)
        .and_then(|param| match &param.value {
            ParamValue::Text(value) => Some(value.as_str()),
            _ => None,
        })
}

struct Builder {
    argv: Vec<String>,
    display: Vec<String>,
}

impl Builder {
    fn new(words: &[&str]) -> Self {
        Self {
            argv: words.iter().map(|w| (*w).to_owned()).collect(),
            display: std::iter::once("spot".to_owned())
                .chain(words.iter().map(|w| (*w).to_owned()))
                .collect(),
        }
    }

    fn flag(mut self, name: &str, value: &str) -> Self {
        self.argv.push(format!("--{name}={value}"));
        self.display
            .push(format!("--{name} {}", quote(value, Shell::Posix)));
        self
    }

    fn secret(mut self, name: &str, value: &str) -> Self {
        self.argv.push(format!("--{name}={value}"));
        self.display.push(format!("--{name} {SECRET_PLACEHOLDER}"));
        self
    }

    fn done(self) -> CliLine {
        CliLine {
            argv: self.argv,
            display: self.display.join(" "),
        }
    }
}

/// `spot backup run --site S --target T`.
#[must_use]
pub fn backup_run(site: &str, target: &str) -> CliLine {
    Builder::new(&["backup", "run"])
        .flag("site", site)
        .flag("target", target)
        .done()
}

/// `spot backup list --site S`.
#[must_use]
pub fn backup_list(site: &str) -> CliLine {
    Builder::new(&["backup", "list"]).flag("site", site).done()
}

/// `spot backup verify --site S --id ID`.
#[must_use]
pub fn backup_verify(site: &str, backup_id: &str) -> CliLine {
    Builder::new(&["backup", "verify"])
        .flag("site", site)
        .flag("id", backup_id)
        .done()
}

/// `spot restore preflight --site S --from ID --target T`.
#[must_use]
pub fn restore_preflight(site: &str, backup_id: &str, target: &str) -> CliLine {
    Builder::new(&["restore", "preflight"])
        .flag("site", site)
        .flag("from", backup_id)
        .flag("target", target)
        .done()
}

/// `spot restore apply --site S --from ID --target T --confirm TOKEN`.
#[must_use]
pub fn restore_apply(site: &str, backup_id: &str, target: &str, confirm: &str) -> CliLine {
    Builder::new(&["restore", "apply"])
        .flag("site", site)
        .flag("from", backup_id)
        .flag("target", target)
        .secret("confirm", confirm)
        .done()
}

/// `spot migrate-apply maintenance enter --site S --reason R`.
#[must_use]
pub fn maintenance_enter(site: &str, reason: &str) -> CliLine {
    Builder::new(&["migrate-apply", "maintenance", "enter"])
        .flag("site", site)
        .flag("reason", reason)
        .done()
}

/// `spot migrate-apply maintenance leave --site S`.
#[must_use]
pub fn maintenance_leave(site: &str) -> CliLine {
    Builder::new(&["migrate-apply", "maintenance", "leave"])
        .flag("site", site)
        .done()
}

/// `spot migrate-hold status --site S`.
#[must_use]
pub fn hold_status(site: &str) -> CliLine {
    Builder::new(&["migrate-hold", "status"])
        .flag("site", site)
        .done()
}

/// Whether a confirm token is plausible before any command is started: non-empty, printable
/// ASCII without spaces (what `spot restore preflight` prints). The CLI is the judge of whether
/// it is the right token, still unused and unexpired.
#[must_use]
pub fn plausible_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 200 && token.bytes().all(|b| b.is_ascii_graphic())
}
