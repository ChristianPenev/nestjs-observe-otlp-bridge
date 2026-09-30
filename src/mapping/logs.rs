//! Observe log records into OTLP `LogRecord`s.
//!
//! Both correlation ids carry across. The SDK stamps each line with the trace it
//! belongs to and the id of the span that wrote it, and both go through the same
//! derivation the trace mapper uses - so "logs for this trace" and "logs for this
//! span" both work in the backend.
//!
//! A span id that names a span in a batch that has not arrived yet, or has already
//! gone, still exports. Logs and traces flush independently, so a dangling link is
//! normal and transient; dropping the id to avoid one would break the common case
//! to tidy the rare one.

use chrono::{DateTime, Utc};
use opentelemetry_proto::tonic::logs::v1::{LogRecord as OtlpLogRecord, ResourceLogs, ScopeLogs};

use crate::mapping::attributes::{self as attrs, AttributeBuilder};
use crate::mapping::ids;
use crate::mapping::traces::{resource, scope};
use crate::observe::model::*;

/// Builds the `ResourceLogs` for a batch, or `None` when it carried no logs.
pub fn map(batch: &Batch) -> Option<ResourceLogs> {
    if batch.logs.is_empty() {
        return None;
    }

    Some(ResourceLogs {
        resource: Some(resource(&batch.service)),
        scope_logs: vec![ScopeLogs {
            scope: Some(scope()),
            log_records: batch.logs.iter().map(map_record).collect(),
            schema_url: String::new(),
        }],
        schema_url: String::new(),
    })
}

fn map_record(record: &LogRecord) -> OtlpLogRecord {
    let mut builder = AttributeBuilder::new();
    builder.tags(&record.attributes);
    builder.maybe_str(attrs::NESTJS_LOG_CONTEXT, record.context.clone());
    builder.maybe_str(attrs::NESTJS_LOG_LEVEL, record.raw_level.clone());
    builder.maybe_str(attrs::NESTJS_SPAN_ID, record.observe_span_id.clone());

    let nanos = to_nanos(record.timestamp);

    OtlpLogRecord {
        time_unix_nano: nanos,
        // The SDK timestamps a line when it is written, which is also when it was
        // observed - there is no separate collection time to report.
        observed_time_unix_nano: nanos,
        severity_number: record.severity.number(),
        severity_text: record.severity.text().unwrap_or_default().to_string(),
        body: Some(attrs::string_value(record.body.clone())),
        attributes: builder.build(),
        trace_id: record
            .trace_id
            .as_deref()
            .map(|id| ids::trace_id(id).to_vec())
            .unwrap_or_default(),
        span_id: record
            .observe_span_id
            .as_deref()
            .map(|id| ids::span_id_from(id).to_vec())
            .unwrap_or_default(),
        ..Default::default()
    }
}

fn to_nanos(timestamp: DateTime<Utc>) -> u64 {
    timestamp.timestamp_nanos_opt().unwrap_or(0).max(0) as u64
}

#[cfg(test)]
mod tests {
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

    const LOGS: &str = r#"{"serviceId":"api","logs":[{
        "timestamp":1759312800000,"text":"user not found","level":"warn",
        "context":"UsersService","traceId":"0198f3a1-2b4c-7d8e-9f01-23456789abcd",
        "spanId":"01a0f4a1-5d24-74d0-9ae8-316500f03919","attributes":{"userId":"u-1"}
    }]}"#;

    #[test]
    fn a_log_keeps_its_body_and_severity() {
        let records = mapped(LOGS);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert!(matches!(
            record.body.as_ref().unwrap().value.as_ref().unwrap(),
            any_value::Value::StringValue(value) if value == "user not found"
        ));
        assert_eq!(record.severity_number, Severity::Warn.number());
        assert_eq!(record.severity_text, "WARN");
    }

    #[test]
    fn a_log_is_correlated_to_its_trace() {
        let records = mapped(LOGS);
        // The same derivation the trace mapper uses, so the two line up.
        assert_eq!(
            hex::encode(&records[0].trace_id),
            "0198f3a12b4c7d8e9f0123456789abcd"
        );
    }

    #[test]
    fn a_log_links_to_the_span_that_wrote_it() {
        let records = mapped(LOGS);
        assert_eq!(records[0].span_id.len(), 8);
        // The same derivation the trace mapper applies to a span's own `s`, so the
        // two ends of the link meet.
        assert_eq!(
            records[0].span_id,
            crate::mapping::ids::span_id_from("01a0f4a1-5d24-74d0-9ae8-316500f03919").to_vec()
        );
        // The original id is kept too, for cross-referencing with the hosted
        // collector.
        assert_eq!(
            string_attribute(&records[0], attrs::NESTJS_SPAN_ID),
            Some("01a0f4a1-5d24-74d0-9ae8-316500f03919")
        );
    }

    #[test]
    fn a_log_with_no_span_still_exports_with_its_trace() {
        let payload = r#"{"serviceId":"api","logs":[
            {"timestamp":1759312800000,"text":"x","traceId":"t1"}
        ]}"#;
        let records = mapped(payload);
        assert!(records[0].span_id.is_empty());
        assert!(!records[0].trace_id.is_empty());
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
    fn structured_metadata_survives() {
        let records = mapped(LOGS);
        assert_eq!(string_attribute(&records[0], "userId"), Some("u-1"));
    }

    #[test]
    fn a_log_without_a_trace_id_still_exports() {
        let payload = r#"{"serviceId":"api","logs":[
            {"timestamp":1759312800000,"text":"boot"}
        ]}"#;
        let records = mapped(payload);
        assert_eq!(records.len(), 1);
        assert!(records[0].trace_id.is_empty());
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
}
