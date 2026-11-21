//! The bridge against a fake sender: requests pass through the sender, a denied host fails with
//! the egress code and never reaches it, and a store built on the bridge reports the same code.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use http::{HeaderMap, StatusCode};
use object_store::ClientOptions;
use object_store::client::{HttpConnector, HttpRequestBody};
use spot_egress::{
    Authorized, DenialSink, EgressConnector, EgressError, EgressPolicy, Guard, HttpRequest,
    InMemorySink, Purpose, Resolver, Response, Sender,
};

use super::egress::EgressHttp;

struct StaticResolver(HashMap<String, Vec<IpAddr>>);

#[async_trait]
impl Resolver for StaticResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, std::io::Error> {
        self.0
            .get(host)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such host"))
    }
}

/// Records what it was asked to send and answers 200 with a fixed body.
#[derive(Default)]
struct FakeSender {
    seen: Mutex<Vec<(String, String, usize)>>,
}

#[async_trait]
impl Sender for FakeSender {
    async fn send_request(
        &self,
        request: &HttpRequest,
        authorized: &Authorized,
        _max_body: u64,
    ) -> Result<Response, EgressError> {
        assert!(!authorized.socket_addrs().is_empty());
        self.seen.lock().unwrap().push((
            request.method.as_str().to_owned(),
            request.url.path().to_owned(),
            request.body.as_ref().map_or(0, |b| b.len()),
        ));
        let mut headers = HeaderMap::new();
        headers.insert("etag", "\"abc\"".parse().unwrap());
        Ok(Response {
            status: StatusCode::OK,
            headers,
            body: b"object bytes".to_vec(),
        })
    }
}

fn rig(policy: &str) -> (EgressHttp, Arc<FakeSender>, Arc<InMemorySink>) {
    let ip: IpAddr = "93.184.216.34".parse().unwrap();
    let resolver = Arc::new(StaticResolver(
        [("objects.example.test".to_owned(), vec![ip])]
            .into_iter()
            .collect(),
    ));
    let sink = Arc::new(InMemorySink::new());
    let guard = Arc::new(Guard::new(
        EgressPolicy::from_toml(policy).unwrap(),
        resolver,
        sink.clone(),
        Arc::new(spot_core::SystemClock::new()),
    ));
    let sender = Arc::new(FakeSender::default());
    let connector = EgressConnector::new(guard, "acme", Purpose::Connector);
    (EgressHttp::new(connector, sender.clone()), sender, sink)
}

const ALLOW: &str = r#"
[tenants.acme]
[[tenants.acme.allow]]
scheme = "https"
host = "objects.example.test"
ports = [443]
purpose = "connector"
"#;

const DENY: &str = r#"
[tenants.acme]
"#;

fn request(method: &str, uri: &str, body: Vec<u8>) -> object_store::client::HttpRequest {
    http::Request::builder()
        .method(method)
        .uri(uri)
        .body(HttpRequestBody::from(body))
        .unwrap()
}

#[tokio::test]
async fn object_store_calls_pass_through_the_sender() {
    let (http, sender, sink) = rig(ALLOW);
    let client = http.connect(&ClientOptions::new()).unwrap();

    let put = client
        .execute(request(
            "PUT",
            "https://objects.example.test/bucket/key",
            b"hello".to_vec(),
        ))
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::OK);
    let got = client
        .execute(request(
            "GET",
            "https://objects.example.test/bucket/key",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(got.headers().get("etag").unwrap(), "\"abc\"");
    assert_eq!(&got.into_body().bytes().await.unwrap()[..], b"object bytes");

    let seen = sender.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            ("PUT".to_owned(), "/bucket/key".to_owned(), 5),
            ("GET".to_owned(), "/bucket/key".to_owned(), 0),
        ]
    );
    assert!(sink.records().is_empty());
}

#[tokio::test]
async fn a_denied_host_fails_with_the_egress_code_and_never_reaches_the_sender() {
    let (http, sender, sink) = rig(DENY);
    let client = http.connect(&ClientOptions::new()).unwrap();
    let err = client
        .execute(request(
            "GET",
            "https://objects.example.test/bucket/key",
            vec![],
        ))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("egress.denied_by_policy"), "{text}");
    assert!(sender.seen.lock().unwrap().is_empty());
    assert_eq!(sink.records().len(), 1);
}

#[tokio::test]
async fn a_method_the_sender_does_not_have_is_refused_before_anything_is_sent() {
    let (http, sender, _sink) = rig(ALLOW);
    let client = http.connect(&ClientOptions::new()).unwrap();
    let err = client
        .execute(request(
            "HEAD",
            "https://objects.example.test/bucket/key",
            vec![],
        ))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("egress.method_unsupported"));
    assert!(sender.seen.lock().unwrap().is_empty());
}

#[cfg(feature = "s3")]
#[tokio::test]
async fn a_store_built_on_the_bridge_reports_the_egress_code() {
    use crate::key::{FileKey, Visibility};
    use crate::object::{ObjectFileStore, S3Config};
    use crate::store::FileStore;
    use spot_core::SiteId;

    let (http, sender, _sink) = rig(DENY);
    let store = ObjectFileStore::new_s3(
        S3Config {
            endpoint: Some("https://objects.example.test".parse().unwrap()),
            region: Some("us-east-1".to_owned()),
            bucket: "bucket".to_owned(),
            access_key_id: Some("test-key".to_owned()),
            secret_access_key: Some("test-secret".to_owned()),
            allow_http: false,
            path_style: true,
        },
        http,
    )
    .unwrap();
    let site = SiteId::new("acme").unwrap();
    let key = FileKey::new(Visibility::Private, "docs/a.txt").unwrap();
    let err = store.get(&site, key).await.err().unwrap();
    let text = format!("{err:?}");
    assert!(text.contains("egress.denied_by_policy"), "{text}");
    assert!(sender.seen.lock().unwrap().is_empty());
}
