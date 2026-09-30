//! Wire format in, normalized model out.
//!
//! This is the only module that knows what the SDK's single-letter keys mean. It
//! also resolves everything the wire leaves implicit - display names, component
//! types, absolute timestamps, the split of nested metric objects into individual
//! streams - so that `mapping` translates a settled model rather than interpreting a
//! payload a second time.

use chrono::{DateTime, Utc};
use flate2::read::GzDecoder;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io::Read;

use crate::observe::component;
use crate::observe::model::*;
use crate::observe::protocol;

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
        push_runtime_metrics(&runtime, &mut metrics);
    }
    for metric in batch.custom {
        push_custom_metric(metric, &mut metrics);
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

// --- metrics -----------------------------------------------------------------

/// Splits the nested runtime object into one stream per measurement.
///
/// The counters here - GC count and duration - are *deltas*: the SDK zeroes them
/// after every collection window, so they must be exported with delta temporality
/// rather than as cumulative totals, or a backend will read each window's figure as
/// a running total that keeps resetting.
fn push_runtime_metrics(runtime: &protocol::RuntimeMetrics, out: &mut Vec<Metric>) {
    let gauge = |name: &str, unit: Option<&'static str>, value: Option<f64>| {
        value.map(|value| Metric {
            name: name.to_string(),
            description: None,
            unit,
            kind: MetricKind::Gauge,
            value: MetricValue::F64(value),
            attributes: Attributes::new(),
            timestamp: None,
        })
    };

    if let Some(cpu) = &runtime.cpu {
        out.extend(gauge("nestjs.runtime.cpu.user", Some("ms"), cpu.user));
        out.extend(gauge("nestjs.runtime.cpu.system", Some("ms"), cpu.system));
        out.extend(gauge(
            "nestjs.runtime.cpu.utilization",
            Some("%"),
            cpu.percentage_used,
        ));
    }

    if let Some(memory) = &runtime.memory {
        // Megabytes, not bytes: `NodeRuntimeMetricsService` divides by 1024 twice
        // before sending. Reporting `By` here would overstate memory by 10^6.
        out.extend(gauge("nestjs.runtime.memory.rss", Some("MBy"), memory.rss));
        out.extend(gauge(
            "nestjs.runtime.memory.heap.total",
            Some("MBy"),
            memory.heap_total,
        ));
        out.extend(gauge(
            "nestjs.runtime.memory.heap.used",
            Some("MBy"),
            memory.heap_used,
        ));
        out.extend(gauge(
            "nestjs.runtime.memory.external",
            Some("MBy"),
            memory.external,
        ));
        out.extend(gauge(
            "nestjs.runtime.memory.array_buffers",
            Some("MBy"),
            memory.array_buffers,
        ));
        out.extend(gauge(
            "nestjs.runtime.memory.utilization",
            Some("%"),
            memory.percentage_used,
        ));
    }

    if let Some(event_loop) = &runtime.event_loop {
        out.extend(gauge(
            "nestjs.runtime.event_loop.delay",
            Some("ms"),
            event_loop.lag,
        ));
        // A ratio in 0..1, which UCUM writes as the dimensionless unit `1`.
        out.extend(gauge(
            "nestjs.runtime.event_loop.utilization",
            Some("1"),
            event_loop.utilization,
        ));
    }

    if let Some(gc) = &runtime.gc {
        if let Some(count) = gc.count {
            out.push(Metric {
                name: "nestjs.runtime.gc.collections".to_string(),
                description: Some("Garbage collections in the reporting window".to_string()),
                unit: Some("{collection}"),
                kind: MetricKind::Counter,
                value: MetricValue::F64(count),
                attributes: Attributes::new(),
                timestamp: None,
            });
        }
        if let Some(duration) = gc.total_duration {
            out.push(Metric {
                name: "nestjs.runtime.gc.duration".to_string(),
                description: Some("Time spent in garbage collection in the window".to_string()),
                unit: Some("ms"),
                kind: MetricKind::Counter,
                value: MetricValue::F64(duration),
                attributes: Attributes::new(),
                timestamp: None,
            });
        }
        if let Some(breakdown) = &gc.breakdown {
            for (kind, bucket) in [
                ("minor", &breakdown.minor),
                ("major", &breakdown.major),
                ("incremental", &breakdown.incremental),
            ] {
                let Some(bucket) = bucket else { continue };
                let mut attributes = Attributes::new();
                attributes.insert(
                    "nestjs.gc.kind".to_string(),
                    Value::String(kind.to_string()),
                );

                if let Some(count) = bucket.count() {
                    out.push(Metric {
                        name: "nestjs.runtime.gc.collections.by_kind".to_string(),
                        description: None,
                        unit: Some("{collection}"),
                        kind: MetricKind::Counter,
                        value: MetricValue::F64(count),
                        attributes: attributes.clone(),
                        timestamp: None,
                    });
                }
                if let Some(duration) = bucket.duration_ms() {
                    out.push(Metric {
                        name: "nestjs.runtime.gc.duration.by_kind".to_string(),
                        description: None,
                        unit: Some("ms"),
                        kind: MetricKind::Counter,
                        value: MetricValue::F64(duration),
                        attributes,
                        timestamp: None,
                    });
                }
            }
        }
    }
}

/// Turns one application metric into one or more streams.
///
/// Every value the SDK sends for a custom metric is a map of label set to number -
/// even an unlabelled one, which arrives as `{"default": 42}` - so each field can
/// expand into several points.
///
/// A counter is exported from `increase` rather than `value`. `value` is the
/// cumulative total held in the application's own memory, which resets to zero when
/// the process restarts; `increase` is what that total rose by since the last
/// successful flush, which is exactly a delta sum and is additive across instances.
/// The SDK computes it for this reason and only advances its baseline once a flush
/// has actually been written.
///
/// A summary becomes one gauge per quantile. OTLP has a summary point type, but it
/// is legacy and thinly supported, and a `quantile` attribute is what a backend can
/// actually chart.
fn push_custom_metric(metric: protocol::CustomMetric, out: &mut Vec<Metric>) {
    let mut base = to_attributes(metric.tags);
    // `l` is the metric's *declared* label names, not values. Only useful as
    // documentation, and it would collide with the real per-point labels below.
    let _ = &metric.labels;

    let timestamp = metric.last_updated.map(from_millis);
    let name = format!("nestjs.custom.{}", metric.name);
    let metric_type = metric.metric_type.as_deref().unwrap_or("gauge");

    // A type a later SDK introduces is still exported, as a gauge, but says what it
    // really was rather than silently claiming to be one.
    if !matches!(metric_type, "counter" | "gauge" | "summary") {
        base.insert(
            "nestjs.metric.type".to_string(),
            Value::String(metric_type.to_string()),
        );
    }

    let mut emit = |name: String,
                    kind: MetricKind,
                    source: &Option<Value>,
                    suffix_attrs: Option<(&str, &str)>| {
        let Some(source) = source else { return };
        for (labels, number) in flatten_series(source) {
            let mut attributes = base.clone();
            attributes.extend(labels);
            if let Some((key, value)) = suffix_attrs {
                attributes.insert(key.to_string(), Value::String(value.to_string()));
            }
            out.push(Metric {
                name: name.clone(),
                description: metric.description.clone(),
                unit: None,
                kind,
                value: MetricValue::F64(number),
                attributes,
                timestamp,
            });
        }
    };

    match metric_type {
        "summary" => {
            for (quantile, source) in [
                ("0.5", &metric.p50),
                ("0.95", &metric.p95),
                ("0.99", &metric.p99),
            ] {
                emit(
                    name.clone(),
                    MetricKind::Gauge,
                    source,
                    Some(("quantile", quantile)),
                );
            }
            emit(
                format!("{name}.count"),
                MetricKind::Counter,
                &metric.observations,
                None,
            );
            emit(
                format!("{name}.sum"),
                MetricKind::Counter,
                &metric.total,
                None,
            );
            emit(
                format!("{name}.max"),
                MetricKind::Gauge,
                &metric.maximum,
                None,
            );
        }
        "counter" => {
            // Prefer the delta; fall back to the cumulative reading when an older
            // SDK sent no `iv`, which is better than reporting nothing.
            let source = if metric.increase.is_some() {
                &metric.increase
            } else {
                &metric.value
            };
            emit(name.clone(), MetricKind::Counter, source, None);
        }
        _ => emit(name.clone(), MetricKind::Gauge, &metric.value, None),
    }
}

/// Reads one metric field into its individual series.
///
/// The SDK keys each series by `stringifyLabel` - `JSON.stringify` of the label
/// object with its keys sorted - and uses the literal `"default"` for a metric with
/// no labels. Both are decoded here: a JSON object key becomes real attributes, so
/// `{"route":"/login"}` arrives in the backend as a filterable `route` attribute
/// rather than as a string nobody can group by. A bare number is also accepted, in
/// case the shape is ever corrected to match the SDK's own contract.
fn flatten_series(value: &Value) -> Vec<(Attributes, f64)> {
    match value {
        Value::Number(number) => number
            .as_f64()
            .map(|number| vec![(Attributes::new(), number)])
            .unwrap_or_default(),
        Value::Object(map) => map
            .iter()
            .filter_map(|(key, value)| Some((parse_label_key(key), value.as_f64()?)))
            .collect(),
        _ => Vec::new(),
    }
}

/// Turns a series key back into attributes.
///
/// Anything that is not a JSON object - `"default"`, or a key from some future
/// encoding - yields no attributes rather than a made-up one. For `"default"` that
/// is exactly right: it means the metric was never labelled.
fn parse_label_key(key: &str) -> Attributes {
    if key == "default" {
        return Attributes::new();
    }
    match serde_json::from_str::<Value>(key) {
        Ok(Value::Object(map)) => map.into_iter().collect(),
        _ => Attributes::new(),
    }
}

#[cfg(test)]
mod tests {
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
    fn runtime_memory_is_reported_in_megabytes() {
        let payload = br#"{"serviceId":"api","runtime":{"m":{"r":128.5,"hu":64.0}}}"#;
        let batch = decode(payload, false).unwrap();
        let rss = batch
            .metrics
            .iter()
            .find(|metric| metric.name == "nestjs.runtime.memory.rss")
            .unwrap();
        assert_eq!(rss.unit, Some("MBy"));
        assert!(matches!(rss.value, MetricValue::F64(value) if value == 128.5));
    }

    #[test]
    fn gc_breakdown_accepts_the_object_shape_the_sdk_actually_sends() {
        // The SDK's own contract declares these as numbers; 0.3.5 sends objects.
        let payload = br#"{"serviceId":"api","runtime":{"g":{"c":3,"td":12.5,
            "b":{"m":{"count":2,"duration":4.0},"j":{"count":1,"duration":8.5}}}}}"#;
        let batch = decode(payload, false).unwrap();
        let by_kind: Vec<_> = batch
            .metrics
            .iter()
            .filter(|metric| metric.name == "nestjs.runtime.gc.collections.by_kind")
            .collect();
        assert_eq!(by_kind.len(), 2);
        let durations: Vec<_> = batch
            .metrics
            .iter()
            .filter(|metric| metric.name == "nestjs.runtime.gc.duration.by_kind")
            .collect();
        assert_eq!(durations.len(), 2);
    }

    #[test]
    fn gc_breakdown_still_accepts_bare_numbers() {
        let payload = br#"{"serviceId":"api","runtime":{"g":{"b":{"m":2,"j":1}}}}"#;
        let batch = decode(payload, false).unwrap();
        let by_kind: Vec<_> = batch
            .metrics
            .iter()
            .filter(|metric| metric.name == "nestjs.runtime.gc.collections.by_kind")
            .collect();
        assert_eq!(by_kind.len(), 2);
    }

    #[test]
    fn gc_counters_are_deltas_not_gauges() {
        let payload = br#"{"serviceId":"api","runtime":{"g":{"c":3,"td":12.5}}}"#;
        let batch = decode(payload, false).unwrap();
        let collections = batch
            .metrics
            .iter()
            .find(|metric| metric.name == "nestjs.runtime.gc.collections")
            .unwrap();
        assert_eq!(collections.kind, MetricKind::Counter);
    }

    #[test]
    fn an_unlabelled_counter_yields_one_series_with_no_label_attributes() {
        // The SDK sends `{"default": n}` for a metric that was never labelled.
        let payload = br#"{"serviceId":"api","custom":[
            {"n":"orders.placed","t":"counter","v":{"default":42},
             "iv":{"default":7},"tg":{"region":"eu"}}
        ]}"#;
        let batch = decode(payload, false).unwrap();
        assert_eq!(batch.metrics.len(), 1);
        let metric = &batch.metrics[0];
        assert_eq!(metric.name, "nestjs.custom.orders.placed");
        assert_eq!(metric.kind, MetricKind::Counter);
        assert_eq!(metric.attributes.get("region").unwrap(), "eu");
        // `default` is not a label, so it must not become an attribute.
        assert!(!metric.attributes.contains_key("default"));
        // The delta is exported, not the cumulative total.
        assert!(matches!(metric.value, MetricValue::F64(value) if value == 7.0));
    }

    #[test]
    fn a_labelled_counter_becomes_one_point_per_label_set() {
        let payload = br#"{"serviceId":"api","custom":[
            {"n":"logins","t":"counter",
             "iv":{"{\"route\":\"/login\"}":3,"{\"route\":\"/sso\"}":5}}
        ]}"#;
        let batch = decode(payload, false).unwrap();
        assert_eq!(batch.metrics.len(), 2);
        // The serialized label key is decoded back into a real attribute.
        let login = batch
            .metrics
            .iter()
            .find(|metric| metric.attributes.get("route") == Some(&Value::String("/login".into())))
            .unwrap();
        assert!(matches!(login.value, MetricValue::F64(value) if value == 3.0));
    }

    #[test]
    fn a_counter_without_a_delta_falls_back_to_its_total() {
        let payload = br#"{"serviceId":"api","custom":[
            {"n":"orders.placed","t":"counter","v":{"default":42}}
        ]}"#;
        let batch = decode(payload, false).unwrap();
        assert!(matches!(batch.metrics[0].value, MetricValue::F64(value) if value == 42.0));
    }

    #[test]
    fn a_gauge_reports_its_current_value() {
        let payload = br#"{"serviceId":"api","custom":[
            {"n":"queue.depth","t":"gauge","v":{"default":17}}
        ]}"#;
        let batch = decode(payload, false).unwrap();
        assert_eq!(batch.metrics[0].kind, MetricKind::Gauge);
        assert!(matches!(batch.metrics[0].value, MetricValue::F64(value) if value == 17.0));
    }

    #[test]
    fn a_summary_becomes_one_gauge_per_quantile() {
        let payload = br#"{"serviceId":"api","custom":[
            {"n":"checkout.ms","t":"summary",
             "q50":{"default":12.0},"q95":{"default":40.0},"q99":{"default":90.0},
             "ct":{"default":100},"sm":{"default":2000}}
        ]}"#;
        let batch = decode(payload, false).unwrap();
        let quantiles: Vec<_> = batch
            .metrics
            .iter()
            .filter(|metric| metric.name == "nestjs.custom.checkout.ms")
            .collect();
        assert_eq!(quantiles.len(), 3);
        assert!(quantiles.iter().any(|metric| {
            metric.attributes.get("quantile") == Some(&Value::String("0.95".to_string()))
        }));
        assert!(
            batch
                .metrics
                .iter()
                .any(|metric| metric.name == "nestjs.custom.checkout.ms.count")
        );
    }

    #[test]
    fn a_custom_metric_does_not_reject_the_batch_it_rides_in() {
        // Regression: typing `v` as a number made serde fail the entire batch - the
        // snapshots included - over one custom metric's per-label map.
        let payload = br#"{"serviceId":"api",
            "snapshots":[{"ti":"t1","p":"http","op":"/x","d":1}],
            "custom":[{"n":"c","t":"counter","v":{"default":1}}]}"#;
        let batch = decode(payload, false).unwrap();
        assert_eq!(batch.operations.len(), 1);
        assert_eq!(batch.metrics.len(), 1);
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
}
