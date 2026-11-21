//! The service side: registering the advertisement, re-announcing on address changes,
//! answering queries, and withdrawing with a goodbye on stop or drop.
//!
//! Failure behaviour (ADR-068 A1, the brief's rule): a failure to bind multicast —
//! daemon start, registration, or a daemon-level bind error — never fails the
//! service. [`Advertiser::start`] returns a handle that reports
//! [`AdvertiserState::Degraded`] together with a
//! [`DiscoveryError::MulticastUnavailable`] (whose catalogue remedy names the likely
//! causes: firewall, VPN adapter, no network). Only a record that cannot be encoded
//! or an empty permitted interface set returns `Err`.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mdns_sd::{DaemonEvent, IfKind, IfPredicate, ServiceDaemon, ServiceInfo};

use crate::SERVICE_TYPE;
use crate::error::DiscoveryError;
use crate::record::{AdvertiserConfig, is_advertisable_address};

/// Everything the service knows about its running advertisement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvertiserState {
    /// Registered and announcing on the allowed interfaces.
    Announced,
    /// Multicast unavailable or unconfirmed: the service keeps running degraded —
    /// discovery is off, everything else unaffected. A manual address is the
    /// documented fallback.
    Degraded,
    /// Withdrawn by `stop()` or dropped.
    Withdrawn,
}

/// What a caller can poll for diagnostics and health pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvertiserStatus {
    /// The live state.
    pub state: AdvertiserState,
    /// The typed environment problem behind a `Degraded` state, if any.
    pub last_error: Option<DiscoveryError>,
}

#[derive(Debug, Default)]
struct SharedStatus {
    registered: bool,
    error: Option<DiscoveryError>,
}

enum AdvertiserCommand {
    Stop,
}

/// A live advertisement. Dropping it withdraws the record (goodbye) and shuts the
/// daemon down.
#[derive(Debug)]
pub struct AdvertiserHandle {
    sender: Option<std::sync::mpsc::Sender<AdvertiserCommand>>,
    shared: Arc<Mutex<SharedStatus>>,
}

impl AdvertiserHandle {
    /// Polls the current state.
    #[must_use]
    pub fn status(&self) -> AdvertiserStatus {
        let guard = self
            .shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = if guard.error.is_some() {
            AdvertiserState::Degraded
        } else if guard.registered {
            AdvertiserState::Announced
        } else {
            AdvertiserState::Degraded
        };
        AdvertiserStatus {
            state,
            last_error: guard.error.clone(),
        }
    }

    /// Withdraws the record (mDNS goodbye) and marks the handle withdrawn.
    pub fn stop(&self) {
        if let Some(sender) = self.sender.as_ref() {
            let _ = sender.send(AdvertiserCommand::Stop);
        }
        let mut guard = self
            .shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.registered = false;
    }
}

impl Drop for AdvertiserHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A degenerate handle for a before-the-daemon degradation (empty permitted
/// interface set): the state is final and no thread exists.
fn degraded_handle(error: DiscoveryError) -> AdvertiserHandle {
    AdvertiserHandle {
        sender: None,
        shared: Arc::new(Mutex::new(SharedStatus {
            registered: false,
            error: Some(error),
        })),
    }
}

/// The advertiser: one static entry point per the brief.
pub struct Advertiser;

impl Advertiser {
    /// Starts advertising `config.advertisement` under `_spotledger._tcp.local.`
    ///
    /// Interfaces: when `config.interfaces` is non-empty, exactly those addresses
    /// are published — still filtered through [`is_advertisable_address`], so a
    /// mistakenly listed public address is refused, not advertised. When empty, the
    /// daemon auto-publishes on the host's interfaces under a private-range
    /// predicate (plus loopback, which mDNS needs for same-host discovery and which
    /// never leaves the machine; public addresses stay refused in both modes).
    ///
    /// Re-announce on address changes is the daemon's `addr_auto` behaviour; the
    /// monitor records it. Withdrawal sends a goodbye on `stop()` and on drop.
    pub fn start(config: AdvertiserConfig) -> Result<AdvertiserHandle, DiscoveryError> {
        let advertisement = config.effective_advertisement();
        let txt = advertisement.to_txt()?;
        let instance = advertisement.service_instance.0.clone();
        let fullname = format!("{instance}.{SERVICE_TYPE}");
        let host = format!("{instance}.local.");

        let mut intfs: Vec<IfKind> = Vec::new();
        let mut explicit_addrs: Vec<IpAddr> = Vec::new();
        for a in &config.interfaces {
            if is_advertisable_address(*a) {
                explicit_addrs.push(*a);
            }
        }
        if !config.interfaces.is_empty() && explicit_addrs.is_empty() {
            // Every address the administrator listed was refused by the filter: do
            // not secretly fall back to other interfaces; degrade and say why.
            return Ok(degraded_handle(DiscoveryError::RecordInvalid {
                reason: "no advertisement interface passed the private-range filter; \
                         refusing to advertise elsewhere"
                    .to_string(),
            }));
        }
        if explicit_addrs.is_empty() {
            // Default mode: the daemon resolves the host's own interfaces; the
            // predicate keeps any public address out.
            intfs.push(IfKind::Predicate(IfPredicate::new(|intf| {
                is_advertisable_address(intf.ip())
            })));
            intfs.push(IfKind::LoopbackV4);
            intfs.push(IfKind::LoopbackV6);
        } else {
            for a in &explicit_addrs {
                intfs.push(IfKind::Addr(*a));
            }
        }

        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                return Ok(degraded_handle(DiscoveryError::MulticastUnavailable {
                    detail: e.to_string(),
                }));
            }
        };

        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &instance,
            &host,
            explicit_addrs.as_slice(),
            advertisement.port,
            txt.as_slice(),
        )
        .map_err(|e| DiscoveryError::RecordInvalid {
            reason: format!("the advertisement could not be registered: {e}"),
        })?;
        // `set_interfaces` carries the interface policy for both modes (with no
        // explicit addresses, `addr_auto` publishes only on predicate-matching
        // interfaces).
        let mut service = service;
        service.set_interfaces(intfs);
        if explicit_addrs.is_empty() {
            service = service.enable_addr_auto();
        }

        let shared = Arc::new(Mutex::new(SharedStatus::default()));
        let monitor_receiver =
            daemon
                .monitor()
                .map_err(|e| DiscoveryError::MulticastUnavailable {
                    detail: format!("the daemon monitor failed: {e}"),
                })?;

        daemon
            .register(service)
            .map_err(|e| DiscoveryError::MulticastUnavailable {
                detail: format!("the advertisement could not be registered: {e}"),
            })?;

        // Status reader: waits for registration or an error, within the bound.
        let deadline = Instant::now() + REGISTER_CONFIRM_TIMEOUT;
        loop {
            match monitor_receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(DaemonEvent::Announce(full, _)) if full == fullname => {
                    let mut g = shared
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    g.registered = true;
                    break;
                }
                Ok(DaemonEvent::Error(e)) => {
                    let mut g = shared
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    g.error = Some(DiscoveryError::MulticastUnavailable {
                        detail: e.to_string(),
                    });
                    break;
                }
                Ok(_) => {}
                Err(mdns_sd::RecvTimeoutError::Timeout) => {
                    if Instant::now() >= deadline {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        let confirmed = {
            let g = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            g.registered && g.error.is_none()
        };
        if !confirmed {
            let mut g = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if g.error.is_none() {
                g.error = Some(DiscoveryError::MulticastUnavailable {
                    detail: "the registration was not confirmed in time; multicast may be blocked"
                        .to_string(),
                });
            }
        }

        // The keeper thread: on stop (or channel drop) withdraws with a goodbye,
        // waits briefly so the goodbye leaves, then shuts the daemon down.
        let sender = {
            let (tx, rx) = std::sync::mpsc::channel::<AdvertiserCommand>();
            let daemon_for_thread = daemon;
            let fullname_for_thread = fullname;
            std::thread::Builder::new()
                .name("spot-discovery-advertiser".to_string())
                .spawn(move || {
                    let mut withdrew = false;
                    if let Ok(cmd) = rx.recv() {
                        match cmd {
                            AdvertiserCommand::Stop => {
                                let _ = daemon_for_thread.unregister(&fullname_for_thread);
                                withdrew = true;
                            }
                        }
                    }
                    if !withdrew {
                        let _ = daemon_for_thread.unregister(&fullname_for_thread);
                    }
                    std::thread::sleep(GOODBYE_GRACE);
                    let _ = daemon_for_thread.shutdown();
                })
                .map_err(|e| DiscoveryError::MulticastUnavailable {
                    detail: format!("the advertiser thread failed to start: {e}"),
                })?;
            tx
        };

        Ok(AdvertiserHandle {
            sender: Some(sender),
            shared,
        })
    }
}

/// How long `start` waits for the daemon to confirm the registration before
/// reporting degradation.
pub const REGISTER_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);
/// How long the goodbye gets to leave before the daemon shuts down.
pub const GOODBYE_GRACE: Duration = Duration::from_millis(200);
