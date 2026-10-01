//! Tests for the ingest endpoint.

use super::*;
use crate::config::{OtlpConfig, OtlpProtocol};
use axum::body::Body;
use axum::http::Request;
use flate2::{Compression, write::GzEncoder};
use std::collections::HashMap;
use std::io::Write;
use std::time::Duration;
use tower::ServiceExt;

/// An exporter pointed at a port nothing listens on, so exports fail fast.
fn failing_exporter() -> Exporter {
    Exporter::new(OtlpConfig {
        endpoint: "http://127.0.0.1:1".to_string(),
        protocol: OtlpProtocol::HttpProtobuf,
        headers: HashMap::new(),
        timeout: Duration::from_millis(100),
    })
    .unwrap()
}

fn app(auth: Option<IngestAuth>) -> Router {
    router(Arc::new(AppState {
        exporter: failing_exporter(),
        auth,
    }))
}

fn gzip(payload: &str) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(payload.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

fn post_batch(body: Vec<u8>, gzipped: bool, headers: &[(&str, &str)]) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri(TELEMETRY_PATH)
        .header("content-type", "application/json");
    if gzipped {
        request = request.header("content-encoding", "gzip");
    }
    for (key, value) in headers {
        request = request.header(*key, *value);
    }
    request.body(Body::from(body)).unwrap()
}

#[tokio::test]
async fn an_empty_batch_is_accepted_without_contacting_the_exporter() {
    // The agent flushes on a timer, so an idle application sends these.
    let response = app(None)
        .oneshot(post_batch(gzip(r#"{"serviceId":"api"}"#), true, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_success_body_carries_the_field_the_agent_reads() {
    let response = app(None)
        .oneshot(post_batch(gzip(r#"{"serviceId":"api"}"#), true, &[]))
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(parsed["degraded"], serde_json::Value::Bool(false));
}

#[tokio::test]
async fn a_plain_json_body_is_accepted_too() {
    // What anyone reaches for when reproducing a problem by hand.
    let response = app(None)
        .oneshot(post_batch(br#"{"serviceId":"api"}"#.to_vec(), false, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_malformed_body_is_a_400_so_the_agent_repairs_and_retries() {
    let response = app(None)
        .oneshot(post_batch(gzip("not json"), true, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_body_that_is_not_gzip_despite_the_header_is_a_400() {
    let response = app(None)
        .oneshot(post_batch(b"{}".to_vec(), true, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_failed_export_is_a_502_not_a_silent_success() {
    let payload = r#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/x","d":5}
        ]}"#;
    let response = app(None)
        .oneshot(post_batch(gzip(payload), true, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn credentials_are_not_checked_unless_configured() {
    let response = app(None)
        .oneshot(post_batch(gzip(r#"{"serviceId":"api"}"#), true, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn matching_credentials_are_accepted() {
    let auth = IngestAuth {
        app_key: "k".to_string(),
        app_secret: "s".to_string(),
    };
    let response = app(Some(auth))
        .oneshot(post_batch(
            gzip(r#"{"serviceId":"api"}"#),
            true,
            &[("x-api-key", "k"), ("x-api-secret", "s")],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_wrong_secret_is_a_401() {
    let auth = IngestAuth {
        app_key: "k".to_string(),
        app_secret: "s".to_string(),
    };
    let response = app(Some(auth))
        .oneshot(post_batch(
            gzip(r#"{"serviceId":"api"}"#),
            true,
            &[("x-api-key", "k"), ("x-api-secret", "wrong")],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn missing_credentials_are_a_401_when_they_are_required() {
    let auth = IngestAuth {
        app_key: "k".to_string(),
        app_secret: "s".to_string(),
    };
    let response = app(Some(auth))
        .oneshot(post_batch(gzip(r#"{"serviceId":"api"}"#), true, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn health_answers_without_any_credentials() {
    let response = app(Some(IngestAuth {
        app_key: "k".to_string(),
        app_secret: "s".to_string(),
    }))
    .oneshot(
        Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn an_unknown_path_is_a_404() {
    let response = app(None)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/traces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
