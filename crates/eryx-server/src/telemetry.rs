//! OpenTelemetry tracing setup for the eryx gRPC server.
//!
//! Configures a [`tracing_subscriber`] pipeline with:
//! - An [`EnvFilter`] layer (default: `info,h2=warn,tonic::transport=warn`)
//! - An optional OpenTelemetry OTLP exporter (enabled when `OTEL_EXPORTER_OTLP_ENDPOINT` is set)
//! - A [`tracing_error::ErrorLayer`] for `SpanTrace` capture
//! - A [`tracing_logfmt`] formatting layer for structured stdout logs

use std::time::Duration;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig};
use opentelemetry_sdk::{
    Resource,
    propagation::TraceContextPropagator,
    trace::{Sampler, SdkTracerProvider},
};
use tracing_error::ErrorLayer;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Initialize the tracing subscriber with optional OpenTelemetry export.
///
/// When the `OTEL_EXPORTER_OTLP_ENDPOINT` environment variable is set, an OTLP
/// gRPC span exporter is configured with a 3-second timeout and `AlwaysOn` sampler.
/// Otherwise, only local log output is produced.
///
/// Returns `Some(provider)` when OTel is enabled so the caller can call
/// `provider.shutdown()` on graceful termination, or `None` when OTel is disabled.
///
/// # Errors
///
/// Returns an error if the OTLP exporter fails to build (e.g. invalid endpoint).
pub fn setup_tracing() -> Result<Option<SdkTracerProvider>, Box<dyn std::error::Error>> {
    // Register the W3C TraceContext propagator so incoming `traceparent` headers
    // are extracted and outgoing requests propagate context.
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    let (telemetry_layer, provider) =
        match std::env::var(opentelemetry_otlp::OTEL_EXPORTER_OTLP_ENDPOINT) {
            Ok(endpoint) => {
                let exporter = SpanExporter::builder()
                    .with_tonic()
                    .with_endpoint(endpoint)
                    .with_timeout(Duration::from_secs(3))
                    .build()?;

                let resource = Resource::builder().with_service_name("eryx-server").build();

                let provider = SdkTracerProvider::builder()
                    .with_sampler(Sampler::AlwaysOn)
                    .with_resource(resource)
                    .with_batch_exporter(exporter)
                    .build();

                let tracer = provider.tracer("eryx-server");
                tracing::info!("OpenTelemetry tracing initialized");
                (
                    Some(tracing_opentelemetry::layer().with_tracer(tracer)),
                    Some(provider),
                )
            }
            Err(_) => (None, None),
        };

    let filter_layer = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,h2=warn,tonic::transport=warn"));

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(telemetry_layer)
        .with(ErrorLayer::default())
        .with(tracing_logfmt::layer())
        .init();

    Ok(provider)
}
