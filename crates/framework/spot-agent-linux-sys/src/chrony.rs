//! Time synchronisation through chrony: status, sources and tracking, setting the NTP
//! servers through a Spotledger-owned drop-in, and a forced step with a refuse-if-large-jump
//! guard.
//!
//! Offsets are integer nanoseconds: the decimal seconds `chronyc` prints are parsed with
//! integer arithmetic, never a float (ADR-12). The drop-in lives at
//! `/etc/chrony.d/90-spotledger-sources.conf`, so this module never rewrites the
//! distribution's own `/etc/chrony.conf`.

use std::net::IpAddr;
use std::path::PathBuf;

use crate::catalogue::op_id;
use crate::error::{SysError, SysResult, TIME_JUMP_TOO_LARGE};
use crate::preview::{Change, Check, Outcome, Preview};
use crate::sys::{CommandRunner, CommandSpec, FileSystem};
use spot_mgmt_ops::Risk;

/// The Spotledger chrony drop-in.
pub const CHRONY_DROP_IN: &str = "/etc/chrony.d/90-spotledger-sources.conf";

/// How chrony's own clock compares to its reference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tracking {
    /// The reference id (usually a hex address and a name).
    pub reference_id: String,
    /// The stratum.
    pub stratum: u32,
    /// How far the system clock is from the reference, in nanoseconds (positive is fast).
    pub system_time_nanos: i64,
    /// The last measured offset, in nanoseconds.
    pub last_offset_nanos: i64,
    /// The leap-second status word (`Normal`, `Insert second`, ...).
    pub leap_status: String,
}

/// One time source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The source mode character (`^`, `=`, `#`).
    pub mode: String,
    /// The state character (`*`, `+`, `-`, `?`, `x`, `~`).
    pub state: String,
    /// The source name or address.
    pub name: String,
    /// The stratum column.
    pub stratum: String,
    /// The poll interval column.
    pub poll: String,
    /// The reachability register.
    pub reach: String,
    /// The last sample column, verbatim.
    pub last_sample: String,
}

/// The overall time status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeStatus {
    /// The tracking information.
    pub tracking: Tracking,
    /// The configured sources.
    pub sources: Vec<Source>,
    /// Whether at least one source is selected (state `*`).
    pub synced: bool,
}

/// The time module.
pub struct TimeModule<'a> {
    runner: &'a dyn CommandRunner,
    fs: &'a dyn FileSystem,
}

impl<'a> TimeModule<'a> {
    /// Wraps a command runner and a filesystem.
    #[must_use]
    pub fn new(runner: &'a dyn CommandRunner, fs: &'a dyn FileSystem) -> Self {
        Self { runner, fs }
    }

    /// The tracking information from `chronyc tracking`.
    ///
    /// # Errors
    /// The command's failures and [`crate::error::PARSE`].
    pub fn tracking(&self) -> SysResult<Tracking> {
        let spec = CommandSpec::new("chronyc")?.arg("tracking")?;
        let output = self.runner.run(&spec)?;
        output.require_success("reading chrony tracking")?;
        parse_tracking(output.stdout())
    }

    /// The sources from `chronyc sources -v`.
    ///
    /// # Errors
    /// The command's failures and [`crate::error::PARSE`].
    pub fn sources(&self) -> SysResult<Vec<Source>> {
        let spec = CommandSpec::new("chronyc")?.args(["sources", "-v"])?;
        let output = self.runner.run(&spec)?;
        output.require_success("reading chrony sources")?;
        parse_sources(output.stdout())
    }

    /// The overall status: tracking, sources and whether a source is selected.
    ///
    /// # Errors
    /// The commands' failures and [`crate::error::PARSE`].
    pub fn status(&self) -> SysResult<TimeStatus> {
        let tracking = self.tracking()?;
        let sources = self.sources()?;
        let synced = sources.iter().any(|source| source.state == "*");
        Ok(TimeStatus {
            tracking,
            sources,
            synced,
        })
    }

    /// The configured NTP servers, read from the drop-in.
    ///
    /// # Errors
    /// [`crate::error::IO`] when the drop-in cannot be read; an absent drop-in is empty.
    pub fn configured_servers(&self) -> SysResult<Vec<String>> {
        let path = PathBuf::from(CHRONY_DROP_IN);
        if !self.fs.exists(&path)? {
            return Ok(Vec::new());
        }
        let text = self.fs.read_to_string(&path)?;
        Ok(parse_server_lines(&text))
    }

    /// Plans setting the NTP servers.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] for a bad server list.
    pub fn plan_servers(&self, servers: &[String]) -> SysResult<Preview> {
        validate_servers(servers)?;
        let current = self.configured_servers()?;
        Ok(
            Preview::new(op_id(&["os", "time", "servers"]), Risk::Medium)
                .change(Change::new(
                    "servers",
                    Some(current.join(",")),
                    Some(servers.join(",")),
                ))
                .note(format!("written to {CHRONY_DROP_IN}")),
        )
    }

    /// Writes the drop-in and reloads chrony's sources; a no-op when the servers match.
    ///
    /// # Errors
    /// [`crate::error::INVALID_ARGUMENT`] for a bad server list, and the write and reload failures.
    pub fn apply_servers(&self, servers: &[String]) -> SysResult<Outcome> {
        validate_servers(servers)?;
        let contents = render_server_lines(servers);
        let path = PathBuf::from(CHRONY_DROP_IN);
        if self.fs.exists(&path)? && self.fs.read_to_string(&path)? == contents {
            return Ok(Outcome::new(
                op_id(&["os", "time", "servers"]),
                false,
                "the NTP servers are already set",
            ));
        }
        self.fs.write_atomic(&path, &contents)?;
        let spec = CommandSpec::new("chronyc")?.args(["reload", "sources"])?;
        let output = self.runner.run(&spec)?;
        output.require_success("reloading chrony sources")?;
        Ok(Outcome::new(
            op_id(&["os", "time", "servers"]),
            true,
            format!("set {} NTP servers", servers.len()),
        ))
    }

    /// Verifies the configured servers match.
    ///
    /// # Errors
    /// [`crate::error::IO`] when the drop-in cannot be read.
    pub fn verify_servers(&self, servers: &[String]) -> SysResult<Check> {
        let current = self.configured_servers()?;
        if current == servers {
            Ok(Check::ok(
                op_id(&["os", "time", "servers"]),
                format!("{} servers are set", servers.len()),
            ))
        } else {
            Ok(Check::failed(
                op_id(&["os", "time", "servers"]),
                format!("the servers are {}", current.join(",")),
            ))
        }
    }

    /// Forces a step (`chronyc makestep`), refusing when the clock is off by more than
    /// `max_jump_nanos`. `now` is the instant the step is requested at, recorded in the
    /// outcome so the log can order it.
    ///
    /// # Errors
    /// [`TIME_JUMP_TOO_LARGE`] when the offset exceeds the guard, and the command's failures.
    pub fn force_step(&self, now: i64, max_jump_nanos: i64) -> SysResult<Outcome> {
        let tracking = self.tracking()?;
        let offset = tracking.system_time_nanos;
        if offset.abs() > max_jump_nanos {
            return Err(SysError::new(
                TIME_JUMP_TOO_LARGE,
                format!(
                    "the clock is off by {offset} ns, beyond the {} ns guard",
                    max_jump_nanos
                ),
            ));
        }
        let spec = CommandSpec::new("chronyc")?.args(["makestep", "1", "3"])?;
        let output = self.runner.run(&spec)?;
        output.require_success("forcing a chrony step")?;
        Ok(Outcome::new(
            op_id(&["os", "time", "step"]),
            true,
            format!("stepped the clock at {now} from an offset of {offset} ns"),
        ))
    }
}

/// Parses `chronyc tracking`.
///
/// # Errors
/// [`crate::error::PARSE`] when a value that must be present is malformed.
pub fn parse_tracking(text: &str) -> SysResult<Tracking> {
    let mut tracking = Tracking {
        reference_id: String::new(),
        stratum: 0,
        system_time_nanos: 0,
        last_offset_nanos: 0,
        leap_status: String::new(),
    };
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Reference ID" => tracking.reference_id = value.to_owned(),
            "Stratum" => {
                tracking.stratum = value
                    .parse()
                    .map_err(|_| SysError::parse("chrony's stratum is not a number"))?;
            }
            "System time" => {
                tracking.system_time_nanos = parse_offset_nanos(value).ok_or_else(|| {
                    SysError::parse("chrony's system time is not a decimal number")
                })?;
            }
            "Last offset" => {
                tracking.last_offset_nanos = parse_offset_nanos(value).ok_or_else(|| {
                    SysError::parse("chrony's last offset is not a decimal number")
                })?;
            }
            "Leap status" => tracking.leap_status = value.to_owned(),
            _ => {}
        }
    }
    Ok(tracking)
}

/// Parses an offset phrase such as `0.000012345 seconds fast of NTP time`, returning
/// nanoseconds (positive when fast).
#[must_use]
pub fn parse_offset_nanos(text: &str) -> Option<i64> {
    let mut tokens = text.split_whitespace();
    let number = tokens.next()?;
    let nanos = parse_decimal_nanos(number)?;
    let slow = text.contains("slow");
    Some(if slow { -nanos } else { nanos })
}

/// Parses a decimal number of seconds into whole nanoseconds with integer arithmetic.
#[must_use]
pub fn parse_decimal_nanos(text: &str) -> Option<i64> {
    let text = text.trim().trim_start_matches('+');
    let (sign, text) = match text.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, text),
    };
    let (integer, fraction) = match text.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (text, ""),
    };
    let integer: i64 = integer.parse().ok()?;
    let mut digits = String::with_capacity(9);
    for character in fraction.chars().take(9) {
        if !character.is_ascii_digit() {
            return None;
        }
        digits.push(character);
    }
    while digits.len() < 9 {
        digits.push('0');
    }
    let fraction: i64 = digits.parse().ok()?;
    let total = integer.checked_mul(1_000_000_000)?.checked_add(fraction)?;
    Some(sign * total)
}

/// Parses `chronyc sources -v`, skipping the header and the separator lines.
///
/// # Errors
/// [`crate::error::PARSE`] when a source line has too few columns.
pub fn parse_sources(text: &str) -> SysResult<Vec<Source>> {
    let mut sources = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.chars().all(|c| c == '=') {
            continue;
        }
        let first = trimmed.chars().next().unwrap_or(' ');
        if !matches!(first, '^' | '=' | '#') {
            continue;
        }
        let mode = trimmed.chars().next().unwrap_or('^').to_string();
        let rest = &trimmed[mode.len()..];
        let state = rest.chars().next().unwrap_or(' ').to_string();
        let rest = rest[state.len()..].trim_start();
        let mut columns = rest.split_whitespace();
        let name = columns
            .next()
            .ok_or_else(|| SysError::parse("a chrony source line has no name"))?
            .to_owned();
        let stratum = columns.next().unwrap_or("0").to_owned();
        let poll = columns.next().unwrap_or("0").to_owned();
        let reach = columns.next().unwrap_or("0").to_owned();
        let _last_rx = columns.next().unwrap_or("0");
        let last_sample = columns.collect::<Vec<_>>().join(" ");
        sources.push(Source {
            mode,
            state,
            name,
            stratum,
            poll,
            reach,
            last_sample,
        });
    }
    Ok(sources)
}

/// Parses the `server`/`pool` lines of a chrony configuration file.
#[must_use]
pub fn parse_server_lines(text: &str) -> Vec<String> {
    let mut servers = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let mut tokens = line.split_whitespace();
        match tokens.next() {
            Some("server") | Some("pool") => {
                if let Some(name) = tokens.next() {
                    servers.push(name.to_owned());
                }
            }
            _ => {}
        }
    }
    servers
}

/// Renders the chrony drop-in for a list of servers.
#[must_use]
pub fn render_server_lines(servers: &[String]) -> String {
    let mut text = String::from("# Managed by Spotledger (WP-ADM-04). Do not edit by hand.\n");
    for server in servers {
        text.push_str(&format!("server {server} iburst\n"));
    }
    text
}

/// Validates the NTP server list.
///
/// # Errors
/// [`crate::error::INVALID_ARGUMENT`] for an empty list or a bad server name.
pub fn validate_servers(servers: &[String]) -> SysResult<()> {
    if servers.is_empty() {
        return Err(SysError::invalid("at least one NTP server is required"));
    }
    for server in servers {
        if server.is_empty() || server.starts_with('-') || server.contains('\0') {
            return Err(SysError::invalid(format!(
                "{server:?} is not a valid NTP server"
            )));
        }
        let is_ip = server.parse::<IpAddr>().is_ok();
        let is_host = server
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
        if !is_ip && !is_host {
            return Err(SysError::invalid(format!(
                "{server:?} is not a valid NTP server name"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::testing::{FakeFs, FakeRunner, ok_output};

    const TRACKING: &str = "Reference ID    : C0A80101 (time.example)\nStratum         : 3\nRef time (UTC)  : Fri Oct 17 12:00:00 2026\nSystem time     : 0.000012345 seconds fast of NTP time\nLast offset     : +0.000000123 seconds\nRMS offset      : 0.000456789 seconds\nLeap status     : Normal\n";

    const SOURCES: &str = "210 Number of sources = 1\n  .-- Source mode  '^' = server\nMS Name/IP address         Stratum Poll Reach LastRx Last sample\n===============================================================================\n^* time.example                  2   6   377    34    -12us[  -8us] +/-   21ms\n";

    #[test]
    fn decimal_seconds_become_integer_nanos() {
        assert_eq!(parse_decimal_nanos("0.000012345"), Some(12_345));
        assert_eq!(parse_decimal_nanos("+0.000000123"), Some(123));
        assert_eq!(parse_decimal_nanos("-1.5"), Some(-1_500_000_000));
        assert_eq!(parse_decimal_nanos("2"), Some(2_000_000_000));
        assert_eq!(parse_decimal_nanos("x"), None);
        assert_eq!(
            parse_offset_nanos("0.000012345 seconds fast of NTP time"),
            Some(12_345)
        );
        assert_eq!(
            parse_offset_nanos("0.000012345 seconds slow of NTP time"),
            Some(-12_345)
        );
    }

    #[test]
    fn tracking_and_sources_parse() {
        let tracking = parse_tracking(TRACKING).unwrap();
        assert_eq!(tracking.stratum, 3);
        assert_eq!(tracking.system_time_nanos, 12_345);
        assert_eq!(tracking.leap_status, "Normal");
        let sources = parse_sources(SOURCES).unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].state, "*");
        assert_eq!(sources[0].name, "time.example");
        assert_eq!(sources[0].reach, "377");
    }

    #[test]
    fn setting_servers_writes_and_reloads_idempotently() {
        let runner = FakeRunner::new().with_response("chronyc reload sources", ok_output(""));
        let fs = FakeFs::new();
        let module = TimeModule::new(&runner, &fs);
        let servers = vec!["time.example".to_owned(), "10.0.0.2".to_owned()];
        assert!(module.apply_servers(&servers).unwrap().changed);
        assert_eq!(
            fs.file(CHRONY_DROP_IN).unwrap(),
            "# Managed by Spotledger (WP-ADM-04). Do not edit by hand.\nserver time.example iburst\nserver 10.0.0.2 iburst\n"
        );
        assert!(!module.apply_servers(&servers).unwrap().changed);
        assert_eq!(module.configured_servers().unwrap(), servers);
    }

    #[test]
    fn a_large_jump_is_refused() {
        let runner = FakeRunner::new().with_response("chronyc tracking", ok_output(TRACKING));
        let fs = FakeFs::new();
        let module = TimeModule::new(&runner, &fs);
        let error = module.force_step(1_000, 1_000).unwrap_err();
        assert_eq!(error.code, TIME_JUMP_TOO_LARGE);
    }

    #[test]
    fn a_small_jump_steps() {
        let runner = FakeRunner::new()
            .with_response("chronyc tracking", ok_output(TRACKING))
            .with_response("chronyc makestep 1 3", ok_output("200 OK\n"));
        let fs = FakeFs::new();
        let module = TimeModule::new(&runner, &fs);
        let outcome = module.force_step(1_000, 1_000_000_000).unwrap();
        assert!(outcome.changed);
        assert!(outcome.detail.contains("12345"));
    }

    #[test]
    fn bad_server_lists_are_refused() {
        assert!(validate_servers(&[]).is_err());
        assert!(validate_servers(&["-x".to_owned()]).is_err());
        assert!(validate_servers(&["a b".to_owned()]).is_err());
        assert!(validate_servers(&["time.example".to_owned()]).is_ok());
    }
}
