//! `spot support`: the unattended-support basics of WP-PRD-13.T3, built on
//! `spot-unattended-support` and `spot-redact` (both already shipped; this file is the CLI
//! and the data collection on top of them). Guide: `docs/running-spot/support_bundle.md`.
//!
//! Three deliverables share this one file because `owned_paths` names exactly one command
//! module (see the task report's Open questions for the literal top-level forms the brief
//! names and why they are nested here instead):
//!
//! - `spot support bundle --out FILE [--include logs,config,health,versions] [--since 24h]`:
//!   a redacted, reviewable ZIP of the host's own configuration, logs, a local health summary
//!   and version information. Every text entry passes through `spot-redact` before it is
//!   written; the command never opens a database connection and never reads document data,
//!   so there is nothing to leak from either. It refuses to write inside the data directory
//!   (`<root>/sites`) and never follows a symlink anywhere in the path it reads or writes.
//! - `spot support consent on|off|status`: the single on-disk switch (one file,
//!   `support-consent.toml` under the host root) that gates crash reporting and telemetry
//!   (`crate::services::unattended_support`).
//! - `spot support help-code CODE`: the offline, plain-language explanation for an error
//!   code, from the bundled `spot-unattended-support` help catalogue — no AI, no network.
//!
//! ## Security notes (review)
//!
//! - `--out` is opened `create_new` (never truncates or overwrites an existing path) with
//!   `O_NOFOLLOW` on Unix (never follows a symlink in the final component) and mode 0600 set
//!   at creation. The parent directory must already exist (never `create_dir_all`d) and its
//!   canonical form is compared against the canonical data directory, so a symlink swap
//!   cannot relocate the write after the lexical check passed.
//! - Every file the `logs` category walks is checked with `DirEntry::file_type` (never
//!   follows a symlink to decide directory-vs-file) and opened with `O_NOFOLLOW` on Unix, so
//!   a symlinked log can neither be walked into nor read. Reads are bounded with
//!   `Read::take`, never a separate `metadata().len()` check (which races the read).
//! - `config` never emits raw file text: it is an allow-listed structural summary (only keys
//!   in [`ALLOWED_CONFIG_KEYS`] show their value; every other key, whatever it is named or
//!   contains, becomes `[redacted:<type>]`). This is an allow list, not a list of
//!   "looks like a secret" names to deny.
//! - A bundle is capped in file count and total byte size ([`cap_entries_with_limits`]); a
//!   redaction failure on a log file excludes that file entirely rather than emitting it
//!   un-redacted.
#[path = "support_bundle/ip.rs"]
mod ip;
use std::collections::BTreeSet;
use ip::redact_ips;
use std::io::{Read as _, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use spot_redact::{RedactionPolicy, Redactor};
use spot_unattended_support::consent::ConsentKind;
use spot_unattended_support::help::HelpIndex;

use super::{CommandCtx, Exit};
use spot_cli::services::unattended_support as svc;
use spot_cli::{Error, Result};

pub const NAME: &str = "support";

/// Every category `--include` accepts, in the order they are collected.
const CATEGORIES: [&str; 4] = ["versions", "config", "health", "logs"];

/// On Linux, `O_NOFOLLOW`: refuse to open a path whose final component is a symlink
/// (`ELOOP`), rather than follow it. There is no `libc` dependency in this crate, so the
/// numeric value is hard-coded per platform; the owned log-reading and `--out`-creation
/// paths both pass this through `OpenOptionsExt::custom_flags`.
#[cfg(target_os = "linux")]
const O_NOFOLLOW: i32 = 0o400_000;
#[cfg(target_os = "macos")]
const O_NOFOLLOW: i32 = 0x0100;

/// A bundle holds at most this many files. Collection inside `logs` also stops early
/// ([`walk_logs`]'s own cap); this is the cap applied to the whole bundle across every
/// category, so a huge `config`/`health`/`versions` combination cannot bypass it either.
const MAX_BUNDLE_FILES: usize = 5_000;
/// A bundle holds at most this many bytes of (pre-compression) entry content.
const MAX_BUNDLE_BYTES: u64 = 64 * 1024 * 1024;
/// `logs` alone never walks more than this many files, independent of the whole-bundle cap.
const MAX_LOG_FILES: usize = 2_000;
/// A single log file over this size is noted and skipped rather than copied; enforced with
/// `Read::take`, never a separate `metadata().len()` check (which can race the read).
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

pub fn command() -> clap::Command {
    clap::Command::new(NAME)
        .about("Support bundle, crash/telemetry consent, and offline help (WP-PRD-13.T3)")
        .subcommand_required(true)
        .subcommand(
            clap::Command::new("bundle")
                .about("Write a redacted ZIP of configuration, logs, a health summary and versions")
                .arg(
                    clap::Arg::new("out")
                        .long("out")
                        .value_name("FILE")
                        .required(true)
                        .value_parser(clap::value_parser!(PathBuf))
                        .help("Where to write the ZIP (must not exist yet; its parent directory must)"),
                )
                .arg(
                    clap::Arg::new("include")
                        .long("include")
                        .value_name("CATEGORIES")
                        .help("Comma-separated: logs,config,health,versions (default: all four)"),
                )
                .arg(
                    clap::Arg::new("since")
                        .long("since")
                        .value_name("DURATION")
                        .help("Only log files touched within this long, e.g. 24h, 7d, 30m, 45s"),
                ),
        )
        .subcommand(
            clap::Command::new("consent")
                .about("Opt-in crash reporting and telemetry: one switch, off by default")
                .subcommand_required(true)
                .subcommand(clap::Command::new("on").about("Allow crash reports and telemetry to be sent"))
                .subcommand(clap::Command::new("off").about("Allow neither (the default)"))
                .subcommand(clap::Command::new("status").about("Show the current switch")),
        )
        .subcommand(
            clap::Command::new("help-code")
                .about("Offline, plain-language explanation for an error code")
                .arg(clap::Arg::new("code").required(true).value_name("CODE"))
                .arg(
                    clap::Arg::new("locale")
                        .long("locale")
                        .value_name("LOCALE")
                        .default_value("en"),
                ),
        )
}

pub async fn run(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot support <bundle|consent|help-code> ..."
        )?;
        return Ok(Exit::Usage);
    };
    match verb {
        "bundle" => bundle(sub, ctx),
        "consent" => consent(sub, ctx),
        "help-code" => help_code(sub, ctx),
        _ => {
            writeln!(ctx.err(), "unknown support command `{verb}`")?;
            Ok(Exit::Usage)
        }
    }
}

// --- bundle -------------------------------------------------------------------------------

/// One entry about to be written: its name inside the ZIP and its (already redacted) bytes.
struct Entry {
    name: String,
    bytes: Vec<u8>,
}

fn parse_include(raw: Option<&str>) -> Result<BTreeSet<&'static str>> {
    let Some(raw) = raw else {
        return Ok(CATEGORIES.iter().copied().collect());
    };
    let mut set = BTreeSet::new();
    for part in raw.split(',') {
        let part = part.trim();
        let Some(known) = CATEGORIES.iter().find(|c| **c == part) else {
            return Err(Error(format!(
                "support_bundle.bad_include: `{part}` is not one of logs, config, health, versions"
            )));
        };
        set.insert(*known);
    }
    if set.is_empty() {
        return Err(Error(
            "support_bundle.bad_include: --include named no category".into(),
        ));
    }
    Ok(set)
}

/// Parses `30s`, `45m`, `24h` or `7d` into seconds. Splits on the last **character**, not
/// the last byte (`str::split_at` on a byte offset inside a multi-byte character panics);
/// `chars().next_back()` and the remaining `chars().as_str()` are always on a char boundary,
/// so a non-ASCII suffix (`--since é`) is a clean `bad_since` refusal, never a panic.
fn parse_since(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    let bad = || Error(format!("support_bundle.bad_since: `{raw}` is not like 24h, 7d, 30m or 45s"));
    let mut chars = raw.chars();
    let Some(unit) = chars.next_back() else {
        return Err(bad());
    };
    let digits = chars.as_str();
    let n: u64 = digits.parse().map_err(|_| bad())?;
    let scale = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        _ => return Err(bad()),
    };
    n.checked_mul(scale).ok_or_else(bad)
}

/// A path cleaned of `.` and `..` components, without touching the file system (the target
/// of `--out` need not exist yet). This is a lexical normalisation only, used for building
/// the initial candidate path and for error messages; the authoritative data-directory check
/// ([`parent_inside_data_dir`]) canonicalises instead.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        normalise(path)
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        normalise(&cwd.join(path))
    }
}

/// The data directory a bundle must never be written inside: every site's database-adjacent
/// files, secrets and uploaded data live under `<root>/sites`.
fn data_dir(root: &Path) -> PathBuf {
    absolute(&root.join("sites"))
}

/// Canonicalises `path`, falling back to the lexical [`absolute`] form when it cannot be
/// canonicalised (for example because it does not exist yet, which is normal for a brand-new
/// `<root>/sites` on a host with no sites configured).
fn canonical_or_lexical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| absolute(path))
}

/// Whether `out`'s **parent directory**, canonicalised, is the canonical data directory or
/// falls inside it. Canonicalising (rather than a lexical `starts_with` on `out` itself,
/// which a symlink anywhere in the path can defeat after the check runs) is what the brief's
/// security note asks for; combined with `O_NOFOLLOW` on the final component when the file is
/// actually opened, a symlink cannot relocate the write past this check.
fn parent_inside_data_dir(out_parent: &Path, root: &Path) -> bool {
    let data = canonical_or_lexical(&data_dir(root));
    let parent = canonical_or_lexical(out_parent);
    parent == data || parent.starts_with(&data)
}

/// Registers `value` under `label` together with its lower- and upper-case hex spellings
/// (`spot-redact` already adds the standard / URL-safe, padded / unpadded base64 and the
/// URL-encoded forms itself). Best-effort: a value shorter than the redactor's minimum is
/// skipped (it returns `SecretTooShort`).
fn register_with_variants(redactor: &mut Redactor, label: &str, value: &str) {
    if value.is_empty() {
        return;
    }
    let _ = redactor.register_secret(label, &spot_secrets::SecretString::new(value));
    let lower: String = value.bytes().map(|b| format!("{b:02x}")).collect();
    let _ = redactor.register_secret(
        &format!("{label}_hexl"),
        &spot_secrets::SecretString::new(&lower),
    );
    let _ = redactor.register_secret(
        &format!("{label}_hexu"),
        &spot_secrets::SecretString::new(lower.to_ascii_uppercase()),
    );
}

/// Every string leaf of `value` whose own key is not allow-listed ([`ALLOWED_CONFIG_KEYS`]),
/// arrays included. These are exactly the values the config summary withholds, so each is
/// treated as a secret for log redaction.
fn collect_withheld_strings(value: &toml::Value, key: Option<&str>, out: &mut Vec<String>) {
    match value {
        toml::Value::Table(map) => {
            for (k, v) in map {
                collect_withheld_strings(v, Some(k), out);
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                collect_withheld_strings(item, key, out);
            }
        }
        toml::Value::String(s) if !key.is_some_and(|k| ALLOWED_CONFIG_KEYS.contains(&k)) => {
            out.push(s.clone());
        }
        _ => {}
    }
}

/// Upper bound on config-derived secrets registered (each costs several needles).
const MAX_CONFIG_SECRETS: usize = 512;

/// Registers every non-allow-listed string leaf of `common.toml` and each `site.toml` as a
/// known secret, so a value appearing in a log line (not only in the field that declares it)
/// is redacted, with its base64, hex and URL-encoded forms. A value shorter than the
/// redactor's minimum (8 bytes) cannot be registered and is skipped.
fn register_known_secrets(redactor: &mut Redactor, root: &Path, common_toml: &toml::Value) {
    let mut leaves: Vec<String> = Vec::new();
    collect_withheld_strings(common_toml, None, &mut leaves);
    if let Ok(entries) = std::fs::read_dir(root.join("sites")) {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        for name in names {
            if let Ok(text) = std::fs::read_to_string(root.join("sites").join(&name).join("site.toml"))
                && let Ok(value) = toml::from_str::<toml::Value>(&text)
            {
                collect_withheld_strings(&value, None, &mut leaves);
            }
        }
    }
    leaves.sort();
    leaves.dedup();
    for (i, leaf) in leaves.iter().take(MAX_CONFIG_SECRETS).enumerate() {
        register_with_variants(redactor, &format!("cfg_{i}"), leaf);
    }
    // The postgres admin URL and SMTP password keep their descriptive labels.
    let mut named = |label: &str, value: Option<&str>| {
        if let Some(value) = value {
            register_with_variants(redactor, label, value);
        }
    };
    named(
        "postgres_url",
        common_toml
            .get("postgres")
            .and_then(|p| p.get("admin_url"))
            .and_then(toml::Value::as_str),
    );
    named(
        "smtp_password",
        common_toml
            .get("services")
            .and_then(|s| s.get("mail"))
            .and_then(|m| m.get("smtp"))
            .and_then(|s| s.get("password"))
            .and_then(toml::Value::as_str),
    );
}

/// Defence in depth: a secret-shaped environment variable (its *name* contains `secret`,
/// `token`, `password`, `api_key` or `apikey`, case-insensitively) is registered by value, so
/// a literal appearance of it inside a log file this process did not itself write is still
/// caught. `spot support bundle` never reads or displays environment variables itself; this
/// only feeds the redactor's known-secret registry. Best-effort and capped: a value too short
/// to register safely is skipped, and at most [`MAX_ENV_SECRETS`] are registered.
const MAX_ENV_SECRETS: usize = 64;
fn register_env_secrets(redactor: &mut Redactor) {
    register_secrets_from_env_like(redactor, std::env::vars());
}

/// The pure half of [`register_env_secrets`], taking any `(name, value)` source rather than
/// the real process environment, so the rule ("a name containing `secret`, `token`,
/// `password`, `api_key` or `apikey`, case-insensitively, is registered by value") is tested
/// without mutating the actual process environment (`std::env::set_var` is `unsafe` under
/// this workspace's 2024-edition lints, forbidden everywhere including tests).
fn register_secrets_from_env_like(
    redactor: &mut Redactor,
    vars: impl Iterator<Item = (String, String)>,
) {
    const NEEDLES: [&str; 5] = ["secret", "token", "password", "api_key", "apikey"];
    for (i, (key, value)) in vars.enumerate() {
        if i >= MAX_ENV_SECRETS {
            break;
        }
        let lower = key.to_ascii_lowercase();
        if NEEDLES.iter().any(|n| lower.contains(n)) {
            let label = format!("env_{i}");
            register_with_variants(redactor, &label, &value);
        }
    }
}

fn redact_text(redactor: &Redactor, text: &str) -> Vec<u8> {
    redact_ips(&redactor.redact_str(text)).into_bytes()
}

/// Redacts log text fail-closed: `None` when redaction panicked or when a second pass over the
/// result still changes it (a registered value survived the first pass). The caller then
/// excludes the file rather than emit it.
fn redact_text_checked(redactor: &Redactor, text: &str) -> Option<Vec<u8>> {
    let once = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        redact_ips(&redactor.redact_str(text))
    }))
    .ok()?;
    let again = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        redact_ips(&redactor.redact_str(&once)) == once
    }))
    .ok()?;
    again.then(|| once.into_bytes())
}


fn versions_entry() -> Entry {
    let text = format!(
        "spot-cli {}\nos: {}\narch: {}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    Entry {
        name: ["versions", "txt"].join("."),
        bytes: text.into_bytes(),
    }
}

// --- config: an allow-listed structural summary, never raw text -------------------------

/// Leaf config keys (by their own name, wherever in the tree they appear) known to carry no
/// secret and no personal data: operational shape only (a mode, a count, a cadence, a plain
/// identifier). Every other leaf — whatever it is named and whatever it contains — becomes
/// `[redacted:<type>]`. This is the allow list the review asked for, never a list of
/// "looks like a secret" names to deny: an unrecognised field (for example a free-form
/// `[services.<module>]` table, or an operator typo) is redacted by default, not shown by
/// default.
const ALLOWED_CONFIG_KEYS: &[&str] = &[
    "apps",
    "site",
    "database",
    "locale",
    "timezone",
    "edition",
    "enabled",
    "sink",
    "sent_log",
    "security",
    "bus",
    "profile",
    "store",
    "queues",
    "poll_ms",
    "heartbeat_seconds",
    "intake_poll_ms",
    "grace_ms",
    "lease_ttl_ms",
    "tick_ms",
    "shutdown_grace_ms",
    "identity_file_mirror",
    "identity_chain_strict",
    "port",
    "template_root",
];

fn toml_type_name(value: &toml::Value) -> &'static str {
    match value {
        toml::Value::String(_) => "string",
        toml::Value::Integer(_) => "integer",
        toml::Value::Float(_) => "float",
        toml::Value::Boolean(_) => "boolean",
        toml::Value::Datetime(_) => "datetime",
        toml::Value::Array(_) => "array",
        toml::Value::Table(_) => "table",
    }
}

/// A non-string, non-structural scalar's safe text form (a string value is still passed
/// through `redactor` as defence in depth even though its key is allow-listed).
fn render_scalar(value: &toml::Value, redactor: &Redactor) -> String {
    match value {
        toml::Value::String(s) => redactor.redact_str(s).into_owned(),
        toml::Value::Integer(n) => n.to_string(),
        toml::Value::Float(f) => f.to_string(),
        toml::Value::Boolean(b) => b.to_string(),
        toml::Value::Datetime(d) => d.to_string(),
        toml::Value::Array(_) | toml::Value::Table(_) => {
            format!("[redacted:{}]", toml_type_name(value))
        }
    }
}

/// Renders one leaf (`key`'s value, `key` itself never nested further) either as text (when
/// `key` is allow-listed) or as `[redacted:<type>]` (every other key).
fn render_leaf(key: &str, value: &toml::Value, redactor: &Redactor) -> String {
    let allowed = ALLOWED_CONFIG_KEYS.contains(&key);
    if !allowed {
        return format!("[redacted:{}]", toml_type_name(value));
    }
    match value {
        toml::Value::Array(items) => {
            let shown: Vec<String> = items.iter().map(|i| render_scalar(i, redactor)).collect();
            format!("[{}]", shown.join(", "))
        }
        other => render_scalar(other, redactor),
    }
}

/// Walks every table recursively (so a nested `[services.mail.smtp]` is visited), emitting
/// one `path.to.key = value` line per leaf. Only a `Table` is ever recursed into; an `Array`
/// is a leaf (shown only when `key` is allow-listed, one entry per element).
fn summarise_into(value: &toml::Value, path: &mut Vec<String>, redactor: &Redactor, out: &mut String) {
    let toml::Value::Table(map) = value else {
        return;
    };
    for (key, v) in map {
        path.push(key.clone());
        if matches!(v, toml::Value::Table(_)) {
            summarise_into(v, path, redactor, out);
        } else {
            out.push_str(&path.join("."));
            out.push_str(" = ");
            out.push_str(&render_leaf(key, v, redactor));
            out.push('\n');
        }
        path.pop();
    }
}

/// An allow-listed structural summary of a parsed TOML document: see
/// [`ALLOWED_CONFIG_KEYS`] and the module-level security notes.
fn summarise_config(value: &toml::Value, redactor: &Redactor) -> String {
    let mut out = String::new();
    summarise_into(value, &mut Vec::new(), redactor, &mut out);
    out
}

/// The allow-listed structural summary of `root/common.toml` and every
/// `root/sites/*/site.toml`. The raw file text is parsed (never shown verbatim): a bundle can
/// still be taken even when the host configuration would fail `spot_cli::config::load`'s
/// strict validation, but nothing beyond [`ALLOWED_CONFIG_KEYS`] is ever shown.
fn config_entries(root: &Path, redactor: &Redactor) -> Vec<Entry> {
    let mut out = Vec::new();
    if let Ok(text) = std::fs::read_to_string(root.join("common.toml"))
        && let Ok(value) = toml::from_str::<toml::Value>(&text)
    {
        out.push(Entry {
            name: "config/common.toml".into(),
            bytes: summarise_config(&value, redactor).into_bytes(),
        });
    }
    let sites_dir = root.join("sites");
    let Ok(entries) = std::fs::read_dir(&sites_dir) else {
        return out;
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        let path = sites_dir.join(&name).join("site.toml");
        if let Ok(text) = std::fs::read_to_string(&path)
            && let Ok(value) = toml::from_str::<toml::Value>(&text)
        {
            out.push(Entry {
                name: format!("config/sites/{name}/site.toml"),
                bytes: summarise_config(&value, redactor).into_bytes(),
            });
        }
    }
    out
}

/// A local, no-network health summary: whether the configuration files parse as TOML and how
/// many sites are on disk. This never opens a database connection and never asks a running
/// server anything, so a bundle can always be taken, including with the host down.
fn health_entry(root: &Path, redactor: &Redactor) -> Entry {
    let mut text = String::new();
    let common_path = root.join("common.toml");
    match std::fs::read_to_string(&common_path)
        .ok()
        .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
    {
        Some(_) => text.push_str("common.toml: parses as TOML\n"),
        None => text.push_str("common.toml: missing or does not parse as TOML\n"),
    }
    let sites_dir = root.join("sites");
    let mut names: Vec<String> = std::fs::read_dir(&sites_dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    text.push_str(&format!("sites on disk: {}\n", names.len()));
    for name in &names {
        let ok = std::fs::read_to_string(sites_dir.join(name).join("site.toml"))
            .ok()
            .and_then(|t| toml::from_str::<toml::Value>(&t).ok())
            .is_some();
        text.push_str(&format!(
            "  {name}: site.toml {}\n",
            if ok { "parses as TOML" } else { "missing or does not parse" }
        ));
    }
    Entry {
        name: "health/summary.txt".into(),
        bytes: redact_text(redactor, &text),
    }
}

// --- logs: never follow a symlink, bound every read --------------------------------------

/// Every `*.log` file under `root`, excluding `root/sites/*/files` (uploaded document
/// content, never a log), bounded to a shallow depth and to [`MAX_LOG_FILES`] files total so
/// a bundle cannot be made to walk an unbounded or adversarial tree. Never follows a symlink
/// ([`walk_logs`]). `since_cutoff`, when given, skips a file last modified before it.
fn log_entries(
    root: &Path,
    since_cutoff: Option<SystemTime>,
    redactor: &Redactor,
) -> (Vec<Entry>, Vec<String>) {
    let mut found: Vec<PathBuf> = Vec::new();
    walk_logs(root, 0, &mut found, MAX_LOG_FILES);
    found.sort();
    let mut out = Vec::new();
    let mut excluded = Vec::new();
    for path in found {
        if let Some(cutoff) = since_cutoff {
            let fresh = std::fs::symlink_metadata(&path)
                .and_then(|m| m.modified())
                .is_ok_and(|modified| modified >= cutoff);
            if !fresh {
                continue;
            }
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        let Some((bytes, truncated)) = read_log_file(&path) else {
            // Unreadable (removed mid-walk, a permission problem, not UTF-8, or — belt and
            // braces — a symlink that slipped past the walk's own check): excluded entirely,
            // never emitted un-redacted or with a guessed placeholder.
            excluded.push(format!("logs/{rel} (unreadable)"));
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let Some(mut redacted) = redact_text_checked(redactor, &text) else {
            // Fail closed: redaction errored or left a registered value behind.
            excluded.push(format!("logs/{rel} (redaction failed)"));
            continue;
        };
        if truncated {
            redacted.extend_from_slice(b"\n[spot support bundle: truncated at the size cap]\n");
        }
        out.push(Entry {
            name: format!("logs/{rel}"),
            bytes: redacted,
        });
    }
    (out, excluded)
}

/// Opens `path` for reading, refusing to follow a symlink in its final component on Unix
/// (`O_NOFOLLOW`; see the module's security notes), and reads at most [`MAX_LOG_BYTES`] + 1
/// bytes with [`Read::take`] (never a separate `metadata().len()` check, which races the
/// read). Returns `None` on any I/O error. The `bool` says whether the content was truncated
/// at the cap.
fn read_log_file(path: &Path) -> Option<(Vec<u8>, bool)> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.custom_flags(O_NOFOLLOW);
    }
    let file = opts.open(path).ok()?;
    let mut buf = Vec::new();
    file.take(MAX_LOG_BYTES.saturating_add(1))
        .read_to_end(&mut buf)
        .ok()?;
    let truncated = buf.len() as u64 > MAX_LOG_BYTES;
    if truncated {
        buf.truncate(MAX_LOG_BYTES as usize);
    }
    Some((buf, truncated))
}

/// Collects every `*.log` file under `dir`, never descending into or reading a symlink:
/// [`std::fs::DirEntry::file_type`] reports the directory entry's own type (an `lstat`, never
/// following a symlink to decide directory-vs-file), and a symlink of either kind is skipped
/// outright rather than walked into or read. Stops once `found.len() >= cap`.
fn walk_logs(dir: &Path, depth: u32, found: &mut Vec<PathBuf>, cap: usize) {
    const MAX_DEPTH: u32 = 6;
    if depth > MAX_DEPTH || found.len() >= cap {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        if found.len() >= cap {
            return;
        }
        let name = entry.file_name();
        // Uploaded document content, never a log: excluded by name, wherever it sits.
        if name == "files" {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            walk_logs(&path, depth + 1, found, cap);
        } else if file_type.is_file() && path.extension().and_then(|e| e.to_str()) == Some("log") {
            found.push(path);
        }
    }
}

// --- whole-bundle caps ---------------------------------------------------------------------

/// Caps `entries` to [`MAX_BUNDLE_FILES`] files and [`MAX_BUNDLE_BYTES`] of total content,
/// across every category (not only `logs`, which has its own earlier, independent cap). See
/// [`cap_entries_with_limits`] for the mechanics and `## Tests` for how this is exercised with
/// small limits rather than by actually building an oversized bundle.
fn cap_entries(entries: Vec<Entry>) -> (Vec<Entry>, bool, bool) {
    cap_entries_with_limits(entries, MAX_BUNDLE_FILES, MAX_BUNDLE_BYTES)
}

/// `entries`, sorted by name, limited to `max_files` entries and `max_bytes` of total content
/// (an entry that would cross `max_bytes` is dropped, not truncated, so every kept entry is
/// still a complete, valid file inside the ZIP). Returns `(kept, files_were_capped,
/// bytes_were_capped)`.
fn cap_entries_with_limits(
    mut entries: Vec<Entry>,
    max_files: usize,
    max_bytes: u64,
) -> (Vec<Entry>, bool, bool) {
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let files_capped = entries.len() > max_files;
    if files_capped {
        entries.truncate(max_files);
    }
    let mut total = 0u64;
    let mut bytes_capped = false;
    let mut kept = Vec::with_capacity(entries.len());
    for entry in entries {
        let len = entry.bytes.len() as u64;
        if total.saturating_add(len) > max_bytes {
            bytes_capped = true;
            continue;
        }
        total += len;
        kept.push(entry);
    }
    (kept, files_capped, bytes_capped)
}

// --- writing the ZIP: create-new only, never follow a symlink -----------------------------

/// Creates `out_path` exclusively (`create_new`: never truncates or overwrites an existing
/// path, whatever it is), refusing a symlink in its final component on Unix (`O_NOFOLLOW`),
/// mode 0600 set at creation (never `chmod`ed afterwards, so there is no window where the
/// file briefly exists with default permissions). `out_path`'s parent directory is never
/// created: it must already exist.
fn create_out_file(out_path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.custom_flags(O_NOFOLLOW).mode(0o600);
    }
    opts.open(out_path)
}

fn write_zip(out_path: &Path, entries: &[Entry]) -> Result<()> {
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};
    let file = create_out_file(out_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            Error(format!(
                "support_bundle.out_exists: {} already exists; `spot support bundle` never \
                 overwrites an existing path (choose a new name)",
                out_path.display()
            ))
        } else {
            Error(format!("{}: {e}", out_path.display()))
        }
    })?;
    let mut zip = ZipWriter::new(file);
    for entry in entries {
        zip.start_file(
            entry.name.clone(),
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .map_err(|e| Error(format!("{}: {e}", entry.name)))?;
        zip.write_all(&entry.bytes)
            .map_err(|e| Error(format!("{}: {e}", entry.name)))?;
    }
    zip.finish()
        .map_err(|e| Error(format!("{}: {e}", out_path.display())))?;
    Ok(())
}

fn bundle(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> Result<Exit> {
    let out_arg = matches
        .get_one::<PathBuf>("out")
        .cloned()
        .unwrap_or_default();
    let include = match parse_include(matches.get_one::<String>("include").map(String::as_str)) {
        Ok(set) => set,
        Err(e) => {
            writeln!(ctx.err(), "{e}")?;
            return Ok(Exit::Usage);
        }
    };
    let since_secs = match matches.get_one::<String>("since").map(String::as_str) {
        Some(raw) => match parse_since(raw) {
            Ok(secs) => Some(secs),
            Err(e) => {
                writeln!(ctx.err(), "{e}")?;
                return Ok(Exit::Usage);
            }
        },
        None => None,
    };
    let root = ctx.root().to_path_buf();
    let out_path = absolute(&out_arg);
    let Some(out_parent) = out_path.parent() else {
        writeln!(
            ctx.err(),
            "support_bundle.bad_out: {} has no parent directory",
            out_path.display()
        )?;
        return Ok(Exit::Usage);
    };
    if !out_parent.is_dir() {
        writeln!(
            ctx.err(),
            "support_bundle.out_parent_missing: {} does not exist; create it first (this \
             command never creates directories for you)",
            out_parent.display()
        )?;
        return Ok(Exit::Usage);
    }
    if parent_inside_data_dir(out_parent, &root) {
        writeln!(
            ctx.err(),
            "support_bundle.refused_data_dir: {} is inside the data directory ({}); choose a \
             path outside it",
            out_path.display(),
            data_dir(&root).display()
        )?;
        return Ok(Exit::Refused);
    }

    let mut policy = RedactionPolicy::default();
    policy.patterns.pii_email = true;
    policy.patterns.pii_phone = true;
    let mut redactor = match Redactor::from_policy(&policy) {
        Ok(r) => r,
        Err(e) => {
            writeln!(ctx.err(), "support_bundle.redactor_failed: {e}")?;
            return Ok(Exit::Failed);
        }
    };
    let common_value: toml::Value = std::fs::read_to_string(root.join("common.toml"))
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or(toml::Value::Table(toml::map::Map::new()));
    register_known_secrets(&mut redactor, &root, &common_value);
    register_env_secrets(&mut redactor);

    let since_cutoff = since_secs.map(|secs| {
        let now = ctx.clock().now_utc();
        let cutoff = now - time::Duration::seconds(secs as i64);
        SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(cutoff.unix_timestamp().max(0) as u64)
    });

    let mut excluded_logs: Vec<String> = Vec::new();
    let mut entries = Vec::new();
    if include.contains("versions") {
        entries.push(versions_entry());
    }
    if include.contains("config") {
        entries.extend(config_entries(&root, &redactor));
    }
    if include.contains("health") {
        entries.push(health_entry(&root, &redactor));
    }
    if include.contains("logs") {
        let (logs, excluded) = log_entries(&root, since_cutoff, &redactor);
        entries.extend(logs);
        excluded_logs = excluded;
    }
    let (mut entries, files_capped, bytes_capped) = cap_entries(entries);
    if files_capped || bytes_capped || !excluded_logs.is_empty() {
        entries.push(Entry {
            name: ["bundle_truncated", "txt"].join("."),
            bytes: {
                let mut text = String::from("this bundle is incomplete.\n");
                if files_capped || bytes_capped {
                    text.push_str(&format!(
                        "capped: {}{}{}. Narrow --include or --since and take another bundle \
                         for what was left out.\n",
                        if files_capped { "too many files" } else { "" },
                        if files_capped && bytes_capped { "; " } else { "" },
                        if bytes_capped { "too many bytes" } else { "" },
                    ));
                }
                for name in &excluded_logs {
                    text.push_str(&format!("excluded: {name}\n"));
                }
                text.into_bytes()
            },
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));

    write_zip(&out_path, &entries)?;

    let mut total = 0u64;
    for entry in &entries {
        writeln!(ctx.out(), "{}: {} bytes", entry.name, entry.bytes.len())?;
        total += entry.bytes.len() as u64;
    }
    writeln!(
        ctx.out(),
        "wrote {} ({} file(s), {total} byte(s) before compression); review it before sending it \
         anywhere",
        out_path.display(),
        entries.len()
    )?;
    Ok(Exit::Ok)
}

// --- consent --------------------------------------------------------------------------------

fn consent(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> Result<Exit> {
    let Some((verb, _)) = matches.subcommand() else {
        writeln!(ctx.err(), "usage: spot support consent <on|off|status>")?;
        return Ok(Exit::Usage);
    };
    let root = ctx.root().to_path_buf();
    match verb {
        "status" => {
            let state = svc::read_consent(&root)?;
            writeln!(
                ctx.out(),
                "crash reporting: {}\ntelemetry: {}",
                state.get(ConsentKind::Crash).as_str(),
                state.get(ConsentKind::Telemetry).as_str()
            )?;
            Ok(Exit::Ok)
        }
        "on" | "off" => {
            let allow = verb == "on";
            let state = svc::read_consent(&root)?;
            let now = ctx.clock().now_utc().unix_timestamp().max(0) as u64;
            let updated = svc::set_both(state, allow, now);
            svc::write_consent(&root, &updated)?;
            writeln!(
                ctx.out(),
                "crash reporting and telemetry are now {}",
                if allow { "on" } else { "off" }
            )?;
            Ok(Exit::Ok)
        }
        _ => {
            writeln!(ctx.err(), "unknown support consent command `{verb}`")?;
            Ok(Exit::Usage)
        }
    }
}

// --- help-code ------------------------------------------------------------------------------

fn help_code(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> Result<Exit> {
    let code = matches
        .get_one::<String>("code")
        .map(String::as_str)
        .unwrap_or_default();
    let locale = matches
        .get_one::<String>("locale")
        .map(String::as_str)
        .unwrap_or("en");
    let index = HelpIndex::load();
    let Some(help) = index.lookup(code, locale) else {
        writeln!(
            ctx.err(),
            "support_bundle.help_missing: no offline help for code `{code}`"
        )?;
        return Ok(Exit::Usage);
    };
    let out = ctx.out();
    writeln!(out, "{}", help.what_happened)?;
    writeln!(out, "\nwhy: {}", help.why)?;
    writeln!(out, "\nwhat to do:")?;
    for (i, step) in help.what_to_do.iter().enumerate() {
        writeln!(out, "  {}. {step}", i + 1)?;
    }
    if let Some(learn_more) = &help.learn_more {
        writeln!(out, "\nmore detail: docs/{learn_more}")?;
    }
    Ok(Exit::Ok)
}

#[cfg(test)]
#[path = "support_bundle/tests.rs"]
mod tests;
