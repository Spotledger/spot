//! The fixed privileged operation templates and their parameter validation.
//!
//! A template is a program (absolute path) and a token list: literals and typed parameter
//! slots. Argv is built **only** from the template: parameters fill the slots after
//! validation and are never split, joined or interpreted. Validation runs in the agent
//! (before it asks) and again in the helper (which trusts nothing).

/// The type of a parameter slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One of a closed set of strings.
    Enum(&'static [&'static str]),
    /// A systemd unit name with an allowed suffix.
    Unit,
    /// A POSIX account name.
    Account,
    /// A Windows service short name.
    ServiceName,
    /// A decimal integer in `min..=max`.
    Int {
        /// Lowest value.
        min: u64,
        /// Highest value.
        max: u64,
    },
    /// An absolute path confined (after canonicalisation) to `root`.
    Path {
        /// The allow-listed root directory.
        root: &'static str,
    },
}

/// One token of a template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tok {
    /// A fixed argv entry.
    Lit(&'static str),
    /// A validated parameter.
    Param(Kind),
}

/// One privileged operation template.
#[derive(Clone, Copy, Debug)]
pub struct Template {
    /// The operation name (`service.restart`), the argument of `spot-agent-priv run`.
    pub name: &'static str,
    /// The absolute program path.
    pub program: &'static str,
    /// The argv tokens after the program.
    pub tokens: &'static [Tok],
    /// Per-operation timeout in seconds.
    pub timeout_secs: u64,
    /// Output cap in bytes (stdout and stderr each).
    pub output_cap: usize,
    /// Whether the operation needs the catalogue's confirmation flow (reboot, delete data,
    /// network change); the agent enforces it before asking the helper.
    pub confirmation: bool,
}

const UNIT: Tok = Tok::Param(Kind::Unit);
const NP: Tok = Tok::Lit("--no-pager");
const SYSTEMCTL: &str = "/usr/bin/systemctl";

macro_rules! service {
    ($name:expr, $verb:expr, $conf:expr) => {
        Template {
            name: $name,
            program: SYSTEMCTL,
            tokens: &[NP, Tok::Lit($verb), UNIT],
            timeout_secs: 60,
            output_cap: 65_536,
            confirmation: $conf,
        }
    };
}

/// The staging directory certificates are copied from.
pub const CERT_STAGING_ROOT: &str = "/var/lib/spot-agent/staging";
/// The destination root of installed certificates.
pub const CERT_DEST_ROOT: &str = "/etc/pki/ca-trust/source/anchors";

/// Every privileged template. The sudoers generator and the helper read this one table.
pub const TEMPLATES: &[Template] = &[
    service!("service.start", "start", false),
    service!("service.stop", "stop", true),
    service!("service.restart", "restart", false),
    service!("service.reload", "reload", false),
    service!("service.enable", "enable", false),
    service!("service.disable", "disable", false),
    service!("service.mask", "mask", true),
    service!("service.unmask", "unmask", false),
    service!("service.status", "is-active", false),
    Template {
        name: "user.lock",
        program: "/usr/sbin/usermod",
        tokens: &[Tok::Lit("-L"), Tok::Param(Kind::Account)],
        timeout_secs: 30,
        output_cap: 16_384,
        confirmation: false,
    },
    Template {
        name: "user.unlock",
        program: "/usr/sbin/usermod",
        tokens: &[Tok::Lit("-U"), Tok::Param(Kind::Account)],
        timeout_secs: 30,
        output_cap: 16_384,
        confirmation: false,
    },
    Template {
        name: "cert.install",
        program: "/usr/bin/install",
        tokens: &[
            Tok::Lit("-m"),
            Tok::Lit("0644"),
            Tok::Lit("-o"),
            Tok::Lit("root"),
            Tok::Lit("-g"),
            Tok::Lit("root"),
            Tok::Param(Kind::Path {
                root: CERT_STAGING_ROOT,
            }),
            Tok::Param(Kind::Path {
                root: CERT_DEST_ROOT,
            }),
        ],
        timeout_secs: 30,
        output_cap: 16_384,
        confirmation: false,
    },
];

/// Why a template or parameter was refused. Never carries the offending value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// No template of that name.
    UnknownOperation,
    /// The parameter count does not match the template.
    ParamCount,
    /// A parameter failed validation (index into the parameters).
    BadParam(usize),
}

impl Refused {
    /// A stable code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::UnknownOperation => "agent.privilege_unknown_operation",
            Self::ParamCount => "agent.privilege_param_count",
            Self::BadParam(_) => "agent.privilege_bad_param",
        }
    }
}

/// The template called `name`.
#[must_use]
pub fn find(name: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.name == name)
}

impl Template {
    /// The parameter slots, in order.
    #[must_use]
    pub fn slots(&self) -> Vec<Kind> {
        self.tokens
            .iter()
            .filter_map(|t| match t {
                Tok::Param(k) => Some(*k),
                Tok::Lit(_) => None,
            })
            .collect()
    }

    /// Validates `params` and builds the argv (without the program).
    ///
    /// # Errors
    /// [`Refused::ParamCount`] or [`Refused::BadParam`].
    pub fn build(&self, params: &[String]) -> Result<Vec<String>, Refused> {
        let slots = self.slots();
        if slots.len() != params.len() {
            return Err(Refused::ParamCount);
        }
        let mut next = params.iter().enumerate();
        let mut argv = Vec::with_capacity(self.tokens.len());
        for token in self.tokens {
            match token {
                Tok::Lit(l) => argv.push((*l).to_owned()),
                Tok::Param(kind) => {
                    let (i, value) = next.next().ok_or(Refused::ParamCount)?;
                    argv.push(validate(*kind, value).map_err(|()| Refused::BadParam(i))?);
                }
            }
        }
        Ok(argv)
    }

    /// Tries to read a plain `program args...` command as this template, returning the
    /// parameter values; used by the agent side to route a module's command to the helper.
    #[must_use]
    pub fn match_argv(&self, program: &str, args: &[String]) -> Option<Vec<String>> {
        let base = program.rsplit('/').next().unwrap_or(program);
        let own = self.program.rsplit('/').next().unwrap_or(self.program);
        if base != own || (program.contains('/') && program != self.program) {
            return None;
        }
        if args.len() != self.tokens.len() {
            return None;
        }
        let mut params = Vec::new();
        for (token, arg) in self.tokens.iter().zip(args) {
            match token {
                Tok::Lit(l) if *l == arg => {}
                Tok::Lit(_) => return None,
                Tok::Param(_) => params.push(arg.clone()),
            }
        }
        // The agent validates too, so a bad value is never even sent.
        self.build(&params).ok()?;
        Some(params)
    }
}

/// Finds the template a plain command corresponds to, with its parameters.
#[must_use]
pub fn route(program: &str, args: &[String]) -> Option<(&'static Template, Vec<String>)> {
    TEMPLATES
        .iter()
        .find_map(|t| t.match_argv(program, args).map(|p| (t, p)))
}

/// Validates one value for `kind`; returns the value to put in argv (canonical for paths).
///
/// # Errors
/// `Err(())` for anything that does not match; the value is never echoed.
#[allow(clippy::result_unit_err)]
pub fn validate(kind: Kind, value: &str) -> Result<String, ()> {
    if value.is_empty() || value.len() > 4096 || value.starts_with('-') {
        return Err(());
    }
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(());
    }
    match kind {
        Kind::Enum(set) => set.contains(&value).then(|| value.to_owned()).ok_or(()),
        Kind::ServiceName => (v_service(value)).then(|| value.to_owned()).ok_or(()),
        Kind::Unit => valid_unit(value).then(|| value.to_owned()).ok_or(()),
        Kind::Account => valid_account(value).then(|| value.to_owned()).ok_or(()),
        Kind::Int { min, max } => {
            if !value.bytes().all(|b| b.is_ascii_digit()) || value.len() > 19 {
                return Err(());
            }
            let n: u64 = value.parse().map_err(|_| ())?;
            (min..=max).contains(&n).then(|| value.to_owned()).ok_or(())
        }
        Kind::Path { root } => confine(value, root)
            .map(|p| p.display().to_string())
            .ok_or(()),
    }
}

fn v_service(v: &str) -> bool {
    v.len() <= 256
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '$'))
}

const UNIT_SUFFIXES: &[&str] = &[
    ".service", ".socket", ".timer", ".target", ".mount", ".path",
];

fn valid_unit(v: &str) -> bool {
    v.len() <= 255
        && UNIT_SUFFIXES.iter().any(|s| v.ends_with(s))
        && !v.contains("..")
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '@' | '-' | '\\'))
}

/// Whether `v` is a valid POSIX account name.
#[must_use]
pub fn valid_account_name(v: &str) -> bool {
    valid_account(v)
}

fn valid_account(v: &str) -> bool {
    let mut chars = v.chars();
    v.len() <= 32
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Confines `value` to `root`: absolute, no `..`, restricted alphabet, and after
/// canonicalisation (of the path, or of its parent when it does not exist yet) still under
/// the canonical root. Symlinks that escape the root are therefore refused.
#[must_use]
pub fn confine(value: &str, root: &str) -> Option<std::path::PathBuf> {
    use std::path::{Component, Path, PathBuf};
    if !value.starts_with('/')
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+'))
    {
        return None;
    }
    let path = Path::new(value);
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return None;
    }
    let lexical_ok = path.starts_with(root) && path != Path::new(root);
    if !lexical_ok {
        return None;
    }
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| PathBuf::from(root));
    // Canonicalise the nearest existing ancestor (symlinks resolved) and re-append the
    // components that do not exist yet.
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        if existing.symlink_metadata().is_ok() {
            return None; // a dangling symlink
        }
        tail.push(existing.file_name()?.to_owned());
        existing = existing.parent()?.to_path_buf();
    }
    let mut resolved = std::fs::canonicalize(&existing).ok()?;
    for part in tail.into_iter().rev() {
        resolved.push(part);
    }
    (resolved.starts_with(&canonical_root) && resolved != canonical_root).then_some(resolved)
}
