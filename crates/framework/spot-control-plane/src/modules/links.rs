//! The `links` module: the directory that lets one tenant's host find another tenant's host for
//! the link handshake (WP-CTL-09.T8, ADR-058), and the registry view of which links exist.
//!
//! What the control plane knows and relays:
//!
//! - **Directory.** A host publishes its link endpoint under an opaque *handle* it chose
//!   (`PUT /cp/v1/host/links/endpoint`). Whoever holds the handle can ask where to reach that host
//!   (`GET /cp/v1/host/links/directory/{handle}`). A tenant name is returned **only** when its owner
//!   opted in (`share_name`); the default is no name, so the directory leaks nothing before consent.
//!   The tenant behind a publication always comes from the certificate, never from the body.
//! - **Registry.** `spot_control.link_registry` (written by the tenants' link stores) records that a
//!   link exists, between whom, and whether it is live; nothing else. Operators read it scoped to
//!   their tenants (`GET /cp/v1/links`); a host reads only its own links' ids and states.
//!
//! The control plane relays nothing else: no handshake message, grant, key or link data passes
//! through it. These routes are not in `contracts/control-plane` 0.1.0 (a lead contract item).

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Extension, Json, Router};
use deadpool_postgres::Pool;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::{HostIdentity, OperatorCall, RequestId};
use crate::{CpCode, CpCtx, CpError};

/// Registration order.
pub const ORDER: u32 = 900;

/// The module's DDL: the directory, and the registry the tenants' link stores write (the same
/// statements as `spot-tenant-link-pg`'s `REGISTRY_DDL`, so either side may create it first).
const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_link_directory (\
        tenant text PRIMARY KEY, \
        handle text NOT NULL UNIQUE, \
        endpoint text NOT NULL, \
        share_name boolean NOT NULL DEFAULT false, \
        name text, \
        updated_at bigint NOT NULL)",
    "CREATE SCHEMA IF NOT EXISTS spot_control",
    "CREATE TABLE IF NOT EXISTS spot_control.link_registry (\
        tenant_a text NOT NULL, \
        tenant_b text NOT NULL, \
        link_id uuid PRIMARY KEY, \
        state text NOT NULL CHECK (state IN ('live', 'ended')), \
        CHECK (tenant_a <> tenant_b))",
];

struct LinksState {
    pool: Pool,
    clock: Arc<dyn spot_core::Clock>,
}

/// Registers the routes, readiness check and DDL. Without a database pool the module registers
/// nothing but a failing readiness check.
///
/// # Errors
/// Never fails today.
pub fn register(ctx: &mut CpCtx) -> Result<(), CpError> {
    ctx.add_migration(DDL);
    let Some(pool) = ctx.pool.clone() else {
        ctx.add_readiness(Arc::new(|| {
            Box::pin(async { false }) as spot_mgmt_api::ports::BoxFuture<'static, bool>
        }));
        return Ok(());
    };
    let state = Arc::new(LinksState {
        pool: pool.clone(),
        clock: Arc::clone(&ctx.clock),
    });
    ctx.host_routes(
        Router::new()
            .route("/cp/v1/host/links/endpoint", put(publish))
            .route("/cp/v1/host/links/directory/{handle}", get(lookup))
            .route("/cp/v1/host/links/registry", get(own_links))
            .with_state(Arc::clone(&state)),
    );
    ctx.operator_routes(
        Router::new()
            .route("/cp/v1/links", get(list_links))
            .with_state(state),
    );
    ctx.add_readiness(Arc::new(move || {
        let pool = pool.clone();
        Box::pin(async move { pool.get().await.is_ok() })
            as spot_mgmt_api::ports::BoxFuture<'static, bool>
    }));
    Ok(())
}

fn fail(error: CpError, request: &RequestId) -> Response {
    error.into_response_with(&request.0)
}

fn internal(request: &RequestId) -> Response {
    fail(
        CpError::with_reason(CpCode::Internal, "cp.links_store"),
        request,
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Publish {
    handle: String,
    endpoint: String,
    #[serde(default)]
    share_name: bool,
    name: Option<String>,
}

fn valid_handle(handle: &str) -> bool {
    (16..=64).contains(&handle.len())
        && handle
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

fn valid_endpoint(endpoint: &str) -> bool {
    endpoint.len() <= 300
        && endpoint.starts_with("https://")
        && endpoint.len() > "https://".len()
        && endpoint.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

fn tenant_party(tenant: &str) -> String {
    format!("tenant:{tenant}")
}

async fn publish(
    State(state): State<Arc<LinksState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
    body: Bytes,
) -> Response {
    let Ok(input) = serde_json::from_slice::<Publish>(&body) else {
        return fail(CpError::invalid(&["body"]), &request);
    };
    let mut bad = Vec::new();
    if !valid_handle(&input.handle) {
        bad.push("handle");
    }
    if !valid_endpoint(&input.endpoint) {
        bad.push("endpoint");
    }
    if input
        .name
        .as_ref()
        .is_some_and(|name| name.is_empty() || name.chars().count() > 100)
    {
        bad.push("name");
    }
    if input.share_name && input.name.is_none() {
        bad.push("name");
    }
    if !bad.is_empty() {
        return fail(CpError::invalid(&bad), &request);
    }
    // The name is stored only when its owner opted in to sharing it.
    let name = if input.share_name {
        input.name.clone()
    } else {
        None
    };
    let now = state.clock.now_utc().unix_timestamp();
    let Ok(client) = state.pool.get().await else {
        return internal(&request);
    };
    let result = client
        .execute(
            "INSERT INTO cp_link_directory (tenant, handle, endpoint, share_name, name, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (tenant) DO UPDATE SET handle = EXCLUDED.handle, endpoint = EXCLUDED.endpoint, \
                share_name = EXCLUDED.share_name, name = EXCLUDED.name, updated_at = EXCLUDED.updated_at",
            &[&host.tenant, &input.handle, &input.endpoint, &input.share_name, &name, &now],
        )
        .await;
    match result {
        Ok(_) => Json(json!({ "data": { "published": true } })).into_response(),
        // A handle already held by another tenant violates the unique index: the answer does not
        // say whose it is.
        Err(error) if error.code() == Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => {
            fail(CpError::invalid(&["handle"]), &request)
        }
        Err(_) => internal(&request),
    }
}

async fn lookup(
    State(state): State<Arc<LinksState>>,
    Extension(request): Extension<RequestId>,
    Path(handle): Path<String>,
) -> Response {
    if !valid_handle(&handle) {
        return fail(CpError::new(CpCode::NotFound), &request);
    }
    let Ok(client) = state.pool.get().await else {
        return internal(&request);
    };
    let row = client
        .query_opt(
            "SELECT endpoint, share_name, name FROM cp_link_directory WHERE handle = $1",
            &[&handle],
        )
        .await;
    match row {
        Ok(Some(row)) => {
            let endpoint: String = row.get(0);
            let share_name: bool = row.get(1);
            let name: Option<String> = row.get(2);
            let mut data = json!({ "handle": handle, "endpoint": endpoint });
            if let (true, Some(name), Some(map)) = (share_name, name, data.as_object_mut()) {
                map.insert("name".to_owned(), json!(name));
            }
            Json(json!({ "data": data })).into_response()
        }
        Ok(None) => fail(CpError::new(CpCode::NotFound), &request),
        Err(_) => internal(&request),
    }
}

fn link_json(row: &tokio_postgres::Row, with_parties: bool) -> Value {
    let id: String = row.get(0);
    let state: String = row.get(1);
    if with_parties {
        let a: String = row.get(2);
        let b: String = row.get(3);
        json!({ "link_id": id, "state": state, "parties": [a, b] })
    } else {
        json!({ "link_id": id, "state": state })
    }
}

async fn own_links(
    State(state): State<Arc<LinksState>>,
    Extension(host): Extension<HostIdentity>,
    Extension(request): Extension<RequestId>,
) -> Response {
    let Ok(client) = state.pool.get().await else {
        return internal(&request);
    };
    let party = tenant_party(&host.tenant);
    let rows = client
        .query(
            "SELECT link_id::text, state FROM spot_control.link_registry \
             WHERE tenant_a = $1 OR tenant_b = $1 ORDER BY link_id LIMIT 1000",
            &[&party],
        )
        .await;
    match rows {
        Ok(rows) => {
            let data: Vec<Value> = rows.iter().map(|row| link_json(row, false)).collect();
            Json(json!({ "data": data })).into_response()
        }
        Err(_) => internal(&request),
    }
}

async fn list_links(
    State(state): State<Arc<LinksState>>,
    Extension(call): Extension<OperatorCall>,
) -> Response {
    let request = RequestId(call.request_id.clone());
    let Ok(client) = state.pool.get().await else {
        return internal(&request);
    };
    let rows = client
        .query(
            "SELECT link_id::text, state, tenant_a, tenant_b FROM spot_control.link_registry \
             ORDER BY link_id LIMIT 5000",
            &[],
        )
        .await;
    match rows {
        Ok(rows) => {
            // A link is visible when either party is in the operator's scope.
            let data: Vec<Value> = rows
                .iter()
                .filter(|row| {
                    let a: String = row.get(2);
                    let b: String = row.get(3);
                    [a, b].iter().any(|party| {
                        party
                            .strip_prefix("tenant:")
                            .is_some_and(|tenant| call.scope.allows(tenant))
                    })
                })
                .map(|row| link_json(row, true))
                .collect();
            Json(json!({ "data": data })).into_response()
        }
        Err(_) => internal(&request),
    }
}
