//! `run_script`: an app's TypeScript, transpiled and run on the real script engine.
//!
//! A file of the app (`ts/pricing.ts`) is read from the app directory, transpiled with the same
//! transpiler the server uses (no imports are allowed: the engine has no module loader), compiled
//! and run for one exported function. The script sees no host functions: every `host.*` call it
//! makes is refused with a `HostError` the script may catch, so a test of pure logic runs as it
//! would, and one that needs the document store is the `l3-http` suite's job. Time is the
//! harness clock and the random seed is zero, so a run is the same every time.
//!
//! A failure is a [`ScriptFailure`] carrying the engine's stable `script.*` code (or the
//! transpiler's `ts.*` code) and a message with positions only: never source text or arguments.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value as Json;
use spot_ts::{HostBridge, HostError, Limits, ScriptEngine, ScriptId};
use spot_ts_transpile::Options;

/// Why a script did not return a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptFailure {
    /// The stable code: `ts.*` from the transpiler, `script.*` from the engine, or
    /// `app_testkit.app_invalid` for a path that is not a script of the app.
    pub code: String,
    /// A sentence naming positions, never source text or arguments.
    pub message: String,
}

impl ScriptFailure {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ScriptFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ScriptFailure {}

/// The host calls a test script gets: none. Time and the seed are the harness's.
pub(crate) struct TestBridge {
    pub(crate) now_millis: i64,
}

impl HostBridge for TestBridge {
    fn call(&self, name: &str, _args: Json) -> Result<Json, HostError> {
        Err(HostError::new(
            "unsupported",
            &format!("`{name}` is not available to a script under the app test kit"),
        ))
    }

    fn now_millis(&self) -> i64 {
        self.now_millis
    }

    fn random_seed(&self) -> u64 {
        0
    }
}

/// Whether `path` is a relative `.ts` path of plain names, with no parent step, root, drive or
/// hidden part.
pub(crate) fn safe_script_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 200
        && path.ends_with(".ts")
        && !path.contains('\\')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && !part.starts_with('.'))
}

/// Reads `path` under `dir`, never following a symlink and never beyond 1 MiB. `None` for a
/// missing, linked, oversized or unreadable file.
pub(crate) fn read_inside(dir: &Path, path: &str) -> Option<String> {
    let mut file = dir.to_path_buf();
    for part in path.split('/') {
        file.push(part);
        let meta = std::fs::symlink_metadata(&file).ok()?;
        if meta.file_type().is_symlink() {
            return None;
        }
    }
    let meta = std::fs::metadata(&file).ok()?;
    if !meta.is_file() || meta.len() > 1024 * 1024 {
        return None;
    }
    std::fs::read_to_string(&file).ok()
}

fn read(dir: &Path, path: &str) -> Result<String, ScriptFailure> {
    read_inside(dir, path)
        .ok_or_else(|| ScriptFailure::new("app_testkit.app_invalid", "the app has no such script"))
}

/// Transpiles, compiles and runs `entry` of the script `path` of the app at `dir`.
pub(crate) async fn run(
    engine: &ScriptEngine,
    app: &str,
    dir: &Path,
    path: &str,
    entry: &str,
    args: &Json,
    now_millis: i64,
) -> Result<Json, ScriptFailure> {
    if !safe_script_path(path) {
        return Err(ScriptFailure::new(
            "app_testkit.app_invalid",
            "a script path is a relative `.ts` path of plain names",
        ));
    }
    let source = read(dir, path)?;
    let options = Options {
        allow_imports: Vec::new(),
        ..Options::default()
    };
    let transpiled = spot_ts_transpile::transpile(&source, path, &options).map_err(|list| {
        let first = list.first();
        ScriptFailure::new(
            first.map_or("ts.syntax", |d| d.code.as_str()),
            list.iter()
                .map(|d| format!("{}:{}:{}: {}", path, d.line, d.column, d.message))
                .collect::<Vec<_>>()
                .join("; "),
        )
    })?;
    let compiled = engine
        .compile(
            &transpiled.js,
            ScriptId::new(app, path, 0),
            Some(&transpiled.source_map),
        )
        .map_err(|e| ScriptFailure::new(e.code(), format!("{path} does not compile")))?;
    let bridge: Arc<dyn HostBridge> = Arc::new(TestBridge { now_millis });
    engine
        .run(&compiled, entry, args, bridge, Limits::default())
        .await
        .map_err(|e| ScriptFailure::new(e.code(), format!("`{entry}` of {path} failed")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn script_paths_are_plain_relative_ts_files() {
        assert!(safe_script_path("ts/pricing.ts"));
        for bad in [
            "",
            "../x.ts",
            "/x.ts",
            "ts//x.ts",
            "ts/.x.ts",
            "ts\\x.ts",
            "ts/x.js",
            "ts/./x.ts",
        ] {
            assert!(!safe_script_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_bridge_refuses_every_host_call_and_is_deterministic() {
        let bridge = TestBridge { now_millis: 5 };
        assert!(bridge.call("host.get", Json::Null).is_err());
        assert_eq!(bridge.now_millis(), 5);
        assert_eq!(bridge.random_seed(), 0);
    }
}
