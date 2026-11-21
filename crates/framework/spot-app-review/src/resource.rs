//! The resource profile: the component run under the real WASM host with per-call limits.
//!
//! `spot-wasm` does not expose per-call fuel or memory use, so this stage records only what
//! it can observe itself: the outcome of each call, the output size, the host calls its own
//! `Imports` wrapper counted, and the wall time between two [`Stopwatch`] marks. A limit hit
//! or a trap is a `High` finding; a result above the configured share of the output limit is
//! `Medium`.
//!
//! # Which calls are made
//!
//! An author may ship a profile at `assets/profile.toml` (it must also be a declared
//! `[[assets]]` entry), naming the calls to make:
//!
//! ```toml
//! [[call]]
//! entry = "on-event"
//! args = ["{}"]
//! ```
//!
//! When there is no profile — or it does not parse — the review makes one call to the
//! configured default entry ([`ReviewConfig::default_entry`], `on-event`, the test world's
//! health/init entry) with [`ReviewConfig::default_event`] as its argument. A malformed
//! profile is a `Medium` finding and the default entry is profiled instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use serde::Deserialize;
use spot_bundle::Bundle;
use spot_manifest::AppManifest;
use spot_wasm::{Imports, LogLevel, WasmHost};

use crate::config::ReviewConfig;
use crate::finding::{Finding, Severity};
use crate::report::{CallOutcome, CallRecord};
use crate::rules;
use crate::stopwatch::Stopwatch;

/// Profiles the component: instantiates it and calls every entry the author's `profile.toml`
/// names, or the configured default entry when there is none.
///
/// Returns the per-call table and the findings. The host kills a call that hits a limit; the
/// process is unaffected and the next call (or the next review) runs normally.
pub fn resource_profile(
    bundle: &Bundle,
    config: &ReviewConfig,
    stopwatch: &dyn Stopwatch,
) -> (Vec<CallRecord>, Vec<Finding>) {
    let host = match WasmHost::new(config.host.to_host_config()) {
        Ok(host) => host,
        Err(err) => {
            return (
                Vec::new(),
                vec![instantiate_failed(format!("the host engine failed: {err}"))],
            );
        }
    };
    let component = match host.compile(bundle.wasm()) {
        Ok(component) => component,
        Err(err) => {
            return (
                Vec::new(),
                vec![instantiate_failed(format!(
                    "the component did not compile: {err}"
                ))],
            );
        }
    };
    let app = match host.instantiate(&component, granted_from_manifest(bundle.manifest())) {
        Ok(app) => app,
        Err(err) => {
            return (
                Vec::new(),
                vec![instantiate_failed(format!(
                    "the component did not link: {err}"
                ))],
            );
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            return (
                Vec::new(),
                vec![instantiate_failed(format!(
                    "the review could not build its runtime: {err}"
                ))],
            );
        }
    };
    let (calls, mut findings) = calls_for(bundle, config);
    let limits = config.limits.to_limits();
    let mut records = Vec::new();
    for spec in &calls {
        let imports: Arc<CountingImports> = Arc::new(CountingImports::default());
        let arguments = serde_json::Value::Array(spec.args.clone());
        let started = stopwatch.mark();
        let outcome = runtime.block_on(app.call(
            &spec.entry,
            &arguments,
            limits,
            Arc::clone(&imports) as Arc<dyn Imports>,
        ));
        let wall_micros = stopwatch.mark().saturating_sub(started);
        let host_calls = imports.count();
        match outcome {
            Ok(value) => {
                let output_bytes = value.to_string().len();
                records.push(CallRecord {
                    entry: spec.entry.clone(),
                    outcome: CallOutcome::Ok,
                    output_bytes,
                    host_calls,
                    wall_micros,
                });
                if over_output_share(
                    output_bytes,
                    limits.output_bytes,
                    config.output_share_percent,
                ) {
                    findings.push(Finding::new(
                        rules::RESOURCE_OUTPUT_SHARE,
                        Severity::Medium,
                        Some(spec.entry.clone()),
                        format!(
                            "the result is {output_bytes} bytes, above {}% of the output limit \
                             ({} bytes)",
                            config.output_share_percent, limits.output_bytes
                        ),
                        rules::RISK_RESOURCE,
                    ));
                }
            }
            Err(err) => {
                records.push(CallRecord {
                    entry: spec.entry.clone(),
                    outcome: CallOutcome::from_code(err.code()),
                    output_bytes: 0,
                    host_calls,
                    wall_micros,
                });
                if let Some((rule, severity)) = limit_rule(err.code()) {
                    findings.push(Finding::new(
                        rule,
                        severity,
                        Some(spec.entry.clone()),
                        err.to_string(),
                        rules::RISK_RESOURCE,
                    ));
                }
            }
        }
    }
    (records, findings)
}

/// The rule and severity for a `spot-wasm` failure code, or `None` when the call merely
/// returned the app's own error.
fn limit_rule(code: &str) -> Option<(&'static str, Severity)> {
    match code {
        "wasm.out_of_fuel" => Some((rules::RESOURCE_FUEL, Severity::High)),
        "wasm.memory" => Some((rules::RESOURCE_MEMORY, Severity::High)),
        "wasm.too_many_host_calls" => Some((rules::RESOURCE_HOST_CALLS, Severity::High)),
        "wasm.timeout" => Some((rules::RESOURCE_TIMEOUT, Severity::High)),
        "wasm.trap" => Some((rules::RESOURCE_TRAP, Severity::High)),
        "wasm.output_too_large" => Some((rules::RESOURCE_OUTPUT_LIMIT, Severity::High)),
        "wasm.capability_denied" => Some((rules::RESOURCE_CAPABILITY_DENIED, Severity::High)),
        "wasm.missing_export" => Some((rules::RESOURCE_ENTRY_MISSING, Severity::Medium)),
        "wasm.bad_arguments" => Some((rules::RESOURCE_BAD_ARGUMENTS, Severity::Medium)),
        // The app's own error value is not a resource problem.
        "wasm.app_error" => None,
        // `bad_component` or `internal`: the component or the host failed, not the app's code.
        _ => Some((rules::RESOURCE_INSTANTIATE_FAILED, Severity::High)),
    }
}

/// Whether `output` bytes are above `percent` of `limit`.
fn over_output_share(output: usize, limit: usize, percent: u32) -> bool {
    let output = output as u128;
    let limit = limit as u128;
    let percent = percent as u128;
    limit > 0 && output.saturating_mul(100) > limit.saturating_mul(percent)
}

/// A `High` finding that the profile could not run the component.
fn instantiate_failed(message: String) -> Finding {
    Finding::new(
        rules::RESOURCE_INSTANTIATE_FAILED,
        Severity::High,
        Some("manifest: app.wasm".to_owned()),
        message,
        rules::RISK_RESOURCE,
    )
}

/// The calls to make: the author's `profile.toml` when it parses, otherwise the configured
/// default entry. A malformed profile is a `Medium` finding and falls back to the default.
fn calls_for(bundle: &Bundle, config: &ReviewConfig) -> (Vec<CallSpec>, Vec<Finding>) {
    let default = CallSpec {
        entry: config.default_entry.clone(),
        args: vec![serde_json::Value::String(config.default_event.clone())],
    };
    let Some(bytes) = bundle.assets().get(&config.profile_asset) else {
        return (vec![default], Vec::new());
    };
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => {
            return (
                vec![default],
                vec![profile_invalid("the profile is not valid UTF-8")],
            );
        }
    };
    match toml::from_str::<ProfileSpec>(text) {
        Ok(spec) if !spec.call.is_empty() => (spec.call, Vec::new()),
        Ok(_) => (
            vec![default],
            vec![profile_invalid("the profile names no calls")],
        ),
        Err(err) => (
            vec![default],
            vec![profile_invalid(&format!(
                "the profile does not parse: {err}"
            ))],
        ),
    }
}

/// A `Medium` finding for an author profile the review could not use.
fn profile_invalid(why: &str) -> Finding {
    Finding::new(
        rules::RESOURCE_PROFILE_INVALID,
        Severity::Medium,
        None,
        format!("{why}; the review profiled the default entry instead"),
        rules::RISK_RESOURCE,
    )
}

/// The author's `profile.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileSpec {
    /// The calls to make.
    #[serde(default)]
    call: Vec<CallSpec>,
}

/// One call the author asked the profile to make.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CallSpec {
    /// The export to call.
    entry: String,
    /// The JSON arguments, one per parameter.
    #[serde(default)]
    args: Vec<serde_json::Value>,
}

/// The host side the profile supplies: it answers nothing and counts every host call.
#[derive(Debug, Default)]
struct CountingImports {
    count: AtomicU32,
}

impl CountingImports {
    /// How many host calls the wrapper saw.
    fn count(&self) -> u32 {
        self.count.load(Ordering::Relaxed)
    }
}

impl Imports for CountingImports {
    fn log(&self, _level: LogLevel, _message: &str) {
        self.count.fetch_add(1, Ordering::Relaxed);
    }

    fn now(&self) -> i64 {
        self.count.fetch_add(1, Ordering::Relaxed);
        0
    }

    fn get_doc(&self, _doctype: &str, _id: &str) -> Option<String> {
        self.count.fetch_add(1, Ordering::Relaxed);
        None
    }
}

/// The host's granted capabilities, from the manifest's declared capabilities exactly.
fn granted_from_manifest(manifest: &AppManifest) -> spot_wasm::GrantedCapabilities {
    let caps = &manifest.capabilities;
    spot_wasm::GrantedCapabilities {
        network: caps
            .network
            .iter()
            .map(|endpoint| spot_wasm::Endpoint {
                host: endpoint.host.clone(),
                port: endpoint.port,
            })
            .collect(),
        email: caps.email,
        files: match caps.files {
            spot_manifest::FileAccess::None => spot_wasm::FileAccess::None,
            spot_manifest::FileAccess::Read => spot_wasm::FileAccess::Read,
            spot_manifest::FileAccess::ReadWrite => spot_wasm::FileAccess::ReadWrite,
        },
        doctypes: spot_wasm::DocTypeAccess {
            read: caps.doctypes.read.clone(),
            write: caps.doctypes.write.clone(),
        },
    }
}

#[cfg(test)]
#[allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_output_share_is_strictly_above() {
        assert!(!over_output_share(50, 100, 50));
        assert!(over_output_share(51, 100, 50));
        assert!(!over_output_share(0, 100, 50));
        // A zero limit cannot be divided; it never reports a share finding.
        assert!(!over_output_share(0, 0, 50));
    }

    #[test]
    fn every_limit_code_maps_to_a_rule() {
        assert_eq!(
            limit_rule("wasm.out_of_fuel"),
            Some((rules::RESOURCE_FUEL, Severity::High))
        );
        assert_eq!(
            limit_rule("wasm.memory"),
            Some((rules::RESOURCE_MEMORY, Severity::High))
        );
        assert_eq!(
            limit_rule("wasm.too_many_host_calls"),
            Some((rules::RESOURCE_HOST_CALLS, Severity::High))
        );
        assert_eq!(
            limit_rule("wasm.timeout"),
            Some((rules::RESOURCE_TIMEOUT, Severity::High))
        );
        assert_eq!(
            limit_rule("wasm.trap"),
            Some((rules::RESOURCE_TRAP, Severity::High))
        );
        assert_eq!(
            limit_rule("wasm.output_too_large"),
            Some((rules::RESOURCE_OUTPUT_LIMIT, Severity::High))
        );
        assert_eq!(limit_rule("wasm.app_error"), None);
    }
}
