//! The S3-compatible [`ObjectStore`] of the `cloud_backup` module (`WP-CTL-08.T2`): path-style
//! requests signed with AWS Signature Version 4, over an [`S3Transport`] port. The port is the
//! only place that touches a network, so the signing and the protocol are testable without one;
//! the production transport (an HTTP client) is supplied by the host binary.
//!
//! Objects are written create-only (`If-None-Match: *`), so a stored chunk is immutable on the
//! backend too. The clock is injected; the secret key is held privately and never appears in an
//! error, a `Debug` output or a log line.

use std::fmt;
use std::sync::Arc;

use spot_cloud_backup::crypto::{hex, hmac_sha256, sha256_hex};
use spot_cloud_backup::{CloudBackupError, ObjectInfo, ObjectStore, PutOutcome, StoreFuture};
use spot_core::Clock;

/// One request the store asks the transport to send.
#[derive(Clone, Debug)]
pub struct S3Request {
    /// `GET`, `PUT` or `DELETE`.
    pub method: &'static str,
    /// The path and query (`/bucket/key`, `/bucket?list-type=2&prefix=...`).
    pub path: String,
    /// Header name and value pairs, names lower case, including `authorization`.
    pub headers: Vec<(String, String)>,
    /// The body.
    pub body: Vec<u8>,
}

/// The answer.
#[derive(Clone, Debug)]
pub struct S3Response {
    /// The HTTP status.
    pub status: u16,
    /// The body.
    pub body: Vec<u8>,
}

/// Sends a signed request to the endpoint named by the store's host.
pub trait S3Transport: Send + Sync {
    /// Sends `request`.
    ///
    /// # Errors
    /// A short reason (never a credential) when the endpoint cannot be reached.
    fn send<'a>(
        &'a self,
        request: S3Request,
    ) -> spot_mgmt_api::ports::BoxFuture<'a, Result<S3Response, String>>;
}

/// Where the bucket is and who may use it.
#[derive(Clone)]
pub struct S3Config {
    /// The endpoint host (`s3.eu-1.example.com`), used as the `host` header.
    pub host: String,
    /// The bucket (path style).
    pub bucket: String,
    /// The signing region.
    pub region: String,
    /// The access key id.
    pub access_key_id: String,
    secret_access_key: String,
}

impl S3Config {
    /// A configuration.
    #[must_use]
    pub fn new(host: &str, bucket: &str, region: &str, access_key_id: &str, secret: &str) -> Self {
        Self {
            host: host.to_owned(),
            bucket: bucket.to_owned(),
            region: region.to_owned(),
            access_key_id: access_key_id.to_owned(),
            secret_access_key: secret.to_owned(),
        }
    }
}

impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("host", &self.host)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

/// An S3-compatible object store.
pub struct S3Store {
    transport: Arc<dyn S3Transport>,
    config: S3Config,
    clock: Arc<dyn Clock>,
}

fn fail(reason: &str) -> CloudBackupError {
    CloudBackupError::Store {
        reason: reason.to_owned(),
    }
}

/// Percent-encodes everything but the unreserved characters (and `/` when `keep_slash`).
fn encode(text: &str, keep_slash: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && byte == b'/')
        {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = text.find(&open)?.checked_add(open.len())?;
    let end = text.get(start..)?.find(&format!("</{name}>"))?;
    text.get(start..start.checked_add(end)?)
}

impl S3Store {
    /// A store over `transport`.
    #[must_use]
    pub fn new(transport: Arc<dyn S3Transport>, config: S3Config, clock: Arc<dyn Clock>) -> Self {
        Self {
            transport,
            config,
            clock,
        }
    }

    fn stamp(&self) -> (String, String) {
        let now = self.clock.now_utc();
        let date = format!(
            "{:04}{:02}{:02}",
            now.year(),
            u8::from(now.month()),
            now.day()
        );
        let amz = format!(
            "{date}T{:02}{:02}{:02}Z",
            now.hour(),
            now.minute(),
            now.second()
        );
        (date, amz)
    }

    /// Builds the signed request: `path` is the already encoded path, `query` the already
    /// encoded and sorted query.
    fn sign(
        &self,
        method: &'static str,
        path: &str,
        query: &str,
        extra: &[(&str, &str)],
        body: Vec<u8>,
    ) -> S3Request {
        let (date, amz) = self.stamp();
        let payload = sha256_hex(&body);
        let mut headers: Vec<(String, String)> = vec![
            ("host".to_owned(), self.config.host.clone()),
            ("x-amz-content-sha256".to_owned(), payload.clone()),
            ("x-amz-date".to_owned(), amz.clone()),
        ];
        for (name, value) in extra {
            headers.push(((*name).to_owned(), (*value).to_owned()));
        }
        headers.sort();
        let canonical_headers: String = headers.iter().map(|(n, v)| format!("{n}:{v}\n")).collect();
        let signed: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
        let signed = signed.join(";");
        let canonical =
            format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload}");
        let scope = format!("{date}/{}/s3/aws4_request", self.config.region);
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz}\n{scope}\n{}",
            sha256_hex(canonical.as_bytes())
        );
        let k_date = hmac_sha256(
            format!("AWS4{}", self.config.secret_access_key).as_bytes(),
            date.as_bytes(),
        );
        let k_region = hmac_sha256(&k_date, self.config.region.as_bytes());
        let k_service = hmac_sha256(&k_region, b"s3");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let signature = hex(&hmac_sha256(&k_signing, to_sign.as_bytes()));
        headers.push((
            "authorization".to_owned(),
            format!(
                "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
                self.config.access_key_id
            ),
        ));
        let target = if query.is_empty() {
            path.to_owned()
        } else {
            format!("{path}?{query}")
        };
        S3Request {
            method,
            path: target,
            headers,
            body,
        }
    }

    fn object_path(&self, key: &str) -> String {
        format!(
            "/{}/{}",
            encode(&self.config.bucket, false),
            encode(key, true)
        )
    }

    async fn call(&self, request: S3Request) -> Result<S3Response, CloudBackupError> {
        self.transport
            .send(request)
            .await
            .map_err(|_| fail("the object store could not be reached"))
    }
}

impl ObjectStore for S3Store {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, PutOutcome> {
        Box::pin(async move {
            let path = self.object_path(key);
            let response = self
                .call(self.sign("PUT", &path, "", &[("if-none-match", "*")], bytes))
                .await?;
            match response.status {
                200 | 201 => Ok(PutOutcome::Stored),
                409 | 412 => Ok(PutOutcome::Exists),
                _ => Err(fail("the object store refused the write")),
            }
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let path = self.object_path(key);
            let response = self
                .call(self.sign("GET", &path, "", &[], Vec::new()))
                .await?;
            if response.status == 200 {
                Ok(response.body)
            } else {
                Err(fail("the store holds no such object"))
            }
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        Box::pin(async move {
            let path = format!("/{}", encode(&self.config.bucket, false));
            let mut out = Vec::new();
            let mut token: Option<String> = None;
            loop {
                let mut query = String::new();
                if let Some(token) = &token {
                    query.push_str(&format!("continuation-token={}&", encode(token, false)));
                }
                query.push_str(&format!("list-type=2&prefix={}", encode(prefix, false)));
                let response = self
                    .call(self.sign("GET", &path, &query, &[], Vec::new()))
                    .await?;
                if response.status != 200 {
                    return Err(fail("the object store refused the listing"));
                }
                let text = String::from_utf8(response.body)
                    .map_err(|_| fail("the listing is not text"))?;
                let mut rest = text.as_str();
                while let Some(start) = rest.find("<Contents>") {
                    let tail = &rest[start..];
                    let end = tail.find("</Contents>").unwrap_or(tail.len());
                    let block = &tail[..end];
                    if let (Some(key), Some(size)) = (tag(block, "Key"), tag(block, "Size")) {
                        out.push(ObjectInfo {
                            key: key.to_owned(),
                            bytes: size
                                .parse()
                                .map_err(|_| fail("the listing has a bad size"))?,
                        });
                    }
                    rest = &tail[end..];
                }
                if tag(&text, "IsTruncated") == Some("true") {
                    token = tag(&text, "NextContinuationToken").map(str::to_owned);
                    if token.is_none() {
                        return Err(fail("the listing is truncated without a token"));
                    }
                } else {
                    return Ok(out);
                }
            }
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let path = self.object_path(key);
            let response = self
                .call(self.sign("DELETE", &path, "", &[], Vec::new()))
                .await?;
            if matches!(response.status, 200 | 204 | 404) {
                Ok(())
            } else {
                Err(fail("the object store refused the delete"))
            }
        })
    }
}
