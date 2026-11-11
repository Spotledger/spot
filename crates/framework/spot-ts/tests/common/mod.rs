//! Shared test fixtures: a bridge that records what scripts did.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::{Value, json};
use spot_ts::{HostBridge, HostError, HostFn, HostSurface, Shape};

/// Records every host call; answers `get_doc` for known ids, `fail` with an error, `boom` with a
/// panic.
pub struct Recorder {
    pub calls: Mutex<Vec<(String, Value)>>,
    pub now: i64,
    pub seed: u64,
    pub count: AtomicU32,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            now: 1_790_000_000_000,
            seed: 42,
            count: AtomicU32::new(0),
        }
    }

    pub fn calls(&self) -> Vec<(String, Value)> {
        self.calls.lock().unwrap().clone()
    }
}

impl HostBridge for Recorder {
    fn call(&self, name: &str, args: Value) -> Result<Value, HostError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.calls
            .lock()
            .unwrap()
            .push((name.to_owned(), args.clone()));
        match name {
            "get_doc" => match args["id"].as_str() {
                Some("c-1") => Ok(json!({"name": "Acme", "credit": 100})),
                _ => Err(HostError::new("not_found", "no such document")),
            },
            "echo" => Ok(args),
            "boom" => panic!("the host function panicked on purpose"),
            "ping" => Ok(Value::Null),
            other => Err(HostError::new("unknown", other)),
        }
    }

    fn now_millis(&self) -> i64 {
        self.now
    }

    fn random_seed(&self) -> u64 {
        self.seed
    }
}

pub fn surface() -> HostSurface {
    HostSurface::new(vec![
        HostFn::new(
            "get_doc",
            Shape::Object {
                required: vec![("doctype".into(), Shape::Text), ("id".into(), Shape::Text)],
                open: false,
            },
        ),
        HostFn::new("echo", Shape::Any),
        HostFn::new("boom", Shape::Any),
        HostFn::new("ping", Shape::Any),
    ])
}
