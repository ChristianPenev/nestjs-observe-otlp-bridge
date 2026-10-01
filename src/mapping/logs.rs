//! Observe log records into OTLP `LogRecord`s.
//!
//! Both correlation ids carry across, through the same derivations the trace mapper
//! uses, so "logs for this trace" and "logs for this span" both work.
//!
//! A link to a span outside this batch still exports. Logs and traces flush
//! independently, so a dangling link is normal and transient.

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
mod tests;
