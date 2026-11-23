//! What wraps every request, on every surface.
//!
//! Four things, and each of them exists because of something that is hard to
//! recover after the fact: an identifier that ties a log line to a stats row to
//! a retrieval three turns later; a span that carries it and the tenant so no
//! handler has to remember to log them; a body limit, because a 300 MB paste
//! should be refused rather than buffered; and an error envelope, so a 413 from
//! a layer and a 422 from a handler read the same to a client.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use kompressor_core::Credentials;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::Instrument;

use crate::error::{ErrorEnvelope, envelope_for_status, render};
use crate::state::{REQUEST_TIMEOUT, ServerState};

/// Header carrying the request identifier, inbound and outbound.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// The identifier this request is known by, in the request extensions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestId(pub Arc<str>);

impl RequestId {
    /// The identifier as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Read the credential headers.
///
/// Four spellings are accepted because four are in use: `Authorization:
/// Bearer`, the `x-api-key` the Anthropic SDK sends, the `x-kompressor-key` a
/// client uses when its `Authorization` header already belongs to the provider,
/// and the cluster secret. Reading only the bearer form leaves an
/// Anthropic-style client presenting no credential this service recognises, and
/// unrecognised traffic is silently never compressed rather than loudly
/// rejected — which is the worst of both outcomes.
pub fn extract_credentials(headers: &HeaderMap) -> Credentials<'_> {
    let text = |name: &str| -> Option<&str> {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    };

    let bearer = text("authorization").map(|value| match value.split_once(' ') {
        // An API gateway that strips the scheme leaves a bare key; treating it
        // as the credential costs nothing and saves a confusing 401.
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.trim(),
        _ => value,
    });

    Credentials {
        bearer,
        api_key: text("x-api-key"),
        kompressor_key: text("x-kompressor-key"),
        secret: text("x-kompressor-secret"),
        requested_tenant: text("x-kompressor-tenant"),
    }
}

/// Apply the standard stack to `router`.
///
/// `surface` is the label the span and the metrics carry: `api`, `mcp`,
/// `proxy`. Applying this twice is harmless — the inner copy finds the
/// identifier the outer one assigned and reuses it — so a surface may wrap its
/// own routes and still merge ours.
pub fn with_request_middleware(
    router: Router,
    state: Arc<ServerState>,
    surface: &'static str,
) -> Router {
    let max_body = state.max_body_bytes();
    // Applied one at a time, innermost first: a `ServiceBuilder` stack cannot
    // infer the extractor tuple of a `from_fn` middleware, and each `layer` call
    // on the router resolves it.
    router
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(max_body))
        // axum's own 2 MB default would reject a large conversation long before
        // the configured limit, so it is replaced rather than stacked with.
        .layer(DefaultBodyLimit::disable())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(axum::middleware::from_fn(error_envelope))
        // Outermost, so everything above runs inside the span and with the
        // identifier already assigned.
        .layer(axum::middleware::from_fn(
            move |request: Request, next: Next| request_context(surface, request, next),
        ))
}

/// Assign the request identifier and open the span every log line hangs off.
async fn request_context(surface: &'static str, mut request: Request, next: Next) -> Response {
    let id = match request.extensions().get::<RequestId>() {
        // Already wrapped: keep the identifier rather than issuing a second one
        // for the same request, which would split it across two log streams.
        Some(existing) => existing.clone(),
        None => {
            let inbound = request
                .headers()
                .get(REQUEST_ID_HEADER)
                .and_then(|value| value.to_str().ok())
                .and_then(crate::id::sanitize);
            RequestId(Arc::from(
                inbound.unwrap_or_else(crate::id::new_request_id).as_str(),
            ))
        }
    };
    request.extensions_mut().insert(id.clone());

    let span = tracing::info_span!(
        "request",
        request_id = %id,
        // Recorded by the handler once the credential resolves: a request that
        // is refused at the door has no tenant to name.
        tenant = tracing::field::Empty,
        surface = surface,
        method = %request.method(),
        path = %request.uri().path(),
        // Filled in by `metrics::compression` and below; absent when the
        // request compressed nothing.
        mode = tracing::field::Empty,
        tokens_in = tracing::field::Empty,
        tokens_out = tracing::field::Empty,
        http.response.status_code = tracing::field::Empty,
    );
    kompressor_core::trace_ctx::adopt_parent(&span, request.headers());

    let mut response = next.run(request).instrument(span.clone()).await;
    span.record("http.response.status_code", response.status().as_u16());
    if let Ok(value) = HeaderValue::from_str(id.as_str()) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }
    response
}

/// Give every error response the same body.
///
/// Handlers produce theirs through [`crate::ApiError`], but a 413 from the body
/// limit, a 408 from the timeout and a 404 from the router come from layers
/// that know nothing about our shape. A client should not have to parse two
/// kinds of failure, so those are rewritten here, and the identifier — which
/// only this layer has — is stamped onto both.
async fn error_envelope(request: Request, next: Next) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .map(|id| id.as_str().to_string());

    let response = next.run(request).await;
    let status = response.status();
    if status.is_success() || status.is_redirection() || status.is_informational() {
        return response;
    }

    let envelope = match response.extensions().get::<ErrorEnvelope>() {
        Some(envelope) => envelope.clone(),
        // A handler that authored its own JSON failure — a readiness probe
        // listing which dependency is down — said more than an envelope could,
        // so it is left alone.
        None if is_json(response.headers()) => return response,
        None => envelope_for_status(status),
    };

    let (mut parts, _discarded) = response.into_parts();
    let body = render(&envelope, request_id.as_deref());
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    // The old body's length is now wrong, and a stale content-length makes a
    // client hang waiting for bytes that will not come.
    parts.headers.remove(header::CONTENT_LENGTH);
    Response::from_parts(parts, Body::from(body))
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
}

/// Log one line for a compression, at INFO.
///
/// One line per compression, because it is the only place that says what a
/// request actually did without opening the statistics database.
pub fn log_compression(
    tenant: &str,
    stats: &kompressor_engine::messages::CompressStats,
    elapsed: Duration,
) {
    tracing::info!(
        tenant = tenant,
        mode = %stats.mode,
        tokens_in = stats.tokens_in,
        tokens_out = stats.tokens_out,
        ratio = stats.ratio,
        compressed = stats.compressed_messages,
        skipped = stats.skipped_messages,
        refs = stats.refs_emitted,
        engine_ms = elapsed.as_secs_f64() * 1000.0,
        "compress"
    );
}

/// Log one line for a retrieval, at INFO.
///
/// The handle is not a secret — it is a content digest that is useless without
/// the tenant's credential — and it is the only way to follow a fragment from
/// the compression that minted it to the request that asked for it back. The
/// fragment itself is never logged unless `log.payloads` is set.
pub fn log_retrieval(tenant: &str, handle: &str, result: &kompressor_core::Retrieved) {
    tracing::info!(
        tenant = tenant,
        handle = handle,
        found = result.found,
        expired = result.expired,
        ranged = result.ranged,
        truncated = result.truncated,
        chars = result.content.as_ref().map_or(0, |c| c.chars().count()),
        "retrieve"
    );
}

/// Whether the status is one the envelope layer rewrites.
///
/// Exported for surfaces that build responses without going through
/// [`crate::ApiError`] and want to know whether theirs will be replaced.
pub fn is_rewritten(status: StatusCode) -> bool {
    status.is_client_error() || status.is_server_error()
}

/// Build a plain success response with a JSON body.
pub(crate) fn json_response<T: serde::Serialize>(value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (
            [(header::CONTENT_TYPE, "application/json")],
            axum::body::Bytes::from(body),
        )
            .into_response(),
        // Serializing our own response types cannot fail in practice; if it
        // ever did, the caller gets an error it can act on rather than a
        // truncated body it cannot.
        Err(error) => {
            crate::ApiError::Internal(format!("could not render response: {error}")).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn a_bearer_credential_arrives_with_its_scheme_stripped() {
        let prefixed = headers(&[("authorization", "Bearer sk-alpha")]);
        assert_eq!(extract_credentials(&prefixed).bearer, Some("sk-alpha"));

        let lowercase = headers(&[("authorization", "bearer  sk-alpha ")]);
        assert_eq!(extract_credentials(&lowercase).bearer, Some("sk-alpha"));
    }

    #[test]
    fn a_bare_authorization_value_is_still_a_credential() {
        // a gateway that strips the scheme should not turn a valid key into a 401
        let bare = headers(&[("authorization", "sk-alpha")]);
        assert_eq!(extract_credentials(&bare).bearer, Some("sk-alpha"));
    }

    /// Both are read, and they are read into *different* fields.
    ///
    /// Folded together, the dedicated header was indistinguishable from a
    /// provider credential, and this extractor preferred `x-api-key` — the one
    /// more likely to belong to the provider — when both were present. The
    /// resolver then matched a single candidate, so our own key could be masked
    /// entirely and the request fell open to the shared anonymous tenant.
    #[test]
    fn the_anthropic_and_kompressor_key_headers_are_both_read() {
        let anthropic = headers(&[("x-api-key", "sk-alpha")]);
        assert_eq!(extract_credentials(&anthropic).api_key, Some("sk-alpha"));
        let ours = headers(&[("x-kompressor-key", "sk-alpha")]);
        assert_eq!(extract_credentials(&ours).kompressor_key, Some("sk-alpha"));

        // Both at once: neither is dropped, and neither is mistaken for the
        // other.
        let both = headers(&[
            ("x-api-key", "sk-the-providers"),
            ("x-kompressor-key", "sk-ours"),
        ]);
        let credentials = extract_credentials(&both);
        assert_eq!(credentials.api_key, Some("sk-the-providers"));
        assert_eq!(credentials.kompressor_key, Some("sk-ours"));
    }

    #[test]
    fn the_secret_and_the_requested_tenant_are_read_separately() {
        let trusted = headers(&[
            ("x-kompressor-secret", "cluster"),
            ("x-kompressor-tenant", "team-b"),
        ]);
        let got = extract_credentials(&trusted);
        assert_eq!(got.secret, Some("cluster"));
        assert_eq!(got.requested_tenant, Some("team-b"));
        assert_eq!(got.bearer, None);
    }

    #[test]
    fn an_empty_header_is_no_credential_rather_than_an_empty_one() {
        let blank = headers(&[("x-api-key", "   ")]);
        assert_eq!(extract_credentials(&blank).api_key, None);
    }
}
