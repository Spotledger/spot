//! `spot llm`: the operator's side of the LLM service (WP-AI-03.T9). Guide:
//! `docs/running-spot/llm-admin.md`.
//!
//! `show`, `set` (the service's own validator decides), `key set` (a no-echo prompt or one line of
//! stdin, never an argument or the environment; wrapped in the secret type at once, sealed, printed
//! nowhere), `key remove`, `usage`, `sends` (never a text) and `test` (one minimal call through the
//! egress guard: `ok` or the stable `llm_host.*` code only).
//!
//! Every subcommand takes `--site` (optional when the host has one site). Exit codes: 0 done, 1
//! usage, 2 refused (nothing changed), 3 failed. `set`, `key set` and `key remove` write an audit
//! record on the identity audit chain first (kind, tenant, the names of the fields that change,
//! never a value) and change nothing when it cannot be stored. The command works on the site's
//! database like `spot secrets`; a running server reads a change within a minute (its per-tenant
//! stack is rebuilt at most that often), so nothing is asked of it.
use super::{CommandCtx, Exit};
use spot_cli::Result;
use spot_cli::services::egress::{self as egress, EgressHost, Paths};
use spot_cli::services::llm::Settings as LlmSettings;
use spot_cli::services::llm::config::TenantConfig;
use spot_cli::services::llm::gateway::{
    BuiltinProviders, CallError, Caller, Deps, EgressTransports, LlmGateway,
};
use spot_cli::services::llm::prompt::{DenyAllReader, PromptPolicy};
use spot_cli::services::llm::store::{SendRecord, Store, UsageRow};
use spot_cli::services::secrets::{self as secrets, OperatorAudit};
use spot_egress::SystemResolver;
use spot_redact::{RedactionPolicy, Redactor};
use spot_secrets::SecretString;
use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Duration;

pub const NAME: &str = "llm";

/// The services section of `common.toml` the LLM service reads.
const SECTION: &str = "llm";

/// The audit kinds, one per change.
const AUDIT_CONFIG_SET: &str = "llm_config_set";
const AUDIT_KEY_SET: &str = "llm_key_set";
const AUDIT_KEY_REMOVED: &str = "llm_key_removed";

/// The user and purpose of the connection test's send record.
const OPERATOR: &str = "operator";
const TEST_PURPOSE: &str = "connection test";

pub fn command() -> clap::Command {
    let text = |name: &'static str, value: &'static str, help: &'static str| {
        clap::Arg::new(name).long(name).value_name(value).help(help)
    };
    let number = |name: &'static str, help: &'static str| {
        text(name, "N", help).value_parser(clap::value_parser!(u64))
    };
    let flag = |name: &'static str, help: &'static str| {
        clap::Arg::new(name)
            .long(name)
            .help(help)
            .action(clap::ArgAction::SetTrue)
    };
    let site = || {
        text(
            "site",
            "SITE",
            "The site to act on (optional when the host has exactly one)",
        )
    };
    let tenant = || {
        text(
            "tenant",
            "TENANT",
            "The tenant (its name in the egress policy)",
        )
        .required(true)
    };
    clap::Command::new(NAME)
        .about("Configure a tenant's model provider, enter its key, read usage and send records, test the connection")
        .subcommand_required(true)
        .subcommand(
            clap::Command::new("show")
                .about("A tenant's configuration without its key (every tenant when none is named)")
                .arg(text("tenant", "TENANT", "Only this tenant"))
                .arg(flag("json", "One JSON object per tenant"))
                .arg(site()),
        )
        .subcommand(
            clap::Command::new("set")
                .about("Replace a tenant's provider configuration (the key is kept)")
                .arg(tenant())
                .arg(
                    text("kind", "KIND", "The provider family")
                        .required(true)
                        .value_parser(["anthropic", "openrouter", "openai", "ollama"]),
                )
                .arg(text("endpoint", "URL", "The base URL: no credentials, query or fragment").required(true))
                .arg(text("model", "MODEL", "The model id as the endpoint spells it").required(true))
                .arg(
                    text("allow-host", "HOST", "A host the tenant's calls may reach (exact or *.suffix); repeat; must cover the endpoint")
                        .action(clap::ArgAction::Append),
                )
                .arg(
                    text("task", "TASK", "A task the tenant may run; repeat (default: chat)")
                        .action(clap::ArgAction::Append),
                )
                .arg(number("price-in", "Micro-units per million input tokens"))
                .arg(number("price-out", "Micro-units per million output tokens"))
                .arg(
                    number("budget-day", "A daily budget in micro-units (needs a price)")
                        .conflicts_with("budget-month"),
                )
                .arg(
                    number("budget-month", "A monthly budget in micro-units (needs a price)")
                        .conflicts_with("budget-day"),
                )
                .arg(text("embedding-model", "MODEL", "The model that embeds text (not for anthropic)"))
                .arg(
                    text("embedding-dimensions", "N", "The vector width asked of the embedding model")
                        .value_parser(clap::value_parser!(u32)),
                )
                .arg(site()),
        )
        .subcommand(
            clap::Command::new("key")
                .about("Store or remove a tenant's key (write-only)")
                .subcommand_required(true)
                .subcommand(
                    clap::Command::new("set")
                        .about("Read the key from a no-echo prompt, or one line of stdin, and seal it")
                        .arg(tenant())
                        .arg(flag("stdin", "Read one line from standard input instead of prompting"))
                        .arg(site()),
                )
                .subcommand(
                    clap::Command::new("remove")
                        .about("Delete the tenant's stored key")
                        .arg(tenant())
                        .arg(site()),
                ),
        )
        .subcommand(
            clap::Command::new("usage")
                .about("Settled usage per day, task, provider and model")
                .arg(tenant())
                .arg(text("since", "DAYS", "Only days this recent, such as 7d (default: all)"))
                .arg(flag("json", "One JSON object per row"))
                .arg(site()),
        )
        .subcommand(
            clap::Command::new("sends")
                .about("The send records, newest first: what went where, never a text")
                .arg(tenant())
                .arg(
                    text("limit", "N", "How many records")
                        .value_parser(clap::value_parser!(u32).range(1..=1000))
                        .default_value("20"),
                )
                .arg(site()),
        )
        .subcommand(
            clap::Command::new("test")
                .about("One minimal call through the egress guard: prints ok or the llm_host code")
                .arg(tenant())
                .arg(site()),
        )
}

/// Where the audit records of the changing subcommands go. A failure is the text to print; the
/// subcommand then changes nothing.
pub trait AuditTrail: Send + Sync {
    /// Records `kind` for `tenant`, with the names of the fields that change.
    fn record(
        &self,
        kind: &str,
        tenant: &str,
        fields: &[String],
    ) -> std::result::Result<(), String>;
}

/// Where a key comes from: the terminal or one line of standard input. The key is returned as
/// it was typed, to be wrapped in the secret type at once.
pub trait KeyInput: Send {
    /// One key. `from_stdin` is `--stdin`.
    fn read_key(&mut self, from_stdin: bool) -> std::result::Result<String, String>;
}

/// What the subcommands act on. Production builds it from the site (see [`run`]); a test builds
/// it over the in-memory store.
pub struct Services<'a> {
    /// The gateway: configuration, keys, usage, sends and the test call.
    pub gateway: &'a LlmGateway,
    /// The store the gateway runs on, for the listings the gateway has no call for.
    pub store: &'a Store,
    /// The audit chain.
    pub audit: &'a dyn AuditTrail,
    /// The key source.
    pub keys: &'a mut dyn KeyInput,
}

/// The site, the gateway over its database and the audit chain, or the exit to return (already
/// reported).
struct Prod {
    gateway: Arc<LlmGateway>,
    store: Store,
    audit: ChainTrail,
}

/// [`AuditTrail`] over the site's identity audit chain; `None` for the read-only subcommands,
/// which never record.
struct ChainTrail(Option<OperatorAudit>);

impl AuditTrail for ChainTrail {
    fn record(
        &self,
        kind: &str,
        tenant: &str,
        fields: &[String],
    ) -> std::result::Result<(), String> {
        let Some(chain) = &self.0 else {
            return Err("the audit chain was not opened for this subcommand".to_owned());
        };
        // The record carries the tenant and the names of the fields, never a value.
        let reason = format!("tenant={tenant};fields={}", fields.join(","));
        chain
            .required(kind, fields.len() as u64, Some(&reason))
            .map_err(|e| e.to_string())
    }
}

/// The key from the terminal (no echo) or one line of standard input.
struct TerminalKeys;

impl KeyInput for TerminalKeys {
    fn read_key(&mut self, from_stdin: bool) -> std::result::Result<String, String> {
        if from_stdin {
            let mut line = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut line)
                .map_err(|_| "the key could not be read from standard input".to_owned())?;
            return Ok(line.trim_end_matches(['\r', '\n']).to_owned());
        }
        prompt("API key (not echoed): ")
    }
}

/// A line from the controlling terminal with echo off (`stty`); never from an argument or the
/// environment.
fn prompt(label: &str) -> std::result::Result<String, String> {
    #[cfg(unix)]
    {
        let none = || "no terminal; use --stdin".to_owned();
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|_| none())?;
        let reader = tty.try_clone().map_err(|_| none())?;
        let mut terminal = tty.try_clone().map_err(|_| none())?;
        let stty = |args: &[&str]| -> bool {
            let stdin = tty
                .try_clone()
                .map_or(std::process::Stdio::null(), Into::into);
            std::process::Command::new("stty")
                .args(args)
                .stdin(stdin)
                .status()
                .is_ok_and(|s| s.success())
        };
        write!(terminal, "{label}")
            .and_then(|()| terminal.flush())
            .map_err(|_| none())?;
        if !stty(&["-echo"]) {
            return Err("cannot turn terminal echo off; use --stdin".to_owned());
        }
        let mut line = String::new();
        let read = std::io::BufReader::new(reader).read_line(&mut line);
        stty(&["echo"]);
        let _ = writeln!(terminal);
        read.map_err(|_| "the terminal could not be read".to_owned())?;
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }
    #[cfg(not(unix))]
    {
        let _ = label;
        Err("no echo-free terminal here; use --stdin".to_owned())
    }
}

/// Opens the site: its secrets vault, its database and the egress guard, and builds the gateway
/// the way the server does. `writes` opens the audit chain too.
async fn open_site(
    ctx: &mut CommandCtx,
    sub: &clap::ArgMatches,
    writes: bool,
) -> Result<std::result::Result<Prod, Exit>> {
    let loaded = spot_cli::config::load(ctx.root(), false)?;
    let site = match sub.get_one::<String>("site") {
        Some(site) => site.clone(),
        None => match loaded.sites.as_slice() {
            [only] => only.site.clone(),
            _ => {
                writeln!(
                    ctx.err(),
                    "name the site with --site: the host has several or none"
                )?;
                return Ok(Err(Exit::Usage));
            }
        },
    };
    let Some(site_cfg) = loaded.sites.iter().find(|s| s.site == site) else {
        writeln!(ctx.err(), "there is no site `{}`", site.escape_debug())?;
        return Ok(Err(Exit::Usage));
    };
    let settings: LlmSettings = match loaded.common.services.services_extra.get(SECTION) {
        Some(table) => match table.clone().try_into() {
            Ok(settings) => settings,
            Err(_) => {
                writeln!(
                    ctx.err(),
                    "llm_host.settings_invalid: [services.llm] is not valid"
                )?;
                return Ok(Err(Exit::Refused));
            }
        },
        None => LlmSettings::default(),
    };
    if let Err(problem) = settings.check() {
        writeln!(ctx.err(), "{problem}")?;
        return Ok(Err(Exit::Refused));
    }
    let Some(vault_host) = secrets::open(ctx.root(), &loaded, site_cfg).await? else {
        writeln!(
            ctx.err(),
            "llm_host.secrets_unavailable: common.toml has no [services.secrets] table, so keys cannot be sealed"
        )?;
        return Ok(Err(Exit::Refused));
    };
    let backend = ctx.backend(&site).await?;
    let store = Store::pg(backend.pool(), &site);
    if store.prepare().await.is_err() {
        writeln!(
            ctx.err(),
            "llm_host.store_unavailable: the LLM tables could not be created"
        )?;
        return Ok(Err(Exit::Failed));
    }
    let egress_settings =
        egress::Settings::from_table(loaded.common.services.services_extra.get(egress::NAME))?;
    let paths = Paths::resolve(
        &egress_settings,
        Some(ctx.root()),
        &site,
        loaded.sites.len(),
    )?;
    let host = EgressHost::build(
        &egress_settings,
        paths,
        ctx.clock().clone(),
        Arc::new(SystemResolver),
    )?;
    let redactor = Redactor::from_policy(&RedactionPolicy::default()).map_err(|_| {
        spot_cli::Error("llm_host.settings_invalid: the redaction rules did not compile".into())
    })?;
    let gateway = Arc::new(LlmGateway::new(Deps {
        site: site.clone(),
        store: store.clone(),
        vault: vault_host.vault(),
        guard: host.shared(),
        clock: ctx.clock().clone(),
        providers: Arc::new(BuiltinProviders),
        transports: Arc::new(EgressTransports {
            total_timeout: Duration::from_secs(settings.request_timeout_seconds),
        }),
        redactor: Arc::new(redactor),
        reader: Arc::new(DenyAllReader),
        max_retries: 0,
        retention_days: settings.retention_days,
    }));
    let audit = if writes {
        match OperatorAudit::open(&loaded, site_cfg, ctx.clock().clone()).await {
            Ok(chain) => ChainTrail(Some(chain)),
            Err(e) => {
                writeln!(ctx.err(), "{e}")?;
                return Ok(Err(Exit::Refused));
            }
        }
    } else {
        ChainTrail(None)
    };
    Ok(Ok(Prod {
        gateway,
        store,
        audit,
    }))
}

pub async fn run(matches: &clap::ArgMatches, ctx: &mut CommandCtx) -> Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot llm <show|set|key|usage|sends|test> [--site SITE]"
        )?;
        return Ok(Exit::Usage);
    };
    // `key` has a verb of its own; the site argument sits on it.
    let leaf = match (verb, sub.subcommand()) {
        ("key", Some((_, leaf))) => leaf,
        _ => sub,
    };
    let writes = matches!(verb, "set" | "key");
    let prod = match open_site(ctx, leaf, writes).await? {
        Ok(prod) => prod,
        Err(exit) => return Ok(exit),
    };
    let mut keys = TerminalKeys;
    let mut services = Services {
        gateway: &prod.gateway,
        store: &prod.store,
        audit: &prod.audit,
        keys: &mut keys,
    };
    execute(matches, ctx, &mut services).await
}

/// Runs the parsed subcommand against `svc`. Public so the tests drive it over the in-memory
/// store with a scripted egress sender.
///
/// # Errors
///
/// Only a failure to write to the command's own output.
pub async fn execute(
    matches: &clap::ArgMatches,
    ctx: &mut CommandCtx,
    svc: &mut Services<'_>,
) -> Result<Exit> {
    let Some((verb, sub)) = matches.subcommand() else {
        writeln!(
            ctx.err(),
            "usage: spot llm <show|set|key|usage|sends|test> [--site SITE]"
        )?;
        return Ok(Exit::Usage);
    };
    match verb {
        "show" => show(ctx, sub, svc).await,
        "set" => set(ctx, sub, svc).await,
        "key" => match sub.subcommand() {
            Some(("set", leaf)) => key_set(ctx, leaf, svc).await,
            Some(("remove", leaf)) => key_remove(ctx, leaf, svc).await,
            _ => {
                writeln!(ctx.err(), "usage: spot llm key <set|remove> --tenant T")?;
                Ok(Exit::Usage)
            }
        },
        "usage" => show_usage(ctx, sub, svc).await,
        "sends" => sends(ctx, sub, svc).await,
        "test" => connection_test(ctx, sub, svc).await,
        _ => {
            writeln!(ctx.err(), "unknown llm command `{}`", verb.escape_debug())?;
            Ok(Exit::Usage)
        }
    }
}

/// The exit a failed gateway call maps to: a refusal by policy, a missing piece or a spent budget
/// is 2 (nothing was sent or changed); a provider, store or vault failure is 3.
fn exit_of(error: &CallError) -> Exit {
    match error {
        CallError::Provider(_) | CallError::Store(_) | CallError::Vault => Exit::Failed,
        _ => Exit::Refused,
    }
}

/// Prints a failed call (its stable code and fixed sentence) and returns its exit.
fn fail(ctx: &mut CommandCtx, error: &CallError) -> Result<Exit> {
    writeln!(ctx.err(), "{error}")?;
    Ok(exit_of(error))
}

fn tenant_of(sub: &clap::ArgMatches) -> String {
    sub.get_one::<String>("tenant").cloned().unwrap_or_default()
}

/// `Some(config)` when the tenant has one; otherwise the refusal is printed and the exit returned.
///
/// It takes the gateway, not the whole [`Services`]: `&Services` is not `Send` (the key source is
/// `Send` but not `Sync`), and this future is held across an await inside a `Send` command future.
async fn require_tenant(
    ctx: &mut CommandCtx,
    gateway: &LlmGateway,
    tenant: &str,
) -> Result<std::result::Result<TenantConfig, Exit>> {
    match gateway.config(tenant).await {
        Ok(Some(config)) => Ok(Ok(config)),
        Ok(None) => {
            writeln!(
                ctx.err(),
                "{}: the tenant `{}` has no configuration",
                CallError::NotConfigured.code(),
                tenant.escape_debug()
            )?;
            Ok(Err(Exit::Refused))
        }
        Err(error) => Ok(Err(fail(ctx, &error)?)),
    }
}

async fn show(
    ctx: &mut CommandCtx,
    sub: &clap::ArgMatches,
    svc: &mut Services<'_>,
) -> Result<Exit> {
    let tenants: Vec<String> = match sub.get_one::<String>("tenant") {
        Some(tenant) => vec![tenant.clone()],
        None => match svc.store.tenants().await {
            Ok(list) => list,
            Err(error) => {
                writeln!(ctx.err(), "{}: {error}", CallError::Store(error).code())?;
                return Ok(Exit::Failed);
            }
        },
    };
    let json = sub.get_flag("json");
    if tenants.is_empty() && !json {
        writeln!(ctx.out(), "no tenant is configured")?;
        return Ok(Exit::Ok);
    }
    for tenant in &tenants {
        let config = match require_tenant(ctx, svc.gateway, tenant).await? {
            Ok(config) => config,
            Err(exit) => return Ok(exit),
        };
        let has_key = match svc.gateway.has_key(tenant).await {
            Ok(present) => present,
            Err(error) => return fail(ctx, &error),
        };
        let key = if has_key { "present" } else { "absent" };
        if json {
            let mut value = match serde_json::to_value(&config) {
                Ok(value) => value,
                Err(_) => {
                    writeln!(ctx.err(), "a configuration could not be encoded")?;
                    return Ok(Exit::Failed);
                }
            };
            if let Some(object) = value.as_object_mut() {
                object.insert("key".to_owned(), serde_json::Value::from(key));
            }
            writeln!(ctx.out(), "{value}")?;
        } else {
            render_config(ctx, &config, key)?;
        }
    }
    Ok(Exit::Ok)
}

/// One tenant as text. Every field an operator typed is escaped.
fn render_config(ctx: &mut CommandCtx, config: &TenantConfig, key: &str) -> Result<()> {
    let list = |items: &[String]| {
        let escaped: Vec<String> = items.iter().map(|i| i.escape_debug().to_string()).collect();
        escaped.join(", ")
    };
    let budget = config.budget.as_ref().map_or_else(
        || "none".to_owned(),
        |b| {
            let period = format!("{:?}", b.period).to_lowercase();
            format!(
                "{} micro-units per {period}, warning at {}%",
                b.limit_micro, b.warn_at_percent
            )
        },
    );
    let embedding = config.embedding_model.as_ref().map_or_else(
        || "none".to_owned(),
        |model| {
            let width = config
                .embedding_dimensions
                .map_or_else(|| "the model's own".to_owned(), |d| d.to_string());
            format!("{} (width {width})", model.escape_debug())
        },
    );
    let out = ctx.out();
    writeln!(out, "tenant: {}", config.tenant.escape_debug())?;
    writeln!(out, "kind: {}", config.kind.provider_id())?;
    writeln!(out, "endpoint: {}", config.endpoint.escape_debug())?;
    writeln!(out, "model: {}", config.model.escape_debug())?;
    writeln!(out, "allowed hosts: {}", list(&config.allowed_hosts))?;
    writeln!(out, "tasks: {}", list(&config.tasks))?;
    writeln!(
        out,
        "price (micro-units per million tokens): input {}, output {}",
        config.price.input_micro_per_million, config.price.output_micro_per_million
    )?;
    writeln!(out, "budget: {budget}")?;
    writeln!(out, "embedding model: {embedding}")?;
    writeln!(out, "key: {key}")?;
    Ok(())
}

/// The configuration the flags describe, built through the type's own serde so its defaults are
/// the service's, not a copy.
fn config_from(sub: &clap::ArgMatches) -> serde_json::Value {
    let text = |name: &str| sub.get_one::<String>(name).cloned();
    let many = |name: &str| -> Vec<String> {
        sub.get_many::<String>(name)
            .map(|values| values.cloned().collect())
            .unwrap_or_default()
    };
    let mut value = serde_json::json!({
        "tenant": tenant_of(sub),
        "kind": text("kind"),
        "endpoint": text("endpoint"),
        "model": text("model"),
        "allowed_hosts": many("allow-host"),
    });
    if let Some(object) = value.as_object_mut() {
        let tasks = many("task");
        if !tasks.is_empty() {
            object.insert("tasks".to_owned(), serde_json::json!(tasks));
        }
        let price_in = sub.get_one::<u64>("price-in").copied();
        let price_out = sub.get_one::<u64>("price-out").copied();
        if price_in.is_some() || price_out.is_some() {
            object.insert(
                "price".to_owned(),
                serde_json::json!({
                    "input_micro_per_million": price_in.unwrap_or(0),
                    "output_micro_per_million": price_out.unwrap_or(0),
                }),
            );
        }
        let budget = sub
            .get_one::<u64>("budget-day")
            .map(|limit| (*limit, "day"))
            .or_else(|| {
                sub.get_one::<u64>("budget-month")
                    .map(|limit| (*limit, "month"))
            });
        if let Some((limit, period)) = budget {
            object.insert(
                "budget".to_owned(),
                serde_json::json!({ "limit_micro": limit, "period": period }),
            );
        }
        if let Some(model) = text("embedding-model") {
            object.insert("embedding_model".to_owned(), serde_json::json!(model));
        }
        if let Some(width) = sub.get_one::<u32>("embedding-dimensions") {
            object.insert("embedding_dimensions".to_owned(), serde_json::json!(width));
        }
    }
    value
}

/// The names of the fields that differ between the stored configuration and the new one (all of
/// them for a new tenant). Names only.
fn changed_fields(old: Option<&TenantConfig>, new: &TenantConfig) -> Vec<String> {
    let new_value = serde_json::to_value(new).unwrap_or_default();
    let old_value = old.map(|o| serde_json::to_value(o).unwrap_or_default());
    let empty = serde_json::Map::new();
    let new_map = new_value.as_object().unwrap_or(&empty);
    let old_map = old_value.as_ref().and_then(serde_json::Value::as_object);
    let mut names = BTreeSet::new();
    for (name, value) in new_map {
        match old_map.and_then(|m| m.get(name)) {
            Some(previous) if previous == value => {}
            _ => {
                names.insert(name.clone());
            }
        }
    }
    if let Some(old_map) = old_map {
        for name in old_map.keys() {
            if !new_map.contains_key(name) {
                names.insert(name.clone());
            }
        }
    }
    names.into_iter().collect()
}

async fn set(ctx: &mut CommandCtx, sub: &clap::ArgMatches, svc: &mut Services<'_>) -> Result<Exit> {
    let config: TenantConfig = match serde_json::from_value(config_from(sub)) {
        Ok(config) => config,
        Err(_) => {
            writeln!(ctx.err(), "the configuration flags are not usable")?;
            return Ok(Exit::Usage);
        }
    };
    // The service's own validator decides; the sentence is its, and never quotes a value.
    if let Err(problem) = config.validate() {
        writeln!(
            ctx.err(),
            "{}: {problem}",
            CallError::Config(problem).code()
        )?;
        return Ok(Exit::Refused);
    }
    let old = match svc.gateway.config(&config.tenant).await {
        Ok(old) => old,
        Err(error) => return fail(ctx, &error),
    };
    let fields = changed_fields(old.as_ref(), &config);
    if let Err(text) = svc.audit.record(AUDIT_CONFIG_SET, &config.tenant, &fields) {
        writeln!(ctx.err(), "{text}")?;
        writeln!(ctx.err(), "nothing was changed")?;
        return Ok(Exit::Refused);
    }
    if let Err(error) = svc.gateway.put_config(&config).await {
        return fail(ctx, &error);
    }
    let key = match svc.gateway.has_key(&config.tenant).await {
        Ok(true) => "present",
        _ => "absent",
    };
    writeln!(
        ctx.out(),
        "stored the configuration of tenant `{}` (key: {key}); fields changed: {}",
        config.tenant.escape_debug(),
        if fields.is_empty() {
            "none".to_owned()
        } else {
            fields.join(", ")
        }
    )?;
    Ok(Exit::Ok)
}

async fn key_set(
    ctx: &mut CommandCtx,
    sub: &clap::ArgMatches,
    svc: &mut Services<'_>,
) -> Result<Exit> {
    let tenant = tenant_of(sub);
    if let Err(exit) = require_tenant(ctx, svc.gateway, &tenant).await? {
        return Ok(exit);
    }
    // The key is read once, wrapped at once and never printed, logged or put in an error.
    let secret = match svc.keys.read_key(sub.get_flag("stdin")) {
        Ok(line) => SecretString::new(line),
        Err(text) => {
            writeln!(ctx.err(), "{text}")?;
            return Ok(Exit::Usage);
        }
    };
    let fields = ["key".to_owned()];
    if let Err(text) = svc.audit.record(AUDIT_KEY_SET, &tenant, &fields) {
        writeln!(ctx.err(), "{text}")?;
        writeln!(ctx.err(), "nothing was changed")?;
        return Ok(Exit::Refused);
    }
    if let Err(error) = svc.gateway.set_key(&tenant, &secret).await {
        return fail(ctx, &error);
    }
    writeln!(
        ctx.out(),
        "the key of tenant `{}` is stored, sealed and write-only; no command can show it",
        tenant.escape_debug()
    )?;
    Ok(Exit::Ok)
}

async fn key_remove(
    ctx: &mut CommandCtx,
    sub: &clap::ArgMatches,
    svc: &mut Services<'_>,
) -> Result<Exit> {
    let tenant = tenant_of(sub);
    if let Err(exit) = require_tenant(ctx, svc.gateway, &tenant).await? {
        return Ok(exit);
    }
    let fields = ["key".to_owned()];
    if let Err(text) = svc.audit.record(AUDIT_KEY_REMOVED, &tenant, &fields) {
        writeln!(ctx.err(), "{text}")?;
        writeln!(ctx.err(), "nothing was changed")?;
        return Ok(Exit::Refused);
    }
    match svc.gateway.remove_key(&tenant).await {
        Ok(true) => writeln!(
            ctx.out(),
            "the key of tenant `{}` is removed",
            tenant.escape_debug()
        )?,
        Ok(false) => writeln!(
            ctx.out(),
            "tenant `{}` had no stored key",
            tenant.escape_debug()
        )?,
        Err(error) => return fail(ctx, &error),
    }
    Ok(Exit::Ok)
}

/// `7d` as a number of days.
fn parse_days(text: &str) -> Option<u64> {
    let digits = text.strip_suffix('d')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// A UTC day (days since the Unix epoch) as `YYYY-MM-DD`.
fn day_text(day: u64) -> String {
    i32::try_from(day)
        .ok()
        .and_then(|d| time::Date::from_julian_day(2_440_588 + d).ok())
        .map_or_else(|| day.to_string(), |date| date.to_string())
}

async fn show_usage(
    ctx: &mut CommandCtx,
    sub: &clap::ArgMatches,
    svc: &mut Services<'_>,
) -> Result<Exit> {
    let tenant = tenant_of(sub);
    if let Err(exit) = require_tenant(ctx, svc.gateway, &tenant).await? {
        return Ok(exit);
    }
    let now_days = u64::try_from(ctx.clock().now_utc().unix_timestamp()).unwrap_or(0) / 86_400;
    let first = match sub.get_one::<String>("since") {
        Some(text) => match parse_days(text) {
            Some(days) => now_days.saturating_sub(days),
            None => {
                writeln!(ctx.err(), "--since must be a number of days such as 7d")?;
                return Ok(Exit::Usage);
            }
        },
        None => 0,
    };
    let rows: Vec<UsageRow> = match svc.store.usage_since(&tenant, first).await {
        Ok(rows) => rows,
        Err(error) => {
            writeln!(ctx.err(), "{}: {error}", CallError::Store(error).code())?;
            return Ok(Exit::Failed);
        }
    };
    if sub.get_flag("json") {
        for row in &rows {
            let line = serde_json::json!({
                "day": day_text(row.day),
                "task": row.task,
                "provider": row.provider,
                "model": row.model,
                "input": row.input,
                "output": row.output,
                "cache_read": row.cache_read,
                "cache_write": row.cache_write,
                "requests": row.requests,
                "cost_micro": row.cost_micro,
            });
            writeln!(ctx.out(), "{line}")?;
        }
        return Ok(Exit::Ok);
    }
    if rows.is_empty() {
        writeln!(ctx.out(), "no usage recorded")?;
        return Ok(Exit::Ok);
    }
    let mut total_cost: u64 = 0;
    for row in &rows {
        total_cost = total_cost.saturating_add(row.cost_micro);
        writeln!(
            ctx.out(),
            "{} task={} provider={} model={} requests={} input={} output={} cache_read={} cache_write={} cost_micro={}",
            day_text(row.day),
            row.task.escape_debug(),
            row.provider.escape_debug(),
            row.model.escape_debug(),
            row.requests,
            row.input,
            row.output,
            row.cache_read,
            row.cache_write,
            row.cost_micro
        )?;
    }
    writeln!(ctx.out(), "total cost: {total_cost} micro-units")?;
    Ok(Exit::Ok)
}

/// One send record as a text line. Every field a caller controls is escaped.
fn render_send(record: &SendRecord) -> String {
    let when = i64::try_from(record.at_ms / 1_000)
        .ok()
        .and_then(|secs| time::OffsetDateTime::from_unix_timestamp(secs).ok())
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| record.at_ms.to_string());
    let classes: Vec<String> = record
        .classes
        .iter()
        .map(|(class, count)| format!("{}:{count}", class.escape_debug()))
        .collect();
    let doctypes: Vec<String> = record
        .doctypes
        .iter()
        .map(|d| d.escape_debug().to_string())
        .collect();
    format!(
        "{when} {} task={} endpoint={} provider={} model={} digest={} size={} doctypes=[{}] classes=[{}] user={} purpose={} tokens_in={} tokens_out={}",
        record.outcome.escape_debug(),
        record.task.escape_debug(),
        record.endpoint.escape_debug(),
        record.provider.escape_debug(),
        record.model.escape_debug(),
        record.digest.get(..12).unwrap_or(&record.digest),
        record.size_bytes,
        doctypes.join(","),
        classes.join(","),
        record.principal.escape_debug(),
        record.purpose.escape_debug(),
        record.input_tokens,
        record.output_tokens
    )
}

async fn sends(
    ctx: &mut CommandCtx,
    sub: &clap::ArgMatches,
    svc: &mut Services<'_>,
) -> Result<Exit> {
    let tenant = tenant_of(sub);
    if let Err(exit) = require_tenant(ctx, svc.gateway, &tenant).await? {
        return Ok(exit);
    }
    let limit = sub.get_one::<u32>("limit").copied().unwrap_or(20);
    let records = match svc.gateway.sends(&tenant, limit).await {
        Ok(records) => records,
        Err(error) => return fail(ctx, &error),
    };
    if records.is_empty() {
        writeln!(ctx.out(), "no send recorded")?;
        return Ok(Exit::Ok);
    }
    for record in &records {
        writeln!(ctx.out(), "{}", render_send(record))?;
    }
    Ok(Exit::Ok)
}

/// One minimal, content-free call through the gateway: the allow-list, the egress guard (after
/// DNS), the key, the budget and the send record all apply; the tokens are counted as usual.
async fn connection_test(
    ctx: &mut CommandCtx,
    sub: &clap::ArgMatches,
    svc: &mut Services<'_>,
) -> Result<Exit> {
    let tenant = tenant_of(sub);
    let config = match require_tenant(ctx, svc.gateway, &tenant).await? {
        Ok(config) => config,
        Err(exit) => return Ok(exit),
    };
    let Some(task) = config.tasks.first().cloned() else {
        writeln!(ctx.out(), "{}", CallError::TaskUnknown.code())?;
        return Ok(Exit::Refused);
    };
    let (mut request, manifest) = svc
        .gateway
        .prompt(OPERATOR, PromptPolicy::default())
        .text("ping")
        .build(&config.model, "connection-test");
    request.max_output_tokens = Some(1);
    let caller = Caller {
        user: OPERATOR.to_owned(),
        purpose: TEST_PURPOSE.to_owned(),
    };
    match svc
        .gateway
        .complete(&tenant, &task, &caller, request, &manifest)
        .await
    {
        Ok(_) => {
            writeln!(ctx.out(), "ok")?;
            Ok(Exit::Ok)
        }
        Err(error) => {
            // The code only: the sentence of a provider failure names nothing more useful, and
            // the send record has the rest.
            writeln!(ctx.out(), "{}", error.code())?;
            Ok(exit_of(&error))
        }
    }
}
