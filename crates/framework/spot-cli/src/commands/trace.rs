//! `spot trace [REQUEST_ID] [--site S] [--format text|json] [--address HOST:PORT] [--token-file F]`:
//! shows what a request did, from the trace viewer of a running development host
//! (WP-DX-03.T3, guide `docs/running-spot/devtools.md`).
//!
//! The development token comes from `SPOT_DEV_TOKEN` or `--token-file`, never from an argument.
//! The address defaults to the one the running host wrote to `sites/<site>/devtools.json`.
//!
//! Exit codes: 0 found, 1 usage, 3 not found or the host is not reachable.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use clap::{Arg, ArgMatches, Command};

use super::{CommandCtx, Exit};

pub const NAME: &str = "trace";

const TOKEN_ENV: &str = "SPOT_DEV_TOKEN";

pub fn command() -> Command {
    Command::new(NAME)
        .about("Show what a request did, from a running development host's trace viewer")
        .arg(Arg::new("request_id").value_name("REQUEST_ID"))
        .arg(Arg::new("site").long("site").value_name("SITE"))
        .arg(
            Arg::new("format")
                .long("format")
                .value_name("FORMAT")
                .value_parser(["text", "json"])
                .default_value("text"),
        )
        .arg(
            Arg::new("address")
                .long("address")
                .value_name("HOST:PORT")
                .help("The trace viewer address (default: from the running host's status file)"),
        )
        .arg(
            Arg::new("token_file")
                .long("token-file")
                .value_name("FILE")
                .help("Read the development token from this file instead of SPOT_DEV_TOKEN"),
        )
}

/// One GET against the viewer: the status and the body.
fn get(address: SocketAddr, target: &str, token: &str) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(3))
        .map_err(|e| format!("cannot reach the trace viewer on {address}: {e}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: {address}\r\nX-Dev-Token: {token}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).map_err(|e| e.to_string())?;
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .ok_or_else(|| "the trace viewer sent no response".to_owned())?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| "the trace viewer sent a bad status line".to_owned())?;
    Ok((status, body.to_owned()))
}

/// The viewer address: `--address`, or the status file of `site` under `root`.
fn address_of(
    matches: &ArgMatches,
    root: &std::path::Path,
    site: &str,
) -> Result<SocketAddr, String> {
    if let Some(text) = matches.get_one::<String>("address") {
        return text
            .parse()
            .map_err(|_| "--address is not a host:port address".to_owned());
    }
    let path = root.join("sites").join(site).join("devtools.json");
    let text = std::fs::read_to_string(&path).map_err(|_| {
        format!(
            "no running development host for site `{site}` (no {}); start it with services.devtools enabled",
            path.display()
        )
    })?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    json["trace_address"]
        .as_str()
        .and_then(|a| a.parse().ok())
        .ok_or_else(|| "the status file names no trace address".to_owned())
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let site = match matches.get_one::<String>("site") {
        Some(site) => site.clone(),
        None => {
            let only = {
                let sites = ctx.sites()?;
                (sites.len() == 1).then(|| sites[0].site.clone())
            };
            match only {
                Some(site) => site,
                None => {
                    writeln!(ctx.err(), "name the site with --site")?;
                    return Ok(Exit::Usage);
                }
            }
        }
    };
    let token = match matches.get_one::<String>("token_file") {
        Some(file) => std::fs::read_to_string(file)
            .map(|t| t.trim().to_owned())
            .map_err(|e| spot_cli::Error(format!("cannot read the token file: {e}")))?,
        None => match std::env::var(TOKEN_ENV) {
            Ok(token) => token,
            Err(_) => {
                writeln!(
                    ctx.err(),
                    "set {TOKEN_ENV} or pass --token-file (a token is never an argument)"
                )?;
                return Ok(Exit::Usage);
            }
        },
    };
    let address = match address_of(matches, ctx.root(), &site) {
        Ok(address) => address,
        Err(why) => {
            writeln!(ctx.err(), "{why}")?;
            return Ok(Exit::Failed);
        }
    };
    let format = matches
        .get_one::<String>("format")
        .map_or("text", String::as_str);
    let Some(request_id) = matches.get_one::<String>("request_id") else {
        // No request id: the list of traces.
        return match get(address, "/dev/traces", &token) {
            Ok((200, body)) => {
                writeln!(ctx.out(), "{body}")?;
                Ok(Exit::Ok)
            }
            Ok((status, _)) => {
                writeln!(ctx.err(), "the trace viewer answered {status}")?;
                Ok(Exit::Failed)
            }
            Err(why) => {
                writeln!(ctx.err(), "{why}")?;
                Ok(Exit::Failed)
            }
        };
    };
    let list = match get(
        address,
        &format!("/dev/traces?request_id={request_id}"),
        &token,
    ) {
        Ok((200, body)) => body,
        Ok((status, _)) => {
            writeln!(ctx.err(), "the trace viewer answered {status}")?;
            return Ok(Exit::Failed);
        }
        Err(why) => {
            writeln!(ctx.err(), "{why}")?;
            return Ok(Exit::Failed);
        }
    };
    let id = serde_json::from_str::<serde_json::Value>(&list)
        .ok()
        .and_then(|v| v["traces"][0]["traceId"].as_str().map(str::to_owned));
    let Some(id) = id else {
        writeln!(
            ctx.err(),
            "no trace for request {request_id} in site {site}"
        )?;
        return Ok(Exit::Failed);
    };
    match get(
        address,
        &format!("/dev/traces/{id}?format={format}"),
        &token,
    ) {
        Ok((200, body)) => {
            write!(ctx.out(), "{body}")?;
            if !body.ends_with('\n') {
                writeln!(ctx.out())?;
            }
            Ok(Exit::Ok)
        }
        Ok((status, _)) => {
            writeln!(ctx.err(), "the trace viewer answered {status}")?;
            Ok(Exit::Failed)
        }
        Err(why) => {
            writeln!(ctx.err(), "{why}")?;
            Ok(Exit::Failed)
        }
    }
}
