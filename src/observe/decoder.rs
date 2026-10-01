//! Wire format in, normalized model out.
//!
//! This module and its `metrics` child are the only places that know what the SDK's
//! single-letter keys mean. They also resolve everything the wire leaves implicit -
//! display names, component types, absolute timestamps, the split of nested metric
//! objects into individual streams - so that `mapping` translates a settled model
//! rather than interpreting a payload a second time.
//!
//! Snapshots, spans and logs are decoded here; `metrics` takes the two metric
//! sections, which are shaped unlike anything else on the wire and unlike each other.

use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Read;

use crate::observe::component;
use crate::observe::model::*;
use crate::observe::protocol;

mod metrics;

/// Guards against a decompression bomb: a small gzip body that expands without
/// bound. Generous next to a real batch, which the SDK caps by its own buffer.
const MAX_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("could not decompress the gzip body: {0}")]
    Decompress(#[source] std::io::Error),
    #[error("body expanded past the {MAX_DECOMPRESSED_BYTES} byte limit")]
    TooLarge,
    #[error("could not parse the batch as JSON: {0}")]
    Json(#[source] serde_json::Error),
}

/// Decodes a request body into a batch.
///
/// `gzipped` follows the request's `Content-Encoding`. The SDK always compresses,
/// but the header is honoured rather than assumed so that a plain-JSON body - what
/// anyone reaches for when reproducing a problem with `curl` - also works.
pub fn decode(body: &[u8], gzipped: bool) -> Result<Batch, DecodeError> {
    let json = if gzipped {
        let mut decoded = Vec::new();
        GzDecoder::new(body)
            .take(MAX_DECOMPRESSED_BYTES + 1)
            .read_to_end(&mut decoded)
            .map_err(DecodeError::Decompress)?;
        if decoded.len() as u64 > MAX_DECOMPRESSED_BYTES {
            return Err(DecodeError::TooLarge);
        }
        decoded
    } else {
        body.to_vec()
    };

    let batch: protocol::TelemetryBatch =
        serde_json::from_slice(&json).map_err(DecodeError::Json)?;
    Ok(normalize(batch))
}

/// Builds the model from an already-parsed batch.
pub fn normalize(batch: protocol::TelemetryBatch) -> Batch {
    let service = Service {
        id: batch.service_id,
        version: batch.service_version,
    };

    let mut operations = Vec::with_capacity(batch.snapshots.len() + batch.jobs.len());
    operations.extend(batch.snapshots.into_iter().map(normalize_request));
    operations.extend(batch.jobs.into_iter().filter_map(normalize_job));

    let mut metrics = Vec::new();
    if let Some(runtime) = batch.runtime {
        metrics::push_runtime_metrics(&runtime, &mut metrics);
    }
    for metric in batch.custom {
        metrics::push_custom_metric(metric, &mut metrics);
    }

    Batch {
        service,
        operations,
        logs: batch.logs.into_iter().map(normalize_log).collect(),
        metrics,
    }
}

fn normalize_request(snapshot: protocol::RequestSnapshot) -> Operation {
    let protocol_kind = parse_protocol(snapshot.protocol.as_deref());
    let attributes = snapshot.attributes.unwrap_or_default();
    let trace_id = snapshot.trace_id;

    let info = RequestInfo {
        protocol: protocol_kind,
        operation_id: snapshot.operation_id,
        method: attributes.method,
        status_code: attributes.status_code,
        original_url: attributes.original_url,
        user_id: snapshot.user_id,
        captured_request: snapshot.request,
    };

    Operation {
        name: request_name(&info),
        started_at: parse_timestamp(snapshot.called_at.as_deref()),
        duration_ms: snapshot.duration_ms,
        tags: to_attributes(snapshot.tags),
        error: snapshot.error.map(normalize_error),
        spans: normalize_spans(snapshot.traces),
        kind: OperationKind::Request(info),
        trace_id,
    }
}

/// A job becomes an operation, or is dropped when it has no trace id.
///
/// `ti` is optional on a job and required everywhere downstream: OTLP has no span
/// without a trace. Synthesising one from the job id would be worse than dropping -
/// it would put the job in a trace of its own that no request links to, which reads
/// as a real orphan rather than as missing data.
fn normalize_job(snapshot: protocol::JobSnapshot) -> Option<Operation> {
    let trace_id = snapshot.trace_id?;

    let info = JobInfo {
        id: snapshot.id,
        name: snapshot.name,
        source: job_source(snapshot.queue_name.as_deref()),
        queue_name: snapshot.queue_name,
        status: snapshot.status,
        enqueued_at: snapshot.enqueued_at,
        wait_duration_ms: snapshot.wait_duration_ms,
        attempts_made: snapshot.attempts_made,
        max_attempts: snapshot.max_attempts,
    };

    Some(Operation {
        name: job_name(&info),
        started_at: parse_timestamp(snapshot.called_at.as_deref()),
        duration_ms: snapshot.duration_ms,
        tags: to_attributes(snapshot.tags),
        error: snapshot.error.map(normalize_error),
        spans: normalize_spans(snapshot.traces),
        kind: OperationKind::Job(info),
        trace_id,
    })
}

/// The root span's name for a request.
///
/// HTTP follows the semantic conventions' `{method} {route}`. The other transports
/// have no convention to follow, so they use the operation id the SDK built, which
/// is already the most specific label available: `Query.orders` for GraphQL,
/// `gateway:pattern` for a WebSocket message.
fn request_name(info: &RequestInfo) -> String {
    let operation = info
        .operation_id
        .as_deref()
        .or(info.original_url.as_deref())
        .unwrap_or_else(|| info.protocol.as_str());

    match (&info.protocol, info.method.as_deref()) {
        (Protocol::Http, Some(method)) => format!("{method} {operation}"),
        _ => operation.to_string(),
    }
}

/// Whether a job came off a queue or from a timer.
///
/// The `@nestjs/schedule` instrumentation writes the scheduler kind into the queue
/// name, so these four values are what a scheduled run looks like. A real BullMQ
/// queue called `cron` would be misread, which is a fair trade for distinguishing
/// the two at all - and `nestjs.job.queue` still says what the name was.
fn job_source(queue_name: Option<&str>) -> JobSource {
    match queue_name {
        Some("cron") | Some("interval") | Some("timeout") | Some("schedule") => {
            JobSource::Scheduled
        }
        _ => JobSource::Queue,
    }
}

/// The root span's name for a job.
///
/// A queue job follows the messaging conventions' `{operation} {destination}`. A
/// scheduled run has no destination - `interval` is the kind of timer, not a place -
/// so it is named after the handler alone and the kind goes in an attribute;
/// `rollup interval` reads like a queue called "interval", which is exactly the
/// confusion worth avoiding.
fn job_name(info: &JobInfo) -> String {
    match info.source {
        JobSource::Scheduled => info
            .name
            .clone()
            .or_else(|| info.queue_name.clone())
            .unwrap_or_else(|| format!("job {}", info.id)),
        JobSource::Queue => match (info.name.as_deref(), info.queue_name.as_deref()) {
            (Some(name), Some(queue)) => format!("{name} {queue}"),
            (Some(name), None) => name.to_string(),
            (None, Some(queue)) => format!("process {queue}"),
            (None, None) => format!("job {}", info.id),
        },
    }
}

fn normalize_spans(nodes: Vec<protocol::TraceNode>) -> Vec<Span> {
    nodes.into_iter().map(normalize_span).collect()
}

fn normalize_span(node: protocol::TraceNode) -> Span {
    let kind = component::classify(&node);
    let mut tags = to_attributes(node.tags.clone());

    // Lifted out of the tag bag into a field of its own: it changes how the node's
    // duration must be read, so it is not just another label.
    let collapsed_count = tags
        .remove("observe.collapsed")
        .and_then(|value| value.as_i64());

    Span {
        name: span_name(&node, kind),
        component: kind,
        // Kept verbatim even for an outgoing span, where they hold a driver name and
        // an operation rather than a Nest class and method. The mapper needs both -
        // they are where `db.operation.name` and the peer address come from - and it
        // is the mapper, not this, that decides they are not worth a `nestjs.*`
        // attribute.
        class_name: node.class_name.clone(),
        method_name: node.method_key.clone(),
        observe_span_id: node.span_id.clone(),
        manual: kind == Component::Manual,
        start_offset_ms: node.start_offset_ms,
        duration_ms: node.duration_ms,
        tags,
        error: node.error.and_then(normalize_span_error),
        collapsed_count,
        children: normalize_spans(node.children),
    }
}

/// What a span is called in a waterfall.
///
/// `n` wins whenever it is set: the SDK writes it for manual spans and for collapsed
/// nodes, where it already reads `ValidationPipe.transform x27`. Otherwise the name
/// is built from the class and method, in Nest's own `Class.method` form.
fn span_name(node: &protocol::TraceNode, kind: Component) -> String {
    if let Some(name) = node.name.as_deref().filter(|name| !name.is_empty()) {
        return name.to_string();
    }

    let class_name = node.class_name.as_deref().unwrap_or("");
    let method = node.method_key.as_deref().unwrap_or("");

    // An outgoing span's `methodKey` is already a complete label - `SELECT`, or
    // `GET api.stripe.com` - and its "class" is a driver name nobody wants to read.
    if kind.is_outgoing() {
        return if method.is_empty() {
            class_name.to_string()
        } else {
            method.to_string()
        };
    }

    match (class_name.is_empty(), method.is_empty()) {
        (false, false) => format!("{class_name}.{method}"),
        (false, true) => class_name.to_string(),
        (true, false) => method.to_string(),
        (true, true) => "unknown".to_string(),
    }
}

/// `None` when the span did not fail.
///
/// `e: true` is the SDK's "an error occurred but was not captured" form, which every
/// failed non-root span uses. `e: false` is the negative of that and must not be read
/// as a failure - the SDK normally omits the key instead, but a literal `false` is
/// within its own contract and turning it into an errored span would fail a span
/// that succeeded.
fn normalize_span_error(error: protocol::SpanError) -> Option<SpanOutcome> {
    match error {
        protocol::SpanError::Flag(false) => None,
        protocol::SpanError::Flag(true) => Some(SpanOutcome::Failed),
        protocol::SpanError::Detail(detail) => Some(SpanOutcome::Error(normalize_error(detail))),
    }
}

fn normalize_error(error: protocol::ErrorDetail) -> ErrorInfo {
    ErrorInfo {
        class_name: error.cls,
        message: error.message,
        stack: error.stack,
        tags: to_attributes(error.tags),
    }
}

fn normalize_log(entry: protocol::LogEntry) -> LogRecord {
    LogRecord {
        timestamp: from_millis(entry.timestamp),
        body: entry.text,
        severity: parse_severity(entry.level.as_deref()),
        raw_level: entry.level,
        trace_id: entry.trace_id,
        observe_span_id: entry.span_id,
        context: entry.context,
        attributes: to_attributes(entry.attributes),
    }
}

/// Nest's log levels, mapped onto OTLP severities.
///
/// `verbose` is Nest's most detailed level and has no OTLP name of its own; TRACE is
/// the closest, and `nestjs.log.level` keeps the original either way.
fn parse_severity(level: Option<&str>) -> Severity {
    match level.map(str::to_ascii_lowercase).as_deref() {
        Some("trace") | Some("verbose") => Severity::Trace,
        Some("debug") => Severity::Debug,
        Some("log") | Some("info") => Severity::Info,
        Some("warn") | Some("warning") => Severity::Warn,
        Some("error") => Severity::Error,
        Some("fatal") => Severity::Fatal,
        _ => Severity::Unspecified,
    }
}

fn parse_protocol(value: Option<&str>) -> Protocol {
    match value {
        Some("http") => Protocol::Http,
        Some("graphql") => Protocol::GraphQl,
        Some("rpc") => Protocol::Rpc,
        Some("grpc") => Protocol::Grpc,
        Some("ws") => Protocol::WebSocket,
        Some(other) => Protocol::Other(other.to_string()),
        // The SDK marks `protocol` required, so this is a malformed snapshot rather
        // than a non-HTTP one. HTTP is the overwhelmingly likely truth and is a
        // better guess than inventing an "unknown" transport.
        None => Protocol::Http,
    }
}

fn parse_timestamp(value: Option<&str>) -> Option<DateTime<Utc>> {
    let raw = value?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

fn from_millis(millis: f64) -> DateTime<Utc> {
    let nanos = (millis * 1_000_000.0) as i64;
    DateTime::from_timestamp_nanos(nanos)
}

fn to_attributes(tags: Option<Map<String, Value>>) -> Attributes {
    tags.map(|tags| tags.into_iter().collect::<BTreeMap<_, _>>())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
