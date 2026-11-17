//! The optional one-way projection of the business graph to an external graph engine
//! (`[services.graph_projection]`, WP-GRA-07.T3). **Off by default**: with no table, or
//! `enabled = false`, nothing is created: no task, no connection, no cursor.
//!
//! When enabled it needs `endpoint` (an `https` URL whose host is in `allowed_hosts`, this
//! service's egress allow-list) and `secret` (the *name* of a sealed credential in
//! `[services.graph_projection.sealed]`, opened through the site's key provider at call time; never
//! a value in the configuration). A missing, non-https or non-allow-listed endpoint, or a secret
//! that cannot be opened, is a start-up finding: the service stays off and the site serves.
//!
//! - **Source**: the outbox topic `sys_graph_edge` (the projection crate's decoder) from a durable
//!   cursor, a file `sites/<site>/graph_projection.cursor` written after a batch is accepted.
//!   The consumer is off the write path.
//! - **Wire**: one `POST` per batch through the egress guard and sender only, with `Authorization:
//!   Bearer <secret>`. The body holds edge keys and types only (`graph`, `seq`, `op`, `edge_type`,
//!   `from` and `to` as DocType and record id): never a document field value. Pushes are idempotent
//!   upserts keyed by (edge type, from, to), so a redelivery after a crash or a retry changes
//!   nothing. A failure retries with exponential backoff up to `max_backoff_secs` and loses nothing.
//! - **One-way**: nothing is read back from the engine; the response body is dropped.
//! - **Tenant scope**: one projector and one external `database` name per site; events of another
//!   tenant's graph key are skipped.
//! - **Health**: the service is optional; lag and last success are logged (`spot::graph_projection`).
//!   A down engine only delays the projection.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, HeaderValue, header};
use serde::Deserialize;
use serde_json::json;
use spot_bpm_integration::CredentialRef;
use spot_egress::{
    AllowEntry, EgressConnector, EgressPolicy, Guard, HttpRequest, InMemorySink,
    Method as EgressMethod, Purpose, Sender, SystemResolver, TenantPolicy,
};
use spot_graph_projection::decode_record;
use spot_postgres::PgBackend;
use spot_storage_contract::{ChangeFeed as _, FeedPosition};
use tokio::sync::watch;
use url::Url;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the graph (1200), lineage (1210) and the rule graph (1220).
pub const ORDER: u32 = 1230;

const TARGET: &str = "spot::graph_projection";

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Sealed {
    sealed: String,
    aad: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    enabled: bool,
    endpoint: String,
    secret: String,
    allowed_hosts: Vec<String>,
    sealed: Sealed,
    database: String,
    batch: u32,
    interval_ms: u64,
    max_backoff_secs: u64,
    timeout_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            secret: String::new(),
            allowed_hosts: Vec::new(),
            sealed: Sealed::default(),
            database: String::new(),
            batch: 100,
            interval_ms: 1_000,
            max_backoff_secs: 300,
            timeout_secs: 10,
        }
    }
}

/// The endpoint if it is acceptable: https, a host, and the host on the allow-list.
fn endpoint_of(settings: &Settings) -> std::result::Result<Url, String> {
    let url = Url::parse(&settings.endpoint).map_err(|_| "endpoint is not a URL".to_owned())?;
    let host = url
        .host_str()
        .ok_or_else(|| "endpoint has no host".to_owned())?;
    if settings.database.is_empty() {
        return Err("database (the external graph name) is required".to_owned());
    }
    if url.scheme() != "https" {
        return Err("endpoint must be https".to_owned());
    }
    if !settings.allowed_hosts.iter().any(|h| h == host) {
        return Err("endpoint host is not on allowed_hosts".to_owned());
    }
    Ok(url)
}

/// The batch body: edge keys and types only.
fn body_of(graph: &str, events: &[spot_graph_index::EdgeEvent]) -> Vec<u8> {
    let node = |n: &spot_storage_contract::NodeRef| json!({"doctype": n.entity.as_str(), "id": n.id.to_string()});
    let events: Vec<_> = events
        .iter()
        .map(|e| {
            json!({"seq": e.sequence, "op": e.op.as_str(), "edge_type": e.edge.edge_type,
                   "from": node(&e.edge.from), "to": node(&e.edge.to)})
        })
        .collect();
    json!({"graph": graph, "events": events})
        .to_string()
        .into_bytes()
}

struct Projection {
    site: String,
    key: spot_graph_index::GraphKey,
    backend: PgBackend,
    guard: Arc<Guard>,
    sender: Arc<dyn Sender>,
    endpoint: Url,
    token: String,
    settings: Settings,
    cursor: PathBuf,
}

impl Projection {
    fn load_cursor(&self) -> FeedPosition {
        let text = std::fs::read_to_string(&self.cursor).unwrap_or_default();
        let mut parts = text.trim().split(' ').filter_map(|p| p.parse::<u64>().ok());
        match (parts.next(), parts.next()) {
            (Some(generation), Some(seq)) => FeedPosition::new(generation, seq),
            _ => FeedPosition::START,
        }
    }

    fn save_cursor(&self, p: FeedPosition) -> std::result::Result<(), String> {
        std::fs::write(&self.cursor, format!("{} {}", p.generation(), p.seq()))
            .map_err(|e| e.to_string())
    }

    async fn push(&self, body: Vec<u8>) -> std::result::Result<(), String> {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", self.token))
            .map_err(|_| "bad credential".to_owned())?;
        auth.set_sensitive(true);
        headers.insert(header::AUTHORIZATION, auth);
        let request = spot_egress::Request {
            url: self.endpoint.clone(),
            purpose: Purpose::Connector,
            tenant: self.site.clone(),
            actor: None,
        };
        let authorized = self
            .guard
            .authorize(&request)
            .await
            .map_err(|e| e.to_string())?;
        let http = HttpRequest::new(
            EgressMethod::Post,
            self.endpoint.clone(),
            headers,
            Some(body.into()),
        );
        let response = self
            .sender
            .send_request(&http, &authorized, 64 * 1024)
            .await
            .map_err(|e| e.to_string())?;
        if response.status.is_success() {
            Ok(())
        } else {
            Err(format!("the engine answered {}", response.status.as_u16()))
        }
    }

    /// One turn: read a batch after the cursor, push its edge events, move the cursor. Returns
    /// how many records were read.
    async fn turn(&self) -> std::result::Result<usize, String> {
        let position = self.load_cursor();
        let records = self
            .backend
            .read_outbox(position, self.settings.batch)
            .await
            .map_err(|e| e.to_string())?;
        let Some(last) = records.last().map(|r| r.position) else {
            return Ok(0);
        };
        let events: Vec<_> = records
            .iter()
            .filter_map(|r| decode_record(r).ok().flatten())
            .filter(|e| e.graph == self.key)
            .collect();
        if !events.is_empty() {
            self.push(body_of(&self.settings.database, &events)).await?;
        }
        self.save_cursor(last)?;
        Ok(records.len())
    }
}

struct ProjectionService(Arc<Projection>);

impl Service for ProjectionService {
    fn name(&self) -> &'static str {
        "graph_projection"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let (p, mut stop) = (&self.0, stop);
            let mut failures = 0u32;
            loop {
                let wait = if failures == 0 {
                    Duration::from_millis(p.settings.interval_ms)
                } else {
                    Duration::from_secs((1u64 << failures.min(16)).min(p.settings.max_backoff_secs))
                };
                tokio::select! {
                    changed = stop.changed() => if changed.is_err() || *stop.borrow() { break },
                    () = tokio::time::sleep(wait) => match p.turn().await {
                        Ok(n) => {
                            failures = 0;
                            if n > 0 {
                                tracing::info!(target: TARGET, site = %p.site, records = n, "graph projection batch pushed");
                            }
                        }
                        Err(why) => {
                            failures = failures.saturating_add(1);
                            tracing::warn!(target: TARGET, site = %p.site, failures, %why, "graph projection push failed; will retry");
                        }
                    },
                }
            }
            Ok(())
        })
    }
}

/// Registers the projection when `[services.graph_projection]` enables it and its settings hold.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("graph_projection")?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let finding = |why: &str| {
        tracing::error!(target: TARGET, site = %ctx.label(), why, "graph projection stays off");
    };
    let endpoint = match endpoint_of(&settings) {
        Ok(url) => url,
        Err(why) => {
            finding(&why);
            return Ok(());
        }
    };
    let (Some(secrets), true) = (
        ctx.port::<super::secrets::SecretsHost>(),
        !settings.secret.is_empty(),
    ) else {
        finding("no secrets service or no secret reference");
        return Ok(());
    };
    let resolver = super::process_integration::sealed_resolver(
        [(
            settings.secret.clone(),
            settings.sealed.sealed.clone(),
            settings.sealed.aad.clone(),
        )],
        Arc::clone(secrets.keyring()),
    );
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let token = futures_block(resolver.resolve(&CredentialRef::new(settings.secret.clone())))
        .map_err(|_| Error("services.graph_projection: the secret cannot be opened".to_owned()));
    let Ok(token) = token else {
        finding("the secret cannot be opened");
        return Ok(());
    };
    let host = endpoint.host_str().unwrap_or_default().to_owned();
    let mut policy = EgressPolicy {
        default: "deny_non_public".to_owned(),
        dangerous_ports: vec![22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250],
        tenants: Default::default(),
    };
    let entry = AllowEntry {
        scheme: Some("https".to_owned()),
        host,
        ports: endpoint.port().map(|p| vec![p]),
        purpose: None,
        allow_private: None,
    };
    policy.tenants.insert(
        site.clone(),
        TenantPolicy {
            allow: vec![entry],
            ..TenantPolicy::default()
        },
    );
    let guard = Arc::new(Guard::new(
        policy,
        Arc::new(SystemResolver),
        Arc::new(InMemorySink::new()),
        Arc::clone(ctx.clock()),
    ));
    let sender: Arc<dyn Sender> = Arc::new(spot_egress::HttpSender::new(
        EgressConnector::new(Arc::clone(&guard), site.clone(), Purpose::Connector),
        Duration::from_secs(settings.timeout_secs),
    ));
    let cursor = env
        .root
        .join("sites")
        .join(&site)
        .join("graph_projection.cursor");
    let projection = Projection {
        key: spot_graph_index::GraphKey::new(site.clone(), "business"),
        site,
        backend: env.backend.clone(),
        guard,
        sender,
        endpoint,
        token: token.expose().to_owned(),
        settings,
        cursor,
    };
    ctx.add(Arc::new(ProjectionService(Arc::new(projection))));
    Ok(())
}

/// Blocks on `future` from the synchronous registration (the same bridge the other modules use).
fn futures_block<T>(future: impl std::future::Future<Output = T>) -> T {
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| handle.block_on(future))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use spot_graph_index::{EdgeEvent, EdgeKey, EdgeOp, GraphKey};
    use spot_storage_contract::{Name, NodeRef, RecordId, Value};

    fn node(doctype: &str) -> NodeRef {
        NodeRef {
            entity: Name::new(doctype).unwrap(),
            id: RecordId::new_v7(),
        }
    }

    fn settings(endpoint: &str, hosts: &[&str], database: &str) -> Settings {
        Settings {
            enabled: true,
            endpoint: endpoint.to_owned(),
            allowed_hosts: hosts.iter().map(|h| (*h).to_owned()).collect(),
            database: database.to_owned(),
            ..Settings::default()
        }
    }

    #[test]
    fn the_projection_is_off_by_default_and_unknown_keys_are_refused() {
        assert!(!Settings::default().enabled);
        let parsed: Settings = toml::from_str("").unwrap();
        assert!(!parsed.enabled && parsed.endpoint.is_empty() && parsed.secret.is_empty());
        assert!(toml::from_str::<Settings>("enabled = true\nverify_tls = false\n").is_err());
        // A secret is a reference: the sealed text is the only credential-shaped value, and the
        // default holds none.
        assert!(Settings::default().sealed.sealed.is_empty());
    }

    #[test]
    fn only_an_https_allow_listed_endpoint_with_a_database_name_is_accepted() {
        let good = settings(
            "https://graph.example.com:8443/ingest",
            &["graph.example.com"],
            "g",
        );
        assert_eq!(
            endpoint_of(&good).unwrap().host_str(),
            Some("graph.example.com")
        );
        let refused = [
            settings(
                "http://graph.example.com/ingest",
                &["graph.example.com"],
                "g",
            ),
            settings(
                "https://other.example.com/ingest",
                &["graph.example.com"],
                "g",
            ),
            settings("https://graph.example.com/ingest", &[], "g"),
            settings(
                "https://graph.example.com/ingest",
                &["graph.example.com"],
                "",
            ),
            settings("not a url", &["graph.example.com"], "g"),
            settings("", &["graph.example.com"], "g"),
        ];
        for bad in refused {
            assert!(
                endpoint_of(&bad).is_err(),
                "{:?} must be refused",
                bad.endpoint
            );
        }
        // The allow-list matches the whole host, never a suffix or a prefix of it.
        let tricky = settings(
            "https://graph.example.com.evil.test/x",
            &["graph.example.com"],
            "g",
        );
        assert!(endpoint_of(&tricky).is_err());
    }

    #[test]
    fn a_pushed_batch_holds_edge_keys_and_types_only_and_a_redelivery_is_byte_identical() {
        let graph = GraphKey::new("site-a", "business");
        let (task, project) = (node("task"), node("project"));
        let mut added = EdgeEvent::bare(
            7,
            graph.clone(),
            EdgeOp::Add,
            EdgeKey::new("task.project", task.clone(), project.clone()),
        );
        // A property value must never reach the wire.
        added.properties.insert(
            "note".to_owned(),
            Value::Text("confidential-value-123".to_owned()),
        );
        let removed = EdgeEvent::bare(
            8,
            graph,
            EdgeOp::Remove,
            EdgeKey::new("task.project", task.clone(), project.clone()),
        );
        let body = body_of("ext_db", &[added.clone(), removed.clone()]);
        let text = String::from_utf8(body.clone()).unwrap();
        assert!(!text.contains("confidential-value-123"), "{text}");
        assert!(!text.contains("note"), "{text}");
        let sent: serde_json::Value = serde_json::from_str(&text).unwrap();
        let expected = json!({
            "graph": "ext_db",
            "events": [
                {"seq": 7, "op": "add", "edge_type": "task.project",
                 "from": {"doctype": "task", "id": task.id.to_string()},
                 "to": {"doctype": "project", "id": project.id.to_string()}},
                {"seq": 8, "op": "remove", "edge_type": "task.project",
                 "from": {"doctype": "task", "id": task.id.to_string()},
                 "to": {"doctype": "project", "id": project.id.to_string()}},
            ],
        });
        assert_eq!(sent, expected);
        // The same events again (a retry or a redelivery) are the same bytes: the upsert key is
        // (edge type, from, to), so the engine's state does not change.
        assert_eq!(body, body_of("ext_db", &[added, removed]));
    }

    #[test]
    fn an_empty_batch_has_no_events() {
        let sent: serde_json::Value = serde_json::from_slice(&body_of("g", &[])).unwrap();
        assert_eq!(sent, json!({"graph": "g", "events": []}));
    }
}
