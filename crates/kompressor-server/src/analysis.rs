//! Fixed-window compression analysis, using the same authentication as live stats.
use crate::{
    api::authenticate, error::ApiError, live::prefers_html, middleware::json_response,
    state::ServerState,
};
use axum::{
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AnalysisQuery {
    tenant: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    conversation: Option<String>,
    format: Option<String>,
}

pub(crate) async fn analysis(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, false).await
}
pub(crate) async fn page_on_stats_listener(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, true).await
}
async fn respond(
    state: Arc<ServerState>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
    listener: bool,
) -> Result<Response, ApiError> {
    let Query(q) = query.map_err(|e| ApiError::Invalid(format!("query: {}", e.body_text())))?;
    let html = match q.format.as_deref() {
        Some("html") => true,
        Some("json") => false,
        None => listener || prefers_html(&headers),
        Some(_) => return Err(ApiError::Invalid("format must be html or json".into())),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let to = q.to.unwrap_or(now);
    let from = q.from.unwrap_or_else(|| to.saturating_sub(86_400_000));
    if from < 0 || to <= from || to.saturating_sub(from) > 90 * 86_400_000 {
        return Err(ApiError::Invalid(
            "from/to must be epoch milliseconds with 0 <= from < to, spanning at most 90 days"
                .into(),
        ));
    }
    if let Some(c) = &q.conversation
        && (c.len() != 16
            || !c
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    {
        return Err(ApiError::Invalid(
            "conversation must be 16 lowercase hex digits".into(),
        ));
    }
    if html {
        let settings = serde_json::json!({"endpoint": if listener {"/v1/stats/analysis"} else {""}, "listener": listener, "refs": state.core.config().refs.browse,
            "from":from,"to":to,"conversation":q.conversation}).to_string().replace('<', "\\u003c");
        return Ok((
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            include_str!("analysis.html").replace("/*SETTINGS*/{}", &settings),
        )
            .into_response());
    }
    let identity = authenticate(&state, &headers)?;
    let tenant = match q.tenant {
        None => identity.tenant.as_str().to_owned(),
        Some(t) if identity.trusted || t == identity.tenant.as_str() => t,
        Some(_) => return Err(ApiError::Forbidden),
    };
    let report = state
        .with_stats(move |r| r.analysis(&tenant, from, to, q.conversation.as_deref()))
        .await?;
    Ok(json_response(
        &serde_json::json!({"report":report,"stats_dropped":state.core.stats().dropped(),"retention_days":state.core.config().stats.retention_days}),
    ))
}
