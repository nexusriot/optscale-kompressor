//! W3C trace context across the process boundary, for OpenTelemetry export.
//!
//! Both functions are always present and do nothing in a build without the
//! `otel` feature, so the surfaces that call them carry no `cfg`. With the
//! feature they still do nothing until a text-map propagator has been
//! installed, which only `[otel] enabled = true` does: a deployment that has
//! not turned export on neither reads nor writes `traceparent`.

use http::HeaderMap;
use tracing::Span;

/// Make `span` a child of the caller's trace, when the request carries a
/// `traceparent` header. A request without one keeps `span` as a new root.
#[cfg(feature = "otel")]
pub fn adopt_parent(span: &Span, headers: &HeaderMap) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    // Only a *valid remote* context is adopted. Asking the propagator is
    // enough to tell: with no header (or no propagator installed) it returns
    // the empty context, which is the one thing not worth setting.
    let parent = opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.extract(&opentelemetry_http::HeaderExtractor(headers))
    });
    use opentelemetry::trace::TraceContextExt as _;
    if parent.span().span_context().is_valid() {
        let _ = span.set_parent(parent);
    }
}

#[cfg(not(feature = "otel"))]
pub fn adopt_parent(_span: &Span, _headers: &HeaderMap) {}

/// Write the current span's context into `headers` as `traceparent` (and
/// `tracestate`), for a call made on this request's behalf.
#[cfg(feature = "otel")]
pub fn inject_current(headers: &mut HeaderMap) {
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    let context = Span::current().context();
    opentelemetry::global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut opentelemetry_http::HeaderInjector(headers));
    });
}

#[cfg(not(feature = "otel"))]
pub fn inject_current(_headers: &mut HeaderMap) {}
