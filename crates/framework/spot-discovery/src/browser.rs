//! The client side: scanning for Spotlight servers and watching them appear, update
//! and disappear. Every `Found` carries the stable `ServerId`, all reachable
//! addresses (IPv4 first) and the decoded record; `last_seen` rides on watch events.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use mdns_sd::{Receiver, ServiceDaemon, ServiceEvent};

use crate::SERVICE_TYPE;
use crate::error::DiscoveryError;
use crate::record::Advertisement;
use crate::record::ServerId;

/// A discovered server as seen by a browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The stable identity: the service instance id, **not** the IP.
    pub server_id: ServerId,
    /// All reachable addresses, IPv4 first. The instance name is not mangled by
    /// DHCP; addresses are.
    pub addresses: Vec<IpAddr>,
    /// The application port.
    pub port: u16,
    /// The decoded record.
    pub advertisement: Advertisement,
    /// Monotonic milliseconds since the browser's clock origin when this server was
    /// last seen. The caller's `spot_core::Clock::monotonic()` value at event time.
    pub last_seen: u64,
    /// Where this entry came from: a scan, or a manual address entry.
    pub source: FoundSource,
}

/// Where a `Found` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoundSource {
    /// Received over mDNS.
    Mdns,
    /// Typed by the user (the manual fallback) — `Found`-shaped so selection and
    /// trust treat both the same.
    Manual,
}

/// A watch-stream event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoundEvent {
    /// A server appeared (first resolution of this instance id in this watch).
    Appeared(Box<Found>),
    /// A known server changed (new addresses, port, TXT values, or TLS state).
    Updated {
        /// The stable id of the updated server.
        server_id: ServerId,
        /// The fresh view.
        found: Box<Found>,
    },
    /// The server disappeared (goodbye received, or TTL expiry detected by the
    /// daemon).
    Disappeared {
        /// The stable id of the server that went away.
        server_id: ServerId,
    },
}

/// A browser over the LAN.
pub struct Browser;

impl Browser {
    /// One-shot scan: browses for `timeout` and returns the servers found, sorted by
    /// server id, IPv4 addresses first. Never sleeps in tests: the unit tests drive
    /// decoding directly; the loopback integration test (ignored in CI without
    /// multicast) exercises this end to end.
    ///
    /// # Errors
    /// [`DiscoveryError::MulticastUnavailable`] when the daemon cannot start or the
    /// browse cannot be issued (firewall, VPN, no network) — the caller offers the
    /// manual-address route on this error.
    pub fn scan(timeout: Duration) -> Result<Vec<Found>, DiscoveryError> {
        let daemon = ServiceDaemon::new().map_err(|e| DiscoveryError::MulticastUnavailable {
            detail: e.to_string(),
        })?;
        let receiver =
            daemon
                .browse(SERVICE_TYPE)
                .map_err(|e| DiscoveryError::MulticastUnavailable {
                    detail: e.to_string(),
                })?;
        let deadline = std::time::Instant::now() + timeout;
        let mut found: HashMap<ServerId, Found> = HashMap::new();
        while std::time::Instant::now() < deadline {
            match receiver
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            {
                Ok(event) => {
                    if let Some((id, f)) = found_from_event(&event, now_ms()) {
                        found.insert(id, f);
                    }
                }
                Err(mdns_sd::RecvTimeoutError::Timeout) => break,
                Err(mdns_sd::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = daemon.stop_browse(SERVICE_TYPE);
        let _ = daemon.shutdown();
        let mut list: Vec<Found> = found.into_values().collect();
        list.sort_by(|a, b| a.server_id.cmp(&b.server_id));
        Ok(list)
    }

    /// Streaming watch: returns a live receiver of `FoundEvent`s plus a guard that
    /// stops the browse when dropped.
    ///
    /// # Errors
    /// [`DiscoveryError::MulticastUnavailable`] — same causes as `scan`.
    pub fn watch() -> Result<(WatchHandle, std::sync::mpsc::Receiver<FoundEvent>), DiscoveryError> {
        let daemon = ServiceDaemon::new().map_err(|e| DiscoveryError::MulticastUnavailable {
            detail: e.to_string(),
        })?;
        let receiver =
            daemon
                .browse(SERVICE_TYPE)
                .map_err(|e| DiscoveryError::MulticastUnavailable {
                    detail: e.to_string(),
                })?;
        let (tx, rx) = std::sync::mpsc::channel::<FoundEvent>();
        let seen: HashMap<ServerId, Found> = HashMap::new();
        let thread = std::thread::Builder::new()
            .name("spot-discovery-browser".to_string())
            .spawn(move || {
                translate_stream(receiver, tx, seen);
            })
            .map_err(|e| DiscoveryError::MulticastUnavailable {
                detail: format!("the browser thread failed to start: {e}"),
            })?;
        Ok((
            WatchHandle {
                thread: Some(thread),
                daemon: Some(daemon),
            },
            rx,
        ))
    }
}

/// Owns the browse until dropped: dropping stops the browse (and therefore the event
/// stream) and shuts the daemon down.
pub struct WatchHandle {
    thread: Option<std::thread::JoinHandle<()>>,
    daemon: Option<ServiceDaemon>,
}

impl WatchHandle {
    /// Explicitly stops the stream (same as drop, callable for clarity).
    pub fn stop(&mut self) {
        if let Some(d) = self.daemon.take() {
            let _ = d.stop_browse(SERVICE_TYPE);
            let _ = d.shutdown();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The translation from mDNS events to `FoundEvent`s, shared by `scan` and `watch`.
#[must_use]
pub fn found_from_event(event: &ServiceEvent, seen_at_ms: u64) -> Option<(ServerId, Found)> {
    match event {
        ServiceEvent::ServiceResolved(resolved) => {
            if resolved.ty_domain != SERVICE_TYPE {
                return None;
            }
            let txt_pairs: Vec<(String, String)> = resolved
                .txt_properties
                .iter()
                .map(|p| (p.key().to_string(), p.val_str().to_string()))
                .collect();
            let advertisement = Advertisement::from_txt(&txt_pairs).ok()?;
            let instance = advertisement.service_instance.clone();
            // Addresses: resolved set, IPv4 first, scoped variants flattened.
            let mut v4: Vec<IpAddr> = Vec::new();
            let mut v6: Vec<IpAddr> = Vec::new();
            for a in &resolved.addresses {
                let ip = a.to_ip_addr();
                if ip.is_ipv4() {
                    v4.push(ip);
                } else {
                    v6.push(ip);
                }
            }
            v4.sort();
            v6.sort();
            v4.extend(v6);
            if v4.is_empty() {
                return None;
            }
            Some((
                instance.clone(),
                Found {
                    server_id: instance,
                    addresses: v4,
                    port: resolved.port,
                    advertisement,
                    last_seen: seen_at_ms,
                    source: FoundSource::Mdns,
                },
            ))
        }
        // `ServiceFound` precedes resolution; `ServiceRemoved` is handled by the
        // stream layer below (it keys on the fullname), not by this decoder.
        ServiceEvent::ServiceFound(_, _)
        | ServiceEvent::SearchStarted(_)
        | ServiceEvent::SearchStopped(_)
        | ServiceEvent::ServiceRemoved(_, _) => None,
        _ => None,
    }
}

/// Streaming translation loop.
fn translate_stream(
    receiver: Receiver<ServiceEvent>,
    tx: std::sync::mpsc::Sender<FoundEvent>,
    mut seen: HashMap<ServerId, Found>,
) {
    loop {
        match receiver.recv_timeout(STREAM_IDLE) {
            Ok(event) => {
                match &event {
                    ServiceEvent::ServiceResolved(_) => {
                        if let Some((id, found)) = found_from_event(&event, now_ms()) {
                            let is_new = !seen.contains_key(&id);
                            let changed = seen.get(&id).is_some_and(|prior| {
                                prior.addresses != found.addresses
                                    || prior.port != found.port
                                    || prior.advertisement != found.advertisement
                            });
                            seen.insert(id.clone(), found.clone());
                            let ev = if is_new {
                                FoundEvent::Appeared(Box::new(found))
                            } else if changed {
                                FoundEvent::Updated {
                                    server_id: id,
                                    found: Box::new(found),
                                }
                            } else {
                                // Same data again (repeat announcement): refresh
                                // last_seen, emit nothing.
                                continue;
                            };
                            if tx.send(ev).is_err() {
                                return;
                            }
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        // The fullname carries the instance label as its first label.
                        let label = fullname.split('.').next().unwrap_or("");
                        if let Ok(id) = ServerId::new(label)
                            && seen.remove(&id).is_some()
                            && tx.send(FoundEvent::Disappeared { server_id: id }).is_err()
                        {
                            return;
                        }
                    }
                    _ => {}
                }
            }
            Err(mdns_sd::RecvTimeoutError::Timeout) => {
                // The daemon may sit silent for a while; keep looping until the
                // channel itself goes away (stops when the WatchHandle drops).
            }
            Err(mdns_sd::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// How long a stream may idle between events before the loop re-checks liveness.
pub const STREAM_IDLE: Duration = Duration::from_secs(1);

/// Milliseconds from the process start — a monotonic `last_seen` stamp that needs no
/// clock dependency (the *wall* timestamps belong to the caller's `Clock`).
fn now_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::{Found, FoundEvent, FoundSource};
    use crate::record::{
        AdvertisedSiteNames, Advertisement, PROTOCOL_VERSION, ProtocolVersion, ServerId, TlsMode,
    };

    fn adv_txt() -> Vec<(String, String)> {
        vec![
            ("instance".to_string(), "srv-1".to_string()),
            ("port".to_string(), "8080".to_string()),
            ("pv".to_string(), "1".to_string()),
            ("tls".to_string(), "off".to_string()),
            ("ver".to_string(), "0.1.0".to_string()),
        ]
    }

    #[test]
    fn found_source_manual_round_trip() {
        // `Found` with source Manual is what parse_manual callers build; the shape
        // carries through selection unchanged.
        let f = Found {
            server_id: ServerId("manual".to_string()),
            addresses: vec!["192.168.1.10".parse().unwrap_or_else(|e| panic!("{e}"))],
            port: 8080,
            advertisement: Advertisement {
                service_instance: ServerId("manual".to_string()),
                port: 8080,
                protocol_version: PROTOCOL_VERSION,
                tls: TlsMode::Off,
                server_key_fingerprint: None,
                product_version: String::new(),
                site_names: AdvertisedSiteNames::default(),
            },
            last_seen: 0,
            source: FoundSource::Manual,
        };
        assert_eq!(f.source, FoundSource::Manual);
        assert_eq!(f.addresses.len(), 1);
    }

    #[test]
    fn stream_events_shape() {
        // A quick shape check of the stream events the watcher emits.
        let f = Found {
            server_id: ServerId("srv-1".to_string()),
            addresses: vec![],
            port: 1,
            advertisement: Advertisement {
                service_instance: ServerId("srv-1".to_string()),
                port: 1,
                protocol_version: PROTOCOL_VERSION,
                tls: TlsMode::Off,
                server_key_fingerprint: None,
                product_version: String::new(),
                site_names: AdvertisedSiteNames::default(),
            },
            last_seen: 5,
            source: FoundSource::Mdns,
        };
        let appeared: FoundEvent = FoundEvent::Appeared(Box::new(f.clone()));
        let updated: FoundEvent = FoundEvent::Updated {
            server_id: f.server_id.clone(),
            found: Box::new(f.clone()),
        };
        let gone: FoundEvent = FoundEvent::Disappeared {
            server_id: f.server_id.clone(),
        };
        match (appeared, updated, gone) {
            (
                FoundEvent::Appeared(a),
                FoundEvent::Updated { server_id, .. },
                FoundEvent::Disappeared { server_id: d },
            ) => {
                assert_eq!(a.last_seen, 5);
                assert_eq!(server_id, d);
            }
            _ => panic!("shape"),
        }
    }

    #[test]
    fn decoder_helpers_are_usable() {
        // The pure decode path used by `found_from_event` (the event enum itself is
        // exercised by the ignored integration test on a real multicast socket):
        let txt = adv_txt();
        let adv = Advertisement::from_txt(&txt).unwrap_or_else(|e| panic!("{e:?}"));
        assert_eq!(adv.service_instance.as_str(), "srv-1");
        assert_eq!(adv.protocol_version, ProtocolVersion(1));
    }
}
