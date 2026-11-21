//! The `/api/v1/events/*` routes (contract 0.4.0, WP-INT-02.T4) over a fake [`EventStreamPort`]:
//! every status and error code the contract names for each operation, the shape of every
//! request the routes accept or refuse, and the stream's framing, heartbeat and idle limit.
//!
//! The fake decides nothing the real adapter decides; it answers what a test scripts and keeps
//! the arguments the routes passed, so the tests prove the routes' half of the bargain: what is
//! checked before the port, what is passed to it and how its answer becomes the response.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use http_body_util::BodyExt as _;
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::Ports;
use spot_http::api::ports::events::{
    CheckpointOut, ConsumerOut, ConsumerSpecIn, EventFeed, EventStreamPort, EventsResult, FeedItem,
    PartitionOut, PayloadModeIn, PullIn, PullOut, StartIn, StreamIn, StreamLimits,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes::{RouteState, events};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

// The tests live in `events/`, one file per theme; they are included here, not declared as
// modules, so they share this file's imports and the fixtures without any visibility changes.
include!("events/fixture.rs");
include!("events/consumers.rs");
include!("events/pull_ack.rs");
include!("events/stream.rs");
include!("events/mount.rs");
