//! One telemetry setup for every service: structured logs, OpenTelemetry
//! traces, and OpenTelemetry metrics exposed in Prometheus format.
//!
//! Exists so the OTLP/exporter boilerplate isn't copy-pasted across six
//! `main.rs` files that then drift. Each service calls [`init`] once and
//! holds the returned [`Telemetry`] for the life of the process.
//!
//! # Exporting is opt-in
//!
//! Traces are only exported when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
//! With it unset -- which is every deployment today, since nothing in
//! this repo or the infra chart runs a collector yet (see issue #17,
//! which owns the actual stack) -- the trace pipeline is simply not
//! installed, and the service behaves exactly as it did before: JSON
//! logs on stdout, metrics on `/metrics`. That is deliberate: this can
//! land and ship before any collector exists, and turning it on later is
//! a values change rather than a code change.
//!
//! # Why metrics still speak Prometheus
//!
//! Discovery across this project is plain `prometheus.io/*` pod
//! annotations, not PodMonitor/ServiceMonitor CRDs, specifically so
//! nothing has to install prometheus-operator to scrape magpie (see the
//! commit that introduced these metrics). Migrating the metrics API to
//! OpenTelemetry must not quietly break that, so the SDK's meter provider
//! is wired to a Prometheus exporter and [`Telemetry::render`] serves the
//! same exposition format on the same endpoint. A caller that had a
//! `PrometheusHandle` swaps it for this and changes nothing else.

use std::time::Duration;

use anyhow::Context;
use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use prometheus::{Encoder, Registry, TextEncoder};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Environment variable that decides whether traces are exported at all.
/// The standard OTel name, so an operator sets it the way they would for
/// any other OTel-instrumented process.
const OTLP_ENDPOINT_ENV: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Live telemetry state. Dropping this flushes and shuts the exporters
/// down, so a service must keep it alive for the whole of `main` -- the
/// same reason the `tracing_appender` guard it replaces had to be held.
pub struct Telemetry {
    registry: Registry,
    meter_provider: SdkMeterProvider,
    tracer_provider: Option<SdkTracerProvider>,
    /// Keeps the non-blocking log writer's worker thread alive. Dropping
    /// it early stops that thread and silently discards whatever is still
    /// buffered.
    _log_guard: tracing_appender::non_blocking::WorkerGuard,
}

impl Telemetry {
    /// The `/metrics` body, in Prometheus exposition format.
    ///
    /// Returns an empty body rather than failing the request if encoding
    /// goes wrong: a scrape endpoint that 500s drops the pod out of a
    /// scrape rotation, which is a worse failure than one empty sample.
    pub fn render(&self) -> String {
        let mut buffer = Vec::new();
        let encoder = TextEncoder::new();
        if let Err(e) = encoder.encode(&self.registry.gather(), &mut buffer) {
            tracing::warn!("failed to encode Prometheus metrics: {e:#}");
            return String::new();
        }
        String::from_utf8(buffer).unwrap_or_default()
    }

    /// Flushes and shuts down both pipelines. Called automatically on
    /// drop; exposed for a service that wants to do it before a
    /// deliberate `exit`, where drop glue never runs.
    pub fn shutdown(&self) {
        if let Some(provider) = &self.tracer_provider {
            if let Err(e) = provider.shutdown() {
                tracing::warn!("failed to shut down tracer provider: {e:#}");
            }
        }
        if let Err(e) = self.meter_provider.shutdown() {
            tracing::warn!("failed to shut down meter provider: {e:#}");
        }
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Installs logging, metrics and (when configured) trace export for
/// `service_name`, which becomes the OTel `service.name` resource
/// attribute every span and metric is attributed to.
pub fn init(service_name: &'static str) -> anyhow::Result<Telemetry> {
    let resource = Resource::builder()
        .with_service_name(service_name)
        .with_attributes([KeyValue::new("service.version", env!("CARGO_PKG_VERSION"))])
        .build();

    // Metrics first, and unconditionally: these are what `/metrics`
    // serves, and they have to work with no collector in sight.
    let registry = Registry::new();
    let exporter = opentelemetry_prometheus::exporter()
        .with_registry(registry.clone())
        .build()
        .context("failed to build the Prometheus exporter")?;
    let meter_provider = SdkMeterProvider::builder()
        .with_reader(exporter)
        .with_resource(resource.clone())
        .build();
    opentelemetry::global::set_meter_provider(meter_provider.clone());

    // Non-blocking stdout, matching what every service did before: a
    // blocking write on the logging path stalls request handling behind
    // whatever is consuming the pod's stdout.
    let (writer, log_guard) = tracing_appender::non_blocking(std::io::stdout());
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(writer)
        .with_target(false);

    let endpoint = std::env::var(OTLP_ENDPOINT_ENV)
        .ok()
        .filter(|e| !e.trim().is_empty());

    let tracer_provider = match &endpoint {
        Some(endpoint) => Some(build_tracer_provider(resource, endpoint)?),
        // Not an error, and not warned about on every start: no collector
        // is the expected state right now.
        None => None,
    };

    match &tracer_provider {
        Some(provider) => {
            let tracer = provider.tracer(service_name);
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                // Bridges the `tracing` spans this codebase already emits
                // into OTel traces, so no instrumentation call site has to
                // change to gain distributed tracing.
                .with(tracing_opentelemetry::layer().with_tracer(tracer))
                .init();
            tracing::info!(
                "exporting OTLP traces to {}",
                redact(endpoint.as_deref().unwrap_or_default())
            );
        }
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(fmt_layer)
                .init();
        }
    }

    Ok(Telemetry {
        registry,
        meter_provider,
        tracer_provider,
        _log_guard: log_guard,
    })
}

fn build_tracer_provider(resource: Resource, endpoint: &str) -> anyhow::Result<SdkTracerProvider> {
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(Duration::from_secs(5))
        .build()
        .context("failed to build the OTLP span exporter")?;

    Ok(SdkTracerProvider::builder()
        // Batched rather than simple: a simple exporter blocks the thread
        // that closed the span on a network round trip, which on a
        // request path makes the collector's latency ours.
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build())
}

/// Strips any userinfo before an endpoint reaches the log. Collector
/// endpoints do not usually carry credentials, but a hosted one can, and
/// logging it once at startup would put it in every log sink forever.
fn redact(endpoint: &str) -> String {
    match endpoint.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((_, host)) => format!("{scheme}://<redacted>@{host}"),
            None => endpoint.to_string(),
        },
        None => endpoint.to_string(),
    }
}

/// The meter every service records instruments against.
///
/// A thin wrapper over the global provider so call sites don't each
/// repeat the crate-name argument, and so there is one place to change if
/// the naming convention moves.
pub fn meter() -> opentelemetry::metrics::Meter {
    opentelemetry::global::meter("magpie")
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn redact_strips_userinfo() {
        assert_eq!(
            redact("https://user:token@otlp.example.com:4317"),
            "https://<redacted>@otlp.example.com:4317"
        );
    }

    #[test]
    fn redact_leaves_a_plain_endpoint_alone() {
        // The overwhelmingly common shape -- an in-cluster collector.
        assert_eq!(
            redact("http://otel-collector:4317"),
            "http://otel-collector:4317"
        );
    }

    #[test]
    fn redact_tolerates_a_schemeless_value() {
        assert_eq!(redact("otel-collector:4317"), "otel-collector:4317");
    }
}

#[cfg(test)]
mod exposition_tests {
    //! Proves the Prometheus half works without a collector, which is the
    //! state every deployment is in today -- if this broke, `/metrics`
    //! would silently serve nothing and the existing prometheus.io/*
    //! scrape annotations would go from useful to lying.

    use opentelemetry::KeyValue;
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::metrics::SdkMeterProvider;
    use prometheus::{Encoder, Registry, TextEncoder};

    fn render(registry: &Registry) -> String {
        let mut buf = Vec::new();
        TextEncoder::new()
            .encode(&registry.gather(), &mut buf)
            .unwrap();
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn otel_instruments_show_up_in_prometheus_exposition() {
        // Built standalone rather than through `init`, which installs
        // process-global state and so cannot run twice in one test binary.
        let registry = Registry::new();
        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()
            .unwrap();
        let provider = SdkMeterProvider::builder()
            .with_reader(exporter)
            .with_resource(Resource::builder().with_service_name("test").build())
            .build();

        let meter = opentelemetry::metrics::MeterProvider::meter(&provider, "magpie");
        let gauge = meter.u64_gauge("magpie_servers_total").build();
        gauge.record(3, &[KeyValue::new("phase", "running")]);

        let body = render(&registry);
        // The name and the label both have to survive, since existing
        // dashboards and queries key off them.
        assert!(
            body.contains("magpie_servers_total"),
            "missing metric:\n{body}"
        );
        assert!(body.contains("phase=\"running\""), "missing label:\n{body}");
        assert!(body.contains(" 3"), "missing value:\n{body}");
    }

    #[test]
    fn counters_expose_a_total() {
        let registry = Registry::new();
        let exporter = opentelemetry_prometheus::exporter()
            .with_registry(registry.clone())
            .build()
            .unwrap();
        let provider = SdkMeterProvider::builder().with_reader(exporter).build();

        let meter = opentelemetry::metrics::MeterProvider::meter(&provider, "magpie");
        let counter = meter.u64_counter("magpie_rpc_requests").build();
        counter.add(
            1,
            &[KeyValue::new("path", "/x"), KeyValue::new("status", "200")],
        );
        counter.add(
            1,
            &[KeyValue::new("path", "/x"), KeyValue::new("status", "200")],
        );

        let body = render(&registry);
        assert!(
            body.contains("magpie_rpc_requests"),
            "missing counter:\n{body}"
        );
        assert!(body.contains(" 2"), "counter did not accumulate:\n{body}");
    }
}
