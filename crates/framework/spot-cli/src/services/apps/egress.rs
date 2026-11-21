//! An app's network: every outbound call is authorised by `spot-egress` first.
//!
//! The policy an app's guard enforces is the **intersection** of the endpoints the administrator
//! approved for the app (its `GrantedCapabilities.network`) and the site's allow-list
//! (`services.apps.egress_allow`, `host:port` entries). An empty site allow-list, or an app with
//! no approved endpoint, means no network at all. The guard resolves the name once, refuses
//! loopback, private, link-local and metadata addresses after resolution, and returns pinned
//! addresses: the caller connects to those and never resolves again (rebinding-safe). A redirect
//! is a new request: it goes through [`AppEgress::authorize_url`] again.
//!
//! Nothing here sees a request or response body, so none can reach a log.

use std::sync::Arc;
use std::time::Duration;

use spot_core::Clock;
use spot_egress::{
    AllowEntry, Authorized, EgressError, EgressPolicy, Guard, InMemorySink, Purpose, Request,
    Resolver, TenantPolicy,
};

/// `app.egress_denied`: the call is not allowed by the app's approved hosts and the site policy.
pub const EGRESS_DENIED: &str = "app.egress_denied";

/// Most bytes of a response an app may read.
pub const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
/// Longest an outbound call may take.
pub const TOTAL_TIMEOUT: Duration = Duration::from_secs(10);

/// One app's outbound gate.
pub struct AppEgress {
    guard: Guard,
    site: String,
    app: String,
    purpose: Purpose,
}

/// The policy for one app on one site.
#[must_use]
pub fn policy_for(
    site: &str,
    approved: &[(String, u16)],
    site_allow: &[(String, u16)],
) -> EgressPolicy {
    let allow = approved
        .iter()
        .filter(|endpoint| site_allow.contains(endpoint))
        .map(|(host, port)| AllowEntry {
            scheme: None,
            host: host.clone(),
            ports: Some(vec![*port]),
            purpose: Some(Purpose::App(0).as_str().to_owned()),
            // Never a private address: an app cannot be granted the internal network.
            allow_private: None,
        })
        .collect();
    let mut policy = EgressPolicy {
        default: "deny_non_public".to_owned(),
        dangerous_ports: vec![22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250],
        tenants: std::collections::HashMap::new(),
    };
    policy.tenants.insert(
        site.to_owned(),
        TenantPolicy {
            allow,
            connect_timeout_secs: 5,
            tls_timeout_secs: 5,
            total_timeout_secs: TOTAL_TIMEOUT.as_secs(),
            max_response_bytes: MAX_RESPONSE_BYTES,
            max_redirects: 3,
            ..TenantPolicy::default()
        },
    );
    policy
}

impl AppEgress {
    /// The gate of `app` on `site`.
    pub fn new(
        site: &str,
        app: &str,
        approved: &[(String, u16)],
        site_allow: &[(String, u16)],
        resolver: Arc<dyn Resolver>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            guard: Guard::new(
                policy_for(site, approved, site_allow),
                resolver,
                Arc::new(InMemorySink::new()),
                clock,
            ),
            site: site.to_owned(),
            app: app.to_owned(),
            purpose: Purpose::App(0),
        }
    }

    /// Authorises one URL (the first request or a redirect target). `external_allowed` is the
    /// hook context's answer: an in-transaction hook is refused whatever the policy says (ADR-51).
    ///
    /// # Errors
    ///
    /// [`EGRESS_DENIED`] with no detail beyond the egress code.
    pub async fn authorize_url(
        &self,
        url: &str,
        external_allowed: bool,
    ) -> Result<Authorized, String> {
        if !external_allowed {
            return Err(format!(
                "{EGRESS_DENIED}: external calls are not allowed in this phase"
            ));
        }
        let url = url
            .parse()
            .map_err(|_| format!("{EGRESS_DENIED}: the URL is not valid"))?;
        let request = Request {
            url,
            purpose: self.purpose,
            tenant: self.site.clone(),
            actor: Some(self.app.clone()),
        };
        self.guard
            .authorize(&request)
            .await
            .map_err(|e: EgressError| format!("{EGRESS_DENIED}: {}", e.code()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_endpoints_both_approved_and_site_allowed_are_listed() {
        let approved = vec![
            ("api.example.com".to_owned(), 443),
            ("x.example.com".to_owned(), 443),
        ];
        let site = vec![("api.example.com".to_owned(), 443)];
        let policy = policy_for("s", &approved, &site);
        let allow = &policy.tenants["s"].allow;
        assert_eq!(allow.len(), 1);
        assert_eq!(allow[0].host, "api.example.com");
        assert!(allow[0].allow_private.is_none());
    }

    #[test]
    fn an_empty_site_allow_list_means_no_network() {
        let approved = vec![("api.example.com".to_owned(), 443)];
        let policy = policy_for("s", &approved, &[]);
        assert!(policy.tenants["s"].allow.is_empty());
    }
}
