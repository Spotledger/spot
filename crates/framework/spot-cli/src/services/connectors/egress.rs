//! Outbound HTTP for connectors: every call passes `spot-egress` under the calling connector's own
//! allow-list.
//!
//! [`PerConnectorHttp`] is the [`HttpService`] the connector host sends through. The service tags
//! each call's actor `connector:<name>`; this type picks that connector's [`EgressHttp`], built from
//! the connector's declared `endpoints`. A connector with an **empty** list has no entry, so any
//! request it makes is refused before a connection exists (`connectors.egress_denied`). Inside
//! [`EgressHttp`] the guard resolves the name once, refuses loopback, private, link-local and
//! metadata addresses after resolution, pins the connection to the authorised addresses, and the
//! sender re-checks every redirect; response size and total time are capped. Credentials are
//! injected by the host into the request and are never in a log, a metric label or an error.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use spot_connectors::error::{ConnectorError, Result as ConnResult};
use spot_connectors::{EgressHttp, HttpContext, HttpRequest, HttpResponse, HttpService};
use spot_core::Clock;
use spot_egress::{
    AllowEntry, EgressConnector, EgressPolicy, Guard, HttpSender, InMemorySink, Purpose, Resolver,
    Sender, TenantPolicy,
};

/// The actor prefix the service puts on a connector's own calls.
pub const ACTOR_PREFIX: &str = "connector:";

/// The most bytes of a response a connector may read.
pub const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
/// The longest an outbound call may take.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Splits `host` or `host:port` into the host and the ports the entry lists.
fn split_endpoint(endpoint: &str) -> Option<(String, Vec<u16>)> {
    let text = endpoint.trim();
    if text.is_empty() {
        return None;
    }
    match text.rsplit_once(':') {
        Some((host, port)) => {
            let port: u16 = port.parse().ok()?;
            (!host.is_empty() && port != 0).then(|| (host.to_owned(), vec![port]))
        }
        None => Some((text.to_owned(), vec![443, 80])),
    }
}

/// The egress policy of one connector on one site.
#[must_use]
pub fn policy_for(site: &str, endpoints: &[String]) -> EgressPolicy {
    let allow = endpoints
        .iter()
        .filter_map(|e| split_endpoint(e))
        .map(|(host, ports)| AllowEntry {
            scheme: None,
            host,
            ports: Some(ports),
            purpose: Some(Purpose::Connector.as_str().to_owned()),
            // A connector is never granted the internal network.
            allow_private: None,
        })
        .collect();
    let mut policy = EgressPolicy {
        default: "deny_non_public".to_owned(),
        dangerous_ports: vec![22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250],
        tenants: HashMap::new(),
    };
    policy.tenants.insert(
        site.to_owned(),
        TenantPolicy {
            allow,
            connect_timeout_secs: 5,
            tls_timeout_secs: 5,
            total_timeout_secs: CALL_TIMEOUT.as_secs(),
            max_response_bytes: MAX_RESPONSE_BYTES,
            max_redirects: 3,
            ..TenantPolicy::default()
        },
    );
    policy
}

/// Sends each connector's calls through its own guard.
pub struct PerConnectorHttp {
    site: String,
    resolver: Arc<dyn Resolver>,
    clock: Arc<dyn Clock>,
    by_connector: RwLock<HashMap<String, Arc<EgressHttp>>>,
}

impl PerConnectorHttp {
    /// An empty router for `site`.
    pub fn new(site: &str, resolver: Arc<dyn Resolver>, clock: Arc<dyn Clock>) -> Self {
        Self {
            site: site.to_owned(),
            resolver,
            clock,
            by_connector: RwLock::new(HashMap::new()),
        }
    }

    /// Sets (or replaces) the allow-list of `connector`. An empty list removes it: no network.
    pub fn set_endpoints(&self, connector: &str, endpoints: &[String]) {
        let mut map = self.by_connector.write().unwrap_or_else(|p| p.into_inner());
        if endpoints.iter().all(|e| split_endpoint(e).is_none()) {
            map.remove(connector);
            return;
        }
        let guard = Arc::new(Guard::new(
            policy_for(&self.site, endpoints),
            Arc::clone(&self.resolver),
            Arc::new(InMemorySink::new()),
            Arc::clone(&self.clock),
        ));
        let sender: Arc<dyn Sender> = Arc::new(HttpSender::new(
            EgressConnector::new(Arc::clone(&guard), self.site.clone(), Purpose::Connector),
            CALL_TIMEOUT,
        ));
        map.insert(
            connector.to_owned(),
            Arc::new(EgressHttp::new(guard, sender, MAX_RESPONSE_BYTES)),
        );
    }

    /// Drops a connector's entry (an uninstall).
    pub fn remove(&self, connector: &str) {
        self.by_connector
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(connector);
    }
}

#[async_trait]
impl HttpService for PerConnectorHttp {
    async fn send(&self, request: HttpRequest, ctx: &HttpContext) -> ConnResult<HttpResponse> {
        let found = ctx
            .actor
            .as_deref()
            .and_then(|a| a.strip_prefix(ACTOR_PREFIX))
            .and_then(|name| {
                self.by_connector
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .get(name)
                    .cloned()
            });
        match found {
            Some(egress) => egress.send(request, ctx).await,
            None => Err(ConnectorError::EgressDenied {
                host: request
                    .url
                    .parse::<url::Url>()
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_owned))
                    .unwrap_or_else(|| "unknown".to_owned()),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_split_into_host_and_ports() {
        assert_eq!(
            split_endpoint("api.example.com"),
            Some(("api.example.com".to_owned(), vec![443, 80]))
        );
        assert_eq!(
            split_endpoint("api.example.com:8443"),
            Some(("api.example.com".to_owned(), vec![8443]))
        );
        assert_eq!(split_endpoint(" "), None);
        assert_eq!(split_endpoint("h:notaport"), None);
    }

    #[test]
    fn an_empty_list_is_an_empty_allow_list_and_never_private() {
        let policy = policy_for("s", &[]);
        assert!(policy.tenants["s"].allow.is_empty());
        let policy = policy_for("s", &["a.example.com".to_owned()]);
        assert!(policy.tenants["s"].allow[0].allow_private.is_none());
    }
}
