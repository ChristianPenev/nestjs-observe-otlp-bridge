//! The HTTP surface the `@nestjs/observe` agent talks to.
//!
//! The agent POSTs a gzipped JSON batch to `{endpoint}/applications/telemetry` with
//! `x-api-key` and `x-api-secret` headers, and reads the response body looking for
//! `{"degraded": true}`. Everything this module does is shaped by what the agent
//! does with each answer, which is why the status codes below are chosen rather than
//! generic:
//!
//! - **200** - accepted. The body must be JSON, or the agent's `response.json()`
//!   fails; it treats that as absent and carries on, but there is no reason to make
//!   it guess.
//! - **400** - the agent strips what the contract refuses, drops unsalvageable
//!   entries and re-sends *once*. Only worth returning for a body that is genuinely
//!   malformed.
//! - **401/403** - the agent stops reporting and does not recover without a restart,
//!   because it reads credentials once at start-up. Returned only when this bridge
//!   was configured with credentials and they did not match.
//! - **429** - the agent pauses for `Retry-After` and drops everything in the
//!   meantime. Never returned: the bridge has no quota.
//!
//! An export failure answers **502** rather than 200. The agent logs it and drops
//! the batch either way, but a 200 would report success for telemetry that never
//! arrived, and the operator would have nothing to go on.

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use serde_json::json;
use std::sync::Arc;

use crate::config::IngestAuth;
use crate::mapping;
use crate::observe::decoder;
use crate::otlp::exporter::Exporter;

/// The largest batch accepted, before decompression.
///
/// The SDK ships from a fixed-size shared buffer, so a legitimate batch is far
/// smaller than this. `decoder` separately caps what a body may expand *to*.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// The path the agent posts to. Derived from its `endpoint` option, which is
/// documented as a base URL with no path of its own.
const TELEMETRY_PATH: &str = "/applications/telemetry";

pub struct AppState {
    pub exporter: Exporter,
    pub auth: Option<IngestAuth>,
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(TELEMETRY_PATH, post(ingest))
        // Not part of the Observe protocol. Present because a container needs
        // something to answer a liveness probe, and because "is the bridge up" is
        // the first question asked when telemetry stops arriving.
        .route("/health", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn ingest(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(expected) = &state.auth
        && !credentials_match(&headers, expected)
    {
        tracing::warn!("rejected a batch: the credentials did not match");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid app key or secret" })),
        )
            .into_response();
    }

    // The agent always gzips, but the header is honoured rather than assumed so a
    // hand-made `curl` request also works.
    let gzipped = headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("gzip"));

    let batch = match decoder::decode(&body, gzipped) {
        Ok(batch) => batch,
        Err(error) => {
            tracing::warn!(%error, "rejected a batch that could not be decoded");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };

    if batch.is_empty() {
        // Nothing to forward. Still a success: the agent flushes on a timer and an
        // idle application legitimately sends an empty batch.
        return accepted();
    }

    let service = batch.service.id.clone();
    let received_at = Utc::now();
    let span_count = batch.span_count();
    let log_count = batch.logs.len();
    let metric_count = batch.metrics.len();

    let mut failures = Vec::new();

    if let Some(spans) = mapping::traces::map(&batch, received_at)
        && let Err(error) = state.exporter.export_traces(spans).await
    {
        failures.push(format!("traces: {error}"));
    }
    if let Some(logs) = mapping::logs::map(&batch)
        && let Err(error) = state.exporter.export_logs(logs).await
    {
        failures.push(format!("logs: {error}"));
    }
    if let Some(metrics) = mapping::metrics::map(&batch, received_at)
        && let Err(error) = state.exporter.export_metrics(metrics).await
    {
        failures.push(format!("metrics: {error}"));
    }

    if failures.is_empty() {
        tracing::info!(
            service = %service,
            spans = span_count,
            logs = log_count,
            metrics = metric_count,
            "forwarded a batch"
        );
        return accepted();
    }

    // One line per failed batch rather than one per signal: a collector that is
    // down fails all three, and three lines a flush is noise.
    tracing::error!(service = %service, failures = %failures.join("; "), "could not forward a batch");
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": failures.join("; ") })),
    )
        .into_response()
}

/// The success body.
///
/// `degraded` is the field the agent looks for; it is what the hosted collector sets
/// when it is accepting batches while discarding their spans. This bridge never
/// discards, so it is always false - but it is sent explicitly rather than omitted,
/// because a missing field and `false` mean the same thing to the agent only by
/// accident of how it parses.
fn accepted() -> Response {
    (
        StatusCode::OK,
        Json(json!({ "accepted": true, "degraded": false })),
    )
        .into_response()
}

/// Whether the agent presented the configured credentials.
///
/// Compared in full rather than short-circuiting on the first difference. The
/// comparison is not constant-time in any rigorous sense - it is a `==` on strings
/// of possibly different lengths - but the credentials are a deployment's own shared
/// secret between two of its processes, not a user-facing password, and the
/// alternative was a dependency for a threat that does not apply here.
fn credentials_match(headers: &HeaderMap, expected: &IngestAuth) -> bool {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
    };
    header("x-api-key") == expected.app_key && header("x-api-secret") == expected.app_secret
}

#[cfg(test)]
mod tests;
