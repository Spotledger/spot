//! Shared fixtures: a harness that serves the store over memory doubles (or any stores), signed
//! bundles, hand-built multipart bodies and small request helpers. Requests go through the router
//! with `oneshot`, so no socket is opened.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use base64::Engine as _;
use serde_json::{Value, json};
use spot_bundle::{Bundle, BundleBuilder, HostInfo, Limits};
use spot_bundle_signing::{
    BundleSignature, KeyPair, PublisherCertificate, SignedCertificate, TrustedRoot,
    generate_keypair, issue_certificate, sign_bundle,
};
use spot_registry::{
    BlobStore, BlobValidation, FsBlobStore, IndexStore, MemoryIndexStore, Version,
};
use spot_store_server::{
    AdminCredential, Config, ManualClock, Service, ServiceParts, StoreKey, router,
};
use spot_store_state::{MemoryStoreState, ReviewRecord, ReviewStatus, StoreStateStore};
use tower::ServiceExt;

pub const NOW: i64 = 1_000_000;
pub const COMPONENT: &[u8] = b"\0asm\x0d\0\x01\0";
pub const ADMIN: &str = "operator-secret-0123456789-abcdefghijklmnopqrstuvwxyz";
pub const BOUNDARY: &str = "storeserverboundary7e1f";

pub fn root_key() -> KeyPair {
    generate_keypair(&[11; 32])
}

pub fn publisher_key(n: u8) -> KeyPair {
    generate_keypair(&[n; 32])
}

pub fn cert(n: u8) -> SignedCertificate {
    issue_certificate(
        PublisherCertificate {
            publisher_id: format!("publisher{n}"),
            display_name: format!("Publisher {n}"),
            key: publisher_key(n).public(),
            valid_from: 100,
            valid_to: NOW + 100_000_000,
            issued_by: root_key().public().key_id(),
            serial: u64::from(n),
        },
        &root_key(),
    )
}

pub fn manifest(name: &str, version: &str, title: &str) -> String {
    format!("[app]\nname = {name:?}\nversion = {version:?}\ntitle = {title:?}\n")
}

pub fn bundle(name: &str, version: &str, title: &str) -> Vec<u8> {
    BundleBuilder::new(manifest(name, version, title), COMPONENT)
        .build()
        .unwrap()
}

pub fn open(bytes: &[u8]) -> Bundle {
    Bundle::open(
        bytes,
        &Limits::default(),
        &HostInfo {
            version: Version::new(0, 1, 0),
        },
    )
    .unwrap()
}

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The publisher's own signature of `bytes`, as the upload carries it.
pub fn signature_of(bytes: &[u8], publisher: u8) -> BundleSignature {
    let opened = open(bytes);
    sign_bundle(
        &opened.digest(),
        opened.manifest().app.name.as_str(),
        &opened.manifest().app.version.to_string(),
        &cert(publisher),
        &publisher_key(publisher),
        NOW - 10,
    )
    .unwrap()
}

/// `signature_of` with one byte of the signature flipped: it parses, and it does not verify.
pub fn tampered_signature(bytes: &[u8], publisher: u8) -> BundleSignature {
    let good = signature_of(bytes, publisher);
    let mut raw = *good.signature();
    raw[0] ^= 1;
    BundleSignature::from_parts(
        good.app_id.clone(),
        good.app_version.clone(),
        good.publisher_id.clone(),
        good.key_id.clone(),
        good.signed_at,
        raw,
    )
}

pub struct Part {
    pub name: &'static str,
    pub content_type: Option<&'static str>,
    pub data: Vec<u8>,
}

pub fn multipart_body(parts: &[Part]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{}\"\r\n", part.name).as_bytes(),
        );
        if let Some(content_type) = part.content_type {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(&part.data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// The three parts of a correct upload.
pub fn standard_parts(bytes: &[u8], signature: &BundleSignature, publisher: u8) -> Vec<Part> {
    vec![
        Part {
            name: "bundle",
            content_type: Some("application/octet-stream"),
            data: bytes.to_vec(),
        },
        Part {
            name: "signature",
            content_type: Some("text/plain"),
            data: b64(signature.to_json().as_bytes()).into_bytes(),
        },
        Part {
            name: "certificate_chain",
            content_type: Some("text/plain"),
            data: b64(cert(publisher).to_json().as_bytes()).into_bytes(),
        },
    ]
}

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!(
                "not JSON ({e}): status {} body {:?}",
                self.status,
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    }

    /// The error code of an error envelope.
    pub fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    pub fn data(&self) -> Value {
        self.json()["data"].clone()
    }
}

pub struct Harness {
    pub service: Arc<Service>,
    pub router: Router,
    pub clock: Arc<ManualClock>,
    pub state: Arc<dyn StoreStateStore>,
    pub index: Arc<dyn IndexStore>,
    pub rt: tokio::runtime::Runtime,
    pub dir: tempfile::TempDir,
}

/// A configuration whose rate limits never get in the way; a test that needs a limit lowers it.
pub fn generous(config: &mut Config) {
    for rule in [
        &mut config.rates.public_per_address,
        &mut config.rates.publish_per_key,
        &mut config.rates.publish_per_address,
        &mut config.rates.review_per_key,
        &mut config.rates.admin_per_address,
        &mut config.rates.auth_failures_per_address,
    ] {
        rule.max = 100_000;
    }
}

impl Harness {
    pub fn new() -> Self {
        Self::with(|_| {})
    }

    pub fn with(tweak: impl FnOnce(&mut Config)) -> Self {
        Self::on(
            Arc::new(MemoryIndexStore::default()),
            Arc::new(MemoryStoreState::new()),
            tweak,
        )
    }

    /// The store over the given index and state, with a real filesystem blob store.
    pub fn on(
        index: Arc<dyn IndexStore>,
        state: Arc<dyn StoreStateStore>,
        tweak: impl FnOnce(&mut Config),
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let blob_dir = dir.path().join("blobs");
        let temp_dir = dir.path().join("tmp");
        std::fs::create_dir_all(&temp_dir).unwrap();
        let mut config = Config::minimal(blob_dir.clone(), temp_dir);
        generous(&mut config);
        tweak(&mut config);
        config.validate().unwrap();
        let clock = Arc::new(ManualClock::new(NOW));
        let blobs: Arc<dyn BlobStore> =
            Arc::new(FsBlobStore::new(&blob_dir, BlobValidation::default()).unwrap());
        let service = Service::new(ServiceParts {
            config,
            index: Arc::clone(&index),
            blobs,
            state: Arc::clone(&state),
            clock: clock.clone(),
            store_key: StoreKey::from_seed(&[3; 32]),
            admin: AdminCredential::from_secret(ADMIN).unwrap(),
            trust_roots: vec![TrustedRoot::new(root_key().public(), 0, None)],
        })
        .unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        Self {
            router: router(Arc::clone(&service)),
            service,
            clock,
            state,
            index,
            rt,
            dir,
        }
    }

    pub fn blob_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("blobs")
    }

    pub fn temp_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("tmp")
    }

    /// File names directly inside `dir`.
    pub fn names_in(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn send(&self, request: Request<Body>) -> Reply {
        let router = self.router.clone();
        self.rt.block_on(async move {
            let response = router.oneshot(request).await.unwrap();
            let (parts, body) = response.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            Reply {
                status: parts.status,
                headers: parts.headers,
                body: bytes.to_vec(),
            }
        })
    }

    pub fn get(&self, path: &str) -> Reply {
        self.get_with(path, None, &[])
    }

    pub fn get_with(&self, path: &str, token: Option<&str>, headers: &[(&str, &str)]) -> Reply {
        let mut builder = Request::builder().method("GET").uri(path);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        self.send(builder.body(Body::empty()).unwrap())
    }

    pub fn admin_get(&self, path: &str) -> Reply {
        self.get_with(path, Some(ADMIN), &[])
    }

    pub fn admin_post(&self, path: &str, body: Option<Value>) -> Reply {
        self.post_json(path, Some(ADMIN), body, &[])
    }

    pub fn post_json(
        &self,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> Reply {
        let mut builder = Request::builder().method("POST").uri(path);
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = match body {
            Some(value) => builder
                .header("content-type", "application/json")
                .body(Body::from(value.to_string()))
                .unwrap(),
            None => builder.body(Body::empty()).unwrap(),
        };
        self.send(request)
    }

    /// Issues a publisher key for `publisher{n}` and returns it.
    pub fn issue_key(&self, n: u8) -> String {
        let reply = self.admin_post(
            &format!("/admin/v1/publishers/publisher{n}/keys"),
            Some(json!({ "label": "test" })),
        );
        assert_eq!(reply.status, StatusCode::CREATED, "{:?}", reply.json());
        reply.data()["key"].as_str().unwrap().to_owned()
    }

    pub fn publish_request(
        &self,
        token: Option<&str>,
        name: &str,
        parts: &[Part],
        headers: &[(&str, &str)],
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(format!("/store/v1/apps/{name}/versions"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            );
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(Body::from(multipart_body(parts))).unwrap()
    }

    /// A correct, signed publish of `bytes` by `publisher{n}`.
    pub fn publish(&self, token: &str, name: &str, bytes: &[u8], n: u8) -> Reply {
        let parts = standard_parts(bytes, &signature_of(bytes, n), n);
        self.send(self.publish_request(Some(token), name, &parts, &[]))
    }

    /// Publishes a fresh `name` `version` and returns its review id.
    pub fn publish_ok(&self, token: &str, name: &str, version: &str, n: u8) -> String {
        let bytes = bundle(name, version, name);
        let reply = self.publish(token, name, &bytes, n);
        assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.json());
        reply.data()["review_id"].as_str().unwrap().to_owned()
    }

    pub fn approve(&self, name: &str, version: &str) -> Reply {
        self.admin_post(
            &format!("/admin/v1/apps/{name}/versions/{version}/approve"),
            None,
        )
    }

    /// Waits until the review worker has finished the review (done or failed).
    pub fn wait_review(&self, review_id: &str) -> ReviewRecord {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let record = self.state.get_review(review_id).unwrap().unwrap();
            if matches!(record.status, ReviewStatus::Done | ReviewStatus::Failed) {
                return record;
            }
            assert!(Instant::now() < deadline, "the review never finished");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Reads every audit entry as (kind, reason).
    pub fn audit(&self) -> Vec<(String, String)> {
        let reply = self.admin_get("/admin/v1/audit?limit=1000");
        assert_eq!(reply.status, StatusCode::OK);
        reply
            .data()
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["kind"].as_str().unwrap().to_owned(),
                    entry["reason"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }
}
