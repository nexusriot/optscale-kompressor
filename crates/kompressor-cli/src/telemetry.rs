//! OpenTelemetry trace export, compiled in only with `--features otel`.
//!
//! This does not add instrumentation. The proxy and the HTTP middleware already
//! open `tracing` spans (`proxy{request_id}`, the request span in
//! kompressor-server); this module adds one more `tracing` layer that turns
//! those spans into OTLP/HTTP exports. Anything instrumented later is exported
//! without touching this file.
//!
//! Without the feature, [`Guard`] is an empty type and [`init`] only reports
//! that the build cannot export, so the call site in `serve` carries no `cfg`.

use kompressor_core::config::OtelConfig;

/// Flushes whatever is buffered when dropped: hold it for the life of the
/// process.
pub struct Guard {
    #[cfg(feature = "otel")]
    traces: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    #[cfg(feature = "otel")]
    metrics: Option<opentelemetry_sdk::metrics::SdkMeterProvider>,
}

#[cfg(feature = "otel")]
impl Drop for Guard {
    fn drop(&mut self) {
        // Best effort: a collector that is down at shutdown must not hang or
        // fail the exit. The exporter's own timeout bounds each.
        if let Some(traces) = &self.traces {
            let _ = traces.shutdown();
        }
        if let Some(metrics) = &self.metrics {
            let _ = metrics.shutdown();
        }
    }
}

/// What [`init`] sets up.
pub struct Telemetry {
    /// The span layer, when traces are on.
    pub layer: Option<Layer>,
    /// A `metrics` recorder to run beside Prometheus's, when metrics are on.
    /// Hand it to `kompressor_server::metrics::install_with` *before* any
    /// server state is built: the global recorder is set once.
    pub recorder: Option<Box<dyn MetricsRecorder>>,
    pub guard: Guard,
}

/// `metrics::Recorder` without naming the crate in a build that lacks it.
#[cfg(feature = "otel")]
pub trait MetricsRecorder: metrics::Recorder + Send + Sync {}
#[cfg(feature = "otel")]
impl<T: metrics::Recorder + Send + Sync> MetricsRecorder for T {}
#[cfg(not(feature = "otel"))]
pub trait MetricsRecorder: Send + Sync {}

/// What a build without the feature does with `otel.enabled = true`.
#[cfg(not(feature = "otel"))]
pub fn init(cfg: &OtelConfig) -> Option<Telemetry> {
    if cfg.enabled {
        eprintln!(
            "kompressor: otel.enabled = true, but this binary was built without the `otel` \
             feature; no telemetry will be exported"
        );
    }
    None
}

/// Never constructed here (`init` returns `None`); `Identity` is a layer that
/// does nothing, which keeps the call site's types the same in both builds.
#[cfg(not(feature = "otel"))]
pub type Layer = tracing_subscriber::layer::Identity;

/// Set export up, or `None` when it is off or could not be (reported on
/// stderr; the proxy runs regardless).
#[cfg(feature = "otel")]
pub fn init(cfg: &OtelConfig) -> Option<Telemetry> {
    use opentelemetry::KeyValue;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
    use opentelemetry_sdk::Resource;
    use opentelemetry_sdk::propagation::TraceContextPropagator;
    use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};

    if !cfg.enabled || !(cfg.traces || cfg.metrics) {
        return None;
    }

    // `OTEL_EXPORTER_OTLP_HEADERS` (`k=v,k2=v2`) first, the file over it.
    let mut headers: std::collections::HashMap<String, String> =
        std::env::var("OTEL_EXPORTER_OTLP_HEADERS")
            .ok()
            .into_iter()
            .flat_map(|raw| {
                raw.split(',')
                    .filter_map(|kv| kv.split_once('='))
                    .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                    .collect::<Vec<_>>()
            })
            .collect();
    headers.extend(cfg.headers.clone());

    let client = match RuntimeClient::new(cfg.timeout) {
        Ok(client) => client,
        Err(error) => {
            eprintln!("kompressor: otel http client not built: {error}");
            return None;
        }
    };
    let signal_url = |signal: &str| format!("{}/v1/{signal}", cfg.endpoint.trim_end_matches('/'));
    let resource = || {
        Resource::builder()
            .with_service_name(cfg.service_name.clone())
            .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
            .build()
    };

    // `traceparent` in and out. Installed whenever export is on, even for
    // metrics alone: it is what lets a caller's trace id reach the logs-side
    // spans at all, and costs nothing when no header is present.
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());

    let mut guard = Guard {
        traces: None,
        metrics: None,
    };
    let mut layer = None;
    let mut recorder: Option<Box<dyn MetricsRecorder>> = None;

    if cfg.traces {
        match opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_http_client(client.clone())
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(signal_url("traces"))
            .with_timeout(cfg.timeout)
            .with_headers(headers.clone())
            .build()
        {
            Ok(exporter) => {
                let provider = SdkTracerProvider::builder()
                    .with_batch_exporter(exporter)
                    .with_sampler(Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(
                        cfg.sample_ratio,
                    ))))
                    .with_resource(resource())
                    .build();
                layer =
                    Some(tracing_opentelemetry::layer().with_tracer(provider.tracer("kompressor")));
                guard.traces = Some(provider);
            }
            Err(error) => eprintln!("kompressor: otel trace exporter not started: {error}"),
        }
    }

    if cfg.metrics {
        use opentelemetry::metrics::MeterProvider as _;
        use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
        match opentelemetry_otlp::MetricExporter::builder()
            .with_http()
            .with_http_client(client)
            .with_protocol(Protocol::HttpBinary)
            .with_endpoint(signal_url("metrics"))
            .with_timeout(cfg.timeout)
            .with_headers(headers)
            .build()
        {
            Ok(exporter) => {
                let reader = PeriodicReader::builder(exporter)
                    .with_interval(cfg.metrics_interval)
                    .build();
                let provider = SdkMeterProvider::builder()
                    .with_reader(reader)
                    .with_resource(resource())
                    .build();
                recorder = Some(Box::new(
                    metrics_exporter_opentelemetry::Recorder::with_meter(
                        provider.meter("kompressor"),
                    ),
                ));
                guard.metrics = Some(provider);
            }
            Err(error) => eprintln!("kompressor: otel metric exporter not started: {error}"),
        }
    }

    Some(Telemetry {
        layer,
        recorder,
        guard,
    })
}

/// The exporter's HTTP client, on the workspace's reqwest (see the workspace
/// Cargo.toml for why not the one the otlp crate would bring).
///
/// The SDK's batch processor drives exports from a thread of its own with no
/// tokio runtime, so an async reqwest client cannot be polled there; and a
/// *blocking* reqwest client panics if it is built or dropped from inside the
/// proxy's runtime, which `run` does. So the client lives on a small runtime of
/// its own, on its own thread, and requests are spawned onto it: awaiting the
/// join handle works from any executor. The thread idles between exports and
/// ends with the process.
#[cfg(feature = "otel")]
#[derive(Clone, Debug)]
struct RuntimeClient {
    client: reqwest::Client,
    handle: tokio::runtime::Handle,
}

#[cfg(feature = "otel")]
impl RuntimeClient {
    fn new(timeout: std::time::Duration) -> Result<Self, Box<dyn std::error::Error>> {
        let client = reqwest::Client::builder().timeout(timeout).build()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let handle = runtime.handle().clone();
        std::thread::Builder::new()
            .name("kompressor-otel".into())
            .spawn(move || runtime.block_on(std::future::pending::<()>()))?;
        Ok(Self { client, handle })
    }
}

#[cfg(feature = "otel")]
#[async_trait::async_trait]
impl opentelemetry_http::HttpClient for RuntimeClient {
    async fn send_bytes(
        &self,
        request: http::Request<bytes::Bytes>,
    ) -> Result<http::Response<bytes::Bytes>, opentelemetry_http::HttpError> {
        let request = reqwest::Request::try_from(request)?;
        let client = self.client.clone();
        self.handle
            .spawn(async move {
                let mut response = client.execute(request).await?;
                let mut out = http::Response::builder().status(response.status());
                if let Some(headers) = out.headers_mut() {
                    std::mem::swap(headers, response.headers_mut());
                }
                let body = response.bytes().await?;
                Ok::<_, opentelemetry_http::HttpError>(out.body(body)?)
            })
            .await?
    }
}

/// The tracing layer type; generic so it sits on any subscriber.
#[cfg(feature = "otel")]
pub type Layer = tracing_opentelemetry::OpenTelemetryLayer<
    tracing_subscriber::Registry,
    opentelemetry_sdk::trace::Tracer,
>;
