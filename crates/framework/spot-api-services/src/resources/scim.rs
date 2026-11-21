//! The engine-side implementation of the SCIM port (WP-IAM-06.T2): what stands between the
//! `spot-http` route group for `/scim/v2` and the hosting module that holds the engine, the
//! directory and the SCIM core.
//!
//! `spot-http` defines [`ScimPort`]. This file implements it over [`ScimOps`], the one operation
//! the hosting module (`spot-cli`) performs, so this crate needs no SCIM dependency. What it
//! decides here is only what never needs the directory:
//!
//! - **method.** Only the methods SCIM uses (`GET`, `POST`, `PUT`, `PATCH`, `DELETE`) reach the
//!   operation; anything else is answered `405` in RFC 7644 form.
//! - **path.** Only `/scim/v2` and the paths below it reach the operation; the operation never
//!   sees `/scim/v2x` or a path with a dot segment or an empty segment.
//! - **size.** A body over the bound is answered `413` (the route checks it first; the port is a
//!   trait and may be called by something else).
//!
//! Nothing here stores, caches or formats a body, a header or a credential.

use std::sync::Arc;

use spot_http::api::ports::scim::{ScimCall, ScimPort, ScimReply};
use spot_http::api::service::{BoxFuture, RequestCtx};

/// The base path of the SCIM server.
pub const BASE: &str = "/scim/v2";

/// The one operation the hosting module performs: authenticate the SCIM key of `ctx` and serve
/// the call as that credential.
pub trait ScimOps: Send + Sync {
    /// Serves one validated call. Every failure is a [`ScimReply`]; there is no other error.
    fn serve<'a>(&'a self, ctx: &'a RequestCtx, call: ScimCall) -> BoxFuture<'a, ScimReply>;
}

/// The SCIM port over [`ScimOps`].
pub struct EngineScim {
    ops: Arc<dyn ScimOps>,
    max_body_bytes: usize,
}

impl EngineScim {
    /// A port that refuses bodies over `max_body_bytes` and serves the rest through `ops`.
    pub fn new(ops: Arc<dyn ScimOps>, max_body_bytes: usize) -> Self {
        Self {
            ops,
            max_body_bytes,
        }
    }
}

/// Whether `path` is the base or a clean path below it: `/scim/v2` or `/scim/v2/<segments>`
/// with no empty segment and no `.` or `..` segment.
pub fn path_ok(path: &str) -> bool {
    let Some(rest) = path.strip_prefix(BASE) else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    let Some(rest) = rest.strip_prefix('/') else {
        return false;
    };
    // `/scim/v2/` (a trailing slash alone) is the base.
    rest.is_empty()
        || rest
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Whether `method` is one SCIM uses.
pub fn method_ok(method: &str) -> bool {
    matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE")
}

impl ScimPort for EngineScim {
    fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    fn serve<'a>(&'a self, ctx: &'a RequestCtx, call: ScimCall) -> BoxFuture<'a, ScimReply> {
        Box::pin(async move {
            if !method_ok(&call.method) {
                return ScimReply::error(405, None, "This method is not supported.");
            }
            if !path_ok(&call.path) {
                return ScimReply::error(404, None, "No such resource.");
            }
            if call.body.len() > self.max_body_bytes {
                return ScimReply::error(
                    413,
                    Some("tooLarge"),
                    "The request body is larger than this endpoint accepts.",
                );
            }
            self.ops.serve(ctx, call).await
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting(AtomicUsize);

    impl ScimOps for Counting {
        fn serve<'a>(&'a self, _ctx: &'a RequestCtx, _call: ScimCall) -> BoxFuture<'a, ScimReply> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                ScimReply {
                    status: 200,
                    body: "{}".to_owned(),
                    etag: None,
                    location: None,
                    retry_after: None,
                }
            })
        }
    }

    fn call(method: &str, path: &str, body: usize) -> ScimCall {
        ScimCall {
            method: method.to_owned(),
            path: path.to_owned(),
            query: BTreeMap::new(),
            if_match: None,
            body: vec![b' '; body],
        }
    }

    fn ctx() -> RequestCtx {
        RequestCtx {
            site: spot_core::SiteId::new("acme").unwrap(),
            credential: None,
            idempotency_key: None,
            language: None,
            request_id: spot_http::request::RequestId::generate(),
            peer: None,
        }
    }

    #[test]
    fn paths_are_the_base_or_clean_paths_below_it() {
        for ok in [
            "/scim/v2",
            "/scim/v2/",
            "/scim/v2/Users",
            "/scim/v2/Users/abc",
        ] {
            assert!(path_ok(ok), "{ok}");
        }
        for bad in [
            "/scim/v2x",
            "/scim",
            "/api/v1/scim/v2",
            "/scim/v2//Users",
            "/scim/v2/./Users",
            "/scim/v2/../api",
            "scim/v2",
            "",
        ] {
            assert!(!path_ok(bad), "{bad}");
        }
    }

    #[tokio::test]
    async fn only_clean_calls_reach_the_operation() {
        let ops = Arc::new(Counting(AtomicUsize::new(0)));
        let port = EngineScim::new(ops.clone(), 16);
        let ctx = ctx();
        assert_eq!(
            port.serve(&ctx, call("GET", "/scim/v2/Users", 0))
                .await
                .status,
            200
        );
        assert_eq!(
            port.serve(&ctx, call("OPTIONS", "/scim/v2/Users", 0))
                .await
                .status,
            405
        );
        assert_eq!(
            port.serve(&ctx, call("GET", "/scim/v2/../x", 0))
                .await
                .status,
            404
        );
        assert_eq!(
            port.serve(&ctx, call("POST", "/scim/v2/Users", 17))
                .await
                .status,
            413
        );
        assert_eq!(ops.0.load(Ordering::SeqCst), 1);
    }
}
