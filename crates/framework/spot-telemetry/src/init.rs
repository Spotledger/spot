//! Setup: the global subscriber, and thread-scoped subscribers for tests.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::Subscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::{EnvFilter, Layer, Registry};

use crate::json::JsonLayer;
use crate::{TelemetryError, metrics};

/// Environment variable that overrides [`TelemetryConfig::filter`] in [`init`].
pub const FILTER_ENV: &str = "SPOT_LOG";

/// Log line format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// One JSON object per line (production).
    #[default]
    Json,
    /// Human-readable, multi-line (development).
    Pretty,
}

/// Where and how to export traces over OTLP (HTTP/protobuf). Needs the `otlp` cargo feature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OtlpConfig {
    /// Collector endpoint, for example `http://localhost:4318/v1/traces`.
    pub endpoint: String,
    /// Share of new traces to sample, in parts per million (1_000_000 = all, 0 = none).
    /// Requests that arrive with a sampling decision follow the caller's.
    pub sample_ppm: u32,
}

/// What [`init`] sets up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// `EnvFilter` syntax. Default `info`; the `SPOT_LOG` variable overrides it in [`init`].
    pub filter: String,
    /// Log line format. Default [`LogFormat::Json`].
    pub format: LogFormat,
    /// Trace export, if any.
    pub otlp: Option<OtlpConfig>,
    /// The `service.name` of exported traces.
    pub service_name: String,
    /// This node's id, the `service.instance.id` of exported traces.
    pub node_id: String,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            filter: "info".to_owned(),
            format: LogFormat::Json,
            otlp: None,
            service_name: "spotledger".to_owned(),
            node_id: "node-0".to_owned(),
        }
    }
}

/// Keeps telemetry alive. Dropping it flushes and shuts down the OTLP exporter; for a guard
/// from [`init_for_tests`] it also restores the thread's previous subscriber.
#[must_use = "telemetry stops when the guard is dropped"]
pub struct TelemetryGuard {
    _default: Option<tracing::subscriber::DefaultGuard>,
    #[cfg(feature = "otlp")]
    otlp: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otlp")]
        if let Some(provider) = self.otlp.take() {
            // Best effort: a collector that is down must not fail shutdown.
            let _ = provider.shutdown();
        }
    }
}

static INITIALISED: AtomicBool = AtomicBool::new(false);

fn parse_filter(filter: &str) -> Result<EnvFilter, TelemetryError> {
    EnvFilter::try_new(filter).map_err(|e| TelemetryError::InvalidFilter {
        filter: filter.to_owned(),
        reason: e.to_string(),
    })
}

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync + 'static>;

fn format_layer<W>(format: LogFormat, writer: W) -> BoxedLayer
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    match format {
        LogFormat::Json => Box::new(JsonLayer::new(writer)),
        LogFormat::Pretty => Box::new(
            tracing_subscriber::fmt::layer()
                .pretty()
                .with_writer(writer),
        ),
    }
}

struct Built {
    subscriber: Box<dyn Subscriber + Send + Sync + 'static>,
    #[cfg(feature = "otlp")]
    otlp: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

fn build<W>(config: &TelemetryConfig, filter: &str, writer: W) -> Result<Built, TelemetryError>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let filter = parse_filter(filter)?;
    #[cfg_attr(not(feature = "otlp"), allow(unused_mut))]
    let mut layers: Vec<BoxedLayer> = vec![format_layer(config.format, writer)];

    #[cfg(feature = "otlp")]
    let otlp = match &config.otlp {
        Some(cfg) => {
            let provider = crate::otlp::provider(cfg, &config.service_name, &config.node_id)?;
            layers.push(Box::new(
                tracing_opentelemetry::layer().with_tracer(crate::otlp::tracer(&provider)),
            ));
            Some(provider)
        }
        None => None,
    };
    #[cfg(not(feature = "otlp"))]
    if config.otlp.is_some() {
        return Err(TelemetryError::OtlpNotCompiled);
    }

    // The filter is outermost, so it applies to every layer.
    let subscriber = Registry::default().with(layers).with(filter);
    Ok(Built {
        subscriber: Box::new(subscriber),
        #[cfg(feature = "otlp")]
        otlp,
    })
}

/// Installs the global `tracing` subscriber (to stdout) and the Prometheus recorder.
///
/// The filter is `SPOT_LOG` if set, else `config.filter`. Fails with
/// [`TelemetryError::AlreadyInitialised`] on a second call, with
/// [`TelemetryError::InvalidFilter`] for a bad filter, and with
/// [`TelemetryError::OtlpNotCompiled`] if `config.otlp` is set without the `otlp` feature.
/// An unreachable OTLP endpoint is not an error here: export is asynchronous and best effort.
/// With `otlp` enabled, call it from inside a Tokio runtime.
pub fn init(config: TelemetryConfig) -> Result<TelemetryGuard, TelemetryError> {
    let filter = std::env::var(FILTER_ENV).unwrap_or_else(|_| config.filter.clone());
    if INITIALISED.swap(true, Ordering::SeqCst) {
        return Err(TelemetryError::AlreadyInitialised);
    }
    let result = (|| {
        let built = build(&config, &filter, io::stdout)?;
        metrics::install()?;
        tracing::subscriber::set_global_default(built.subscriber)
            .map_err(|_| TelemetryError::AlreadyInitialised)?;
        Ok(TelemetryGuard {
            _default: None,
            #[cfg(feature = "otlp")]
            otlp: built.otlp,
        })
    })();
    if result.is_err() {
        // A failed attempt must not block a corrected one.
        INITIALISED.store(false, Ordering::SeqCst);
    }
    result
}

/// Like [`init`], but the subscriber is installed for the **current thread only**, writes to
/// `writer`, and ignores `SPOT_LOG`. Dropping the guard restores the previous subscriber.
/// Use it with `#[tokio::test]` (current-thread runtime) to capture output without touching
/// the global subscriber.
pub fn init_for_tests_with<W>(
    config: TelemetryConfig,
    writer: W,
) -> Result<TelemetryGuard, TelemetryError>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let built = build(&config, &config.filter, writer)?;
    // Tests share one process-wide recorder; if another one is installed, metrics are simply
    // not rendered and logging still works.
    let _ = metrics::install();
    let guard = tracing::subscriber::set_default(built.subscriber);
    Ok(TelemetryGuard {
        _default: Some(guard),
        #[cfg(feature = "otlp")]
        otlp: built.otlp,
    })
}

/// [`init_for_tests_with`] with the default config but the filter `trace`.
pub fn init_for_tests<W>(writer: W) -> TelemetryGuard
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    let config = TelemetryConfig {
        filter: "trace".to_owned(),
        ..TelemetryConfig::default()
    };
    // The default config has no OTLP and the filter is valid, so this cannot fail.
    match init_for_tests_with(config, writer) {
        Ok(guard) => guard,
        Err(_) => TelemetryGuard {
            _default: None,
            #[cfg(feature = "otlp")]
            otlp: None,
        },
    }
}
