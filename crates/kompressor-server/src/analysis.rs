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
    compare_from: Option<i64>,
    compare_to: Option<i64>,
    model: Option<String>,
    mode: Option<String>,
    surface: Option<String>,
    experiment_id: Option<String>,
    run_id: Option<String>,
}

pub(crate) async fn analysis(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, false, View::Overview).await
}
pub(crate) async fn page_on_stats_listener(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, true, View::Overview).await
}
#[derive(Clone, Copy, PartialEq)]
enum View {
    Overview,
    Retrieval,
    Latency,
    Comparison,
}

pub(crate) async fn retrieval(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, false, View::Retrieval).await
}
pub(crate) async fn latency(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, false, View::Latency).await
}
pub(crate) async fn comparison(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    respond(state, headers, query, false, View::Comparison).await
}

async fn respond(
    state: Arc<ServerState>,
    headers: HeaderMap,
    query: Result<Query<AnalysisQuery>, QueryRejection>,
    listener: bool,
    view: View,
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
    let filter = kompressor_stats::history::HistoryFilter {
        model: q.model,
        mode: q.mode,
        surface: q.surface,
        experiment_id: q.experiment_id,
        run_id: q.run_id,
    };
    if filter.mode.as_deref().is_some_and(|m| {
        !["aggressive", "lossless", "recoverable", "off", "unrecorded"].contains(&m)
    }) || filter
        .surface
        .as_deref()
        .is_some_and(|s| !["api", "mcp", "proxy", "cli"].contains(&s))
    {
        return Err(ApiError::Invalid("unsupported mode or surface".into()));
    }
    if view != View::Comparison
        && (q.compare_from.is_some()
            || q.compare_to.is_some()
            || filter.model.is_some()
            || filter.mode.is_some()
            || filter.surface.is_some()
            || filter.experiment_id.is_some()
            || filter.run_id.is_some())
    {
        return Err(ApiError::Invalid(
            "comparison dates and cohort filters are supported only at /stats/analysis/compare"
                .into(),
        ));
    }
    let baseline = match (q.compare_from, q.compare_to) {
        (None, None) => (from.saturating_sub(to - from), from),
        (Some(f), Some(t)) if f >= 0 && t > f && t.saturating_sub(f) <= 90 * 86_400_000 => (f, t),
        _ => {
            return Err(ApiError::Invalid(
                "compare_from/compare_to must be a valid pair spanning at most 90 days".into(),
            ));
        }
    };
    if view == View::Comparison && baseline.1 <= baseline.0 {
        return Err(ApiError::Invalid(
            "choose a comparison window before or after this range".into(),
        ));
    }
    if view != View::Overview && q.conversation.is_some() {
        return Err(ApiError::Invalid(
            "conversation detail is available only on the overview".into(),
        ));
    }
    if html && view != View::Overview {
        return Err(ApiError::Invalid(
            "diagnostics serve JSON; open /stats/analysis?format=html for the dashboard".into(),
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
    if view != View::Overview {
        let report_tenant = tenant.clone();
        let mut data = state
            .with_stats(move |r| match view {
                View::Retrieval => serde_json::to_value(r.retrieval_analysis(&tenant, from, to)?)
                    .map_err(Into::into),
                View::Latency => {
                    serde_json::to_value(r.latency_analysis(&tenant, from, to)?).map_err(Into::into)
                }
                View::Comparison => serde_json::to_value(r.compare_history(
                    &tenant,
                    (from, to),
                    baseline,
                    &filter,
                )?)
                .map_err(Into::into),
                View::Overview => unreachable!(),
            })
            .await?;
        if let Some(obj) = data.as_object_mut() {
            obj.insert("tenant".into(), serde_json::json!(report_tenant));
            obj.insert("from_ms".into(), serde_json::json!(from));
            obj.insert("to_ms".into(), serde_json::json!(to));
            obj.insert(
                "stats_dropped".into(),
                serde_json::json!(state.core.stats().dropped()),
            );
        }
        return Ok(json_response(&data));
    }
    let report = state
        .with_stats(move |r| r.analysis(&tenant, from, to, q.conversation.as_deref()))
        .await?;
    Ok(json_response(
        &serde_json::json!({"report":report,"stats_dropped":state.core.stats().dropped(),"retention_days":state.core.config().stats.retention_days}),
    ))
}
