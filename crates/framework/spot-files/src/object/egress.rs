//! The object stores' HTTP path goes through `spot-egress` (WP-SEC-09.T12).
//!
//! `object_store` builds its own `reqwest` client unless it is handed an
//! [`HttpConnector`]. [`EgressHttp`] is that connector: every request an S3 or Azure store
//! makes is authorized for the connector's tenant and purpose, the destination is pinned to the
//! vetted addresses, redirects are re-authorized hop by hop, and a refusal surfaces as the
//! catalogued `egress.*` code (never a bare connection error). Hosts build the connector from
//! the site's shared guard and a [`Sender`] (production: [`spot_egress::HttpSender`]; tests: a
//! fake) and pass it to `ObjectFileStore::new_s3` / `new_azure`, which no longer have a
//! constructor that builds a client of their own.
//!
//! # Known limit
//!
//! `spot_egress::Method` has no `HEAD`, so a `HEAD` request fails here with an `egress.*`
//! message instead of leaving the process by a path the policy does not see. See the report of
//! WP-SEC-09.T12 (open question 1): adding `Method::Head` to the egress sender is a one-line
//! change in [`wire_method`].

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use http_body_util::{BodyExt, Full};
use object_store::ClientOptions;
use object_store::client::{
    HttpClient, HttpConnector, HttpError, HttpErrorKind, HttpRequest, HttpResponse,
    HttpResponseBody, HttpService,
};
use spot_egress::{EgressConnector, EgressError, HttpRequest as EgressRequest, Method, Sender};
use url::Url;

/// The policy-checked HTTP path of an object store.
#[derive(Clone)]
pub struct EgressHttp {
    connector: EgressConnector,
    sender: Arc<dyn Sender>,
    max_redirects: u32,
}

impl EgressHttp {
    /// A path that authorizes through `connector` (its tenant and purpose) and sends through
    /// `sender`. Redirects are not followed (an object store answers with the final
    /// location itself); see [`EgressHttp::with_max_redirects`].
    pub fn new(connector: EgressConnector, sender: Arc<dyn Sender>) -> Self {
        Self {
            connector,
            sender,
            max_redirects: 0,
        }
    }

    /// Follow at most `hops` redirects, each re-authorized by the policy.
    #[must_use]
    pub fn with_max_redirects(mut self, hops: u32) -> Self {
        self.max_redirects = hops;
        self
    }
}

impl fmt::Debug for EgressHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EgressHttp")
            .field("tenant", &self.connector.tenant())
            .field("max_redirects", &self.max_redirects)
            .finish_non_exhaustive()
    }
}

impl HttpConnector for EgressHttp {
    fn connect(&self, _options: &ClientOptions) -> object_store::Result<HttpClient> {
        // The client options (timeouts, proxy, TLS roots) belong to the egress sender and the
        // tenant's policy, not to the store.
        Ok(HttpClient::new(EgressService { http: self.clone() }))
    }
}

#[derive(Debug)]
struct EgressService {
    http: EgressHttp,
}

/// An egress refusal or failure as the error source of an [`HttpError`]: the catalogue code,
/// then the short text. Never a host, address or credential.
#[derive(Debug)]
struct EgressFailure {
    code: &'static str,
    text: String,
}

impl fmt::Display for EgressFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.text)
    }
}

impl std::error::Error for EgressFailure {}

fn failure(kind: HttpErrorKind, code: &'static str, text: impl Into<String>) -> HttpError {
    HttpError::new(
        kind,
        EgressFailure {
            code,
            text: text.into(),
        },
    )
}

/// An egress error as an [`HttpError`]. A refusal is never retried (the store's retry layer
/// retries `Connect` and `Request`); a failure to reach the destination is.
fn egress_error(e: &EgressError) -> HttpError {
    let kind = match e {
        EgressError::ResolutionFailed => HttpErrorKind::Connect,
        _ => HttpErrorKind::Unknown,
    };
    failure(kind, e.code(), e.to_string())
}

/// The sender's method for an HTTP method, when it has one.
fn wire_method(method: &http::Method) -> Result<Method, HttpError> {
    match *method {
        http::Method::GET => Ok(Method::Get),
        http::Method::PUT => Ok(Method::Put),
        http::Method::POST => Ok(Method::Post),
        http::Method::DELETE => Ok(Method::Delete),
        http::Method::PATCH => Ok(Method::Patch),
        _ => Err(failure(
            HttpErrorKind::Unknown,
            "egress.method_unsupported",
            format!("the egress sender cannot send {method}"),
        )),
    }
}

#[async_trait]
impl HttpService for EgressService {
    async fn call(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let (parts, body) = req.into_parts();
        let method = wire_method(&parts.method)?;
        let url = Url::parse(&parts.uri.to_string()).map_err(|_| {
            failure(
                HttpErrorKind::Unknown,
                EgressError::InvalidUrl.code(),
                EgressError::InvalidUrl.to_string(),
            )
        })?;
        let bytes = body.collect().await?.to_bytes();
        let body = if bytes.is_empty() { None } else { Some(bytes) };
        let request = EgressRequest::new(method, url, parts.headers, body);
        let response = self
            .http
            .connector
            .follow_request_redirects(self.http.sender.as_ref(), request, self.http.max_redirects)
            .await
            .map_err(|e| egress_error(&e))?;

        let body = HttpResponseBody::new(
            Full::new(spot_egress::Bytes::from(response.body)).map_err(|never| match never {}),
        );
        let mut out = http::Response::new(body);
        *out.status_mut() = response.status;
        *out.headers_mut() = response.headers;
        Ok(out)
    }
}
