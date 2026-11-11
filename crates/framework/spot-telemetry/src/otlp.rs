//! OTLP trace export (cargo feature `otlp`).

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

use crate::{OtlpConfig, TelemetryError};

/// Parts per million to the ratio the SDK sampler takes. The only float in the crate besides
/// metric values, which the `metrics` facade takes as `f64`.
#[allow(clippy::float_arithmetic)]
pub(crate) fn ratio_from_ppm(ppm: u32) -> f64 {
    f64::from(ppm.min(1_000_000)) / 1_000_000.0
}

/// A parent-based ratio sampler: honour the caller's decision, else sample `ppm` per million.
pub(crate) fn sampler(ppm: u32) -> Sampler {
    Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio_from_ppm(ppm))))
}

/// Builds the exporter pipeline. Export is asynchronous and best effort, so an unreachable
/// endpoint does not fail this call.
pub(crate) fn provider(
    cfg: &OtlpConfig,
    service_name: &str,
    node_id: &str,
) -> Result<SdkTracerProvider, TelemetryError> {
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(cfg.endpoint.clone())
        .build()
        .map_err(|e| TelemetryError::Otlp(e.to_string()))?;
    let resource = Resource::builder()
        .with_service_name(service_name.to_owned())
        .with_attribute(opentelemetry::KeyValue::new(
            "service.instance.id",
            node_id.to_owned(),
        ))
        .build();
    Ok(SdkTracerProvider::builder()
        .with_sampler(sampler(cfg.sample_ppm))
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build())
}

/// A tracer from the provider, for `tracing-opentelemetry`.
pub(crate) fn tracer(provider: &SdkTracerProvider) -> opentelemetry_sdk::trace::Tracer {
    provider.tracer("spot-telemetry")
}

#[cfg(test)]
mod tests {
    use opentelemetry_sdk::trace::InMemorySpanExporter;
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::*;

    fn exported_spans(ppm: u32) -> usize {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_sampler(sampler(ppm))
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::Registry::default()
            .with(tracing_opentelemetry::layer().with_tracer(tracer(&provider)));
        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..50 {
                tracing::info_span!("work").in_scope(|| {});
            }
        });
        let _ = provider.force_flush();
        exporter.get_finished_spans().map_or(0, |spans| spans.len())
    }

    #[test]
    fn ratio_conversion() {
        assert_eq!(ratio_from_ppm(0).to_bits(), 0f64.to_bits());
        assert_eq!(ratio_from_ppm(1_000_000).to_bits(), 1f64.to_bits());
        assert_eq!(ratio_from_ppm(u32::MAX).to_bits(), 1f64.to_bits());
        assert_eq!(ratio_from_ppm(500_000).to_bits(), 0.5f64.to_bits());
    }

    #[test]
    fn zero_ppm_samples_nothing() {
        assert_eq!(exported_spans(0), 0);
    }

    #[test]
    fn full_ppm_samples_everything() {
        assert_eq!(exported_spans(1_000_000), 50);
    }
}
