//! Tests for log mapping.

use super::*;
use crate::observe::decoder;
use opentelemetry_proto::tonic::common::v1::any_value;

fn mapped(payload: &str) -> Vec<OtlpLogRecord> {
    let batch = decoder::decode(payload.as_bytes(), false).unwrap();
    map(&batch)
        .map(|logs| logs.scope_logs[0].log_records.clone())
        .unwrap_or_default()
}

fn string_attribute<'a>(record: &'a OtlpLogRecord, key: &str) -> Option<&'a str> {
    match record
        .attributes
        .iter()
        .find(|entry| entry.key == key)?
        .value
        .as_ref()?
        .value
        .as_ref()?
    {
        any_value::Value::StringValue(value) => Some(value),
        _ => None,
    }
}

const SPAN_UUID: &str = "01a0f4a1-5d24-74d0-9ae8-316500f03919";

const LOGS: &str = r#"{"serviceId":"api","logs":[{
        "timestamp":1759312800000,"text":"user not found","level":"warn",
        "context":"UsersService","traceId":"0198f3a1-2b4c-7d8e-9f01-23456789abcd",
        "spanId":"01a0f4a1-5d24-74d0-9ae8-316500f03919","attributes":{"userId":"u-1"}
    }]}"#;

#[test]
fn a_log_keeps_its_body_severity_and_metadata() {
    let records = mapped(LOGS);
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert!(matches!(
        record.body.as_ref().unwrap().value.as_ref().unwrap(),
        any_value::Value::StringValue(value) if value == "user not found"
    ));
    assert_eq!(record.severity_number, Severity::Warn.number());
    assert_eq!(record.severity_text, "WARN");
    // Structured metadata the application attached is passed through.
    assert_eq!(string_attribute(record, "userId"), Some("u-1"));
}

#[test]
fn a_log_is_correlated_to_its_trace_and_to_the_span_that_wrote_it() {
    let records = mapped(LOGS);
    // The same derivation the trace mapper uses, so the two line up.
    assert_eq!(
        hex::encode(&records[0].trace_id),
        "0198f3a12b4c7d8e9f0123456789abcd"
    );
    assert_eq!(records[0].span_id.len(), 8);
    // The same derivation the trace mapper applies to a span's own `s`, so the
    // two ends of the link meet.
    assert_eq!(
        records[0].span_id,
        crate::mapping::ids::span_id_from(SPAN_UUID).to_vec()
    );
    // The original id is kept too, for cross-referencing with the hosted
    // collector.
    assert_eq!(
        string_attribute(&records[0], attrs::NESTJS_SPAN_ID),
        Some(SPAN_UUID)
    );
}

#[test]
fn a_log_still_exports_when_its_correlation_ids_are_missing() {
    // Logs and traces flush independently, so a line can arrive with only part
    // of its correlation - or none of it. Dropping those would lose real logs.
    let no_span = mapped(
        r#"{"serviceId":"api","logs":[
            {"timestamp":1759312800000,"text":"x","traceId":"t1"}
        ]}"#,
    );
    assert!(no_span[0].span_id.is_empty(), "no span id was sent");
    assert!(!no_span[0].trace_id.is_empty(), "but the trace id survives");

    let neither = mapped(
        r#"{"serviceId":"api","logs":[
            {"timestamp":1759312800000,"text":"boot"}
        ]}"#,
    );
    assert_eq!(neither.len(), 1, "a log with no trace id still exports");
    assert!(neither[0].trace_id.is_empty());
}

#[test]
fn nest_context_and_original_level_are_preserved() {
    let records = mapped(LOGS);
    assert_eq!(
        string_attribute(&records[0], attrs::NESTJS_LOG_CONTEXT),
        Some("UsersService")
    );
    assert_eq!(
        string_attribute(&records[0], attrs::NESTJS_LOG_LEVEL),
        Some("warn")
    );
}

#[test]
fn verbose_keeps_its_name_even_though_otlp_has_no_such_severity() {
    let payload = r#"{"serviceId":"api","logs":[
            {"timestamp":1759312800000,"text":"x","level":"verbose"}
        ]}"#;
    let records = mapped(payload);
    assert_eq!(records[0].severity_number, Severity::Trace.number());
    assert_eq!(
        string_attribute(&records[0], attrs::NESTJS_LOG_LEVEL),
        Some("verbose")
    );
}

#[test]
fn the_timestamp_is_the_millisecond_the_sdk_sent() {
    let records = mapped(LOGS);
    assert_eq!(records[0].time_unix_nano, 1_759_312_800_000_000_000);
    assert_eq!(
        records[0].observed_time_unix_nano,
        records[0].time_unix_nano
    );
}

#[test]
fn a_batch_with_no_logs_maps_to_nothing() {
    let batch = decoder::decode(br#"{"serviceId":"api"}"#, false).unwrap();
    assert!(map(&batch).is_none());
}
