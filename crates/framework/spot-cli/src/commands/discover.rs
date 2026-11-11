//! `spot discover [--timeout 3s]` and `spot discover pin ID`: the client side of LAN discovery
//! (WP-PRD-06.T2), over `spot-discovery`'s browser and trust store. Guide:
//! `docs/running-spot/discovery.md`.
//!
//! `discover` scans for `_spotledger._tcp.local.` servers and lists each one's name, address,
//! TLS mode, advertised fingerprint (short form) and **trust state**:
//!
//! - `new`: nothing is pinned for this server yet;
//! - `pinned`: the stored fingerprint matches the one just advertised;
//! - `changed-warning`: a fingerprint *is* stored, and it does **not** match — a server
//!   presenting a different key than the one a user trusted before. This is shown, never
//!   auto-trusted, and never silently upgraded to `pinned`.
//!
//! `discover pin ID` performs the TOFU (trust-on-first-use) pairing. It shows the fingerprint the
//! server advertises (short form, to be compared out of band) and stores **nothing** until the
//! user confirms: an interactive `y` at the terminal, or an explicit `--yes`; without a terminal
//! and without `--yes` it refuses. It then re-resolves the server and stores the fingerprint
//! that was shown only if the server still advertises exactly that one. A server already pinned
//! to the same fingerprint is a no-op; one whose fingerprint **changed** is refused unless
//! `--force` is given (and the new fingerprint is then confirmed the same way).
//!
//! Exit codes follow the host-wide convention: 0 done, 1 usage, 2 refused (nothing changed), 3
//! failed (multicast unavailable, or the trust store could not be read or written).

use std::io::IsTerminal;
use std::net::IpAddr;
use std::time::Duration;

use clap::{Arg, ArgAction, ArgMatches, Command};
use spot_discovery::browser::{Browser, Found};
use spot_discovery::record::{Fingerprint, ServerId};
use spot_discovery::trust::{FileTrustStore, TrustDecision, TrustStore, TrustedServer, verify_paired};
use spot_discovery::DiscoveryError;

use super::{CommandCtx, Exit};

pub const NAME: &str = "discover";

/// The trust store file, relative to `--root`. One file for the whole host: discovery is not
/// scoped to a site.
const TRUST_FILE: &str = "discovery-trust.json";

fn timeout_arg() -> Arg {
    Arg::new("timeout")
        .long("timeout")
        .value_name("DURATION")
        .default_value("3s")
        .help("How long to wait for advertisements, e.g. 3s or 500ms")
}

pub fn command() -> Command {
    Command::new(NAME)
        .about("Find Spotledger servers on the LAN and manage pinned fingerprints")
        .arg(timeout_arg())
        .subcommand(
            Command::new("pin")
                .about("Pin (or deliberately re-pin) a discovered server's fingerprint")
                .arg(
                    Arg::new("id")
                        .value_name("ID")
                        .required(true)
                        .help("The server's instance id, as listed by `spot discover`"),
                )
                .arg(timeout_arg())
                .arg(
                    Arg::new("yes")
                        .long("yes")
                        .action(ArgAction::SetTrue)
                        .help("Confirm without asking; only after comparing the shown fingerprint out of band"),
                )
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("Re-pin even though the fingerprint changed since it was last pinned"),
                ),
        )
}

pub async fn run(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    match matches.subcommand() {
        None => list(matches, ctx).await,
        Some(("pin", sub)) => pin(sub, ctx).await,
        Some((other, _)) => {
            writeln!(ctx.err(), "unknown discover command `{other}`")?;
            Ok(Exit::Usage)
        }
    }
}

/// Parses a duration of the shape `3s`, `500ms` or a bare number of seconds, bounded to 1
/// millisecond .. 60 seconds (long enough to find a server, short enough that a script never
/// hangs on a dead network).
fn parse_timeout(text: &str) -> Result<Duration, String> {
    let trimmed = text.trim();
    let (number, millis) = if let Some(n) = trimmed.strip_suffix("ms") {
        (n, true)
    } else if let Some(n) = trimmed.strip_suffix('s') {
        (n, false)
    } else {
        (trimmed, false)
    };
    let value: u64 = number
        .parse()
        .map_err(|_| format!("`{text}` is not a duration like 3s or 500ms"))?;
    let duration = if millis {
        Duration::from_millis(value)
    } else {
        Duration::from_secs(value)
    };
    if duration.is_zero() || duration > Duration::from_secs(60) {
        return Err("--timeout must be between 1ms and 60s".to_owned());
    }
    Ok(duration)
}

fn read_timeout(ctx: &mut CommandCtx, matches: &ArgMatches) -> Option<Duration> {
    let text = matches
        .get_one::<String>("timeout")
        .map(String::as_str)
        .unwrap_or("3s");
    match parse_timeout(text) {
        Ok(d) => Some(d),
        Err(message) => {
            let _ = writeln!(ctx.err(), "{message}");
            None
        }
    }
}

fn trust_store(ctx: &CommandCtx) -> FileTrustStore {
    FileTrustStore::new(ctx.root().join(TRUST_FILE))
}

/// The three states the brief names, in the words it uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrustState {
    New,
    Pinned,
    ChangedWarning,
}

impl TrustState {
    fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Pinned => "pinned",
            Self::ChangedWarning => "changed-warning",
        }
    }
}

/// Classifies one found server against the store. A server with no fingerprint (TLS off) has
/// nothing to pin and is always `new`: the UI states that TLS is off through the record's own
/// `tls` column, never by inventing a trust decision for a key that does not exist.
fn classify(
    store: &dyn TrustStore,
    id: &ServerId,
    fingerprint: Option<&Fingerprint>,
) -> Result<TrustState, DiscoveryError> {
    let Some(fp) = fingerprint else {
        return Ok(TrustState::New);
    };
    match verify_paired(store, id, fp) {
        Ok(TrustDecision::PairingRequired) => Ok(TrustState::New),
        Ok(TrustDecision::Trusted(_)) => Ok(TrustState::Pinned),
        Err(DiscoveryError::FingerprintChanged) => Ok(TrustState::ChangedWarning),
        Err(e) => Err(e),
    }
}

async fn list(matches: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some(timeout) = read_timeout(ctx, matches) else {
        return Ok(Exit::Usage);
    };
    let store = trust_store(ctx);
    let found = match tokio::task::spawn_blocking(move || Browser::scan(timeout)).await {
        Ok(Ok(found)) => found,
        Ok(Err(e)) => {
            writeln!(ctx.err(), "{e}")?;
            return Ok(Exit::Failed);
        }
        Err(_) => {
            writeln!(ctx.err(), "the scan did not finish")?;
            return Ok(Exit::Failed);
        }
    };
    if found.is_empty() {
        writeln!(
            ctx.out(),
            "no servers found within {timeout:?}; if this network has no multicast, use the \
             manual address your administrator gave you"
        )?;
        return Ok(Exit::Ok);
    }
    writeln!(
        ctx.out(),
        "{:<32} {:<17} {:<6} {:<19} TRUST",
        "NAME", "ADDRESS", "TLS", "FINGERPRINT"
    )?;
    for server in &found {
        let state = classify(
            &store,
            &server.server_id,
            server.advertisement.server_key_fingerprint.as_ref(),
        )
        .map_err(|e| spot_cli::Error(e.to_string()))?;
        writeln!(
            ctx.out(),
            "{:<32} {:<17} {:<6} {:<19} {}",
            server.server_id.as_str(),
            address_of(server),
            server.advertisement.tls.as_txt(),
            server
                .advertisement
                .server_key_fingerprint
                .as_ref()
                .map_or_else(|| "-".to_owned(), short_fingerprint),
            state.as_str()
        )?;
    }
    writeln!(
        ctx.out(),
        "Compare a fingerprint out of band (aloud, or on the server) before running \
         `spot discover pin ID`; it is not proof of identity until you do."
    )?;
    Ok(Exit::Ok)
}

fn address_of(found: &Found) -> String {
    found
        .addresses
        .first()
        .map(IpAddr::to_string)
        .unwrap_or_default()
}

/// The advertised fingerprint in the short form a person can read out and compare: the first 16
/// hex characters in groups of four (`ab12-cd34-ef56-7890`). It is a display of the prefix only;
/// what is stored is always the full fingerprint that was shown.
fn short_fingerprint(fp: &Fingerprint) -> String {
    let prefix: Vec<char> = fp.0.chars().take(16).collect();
    prefix
        .chunks(4)
        .map(|c| c.iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("-")
}

/// What the user decided at the confirmation step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Confirm {
    Yes,
    No,
    /// Not a terminal and no `--yes`: there is nobody to ask.
    NeedTerminal,
}

/// Pure decision: `--yes` confirms; otherwise a terminal is required and only `y` or `yes`
/// (any case) confirms. `answer` is read lazily, so nothing is read when `--yes` is given or
/// when there is no terminal.
fn confirm_decision(yes: bool, is_tty: bool, answer: impl FnOnce() -> Option<String>) -> Confirm {
    if yes {
        return Confirm::Yes;
    }
    if !is_tty {
        return Confirm::NeedTerminal;
    }
    match answer() {
        Some(a) if matches!(a.trim().to_ascii_lowercase().as_str(), "y" | "yes") => Confirm::Yes,
        _ => Confirm::No,
    }
}

/// How one `pin` invocation ended.
#[derive(Debug, PartialEq, Eq)]
enum PinOutcome {
    AlreadyPinned,
    Pinned,
    Repinned,
    NeedsTerminal,
    Declined,
    ChangedNeedsForce,
    /// The server advertised a different fingerprint when re-resolved than the one shown.
    ChangedSinceShown,
    /// The server was not found when re-resolved.
    Gone,
}

/// The whole pin decision, after the first scan found `shown`. Prints the fingerprint it is
/// about to pin, asks (or reads `--yes`), re-resolves, and stores **the fingerprint shown**
/// only when the re-resolved one is identical. Nothing is written before every check passes.
#[allow(clippy::too_many_arguments)]
fn pin_core(
    out: &mut dyn std::io::Write,
    store: &dyn TrustStore,
    id: &ServerId,
    shown: &Fingerprint,
    force: bool,
    yes: bool,
    is_tty: bool,
    read_line: impl FnOnce() -> Option<String>,
    rescan: impl FnOnce() -> Option<Fingerprint>,
    now: u64,
) -> Result<PinOutcome, DiscoveryError> {
    let existing = match verify_paired(store, id, shown) {
        Ok(TrustDecision::Trusted(_)) => return Ok(PinOutcome::AlreadyPinned),
        Ok(TrustDecision::PairingRequired) => false,
        Err(DiscoveryError::FingerprintChanged) => {
            if !force {
                return Ok(PinOutcome::ChangedNeedsForce);
            }
            true
        }
        Err(e) => return Err(e),
    };
    let _ = writeln!(
        out,
        "`{id}` advertises fingerprint {} (first 16 hex characters).\n\
         Compare it out of band (read it aloud or check it on the server) before trusting it.",
        short_fingerprint(shown)
    );
    if !yes && is_tty {
        let _ = write!(out, "Pin this fingerprint for `{id}`? [y/N] ");
        let _ = out.flush();
    }
    match confirm_decision(yes, is_tty, read_line) {
        Confirm::Yes => {}
        Confirm::No => return Ok(PinOutcome::Declined),
        Confirm::NeedTerminal => return Ok(PinOutcome::NeedsTerminal),
    }
    match rescan() {
        None => return Ok(PinOutcome::Gone),
        Some(current) if current != *shown => return Ok(PinOutcome::ChangedSinceShown),
        Some(_) => {}
    }
    store.store(TrustedServer {
        server_id: id.clone(),
        fingerprint: shown.clone(),
        first_seen: now,
    })?;
    Ok(if existing {
        PinOutcome::Repinned
    } else {
        PinOutcome::Pinned
    })
}

fn scan_fingerprint(timeout: Duration, id: &ServerId) -> Option<Fingerprint> {
    Browser::scan(timeout)
        .ok()?
        .into_iter()
        .find(|f| &f.server_id == id)
        .and_then(|f| f.advertisement.server_key_fingerprint)
}

async fn pin(sub: &ArgMatches, ctx: &mut CommandCtx) -> spot_cli::Result<Exit> {
    let Some(timeout) = read_timeout(ctx, sub) else {
        return Ok(Exit::Usage);
    };
    let Some(id_text) = sub.get_one::<String>("id") else {
        writeln!(
            ctx.err(),
            "usage: spot discover pin <ID> [--yes] [--force] [--timeout DURATION]"
        )?;
        return Ok(Exit::Usage);
    };
    let id = match ServerId::new(id_text) {
        Ok(id) => id,
        Err(e) => {
            writeln!(ctx.err(), "{e}")?;
            return Ok(Exit::Usage);
        }
    };
    let force = sub.get_flag("force");
    let yes = sub.get_flag("yes");
    let found = match tokio::task::spawn_blocking(move || Browser::scan(timeout)).await {
        Ok(Ok(found)) => found,
        Ok(Err(e)) => {
            writeln!(ctx.err(), "{e}")?;
            return Ok(Exit::Failed);
        }
        Err(_) => {
            writeln!(ctx.err(), "the scan did not finish")?;
            return Ok(Exit::Failed);
        }
    };
    let Some(server) = found.into_iter().find(|f| f.server_id == id) else {
        writeln!(
            ctx.err(),
            "no server named `{id}` was found within {timeout:?}"
        )?;
        return Ok(Exit::Refused);
    };
    let Some(shown) = server.advertisement.server_key_fingerprint.clone() else {
        writeln!(
            ctx.err(),
            "`{id}` advertises TLS off, so it has no fingerprint to pin"
        )?;
        return Ok(Exit::Refused);
    };
    let store = trust_store(ctx);
    let now = now_unix(ctx);
    let is_tty = std::io::stdin().is_terminal();
    let outcome = pin_core(
        ctx.out(),
        &store,
        &id,
        &shown,
        force,
        yes,
        is_tty,
        || {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).ok()?;
            Some(line)
        },
        || scan_fingerprint(timeout, &id),
        now,
    )
    .map_err(|e| spot_cli::Error(e.to_string()))?;
    match outcome {
        PinOutcome::AlreadyPinned => {
            writeln!(ctx.out(), "`{id}` is already pinned to this fingerprint")?;
            Ok(Exit::Ok)
        }
        PinOutcome::Pinned => {
            writeln!(ctx.out(), "pinned `{id}`")?;
            Ok(Exit::Ok)
        }
        PinOutcome::Repinned => {
            writeln!(ctx.out(), "re-pinned `{id}` (the fingerprint had changed)")?;
            Ok(Exit::Ok)
        }
        PinOutcome::NeedsTerminal => {
            writeln!(
                ctx.err(),
                "nothing pinned: confirming a fingerprint needs a terminal; re-run with --yes \
                 only after comparing the fingerprint above out of band"
            )?;
            Ok(Exit::Refused)
        }
        PinOutcome::Declined => {
            writeln!(ctx.err(), "nothing pinned: not confirmed")?;
            Ok(Exit::Refused)
        }
        PinOutcome::ChangedNeedsForce => {
            writeln!(
                ctx.err(),
                "{}: `{id}`'s fingerprint changed since it was pinned; verify the new \
                 fingerprint out of band, then re-run with --force to re-pin deliberately",
                DiscoveryError::FingerprintChanged.code()
            )?;
            Ok(Exit::Refused)
        }
        PinOutcome::ChangedSinceShown => {
            writeln!(
                ctx.err(),
                "nothing pinned: `{id}` advertised a different fingerprint than the one shown; \
                 run `spot discover` again and compare the new fingerprint"
            )?;
            Ok(Exit::Refused)
        }
        PinOutcome::Gone => {
            writeln!(
                ctx.err(),
                "nothing pinned: `{id}` could not be found again to confirm its fingerprint"
            )?;
            Ok(Exit::Refused)
        }
    }
}

fn now_unix(ctx: &CommandCtx) -> u64 {
    u64::try_from(ctx.clock().now_utc().unix_timestamp()).unwrap_or(0)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use spot_discovery::trust::InMemoryTrustStore;
    use spot_discovery::trust::FileTrustStore;

    #[test]
    fn durations_parse_seconds_milliseconds_and_bare_numbers() {
        assert_eq!(parse_timeout("3s").unwrap(), Duration::from_secs(3));
        assert_eq!(parse_timeout("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_timeout("7").unwrap(), Duration::from_secs(7));
    }

    #[test]
    fn durations_out_of_bounds_or_malformed_are_refused() {
        assert!(parse_timeout("0s").is_err());
        assert!(parse_timeout("61s").is_err());
        assert!(parse_timeout("abc").is_err());
        assert!(parse_timeout("").is_err());
    }

    fn fp(byte: u8) -> Fingerprint {
        Fingerprint(format!("{byte:02x}").repeat(16))
    }

    #[test]
    fn classify_reports_new_for_an_unseen_server() {
        let store = InMemoryTrustStore::new();
        let id = ServerId::new("srv-1").unwrap();
        let state = classify(&store, &id, Some(&fp(0xaa))).unwrap();
        assert_eq!(state, TrustState::New);
    }

    #[test]
    fn classify_reports_pinned_for_a_matching_fingerprint() {
        let store = InMemoryTrustStore::new();
        let id = ServerId::new("srv-1").unwrap();
        store
            .store(TrustedServer {
                server_id: id.clone(),
                fingerprint: fp(0xaa),
                first_seen: 1,
            })
            .unwrap();
        let state = classify(&store, &id, Some(&fp(0xaa))).unwrap();
        assert_eq!(state, TrustState::Pinned);
    }

    /// The fingerprint-change-detected test the brief asks for: a server that presents a
    /// different key than the one pinned is `ChangedWarning`, never silently `Pinned`.
    #[test]
    fn classify_detects_a_changed_fingerprint() {
        let store = InMemoryTrustStore::new();
        let id = ServerId::new("srv-1").unwrap();
        store
            .store(TrustedServer {
                server_id: id.clone(),
                fingerprint: fp(0xaa),
                first_seen: 1,
            })
            .unwrap();
        let state = classify(&store, &id, Some(&fp(0xbb))).unwrap();
        assert_eq!(state, TrustState::ChangedWarning);
    }

    #[test]
    fn classify_is_new_when_there_is_no_fingerprint_to_pin() {
        let store = InMemoryTrustStore::new();
        let id = ServerId::new("srv-1").unwrap();
        assert_eq!(classify(&store, &id, None).unwrap(), TrustState::New);
    }

    #[test]
    fn trust_state_words_match_the_brief_exactly() {
        assert_eq!(TrustState::New.as_str(), "new");
        assert_eq!(TrustState::Pinned.as_str(), "pinned");
        assert_eq!(TrustState::ChangedWarning.as_str(), "changed-warning");
    }

    fn temp_store() -> (FileTrustStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "spot-cli-pin-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t").replace("::", "-")
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (FileTrustStore::new(dir.join("trust.json")), dir)
    }

    fn pinned(store: &dyn TrustStore, id: &ServerId) -> Option<Fingerprint> {
        store.lookup(id).unwrap().map(|t| t.fingerprint)
    }

    #[test]
    fn the_short_fingerprint_is_sixteen_hex_characters_in_groups_of_four() {
        assert_eq!(short_fingerprint(&fp(0xab)), "abab-abab-abab-abab");
    }

    #[test]
    fn no_terminal_and_no_yes_refuses_and_writes_nothing() {
        let (store, dir) = temp_store();
        let id = ServerId::new("srv-1").unwrap();
        let mut out = Vec::new();
        let outcome = pin_core(
            &mut out, &store, &id, &fp(0xaa), false, false, false,
            || panic!("must not read an answer without a terminal"),
            || panic!("must not re-resolve before confirmation"),
            5,
        )
        .unwrap();
        assert_eq!(outcome, PinOutcome::NeedsTerminal);
        assert!(pinned(&store, &id).is_none());
        assert!(!dir.join("trust.json").exists(), "nothing was written");
        assert!(String::from_utf8(out).unwrap().contains("aaaa-aaaa-aaaa-aaaa"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_terminal_answer_other_than_yes_writes_nothing() {
        let (store, dir) = temp_store();
        let id = ServerId::new("srv-2").unwrap();
        let outcome = pin_core(
            &mut Vec::new(), &store, &id, &fp(0xaa), false, false, true,
            || Some("n\n".to_owned()),
            || panic!("must not re-resolve"),
            5,
        )
        .unwrap();
        assert_eq!(outcome, PinOutcome::Declined);
        assert!(!dir.join("trust.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn yes_pins_the_shown_fingerprint() {
        let (store, dir) = temp_store();
        let id = ServerId::new("srv-3").unwrap();
        let outcome = pin_core(
            &mut Vec::new(), &store, &id, &fp(0xaa), false, true, false,
            || panic!("--yes never reads"),
            || Some(fp(0xaa)),
            5,
        )
        .unwrap();
        assert_eq!(outcome, PinOutcome::Pinned);
        assert_eq!(pinned(&store, &id), Some(fp(0xaa)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_interactive_yes_pins_the_shown_fingerprint() {
        let (store, dir) = temp_store();
        let id = ServerId::new("srv-4").unwrap();
        let outcome = pin_core(
            &mut Vec::new(), &store, &id, &fp(0xbb), false, false, true,
            || Some("Yes\n".to_owned()),
            || Some(fp(0xbb)),
            5,
        )
        .unwrap();
        assert_eq!(outcome, PinOutcome::Pinned);
        assert_eq!(pinned(&store, &id), Some(fp(0xbb)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_fingerprint_that_changed_between_show_and_store_is_refused() {
        let (store, dir) = temp_store();
        let id = ServerId::new("srv-5").unwrap();
        let outcome = pin_core(
            &mut Vec::new(), &store, &id, &fp(0xaa), false, true, false,
            || None,
            || Some(fp(0xbb)),
            5,
        )
        .unwrap();
        assert_eq!(outcome, PinOutcome::ChangedSinceShown);
        assert!(!dir.join("trust.json").exists(), "nothing was written");
        let gone = pin_core(
            &mut Vec::new(), &store, &id, &fp(0xaa), false, true, false,
            || None,
            || None,
            5,
        )
        .unwrap();
        assert_eq!(gone, PinOutcome::Gone);
        assert!(!dir.join("trust.json").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_changed_pin_needs_force_even_with_yes() {
        let (store, dir) = temp_store();
        let id = ServerId::new("srv-6").unwrap();
        store
            .store(TrustedServer { server_id: id.clone(), fingerprint: fp(0xaa), first_seen: 1 })
            .unwrap();
        let outcome = pin_core(
            &mut Vec::new(), &store, &id, &fp(0xbb), false, true, false,
            || None,
            || panic!("refused before re-resolving"),
            5,
        )
        .unwrap();
        assert_eq!(outcome, PinOutcome::ChangedNeedsForce);
        assert_eq!(pinned(&store, &id), Some(fp(0xaa)));
        let _ = std::fs::remove_dir_all(dir);
    }
}
