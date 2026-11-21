//! `spot mgmt`: the command-line client of the management API (WP-ADM-01.T7, ADR-44). Guide:
//! `docs/running-spot/mgmt.md`.
//!
//! The CLI is an equal client of the API: `list`, `describe OP`, `validate OP`, `run OP`,
//! `status ID` and `cancel ID` call `https://host:port/mgmt/v1/...` over mutual TLS. `validate`
//! prints the **equivalent command line** the server renders; `run` builds the signed envelope
//! locally with the administrator key (`--sign-with FILE`) and posts it.
//!
//! Trust material helpers: `ca init --dir D` (an operator CA, a client certificate and optionally a
//! server certificate, ECDSA P-256, files mode 0600), `key gen --out FILE` (an Ed25519
//! administrator key; prints the public key for the host's `administrator_keys`) and `key sign`
//! (builds a signed envelope offline).
//!
//! # Rules
//!
//! - TLS verification is mandatory and pinned to `--ca`; there is no `--insecure`.
//! - A secret is never a `--param` value (it would sit in the shell history and the process
//!   list): the server's catalogue says which parameters are secret, and those must come from
//!   `--param-file k=PATH` or `--param-env k=NAME`.
//! - Key files must be mode 0600 on Unix; a looser file is refused. Keys are never printed or
//!   logged; the private key never crosses the wire.
//! - Time is the context clock's, never the wall clock.
//!
//! Exit codes: 0 done, 1 usage, 2 refused (`mgmt.forbidden`, `mgmt.approval_required`, no
//! signature key for a signed operation; nothing changed), 3 failed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, Command};
use rustls::pki_types::pem::PemObject as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use serde_json::{Value, json};
use spot_mgmt_api::wire::{self, WireCommand, WireEnvelope};
use spot_signed_command::{Command as SignedCommand, Signer as _, SoftwareSigner, sign};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{CommandCtx, Exit};

pub const NAME: &str = "mgmt";

/// How long a signed command we build stays valid (the contract allows 10 minutes).
const COMMAND_TTL_SECS: i64 = 300;
/// `--wait` polls at most this many times (500 ms apart): ten minutes.
const WAIT_POLLS: u32 = 1200;

fn remote_args(command: Command) -> Command {
    command
        .arg(Arg::new("url").long("url").value_name("URL").required(true).help("The management listener, https://host:port"))
        .arg(Arg::new("cert").long("cert").value_name("FILE").required(true).help("The operator client certificate (PEM)"))
        .arg(Arg::new("key").long("key").value_name("FILE").required(true).help("The operator client key (PEM)"))
        .arg(Arg::new("ca").long("ca").value_name("FILE").required(true).help("The CA that signed the server certificate (PEM); the server is verified against it"))
        .arg(Arg::new("json").long("json").action(ArgAction::SetTrue).help("Print the server's JSON answer"))
}

fn param_args(command: Command) -> Command {
    command
        .arg(
            Arg::new("param")
                .long("param")
                .value_name("K=V")
                .action(ArgAction::Append)
                .help("A parameter value (never a secret)"),
        )
        .arg(
            Arg::new("param-file")
                .long("param-file")
                .value_name("K=PATH")
                .action(ArgAction::Append)
                .help("A parameter value read from a file (for secrets)"),
        )
        .arg(
            Arg::new("param-env")
                .long("param-env")
                .value_name("K=NAME")
                .action(ArgAction::Append)
                .help("A parameter value read from an environment variable (for secrets)"),
        )
}

fn id_arg(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name).value_name(name).required(true).help(help)
}

pub fn command() -> Command {
    Command::new(NAME)
        .about("Call the management API of a site, and create the trust material it needs")
        .subcommand_required(true)
        .subcommand(remote_args(Command::new("list").about("List the operations you may call")))
        .subcommand(remote_args(Command::new("describe").about("Show one operation").arg(id_arg("op", "The operation id, e.g. site.status"))))
        .subcommand(param_args(remote_args(Command::new("validate").about("Check an invocation and show the equivalent command line").arg(id_arg("op", "The operation id")))))
        .subcommand(
            param_args(remote_args(Command::new("run").about("Run an operation").arg(id_arg("op", "The operation id"))))
                .arg(Arg::new("audience").long("audience").value_name("SITE").required(true).help("The site id the command is for"))
                .arg(Arg::new("sign-with").long("sign-with").value_name("FILE").help("The administrator key file (mode 0600)"))
                .arg(Arg::new("sign-with-pkcs11").long("sign-with-pkcs11").value_name("URI").help("A PKCS#11 hardware key (not available in this build)"))
                .arg(Arg::new("wait").long("wait").action(ArgAction::SetTrue).help("Wait for the command to finish")),
        )
        .subcommand(remote_args(Command::new("status").about("The status of a command").arg(id_arg("id", "The command id"))))
        .subcommand(remote_args(Command::new("cancel").about("Cancel a command").arg(id_arg("id", "The command id"))))
        .subcommand(
            Command::new("ca").about("Operator certificate authority").subcommand_required(true).subcommand(
                Command::new("init")
                    .about("Create an operator CA and a client certificate")
                    .arg(Arg::new("dir").long("dir").value_name("DIR").required(true).help("Where to write ca.pem, ca.key, client.pem, client.key"))
                    .arg(Arg::new("cn").long("cn").value_name("NAME").default_value("operator").help("The client certificate's common name (the host's operator allow-list entry is CN=NAME)"))
                    .arg(Arg::new("days").long("days").value_name("N").default_value("365").help("Validity of the certificates in days"))
                    .arg(Arg::new("server-name").long("server-name").value_name("DNS_OR_IP").help("Also issue server.pem/server.key for this name")),
            ),
        )
        .subcommand(
            Command::new("key")
                .about("Administrator signing keys")
                .subcommand_required(true)
                .subcommand(
                    Command::new("gen")
                        .about("Create an administrator key; prints the public key for the host config")
                        .arg(Arg::new("out").long("out").value_name("FILE").required(true).help("The key file to create (mode 0600, never overwritten)")),
                )
                .subcommand(
                    param_args(Command::new("sign").about("Build a signed envelope offline").arg(id_arg("op", "The operation id")))
                        .arg(Arg::new("key-file").long("key-file").value_name("FILE").required(true).help("The administrator key file"))
                        .arg(Arg::new("audience").long("audience").value_name("SITE").required(true).help("The site id the command is for"))
                        .arg(Arg::new("out").long("out").value_name("FILE").help("Write the envelope here instead of printing it")),
                ),
        )
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let outcome = match matches.subcommand() {
        Some(("ca", sub)) => match sub.subcommand() {
            Some(("init", args)) => ca_init(args, ctx),
            _ => Err(Failure::usage("unknown `mgmt ca` command")),
        },
        Some(("key", sub)) => match sub.subcommand() {
            Some(("gen", args)) => key_gen(args, ctx),
            Some(("sign", args)) => key_sign(args, ctx),
            _ => Err(Failure::usage("unknown `mgmt key` command")),
        },
        Some((name, args)) => remote(name, args, ctx).await,
        None => Err(Failure::usage("a subcommand is required")),
    };
    match outcome {
        Ok(()) => Ok(Exit::Ok),
        Err(failure) => {
            writeln!(ctx.err(), "{}", failure.message)?;
            Ok(failure.exit)
        }
    }
}

/// A failed command: the message (never a secret) and how it exits.
struct Failure {
    message: String,
    exit: Exit,
}

impl Failure {
    fn usage(message: &str) -> Self {
        Self {
            message: message.to_owned(),
            exit: Exit::Usage,
        }
    }
    fn refused(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit: Exit::Refused,
        }
    }
    fn failed(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit: Exit::Failed,
        }
    }
}

type Outcome<T> = Result<T, Failure>;

fn arg<'a>(matches: &'a ArgMatches, name: &str) -> &'a str {
    matches.get_one::<String>(name).map_or("", String::as_str)
}

fn say(ctx: &mut CommandCtx, text: &str) -> Outcome<()> {
    writeln!(ctx.out(), "{text}").map_err(|_| Failure::failed("the output could not be written"))
}

// ---- files and keys ---------------------------------------------------------------------------

/// Whether a file's mode lets group or others in (Unix only; elsewhere the check is skipped).
fn too_open(path: &Path) -> Outcome<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(path)
            .map_err(|_| Failure::failed(format!("{}: the file cannot be read", path.display())))?
            .permissions()
            .mode();
        Ok(mode & 0o077 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

/// Creates a new file (never overwrites) with mode 0600 on Unix.
fn write_private(path: &Path, bytes: &[u8]) -> Outcome<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|_| {
        Failure::failed(format!(
            "{}: the file cannot be created (it must not exist)",
            path.display()
        ))
    })?;
    file.write_all(bytes)
        .map_err(|_| Failure::failed(format!("{}: the file cannot be written", path.display())))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex_32(text: &str) -> Option<[u8; 32]> {
    let bytes = text.trim().as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, pair) in bytes.chunks(2).enumerate() {
        let high = char::from(*pair.first()?).to_digit(16)?;
        let low = char::from(*pair.get(1)?).to_digit(16)?;
        *out.get_mut(index)? = u8::try_from(high * 16 + low).ok()?;
    }
    Some(out)
}

/// Reads an administrator key file: 64 hex characters, mode 0600.
fn load_signer(path: &Path) -> Outcome<SoftwareSigner> {
    if too_open(path)? {
        return Err(Failure::refused(format!(
            "{}: the key file is readable by others; use chmod 600",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|_| Failure::failed(format!("{}: the key file cannot be read", path.display())))?;
    let seed = unhex_32(&text).ok_or_else(|| {
        Failure::failed(format!("{}: not an administrator key file", path.display()))
    })?;
    SoftwareSigner::from_seed(&seed)
        .map_err(|_| Failure::failed("the administrator key is unusable"))
}

fn key_gen(args: &ArgMatches, ctx: &mut CommandCtx) -> Outcome<()> {
    use rand::TryRngCore as _;
    let mut seed = [0u8; 32];
    rand::rngs::OsRng
        .try_fill_bytes(&mut seed)
        .map_err(|_| Failure::failed("the operating system's random source failed"))?;
    let signer =
        SoftwareSigner::from_seed(&seed).map_err(|_| Failure::failed("key generation failed"))?;
    write_private(Path::new(arg(args, "out")), hex(&seed).as_bytes())?;
    say(
        ctx,
        &format!("public_key = \"{}\"", hex(&signer.public_key_bytes())),
    )?;
    say(ctx, &format!("key_id     = \"{}\"", signer.key_id()))?;
    say(
        ctx,
        "Add the public key to the host's administrator_keys. The private key stays in the file; guard it.",
    )
}

// ---- parameters ---------------------------------------------------------------------------------

enum Source {
    Literal(String),
    File(PathBuf),
    Env(String),
}

fn split_pair(text: &str) -> Outcome<(String, String)> {
    let (key, value) = text
        .split_once('=')
        .ok_or_else(|| Failure::usage("a parameter is written name=value"))?;
    if key.is_empty() {
        return Err(Failure::usage("a parameter needs a name"));
    }
    Ok((key.to_owned(), value.to_owned()))
}

fn collect(matches: &ArgMatches) -> Outcome<Vec<(String, Source)>> {
    let mut out = Vec::new();
    for (flag, kind) in [("param", 0), ("param-file", 1), ("param-env", 2)] {
        for text in matches.get_many::<String>(flag).into_iter().flatten() {
            let (key, value) = split_pair(text)?;
            out.push((
                key,
                match kind {
                    0 => Source::Literal(value),
                    1 => Source::File(PathBuf::from(value)),
                    _ => Source::Env(value),
                },
            ));
        }
    }
    Ok(out)
}

/// One parameter's wire value: typed by the operation's description when there is one.
fn typed(value: &str, ty: Option<&str>) -> Outcome<Value> {
    match ty {
        Some("bool") => match value {
            "true" => Ok(json!(true)),
            "false" => Ok(json!(false)),
            _ => Err(Failure::usage("a boolean parameter is true or false")),
        },
        Some("int") => value
            .parse::<i64>()
            .map(|n| json!(n))
            .map_err(|_| Failure::usage("an integer parameter is a whole number")),
        _ => Ok(json!(value)),
    }
}

/// Resolves the parameters against the operation's parameter descriptions. A secret parameter given
/// as a literal is refused; a source that cannot be read is a failure naming the parameter only.
fn build_params(sources: &[(String, Source)], described: Option<&Value>) -> Outcome<Value> {
    let mut map = serde_json::Map::new();
    for (name, source) in sources {
        let spec = described
            .and_then(|d| d.get("parameters"))
            .and_then(Value::as_array)
            .and_then(|list| list.iter().find(|p| p["name"] == name.as_str()));
        let secret = spec.is_some_and(|p| p["is_secret"] == true);
        let text = match source {
            Source::Literal(value) => {
                if secret {
                    return Err(Failure::refused(format!(
                        "parameter `{name}` is a secret: give it with --param-file {name}=PATH or --param-env {name}=NAME"
                    )));
                }
                value.clone()
            }
            Source::File(path) => std::fs::read_to_string(path)
                .map(|text| text.trim_end_matches(['\n', '\r']).to_owned())
                .map_err(|_| {
                    Failure::failed(format!("parameter `{name}`: the file cannot be read"))
                })?,
            Source::Env(variable) => std::env::var(variable).map_err(|_| {
                Failure::failed(format!(
                    "parameter `{name}`: the environment variable is not set"
                ))
            })?,
        };
        let ty = spec.and_then(|p| p["type"].as_str());
        map.insert(name.clone(), typed(&text, ty)?);
    }
    Ok(Value::Object(map))
}

// ---- the HTTP client ----------------------------------------------------------------------------

struct Remote {
    host: String,
    port: u16,
    config: Arc<rustls::ClientConfig>,
}

fn load_certs(path: &Path) -> Outcome<Vec<CertificateDer<'static>>> {
    let iter = CertificateDer::pem_file_iter(path).map_err(|_| {
        Failure::failed(format!(
            "{}: the certificate file cannot be read",
            path.display()
        ))
    })?;
    let certs: Result<Vec<_>, _> = iter.collect();
    let certs =
        certs.map_err(|_| Failure::failed(format!("{}: not a PEM certificate", path.display())))?;
    if certs.is_empty() {
        return Err(Failure::failed(format!(
            "{}: no certificate in the file",
            path.display()
        )));
    }
    Ok(certs)
}

fn remote_from(args: &ArgMatches) -> Outcome<Remote> {
    let url = arg(args, "url");
    let rest = url.strip_prefix("https://").ok_or_else(|| {
        Failure::usage("--url must start with https:// (there is no plain or --insecure mode)")
    })?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains('/') {
        return Err(Failure::usage("--url is https://host:port with no path"));
    }
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (
            host.to_owned(),
            port.parse::<u16>()
                .map_err(|_| Failure::usage("--url has a bad port"))?,
        ),
        None => (rest.to_owned(), 443),
    };
    let key_path = Path::new(arg(args, "key"));
    if too_open(key_path)? {
        return Err(Failure::refused(format!(
            "{}: the client key is readable by others; use chmod 600",
            key_path.display()
        )));
    }
    let chain = load_certs(Path::new(arg(args, "cert")))?;
    let key = PrivateKeyDer::from_pem_file(key_path).map_err(|_| {
        Failure::failed(format!(
            "{}: the client key cannot be read",
            key_path.display()
        ))
    })?;
    let mut roots = rustls::RootCertStore::empty();
    for certificate in load_certs(Path::new(arg(args, "ca")))? {
        roots
            .add(certificate)
            .map_err(|_| Failure::failed("the --ca certificate is not usable"))?;
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|_| Failure::failed("TLS 1.3 is not available"))?
        .with_root_certificates(roots)
        .with_client_auth_cert(chain, key)
        .map_err(|_| Failure::failed("the client certificate and key do not match"))?;
    Ok(Remote {
        host,
        port,
        config: Arc::new(config),
    })
}

/// Decodes a `Transfer-Encoding: chunked` body.
fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let end = data.windows(2).position(|w| w == b"\r\n")?;
        let size =
            usize::from_str_radix(std::str::from_utf8(data.get(..end)?).ok()?.trim(), 16).ok()?;
        data = data.get(end + 2..)?;
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(data.get(..size)?);
        data = data.get(size + 2..)?;
    }
}

async fn call(
    remote: &Remote,
    method: &str,
    path: &str,
    body: Option<&str>,
    idempotency: Option<&str>,
) -> Outcome<(u16, Value)> {
    let unreachable = || {
        Failure::failed(format!(
            "{}:{}: the server could not be reached or verified",
            remote.host, remote.port
        ))
    };
    let tcp = tokio::net::TcpStream::connect((remote.host.as_str(), remote.port))
        .await
        .map_err(|_| unreachable())?;
    let name = ServerName::try_from(remote.host.clone())
        .map_err(|_| Failure::usage("--url has a bad host name"))?;
    let mut tls = tokio_rustls::TlsConnector::from(Arc::clone(&remote.config))
        .connect(name, tcp)
        .await
        .map_err(|_| unreachable())?;
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAccept: application/json\r\n",
        remote.host
    );
    if let Some(key) = idempotency {
        request.push_str(&format!("Idempotency-Key: {key}\r\n"));
    }
    match body {
        Some(body) => request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )),
        None => request.push_str("\r\n"),
    }
    tls.write_all(request.as_bytes())
        .await
        .map_err(|_| unreachable())?;
    let mut raw = Vec::new();
    tls.read_to_end(&mut raw).await.map_err(|_| unreachable())?;
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| Failure::failed("the server's answer is not HTTP"))?;
    let head = String::from_utf8_lossy(&raw[..split]).to_lowercase();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| Failure::failed("the server's answer has no status"))?;
    let rest = raw.get(split + 4..).unwrap_or_default();
    let payload = if head.contains("transfer-encoding: chunked") {
        dechunk(rest).ok_or_else(|| Failure::failed("the server's answer is malformed"))?
    } else {
        rest.to_vec()
    };
    Ok((
        status,
        serde_json::from_slice(&payload).unwrap_or(Value::Null),
    ))
}

/// An error answer as a failure: the code, the message and the request id; exit 2 for the refusals
/// that change nothing and need a person, 3 for the rest.
fn api_failure(status: u16, body: &Value) -> Failure {
    let error = &body["error"];
    let code = error["code"].as_str().unwrap_or("mgmt.unknown");
    let message = error["message"]
        .as_str()
        .unwrap_or("the server refused the request");
    let request = error["request_id"].as_str().unwrap_or("-");
    let parameter = error["details"]["parameter"]
        .as_str()
        .map(|name| format!(" (parameter `{name}`)"))
        .unwrap_or_default();
    let text = format!("{code} (HTTP {status}): {message}{parameter} [request {request}]");
    match code {
        "mgmt.forbidden" | "mgmt.approval_required" | "mgmt.refused_by_policy" => {
            Failure::refused(text)
        }
        _ => Failure::failed(text),
    }
}

fn expect(status: u16, body: Value, ok: &[u16]) -> Outcome<Value> {
    if ok.contains(&status) {
        Ok(body)
    } else {
        Err(api_failure(status, &body))
    }
}

fn show(
    ctx: &mut CommandCtx,
    json_mode: bool,
    body: &Value,
    text: impl FnOnce(&Value) -> String,
) -> Outcome<()> {
    if json_mode {
        let pretty = serde_json::to_string_pretty(body)
            .map_err(|_| Failure::failed("the answer cannot be printed"))?;
        say(ctx, &pretty)
    } else {
        say(ctx, &text(&body["data"]))
    }
}

// ---- remote subcommands -------------------------------------------------------------------------

async fn describe(remote: &Remote, op: &str) -> Outcome<Value> {
    let (status, body) = call(
        remote,
        "GET",
        &format!("/mgmt/v1/operations/{op}"),
        None,
        None,
    )
    .await?;
    expect(status, body, &[200]).map(|b| b["data"].clone())
}

async fn remote(name: &str, args: &ArgMatches, ctx: &mut CommandCtx) -> Outcome<()> {
    let remote = remote_from(args)?;
    let json_mode = args.get_flag("json");
    match name {
        "list" => {
            let (status, body) =
                call(&remote, "GET", "/mgmt/v1/operations?limit=100", None, None).await?;
            let body = expect(status, body, &[200])?;
            show(ctx, json_mode, &body, |_| {
                body["data"]
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .map(|op| {
                                format!(
                                    "{:<32} {:<9} {}",
                                    op["id"].as_str().unwrap_or(""),
                                    op["risk"].as_str().unwrap_or(""),
                                    op["title"].as_str().unwrap_or("")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default()
            })
        }
        "describe" => {
            let data = describe(&remote, arg(args, "op")).await?;
            show(ctx, json_mode, &json!({ "data": data }), |d| {
                let mut lines = vec![format!(
                    "{} ({}): {}",
                    d["id"].as_str().unwrap_or(""),
                    d["risk"].as_str().unwrap_or(""),
                    d["summary"].as_str().unwrap_or("")
                )];
                lines.push(format!("requires: {}", d["requires"]));
                for p in d["parameters"].as_array().into_iter().flatten() {
                    let secret = if p["is_secret"] == true {
                        " (secret)"
                    } else {
                        ""
                    };
                    lines.push(format!(
                        "  --param {} [{}]{}: {}",
                        p["name"].as_str().unwrap_or(""),
                        p["type"].as_str().unwrap_or(""),
                        secret,
                        p["doc"].as_str().unwrap_or("")
                    ));
                }
                lines.join("\n")
            })
        }
        "validate" => {
            let op = arg(args, "op");
            let described = describe(&remote, op).await?;
            let params = build_params(&collect(args)?, Some(&described))?;
            let request = json!({ "params": params }).to_string();
            let (status, body) = call(
                &remote,
                "POST",
                &format!("/mgmt/v1/operations/{op}/validate"),
                Some(&request),
                None,
            )
            .await?;
            let body = expect(status, body, &[200])?;
            show(ctx, json_mode, &body, |d| {
                d["command_line"].as_str().unwrap_or("").to_owned()
            })
        }
        "run" => run_command(&remote, args, ctx, json_mode).await,
        "status" => {
            let (status, body) = call(
                &remote,
                "GET",
                &format!("/mgmt/v1/commands/{}", arg(args, "id")),
                None,
                None,
            )
            .await?;
            let body = expect(status, body, &[200])?;
            show(ctx, json_mode, &body, status_line)
        }
        "cancel" => {
            let (status, body) = call(
                &remote,
                "POST",
                &format!("/mgmt/v1/commands/{}/cancel", arg(args, "id")),
                None,
                None,
            )
            .await?;
            let body = expect(status, body, &[200])?;
            show(ctx, json_mode, &body, status_line)
        }
        other => Err(Failure::usage(&format!("unknown mgmt command `{other}`"))),
    }
}

fn status_line(data: &Value) -> String {
    format!(
        "{} {} {}{}",
        data["id"].as_str().unwrap_or(""),
        data["operation"].as_str().unwrap_or(""),
        data["state"].as_str().unwrap_or(""),
        data["error_code"]
            .as_str()
            .map(|c| format!(" ({c})"))
            .unwrap_or_default()
    )
}

fn random_hex_16() -> Outcome<String> {
    use rand::TryRngCore as _;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| Failure::failed("the operating system's random source failed"))?;
    Ok(hex(&bytes))
}

async fn run_command(
    remote: &Remote,
    args: &ArgMatches,
    ctx: &mut CommandCtx,
    json_mode: bool,
) -> Outcome<()> {
    if args.get_one::<String>("sign-with-pkcs11").is_some() {
        return Err(Failure::refused(
            "PKCS#11 hardware keys are not available in this build (no PKCS#11 crate in the workspace); use --sign-with FILE",
        ));
    }
    let op = arg(args, "op");
    let described = describe(remote, op).await?;
    let params = build_params(&collect(args)?, Some(&described))?;
    let risk = described["risk"].as_str().unwrap_or("critical");
    let now = ctx.clock().now_utc().unix_timestamp();
    let audience = arg(args, "audience");
    let body = if let Some(path) = args.get_one::<String>("sign-with") {
        let signer = load_signer(Path::new(path))?;
        let command = SignedCommand::new(
            op,
            params,
            audience,
            signer.key_id(),
            now - 5,
            now + COMMAND_TTL_SECS,
        );
        let signed = sign(&command, &signer)
            .map_err(|_| Failure::failed("the command could not be signed"))?;
        wire::encode(&signed, &[])
            .map_err(|_| Failure::failed("the command could not be encoded"))?
    } else if risk == "read" {
        // A read operation needs the certificate only: an envelope without signatures.
        let envelope = WireEnvelope {
            command: WireCommand {
                version: 1,
                id: random_hex_16()?,
                op: op.to_owned(),
                params,
                audience: audience.to_owned(),
                issuer: "unsigned".to_owned(),
                issued_at: now - 5,
                expires_at: now + COMMAND_TTL_SECS,
                nonce: random_hex_16()?,
            },
            signatures: Vec::new(),
            approvals: Vec::new(),
        };
        serde_json::to_string(&envelope)
            .map_err(|_| Failure::failed("the command could not be encoded"))?
    } else {
        return Err(Failure::refused(format!(
            "`{op}` has risk `{risk}` and needs a signature: add --sign-with FILE"
        )));
    };
    let key = random_hex_16()?;
    let (status, answer) =
        call(remote, "POST", "/mgmt/v1/commands", Some(&body), Some(&key)).await?;
    let answer = expect(status, answer, &[202])?;
    let id = answer["data"]["command_id"]
        .as_str()
        .unwrap_or("")
        .to_owned();
    if !args.get_flag("wait") {
        return show(ctx, json_mode, &answer, |d| {
            format!(
                "accepted: command {}",
                d["command_id"].as_str().unwrap_or("")
            )
        });
    }
    for _ in 0..WAIT_POLLS {
        let (status, body) = call(
            remote,
            "GET",
            &format!("/mgmt/v1/commands/{id}"),
            None,
            None,
        )
        .await?;
        let body = expect(status, body, &[200])?;
        let state = body["data"]["state"].as_str().unwrap_or("");
        if matches!(state, "succeeded" | "failed" | "cancelled" | "refused") {
            show(ctx, json_mode, &body, status_line)?;
            return if state == "succeeded" {
                Ok(())
            } else {
                Err(Failure::failed(format!("the command ended {state}")))
            };
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(Failure::failed(
        "the command did not finish in time; check it with `spot mgmt status`",
    ))
}

// ---- trust material -----------------------------------------------------------------------------

fn key_sign(args: &ArgMatches, ctx: &mut CommandCtx) -> Outcome<()> {
    let signer = load_signer(Path::new(arg(args, "key-file")))?;
    // Without the server's description the values are typed by their look: true/false, whole
    // numbers, otherwise text. Secrets still never come from --param.
    let mut map = serde_json::Map::new();
    for (name, source) in collect(args)? {
        let text = match source {
            Source::Literal(value) => value,
            Source::File(path) => std::fs::read_to_string(&path)
                .map(|t| t.trim_end_matches(['\n', '\r']).to_owned())
                .map_err(|_| {
                    Failure::failed(format!("parameter `{name}`: the file cannot be read"))
                })?,
            Source::Env(variable) => std::env::var(&variable).map_err(|_| {
                Failure::failed(format!(
                    "parameter `{name}`: the environment variable is not set"
                ))
            })?,
        };
        let value = match text.as_str() {
            "true" => json!(true),
            "false" => json!(false),
            other => other
                .parse::<i64>()
                .map_or_else(|_| json!(other), |n| json!(n)),
        };
        map.insert(name, value);
    }
    let now = ctx.clock().now_utc().unix_timestamp();
    let command = SignedCommand::new(
        arg(args, "op"),
        Value::Object(map),
        arg(args, "audience"),
        signer.key_id(),
        now - 5,
        now + COMMAND_TTL_SECS,
    );
    let signed =
        sign(&command, &signer).map_err(|_| Failure::failed("the command could not be signed"))?;
    let text = wire::encode(&signed, &[])
        .map_err(|_| Failure::failed("the command could not be encoded"))?;
    match args.get_one::<String>("out") {
        Some(path) => std::fs::write(path, text)
            .map_err(|_| Failure::failed("the envelope file cannot be written")),
        None => say(ctx, &text),
    }
}

fn ca_init(args: &ArgMatches, ctx: &mut CommandCtx) -> Outcome<()> {
    use rcgen::{
        BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    };
    let dir = PathBuf::from(arg(args, "dir"));
    let days: i64 = arg(args, "days")
        .parse()
        .ok()
        .filter(|d| (1..=3650).contains(d))
        .ok_or_else(|| Failure::usage("--days is 1 to 3650"))?;
    std::fs::create_dir_all(&dir)
        .map_err(|_| Failure::failed("the directory cannot be created"))?;
    let now = ctx.clock().now_utc().unix_timestamp();
    let (not_before, not_after) = (
        time::OffsetDateTime::from_unix_timestamp(now - 60)
            .map_err(|_| Failure::failed("the clock is out of range"))?,
        time::OffsetDateTime::from_unix_timestamp(now + days * 86_400)
            .map_err(|_| Failure::failed("the validity is out of range"))?,
    );
    let make = |e: rcgen::Error| Failure::failed(format!("certificate generation failed: {e}"));
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).map_err(make)?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Spot operator CA");
    ca_params.not_before = not_before;
    ca_params.not_after = not_after;
    let ca_key = KeyPair::generate().map_err(make)?;
    let ca_cert = ca_params.self_signed(&ca_key).map_err(make)?;
    let issuer = Issuer::from_params(&ca_params, &ca_key);
    let issue = |cn: &str,
                 sans: Vec<String>,
                 usage: ExtendedKeyUsagePurpose|
     -> Outcome<(String, String)> {
        let mut params = CertificateParams::new(sans).map_err(make)?;
        params.distinguished_name.push(DnType::CommonName, cn);
        params.extended_key_usages = vec![usage];
        params.not_before = not_before;
        params.not_after = not_after;
        let key = KeyPair::generate().map_err(make)?;
        let cert = params.signed_by(&key, &issuer).map_err(make)?;
        Ok((cert.pem(), key.serialize_pem()))
    };
    let cn = arg(args, "cn");
    let (client_pem, client_key) = issue(cn, Vec::new(), ExtendedKeyUsagePurpose::ClientAuth)?;
    write_private(&dir.join("ca.pem"), ca_cert.pem().as_bytes())?;
    write_private(&dir.join("ca.key"), ca_key.serialize_pem().as_bytes())?;
    write_private(&dir.join("client.pem"), client_pem.as_bytes())?;
    write_private(&dir.join("client.key"), client_key.as_bytes())?;
    if let Some(name) = args.get_one::<String>("server-name") {
        let (server_pem, server_key) = issue(
            name,
            vec![name.clone()],
            ExtendedKeyUsagePurpose::ServerAuth,
        )?;
        write_private(&dir.join("server.pem"), server_pem.as_bytes())?;
        write_private(&dir.join("server.key"), server_key.as_bytes())?;
    }
    say(
        ctx,
        &format!(
            "Created ca.pem, ca.key, client.pem and client.key in {}.",
            dir.display()
        ),
    )?;
    say(
        ctx,
        &format!(
            "Add the operator to the host's allow-list as subject \"CN={cn}\" and set operator_ca to ca.pem."
        ),
    )?;
    say(ctx, "Keep ca.key offline once the certificates are issued.")
}
