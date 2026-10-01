//! Tests for the wire decoder.

use super::*;
use flate2::{Compression, write::GzEncoder};
use std::io::Write;

fn gzip(payload: &str) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(payload.as_bytes()).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn decodes_a_gzipped_batch() {
    let payload = r#"{"serviceId":"api","snapshots":[]}"#;
    let batch = decode(&gzip(payload), true).unwrap();
    assert_eq!(batch.service.id, "api");
    assert!(batch.is_empty());
}

#[test]
fn decodes_plain_json_when_not_gzipped() {
    let payload = br#"{"serviceId":"api"}"#;
    let batch = decode(payload, false).unwrap();
    assert_eq!(batch.service.id, "api");
}

#[test]
fn rejects_a_body_that_is_not_gzip() {
    let error = decode(b"{}", true).unwrap_err();
    assert!(matches!(error, DecodeError::Decompress(_)));
}

#[test]
fn null_sections_are_treated_as_absent() {
    // The SDK's 400-repair path can null a section rather than delete it.
    let payload = br#"{"serviceId":"api","snapshots":null,"logs":null,"custom":null}"#;
    let batch = decode(payload, false).unwrap();
    assert!(batch.is_empty());
}

#[test]
fn http_root_span_follows_the_method_and_route_convention() {
    let payload = br#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/users/:id","a":{"m":"GET","sc":200}}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert_eq!(batch.operations[0].name, "GET /users/:id");
}

#[test]
fn graphql_root_span_uses_the_operation_id() {
    let payload = br#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"graphql","op":"Query.orders"}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert_eq!(batch.operations[0].name, "Query.orders");
}

#[test]
fn a_request_without_a_route_falls_back_to_its_url() {
    let payload = br#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","a":{"m":"POST","ou":"/graphql"}}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert_eq!(batch.operations[0].name, "POST /graphql");
}

#[test]
fn span_names_use_nests_class_and_method_form() {
    let payload = br#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/u","t":[{"c":"UsersController","m":"getUser","o":"auto"}]}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert_eq!(batch.operations[0].spans[0].name, "UsersController.getUser");
}

#[test]
fn a_manual_spans_own_name_wins() {
    let payload = br#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/u","t":[{"n":"charge card","o":"manual"}]}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    let span = &batch.operations[0].spans[0];
    assert_eq!(span.name, "charge card");
    assert!(span.manual);
    assert_eq!(span.component, Component::Manual);
}

#[test]
fn collapsed_count_is_lifted_out_of_the_tags() {
    let payload = br#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/u","t":[
              {"n":"ValidationPipe.transform x27","c":"ValidationPipe","m":"transform",
               "o":"auto","d":54,"t":{"observe.collapsed":27}}
            ]}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    let span = &batch.operations[0].spans[0];
    assert_eq!(span.collapsed_count, Some(27));
    // Lifted, not duplicated - it means something specific about `duration_ms`.
    assert!(!span.tags.contains_key("observe.collapsed"));
}

#[test]
fn nesting_is_preserved() {
    let payload = br#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/u","t":[
              {"c":"UsersController","m":"getUser","o":"auto","ch":[
                {"c":"UsersService","m":"findUser","o":"auto","ch":[
                  {"c":"pg","m":"SELECT","o":"auto","t":{"db.system":"postgresql"}}
                ]}
              ]}
            ]}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    let root = &batch.operations[0].spans[0];
    let service = &root.children[0];
    let query = &service.children[0];
    assert_eq!(root.component, Component::Controller);
    assert_eq!(service.component, Component::Service);
    assert_eq!(query.component, Component::DatabaseClient);
    assert_eq!(query.name, "SELECT");
    // The driver name survives decoding; suppressing `nestjs.class.name` for it
    // is the mapper's decision, not this one's.
    assert_eq!(query.class_name.as_deref(), Some("pg"));
}

#[test]
fn a_bare_error_flag_still_marks_the_span_failed() {
    let payload = br#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/u","t":[{"c":"S","m":"m","o":"auto","e":true}]}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert!(matches!(
        batch.operations[0].spans[0].error,
        Some(SpanOutcome::Failed)
    ));
}

#[test]
fn a_false_error_flag_is_not_a_failure() {
    let payload = br#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/u","t":[{"c":"S","m":"m","o":"auto","e":false}]}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert!(batch.operations[0].spans[0].error.is_none());
}

#[test]
fn an_error_object_keeps_its_class_and_stack() {
    let payload = br#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/u","e":{"cls":"NotFoundException","message":"no user","stack":"at x"}}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    let error = batch.operations[0].error.as_ref().unwrap();
    assert_eq!(error.class_name.as_deref(), Some("NotFoundException"));
    assert_eq!(error.stack.as_deref(), Some("at x"));
}

#[test]
fn a_job_without_a_trace_id_is_dropped() {
    // Nothing downstream can place a span without a trace.
    let payload = br#"{"serviceId":"api","jobs":[{"i":"job-1","q":"email"}]}"#;
    let batch = decode(payload, false).unwrap();
    assert!(batch.operations.is_empty());
}

#[test]
fn job_letters_are_read_in_their_own_scope() {
    // `c` is calledAt and `s` is status here, not className and spanId.
    let payload = br#"{"serviceId":"api","jobs":[
            {"i":"job-1","ti":"t9","n":"send","q":"email","s":"completed",
             "c":"2026-10-01T10:00:00.000Z","am":2,"ma":3}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    let operation = &batch.operations[0];
    assert_eq!(operation.name, "send email");
    assert!(operation.started_at.is_some());
    let OperationKind::Job(job) = &operation.kind else {
        panic!("expected a job");
    };
    assert_eq!(job.status.as_deref(), Some("completed"));
    assert_eq!(job.attempts_made, Some(2));
}

#[test]
fn log_levels_map_onto_otlp_severities() {
    let payload = br#"{"serviceId":"api","logs":[
            {"timestamp":1759312800000,"text":"a","level":"warn"},
            {"timestamp":1759312800000,"text":"b","level":"verbose"},
            {"timestamp":1759312800000,"text":"c","level":"log"},
            {"timestamp":1759312800000,"text":"d"}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert_eq!(batch.logs[0].severity, Severity::Warn);
    // Nest's `verbose` has no OTLP name; TRACE is the closest.
    assert_eq!(batch.logs[1].severity, Severity::Trace);
    // Nest's default level is `log`, which is INFO.
    assert_eq!(batch.logs[2].severity, Severity::Info);
    assert_eq!(batch.logs[3].severity, Severity::Unspecified);
}

#[test]
fn objectives_are_dropped_rather_than_invented_into_otlp() {
    let payload = br#"{"serviceId":"api","objectives":[
            {"handler":"UsersController.getUser","operationId":"/users/:id",
             "objectives":[{"availability":99.9}]}
        ]}"#;
    let batch = decode(payload, false).unwrap();
    assert!(batch.is_empty());
}

#[test]
fn a_malformed_body_is_an_error_not_a_panic() {
    assert!(matches!(
        decode(b"not json", false).unwrap_err(),
        DecodeError::Json(_)
    ));
}
