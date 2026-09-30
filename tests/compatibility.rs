//! Compatibility with the real `@nestjs/observe` wire format.
//!
//! The unit tests use hand-written payloads, which prove the mapping is right but
//! not that it matches what the SDK actually sends. These run against captures taken
//! from an unmodified agent - `tests/fixtures/`, produced by the steps at the end of
//! `docs/protocol.md`.
//!
//! This is the file that should fail when a new SDK version changes the wire format.
//! When it does, re-capture the fixtures before changing any assertion: a fixture is
//! evidence, and editing it to match the code defeats the point of having one.

use chrono::{DateTime, Utc};
use nestjs_observe_oss::mapping;
use nestjs_observe_oss::observe::decoder;
use nestjs_observe_oss::observe::model::Batch;
use opentelemetry_proto::tonic::common::v1::any_value;
use opentelemetry_proto::tonic::trace::v1::{Span, span::SpanKind, status::StatusCode};

fn load(name: &str) -> Batch {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("could not read {path}: {e}"));
    decoder::decode(&bytes, false).unwrap_or_else(|e| panic!("could not decode {path}: {e}"))
}

fn received_at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn spans(batch: &Batch) -> Vec<Span> {
    mapping::traces::map(batch, received_at())
        .map(|resource| resource.scope_spans[0].spans.clone())
        .unwrap_or_default()
}

fn attribute<'a>(span: &'a Span, key: &str) -> Option<&'a any_value::Value> {
    span.attributes
        .iter()
        .find(|entry| entry.key == key)?
        .value
        .as_ref()?
        .value
        .as_ref()
}

fn string_attribute<'a>(span: &'a Span, key: &str) -> Option<&'a str> {
    match attribute(span, key)? {
        any_value::Value::StringValue(value) => Some(value),
        _ => None,
    }
}

fn find<'a>(spans: &'a [Span], name: &str) -> &'a Span {
    spans
        .iter()
        .find(|span| span.name == name)
        .unwrap_or_else(|| {
            let names: Vec<_> = spans.iter().map(|s| s.name.as_str()).collect();
            panic!("no span named {name}; got {names:?}")
        })
}

/// Renders one trace as an indented tree, so a structural regression shows up as a
/// readable diff rather than as a failed index.
fn tree(spans: &[Span], trace_id: &[u8]) -> String {
    let group: Vec<_> = spans.iter().filter(|s| s.trace_id == trace_id).collect();
    let mut out = String::new();
    fn walk(group: &[&Span], parent: &[u8], depth: usize, out: &mut String) {
        let mut children: Vec<_> = group
            .iter()
            .filter(|s| s.parent_span_id == parent)
            .collect();
        children.sort_by_key(|s| s.start_time_unix_nano);
        for span in children {
            out.push_str(&"  ".repeat(depth));
            out.push_str(&span.name);
            out.push('\n');
            walk(group, &span.span_id, depth + 1, out);
        }
    }
    walk(&group, &[], 0, &mut out);
    out
}

#[test]
fn a_real_http_request_produces_the_expected_span_tree() {
    let batch = load("http-requests.json");
    let all = spans(&batch);

    let root = find(&all, "GET /users/:id");
    assert_eq!(
        tree(&all, &root.trace_id),
        "\
GET /users/:id
  AuthGuard.canActivate
  LoggingInterceptor.intercept
  UsersController.getUser
    UsersService.findUser
      UsersService.readFromStore
"
    );
}

#[test]
fn nest_roles_are_recovered_from_a_real_capture() {
    // The wire carries no component type at all, so every one of these is inferred.
    let all = spans(&load("http-requests.json"));
    for (name, expected) in [
        ("AuthGuard.canActivate", "guard"),
        ("LoggingInterceptor.intercept", "interceptor"),
        ("UsersController.getUser", "controller"),
        ("UsersService.findUser", "service"),
    ] {
        assert_eq!(
            string_attribute(find(&all, name), "nestjs.type"),
            Some(expected),
            "{name}"
        );
    }
}

#[test]
fn class_and_method_survive_translation() {
    let all = spans(&load("http-requests.json"));
    let span = find(&all, "UsersService.findUser");
    assert_eq!(
        string_attribute(span, "nestjs.class.name"),
        Some("UsersService")
    );
    assert_eq!(
        string_attribute(span, "nestjs.method.name"),
        Some("findUser")
    );
}

#[test]
fn http_conventions_come_through() {
    let all = spans(&load("http-requests.json"));
    let root = find(&all, "GET /users/:id");
    assert_eq!(string_attribute(root, "http.request.method"), Some("GET"));
    assert_eq!(string_attribute(root, "http.route"), Some("/users/:id"));
    assert!(matches!(
        attribute(root, "http.response.status_code"),
        Some(any_value::Value::IntValue(200))
    ));
    assert_eq!(root.kind, SpanKind::Server as i32);
}

#[test]
fn a_thrown_not_found_is_recorded_without_failing_the_request() {
    // Nest answers 404 by throwing. The exception is reported; the span is not
    // marked failed, because a 4xx is not the server's error.
    let all = spans(&load("http-requests.json"));
    let root = find(&all, "GET /users/missing/:id");
    assert!(matches!(
        attribute(root, "http.response.status_code"),
        Some(any_value::Value::IntValue(404))
    ));
    assert_eq!(root.status.as_ref().unwrap().code, StatusCode::Unset as i32);

    let event = &root.events[0];
    assert_eq!(event.name, "exception");
    let kind = event
        .attributes
        .iter()
        .find(|entry| entry.key == "exception.type")
        .and_then(|entry| entry.value.as_ref())
        .and_then(|value| value.value.as_ref());
    assert!(matches!(
        kind,
        Some(any_value::Value::StringValue(value)) if value == "NotFoundException"
    ));
}

#[test]
fn an_unhandled_error_fails_the_request_and_keeps_its_stack() {
    let all = spans(&load("http-requests.json"));
    let root = find(&all, "GET /users/boom/all");
    assert_eq!(root.status.as_ref().unwrap().code, StatusCode::Error as i32);

    let has_stack = root.events[0]
        .attributes
        .iter()
        .any(|entry| entry.key == "exception.stacktrace");
    assert!(has_stack, "the stack trace should survive translation");
}

#[test]
fn every_span_in_a_capture_is_valid_otlp() {
    let all = spans(&load("http-requests.json"));
    assert!(!all.is_empty());
    for span in &all {
        assert_eq!(span.trace_id.len(), 16, "{}", span.name);
        assert_eq!(span.span_id.len(), 8, "{}", span.name);
        assert_ne!(span.trace_id, vec![0u8; 16], "{}", span.name);
        assert_ne!(span.span_id, vec![0u8; 8], "{}", span.name);
        assert!(
            span.end_time_unix_nano >= span.start_time_unix_nano,
            "{} ends before it starts",
            span.name
        );
        assert!(!span.name.is_empty());
    }
}

#[test]
fn span_ids_are_unique_within_a_capture() {
    let all = spans(&load("http-requests.json"));
    let mut ids: Vec<_> = all.iter().map(|span| span.span_id.clone()).collect();
    let total = ids.len();
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids.len(),
        total,
        "duplicate span ids would corrupt the tree"
    );
}

#[test]
fn a_childs_span_always_names_a_parent_in_the_same_trace() {
    let all = spans(&load("http-requests.json"));
    for span in &all {
        if span.parent_span_id.is_empty() {
            continue;
        }
        let parent = all
            .iter()
            .find(|other| other.span_id == span.parent_span_id)
            .unwrap_or_else(|| panic!("{} points at a parent that was not exported", span.name));
        assert_eq!(parent.trace_id, span.trace_id);
    }
}

#[test]
fn a_child_never_starts_before_its_parent() {
    let all = spans(&load("http-requests.json"));
    for span in &all {
        if span.parent_span_id.is_empty() {
            continue;
        }
        let parent = all
            .iter()
            .find(|other| other.span_id == span.parent_span_id)
            .unwrap();
        assert!(
            span.start_time_unix_nano >= parent.start_time_unix_nano,
            "{} starts before {}",
            span.name,
            parent.name
        );
    }
}

#[test]
fn real_logs_carry_both_correlation_ids() {
    let batch = load("http-requests.json");
    let logs = mapping::logs::map(&batch).expect("the capture contains logs");
    let records = &logs.scope_logs[0].log_records;
    assert!(!records.is_empty());

    let correlated = records
        .iter()
        .find(|record| !record.span_id.is_empty())
        .expect("at least one log line names the span that wrote it");
    assert_eq!(correlated.trace_id.len(), 16);
    assert_eq!(correlated.span_id.len(), 8);
}

#[test]
fn a_log_links_to_a_span_that_was_exported() {
    // The join that makes "logs for this span" work in a backend.
    let batch = load("http-requests.json");
    let all = spans(&batch);
    let logs = mapping::logs::map(&batch).unwrap();

    let linked = logs.scope_logs[0]
        .log_records
        .iter()
        .filter(|record| !record.span_id.is_empty())
        .filter(|record| all.iter().any(|span| span.span_id == record.span_id))
        .count();
    assert!(
        linked > 0,
        "no log line resolved to a span in the same capture"
    );
}

#[test]
fn real_runtime_metrics_map_with_the_right_units() {
    use opentelemetry_proto::tonic::metrics::v1::metric;

    let batch = load("runtime-metrics.json");
    let metrics = mapping::metrics::map(&batch, received_at())
        .expect("the capture contains runtime metrics")
        .scope_metrics[0]
        .metrics
        .clone();

    let by_name = |name: &str| {
        metrics
            .iter()
            .find(|metric| metric.name == name)
            .unwrap_or_else(|| panic!("no metric named {name}"))
            .clone()
    };

    // Megabytes, because the SDK divides by 1024 twice before sending.
    assert_eq!(by_name("nestjs.runtime.memory.rss").unit, "MBy");
    assert_eq!(by_name("nestjs.runtime.cpu.user").unit, "ms");
    assert_eq!(by_name("nestjs.runtime.event_loop.delay").unit, "ms");

    // GC figures describe one window, so they are deltas, not running totals.
    let gc = by_name("nestjs.runtime.gc.collections");
    let metric::Data::Sum(sum) = gc.data.as_ref().unwrap() else {
        panic!("gc collections should be a sum");
    };
    assert_eq!(sum.aggregation_temporality, 1, "delta temporality");
    assert!(sum.is_monotonic);
}

#[test]
fn the_resource_names_the_service_the_sdk_reported() {
    let batch = load("http-requests.json");
    let resource = mapping::traces::map(&batch, received_at())
        .unwrap()
        .resource
        .unwrap();
    let name = resource
        .attributes
        .iter()
        .find(|entry| entry.key == "service.name")
        .and_then(|entry| entry.value.as_ref())
        .and_then(|value| value.value.as_ref());
    assert!(matches!(
        name,
        Some(any_value::Value::StringValue(value)) if value == "example-api"
    ));
}

#[test]
fn mapping_a_capture_twice_gives_the_same_result() {
    // A resent batch must not fork into a second, duplicate trace.
    let batch = load("http-requests.json");
    assert_eq!(spans(&batch), spans(&batch));
}

#[test]
fn a_capture_encodes_to_wire_valid_otlp_protobuf() {
    // The default export path. Encoding cannot fail, but a decode-back proves the
    // bytes really are a well-formed `ExportTraceServiceRequest` rather than
    // something only this process can read.
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use prost::Message;

    let batch = load("http-requests.json");
    let request = ExportTraceServiceRequest {
        resource_spans: vec![mapping::traces::map(&batch, received_at()).unwrap()],
    };

    let bytes = request.encode_to_vec();
    assert!(!bytes.is_empty());

    let decoded = ExportTraceServiceRequest::decode(bytes.as_slice()).expect("valid protobuf");
    assert_eq!(decoded, request);

    let spans = &decoded.resource_spans[0].scope_spans[0].spans;
    assert!(spans.iter().all(|span| span.trace_id.len() == 16));
}

#[test]
fn a_capture_encodes_to_otlp_json_with_hex_ids() {
    // OTLP/JSON requires trace and span ids as hex strings, not base64.
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;

    let batch = load("http-requests.json");
    let request = ExportTraceServiceRequest {
        resource_spans: vec![mapping::traces::map(&batch, received_at()).unwrap()],
    };

    let json = serde_json::to_value(&request).expect("serializable");
    let span = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    let trace_id = span["traceId"].as_str().expect("traceId is a string");
    assert_eq!(trace_id.len(), 32);
    assert!(trace_id.chars().all(|c| c.is_ascii_hexdigit()));
    let span_id = span["spanId"].as_str().expect("spanId is a string");
    assert_eq!(span_id.len(), 16);
    assert!(span_id.chars().all(|c| c.is_ascii_hexdigit()));
}
