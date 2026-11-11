//! The WASM boundary view: what one guest call cost, for an app developer.
//!
//! Native step-through of guest code is deliberately out of scope (see [`DWARF_NOTE`]): what an
//! app developer needs first is where the boundary cost is — which export ran, how much fuel it
//! burnt, how much memory it touched, and which host imports it called. [`WasmDebugInfo`] is that
//! view; [`WasmDebugInfo::record`] puts it on the unified trace as one [`Layer::Wasm`] span.
//!
//! `spot-wasm` produces the numbers (`AppInstance::call`, fuel metering and the host-call
//! counter); this crate owns the presentation. The host fills a [`WasmCallReport`] — it does not
//! need to depend on the debugger to do so, and `spot-devtools` does not depend on `wasmtime`.

use std::fmt::Write as _;
use std::time::Duration;

use crate::error::{DevToolsError, Result};
use crate::trace::collector::{ActiveSpan, TraceCollector, TraceContext};
use crate::trace::render::format_duration;
use crate::trace::span::{Layer, micros};

/// Why native step-through of guest code is not here, and where it would go.
pub const DWARF_NOTE: &str = "Native step-through of WebAssembly guest code is out of scope for \
WP-DX-03: it needs a DWARF-aware debugger over the wasmtime JIT (the `wasmtime` crate's \
debugging support plus a DAP backend that maps guest addresses to source), not the host-boundary \
view this crate provides. The boundary view answers what a guest call costs; the DWARF route \
answers what a guest instruction did.";

/// One host import a guest called, with its share of the boundary cost.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostCall {
    /// The import's name (`log`, `get-doc`, ...).
    pub name: String,
    /// How many times the guest called it.
    pub count: u64,
    /// Fuel the import's host side consumed, as the host measured it.
    pub fuel: u64,
}

impl HostCall {
    /// A host call record.
    #[must_use]
    pub fn new(name: &str, count: u64, fuel: u64) -> Self {
        Self {
            name: name.to_owned(),
            count,
            fuel,
        }
    }
}

/// What the WASM host measured for one guest call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WasmCallReport {
    /// The component's content hash (the host's identity for "this app").
    pub component_hash: String,
    /// The export that was called.
    pub export: String,
    /// Fuel the engine reports consumed.
    pub fuel_used: u64,
    /// The linear memory's size after the call, in bytes.
    pub memory_bytes: u64,
    /// The host imports the guest called.
    pub host_calls: Vec<HostCall>,
    /// The call's wall time, measured by the host's clock.
    pub wall_time: Duration,
}

/// The boundary view of one WASM call: component, export, fuel, memory, host calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WasmDebugInfo {
    component_hash: String,
    export: String,
    fuel_used: u64,
    memory_bytes: u64,
    host_calls: Vec<HostCall>,
    wall_time: Duration,
}

impl WasmDebugInfo {
    /// The view of one call.
    ///
    /// # Errors
    ///
    /// [`DevToolsError::Wasm`] when the component hash or the export is empty, or a host-call
    /// name is empty: a view that cannot name what it describes is not useful.
    pub fn new(report: WasmCallReport) -> Result<Self> {
        if report.component_hash.trim().is_empty() {
            return Err(DevToolsError::Wasm {
                message: "the component hash is empty".into(),
            });
        }
        if report.export.trim().is_empty() {
            return Err(DevToolsError::Wasm {
                message: "the export name is empty".into(),
            });
        }
        if let Some(call) = report.host_calls.iter().find(|c| c.name.trim().is_empty()) {
            return Err(DevToolsError::Wasm {
                message: format!("a host call has no name (count {})", call.count),
            });
        }
        Ok(Self {
            component_hash: report.component_hash,
            export: report.export,
            fuel_used: report.fuel_used,
            memory_bytes: report.memory_bytes,
            host_calls: report.host_calls,
            wall_time: report.wall_time,
        })
    }

    /// The component's content hash (shortened to 12 hex digits for display).
    #[must_use]
    pub fn component_hash(&self) -> &str {
        &self.component_hash
    }

    /// The export that ran.
    #[must_use]
    pub fn export(&self) -> &str {
        &self.export
    }

    /// Fuel consumed.
    #[must_use]
    pub fn fuel_used(&self) -> u64 {
        self.fuel_used
    }

    /// Linear memory after the call, in bytes.
    #[must_use]
    pub fn memory_bytes(&self) -> u64 {
        self.memory_bytes
    }

    /// The host calls the guest made.
    #[must_use]
    pub fn host_calls(&self) -> &[HostCall] {
        &self.host_calls
    }

    /// The call's wall time.
    #[must_use]
    pub fn wall_time(&self) -> Duration {
        self.wall_time
    }

    /// The host calls' total fuel.
    #[must_use]
    pub fn host_fuel(&self) -> u64 {
        self.host_calls.iter().map(|call| call.fuel).sum()
    }

    /// The span attributes this view contributes. No attribute carries guest memory or data, only
    /// identities and costs.
    #[must_use]
    pub fn attributes(&self) -> Vec<(String, String)> {
        let mut attributes = vec![
            ("component".to_owned(), short_hash(&self.component_hash)),
            ("export".to_owned(), self.export.clone()),
            ("fuel_used".to_owned(), self.fuel_used.to_string()),
            ("memory_bytes".to_owned(), self.memory_bytes.to_string()),
            ("host_calls".to_owned(), self.host_calls.len().to_string()),
            ("host_fuel".to_owned(), self.host_fuel().to_string()),
            ("wall_micros".to_owned(), micros(self.wall_time).to_string()),
        ];
        for call in &self.host_calls {
            attributes.push((format!("host.{}.count", call.name), call.count.to_string()));
            attributes.push((format!("host.{}.fuel", call.name), call.fuel.to_string()));
        }
        attributes
    }

    /// Records the call on the unified trace as a [`Layer::Wasm`] span that ended after
    /// `wall_time`.
    #[must_use]
    pub fn record(
        &self,
        collector: &TraceCollector,
        parent: &TraceContext,
        at: Duration,
    ) -> ActiveSpan {
        let mut span = collector.child(
            parent,
            Layer::Wasm,
            &format!("wasm:{}", self.export),
            self.attributes(),
            at,
        );
        if span.is_sampled() {
            span.set_attribute("dwarf", "not supported (see DWARF_NOTE)");
        }
        span
    }

    /// The view as the text `spot dev` prints: component, export, fuel, memory and host calls.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "wasm call {}", self.export);
        let _ = writeln!(out, "  component   {}", short_hash(&self.component_hash));
        let _ = writeln!(
            out,
            "  fuel        {} (host imports {})",
            self.fuel_used,
            self.host_fuel()
        );
        let _ = writeln!(out, "  memory      {} bytes", self.memory_bytes);
        let _ = writeln!(out, "  wall        {}", format_duration(self.wall_time));
        if self.host_calls.is_empty() {
            let _ = writeln!(out, "  host calls  none");
        } else {
            let _ = writeln!(out, "  host calls  {}", self.host_calls.len());
            for call in &self.host_calls {
                let _ = writeln!(
                    out,
                    "    {:<20} {:>6} call(s)  {:>10} fuel",
                    call.name, call.count, call.fuel
                );
            }
        }
        out
    }
}

fn short_hash(hash: &str) -> String {
    hash.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{HostCall, WasmCallReport, WasmDebugInfo};
    use crate::trace::collector::TraceCollector;
    use crate::trace::span::Layer;

    fn report() -> WasmCallReport {
        WasmCallReport {
            component_hash: "0123456789abcdef0123456789abcdef".into(),
            export: "on-event".into(),
            fuel_used: 1_234,
            memory_bytes: 65_536,
            host_calls: vec![HostCall::new("log", 2, 10), HostCall::new("get-doc", 1, 40)],
            wall_time: Duration::from_micros(750),
        }
    }

    #[test]
    fn the_view_lists_the_cost_and_the_host_calls() {
        let info = WasmDebugInfo::new(report()).expect("valid report");
        assert_eq!(info.host_fuel(), 50);
        let text = info.render_text();
        assert!(text.contains("wasm call on-event"));
        assert!(text.contains("0123456789ab"));
        assert!(text.contains("1234"));
        assert!(text.contains("65536 bytes"));
        assert!(text.contains("get-doc"));
        assert!(text.contains("750us"));
    }

    #[test]
    fn recording_makes_one_nested_wasm_span() {
        let collector = TraceCollector::new(spot_core::SiteId::new("acme.example").expect("id"), 4);
        let parent = collector.root(
            Layer::Script,
            "script:on_event",
            [("request_id".into(), "r-9".into())],
            Duration::from_millis(1),
        );
        let context = parent.context().expect("sampled");
        let info = WasmDebugInfo::new(report()).expect("valid");
        info.record(&collector, &context, Duration::from_millis(2))
            .end(Duration::from_millis(3));
        parent.end(Duration::from_millis(4));
        let trace = collector.by_request("r-9").expect("trace");
        let wasm: Vec<_> = trace
            .spans
            .iter()
            .filter(|span| span.layer == Layer::Wasm)
            .collect();
        assert_eq!(wasm.len(), 1);
        assert_eq!(wasm[0].name, "wasm:on-event");
        assert_eq!(wasm[0].attributes["fuel_used"], "1234");
        assert_eq!(wasm[0].attributes["host.get-doc.count"], "1");
        assert_eq!(wasm[0].attributes["component"], "0123456789ab");
    }

    #[test]
    fn an_unnamed_component_is_refused() {
        let mut bad = report();
        bad.export = "  ".into();
        assert_eq!(
            WasmDebugInfo::new(bad).expect_err("empty export").code(),
            "devtools.wasm"
        );
        let mut bad = report();
        bad.host_calls.push(HostCall::new("", 1, 0));
        assert!(WasmDebugInfo::new(bad).is_err());
    }
}
