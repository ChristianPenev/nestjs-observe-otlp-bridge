//! The `@nestjs/observe` wire format, transcribed from the SDK's own encoders.
//!
//! Every field below is a single-letter key the SDK's `src/encoders/*` map onto,
//! cross-checked against `agent/telemetry-wire-contract.js` - the SDK's hand-written
//! copy of the agreement it has with the hosted collector. Both were read from
//! `@nestjs/observe@0.3.5`; `docs/protocol.md` records the provenance of each key.
//!
//! The single-letter keys are scoped to their section and collide across sections:
//! `JobSnapshot::c` is `calledAt` and `JobSnapshot::s` is `status`, while on a
//! `TraceNode` those same letters mean `className` and `spanId`. The two must never
//! share a struct.
//!
//! Where the SDK's source and its output disagree, these types follow the output,
//! captured from a real application - see `tests/fixtures`. Three places differ from
//! what `telemetry-wire-contract.js` declares, and each is noted on the field:
//! `GcBreakdown`'s buckets, `CustomMetric::value` and `CustomMetric::increase`.

// This module is a complete transcription of the wire contract, not only the part
// the mapper happens to read. A field with no OTLP counterpart - `forwardLogs`, the
// objective declarations - is still part of the record of what 0.3.5 sends, and
// deleting it would make the next SDK diff harder to read, not easier.
#![allow(dead_code)]

use serde::Deserialize;
use serde_json::{Map, Value};

/// Treats an explicit JSON `null` as an absent field.
///
/// The SDK omits empty sections, but its 400-repair path can null one out rather
/// than delete it, and a batch is worth more than the section that went missing.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// The batch body the agent POSTs to `{endpoint}/applications/telemetry`.
#[derive(Debug, Clone, Deserialize)]
pub struct TelemetryBatch {
    #[serde(rename = "serviceId")]
    pub service_id: String,
    #[serde(rename = "serviceVersion", default)]
    pub service_version: Option<String>,
    #[serde(rename = "forwardLogs", default)]
    pub forward_logs: Option<bool>,
    #[serde(default, deserialize_with = "null_default")]
    pub snapshots: Vec<RequestSnapshot>,
    #[serde(default, deserialize_with = "null_default")]
    pub jobs: Vec<JobSnapshot>,
    #[serde(default)]
    pub runtime: Option<RuntimeMetrics>,
    #[serde(default, deserialize_with = "null_default")]
    pub custom: Vec<CustomMetric>,
    #[serde(default, deserialize_with = "null_default")]
    pub logs: Vec<LogEntry>,
    #[serde(default, deserialize_with = "null_default")]
    pub objectives: Vec<ObjectiveDeclaration>,
}

/// One entry-point execution: an HTTP request, GraphQL operation, RPC message or
/// WebSocket message, with the tree of provider calls it made.
#[derive(Debug, Clone, Deserialize)]
pub struct RequestSnapshot {
    /// `calledAt` - ISO 8601. The only absolute time anchor on the wire: the SDK
    /// deletes `startTimestamp` before encoding, so span times are reconstructed
    /// from this plus each node's `so`.
    #[serde(rename = "ct", default)]
    pub called_at: Option<String>,
    /// `traceId` - a UUIDv7 the agent minted, or a propagated `x-request-id`
    /// matching `^[A-Za-z0-9._:-]{1,128}$`. Not necessarily 16 bytes.
    #[serde(rename = "ti")]
    pub trace_id: String,
    #[serde(rename = "d", default)]
    pub duration_ms: Option<f64>,
    /// `http` | `graphql` | `rpc` | `grpc` | `ws`
    #[serde(rename = "p", default)]
    pub protocol: Option<String>,
    #[serde(rename = "op", default)]
    pub operation_id: Option<String>,
    #[serde(rename = "u", default)]
    pub user_id: Option<String>,
    #[serde(rename = "tg", default)]
    pub tags: Option<Map<String, Value>>,
    #[serde(rename = "e", default)]
    pub error: Option<ErrorDetail>,
    /// Headers, and the body when `http.capture` asked for it.
    #[serde(rename = "rq", default)]
    pub request: Option<Value>,
    #[serde(rename = "a", default)]
    pub attributes: Option<RequestAttributes>,
    #[serde(rename = "t", default, deserialize_with = "null_default")]
    pub traces: Vec<TraceNode>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RequestAttributes {
    /// HTTP method.
    #[serde(rename = "m", default)]
    pub method: Option<String>,
    #[serde(rename = "sc", default)]
    pub status_code: Option<i64>,
    /// For GraphQL this carries the sanitized document rather than the URL.
    #[serde(rename = "ou", default)]
    pub original_url: Option<String>,
}

/// A BullMQ / Bull / `@nestjs/schedule` execution.
#[derive(Debug, Clone, Deserialize)]
pub struct JobSnapshot {
    #[serde(rename = "i")]
    pub id: String,
    #[serde(rename = "ti", default)]
    pub trace_id: Option<String>,
    #[serde(rename = "n", default)]
    pub name: Option<String>,
    #[serde(rename = "q", default)]
    pub queue_name: Option<String>,
    /// `status` here - not `spanId`, which is what `s` means on a `TraceNode`.
    #[serde(rename = "s", default)]
    pub status: Option<String>,
    /// `calledAt` here - not `className`, which is what `c` means on a `TraceNode`.
    #[serde(rename = "c", default)]
    pub called_at: Option<String>,
    #[serde(rename = "d", default)]
    pub duration_ms: Option<f64>,
    #[serde(rename = "ea", default)]
    pub enqueued_at: Option<String>,
    #[serde(rename = "wd", default)]
    pub wait_duration_ms: Option<f64>,
    #[serde(rename = "am", default)]
    pub attempts_made: Option<i64>,
    #[serde(rename = "ma", default)]
    pub max_attempts: Option<i64>,
    #[serde(rename = "tg", default)]
    pub tags: Option<Map<String, Value>>,
    #[serde(rename = "e", default)]
    pub error: Option<ErrorDetail>,
    #[serde(rename = "t", default, deserialize_with = "null_default")]
    pub traces: Vec<TraceNode>,
}

/// One provider method call, and everything it called.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TraceNode {
    /// Manual spans only. A collapsed node reads `ValidationPipe.transform x27`.
    #[serde(rename = "n", default)]
    pub name: Option<String>,
    /// `manual` | `auto`.
    #[serde(rename = "o", default)]
    pub origin: Option<String>,
    /// Carries `observe.collapsed` on a collapsed node.
    #[serde(rename = "t", default)]
    pub tags: Option<Map<String, Value>>,
    #[serde(rename = "d", default)]
    pub duration_ms: Option<f64>,
    #[serde(rename = "e", default)]
    pub error: Option<SpanError>,
    #[serde(rename = "c", default)]
    pub class_name: Option<String>,
    #[serde(rename = "m", default)]
    pub method_key: Option<String>,
    /// A UUIDv7 the agent mints for this invocation. Unique per call, and the value
    /// log records reference.
    #[serde(rename = "s", default)]
    pub span_id: Option<String>,
    /// Milliseconds from the start of the enclosing operation.
    #[serde(rename = "so", default)]
    pub start_offset_ms: Option<f64>,
    #[serde(rename = "ch", default, deserialize_with = "null_default")]
    pub children: Vec<TraceNode>,
}

/// A span's error: `true` when one was seen but not captured, an object otherwise.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SpanError {
    Flag(bool),
    Detail(ErrorDetail),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ErrorDetail {
    #[serde(default)]
    pub cls: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub stack: Option<String>,
    #[serde(default)]
    pub tags: Option<Map<String, Value>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LogEntry {
    /// Milliseconds since the epoch.
    pub timestamp: f64,
    pub text: String,
    #[serde(rename = "traceId", default)]
    pub trace_id: Option<String>,
    /// The id of the span that wrote the line, matching a trace node's `s`.
    #[serde(rename = "spanId", default)]
    pub span_id: Option<String>,
    #[serde(default)]
    pub level: Option<String>,
    /// Nest's logger context - usually the class name.
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub attributes: Option<Map<String, Value>>,
}

/// Process-level gauges, sampled on an interval rather than per request.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RuntimeMetrics {
    #[serde(rename = "c", default)]
    pub cpu: Option<CpuMetrics>,
    #[serde(rename = "m", default)]
    pub memory: Option<MemoryMetrics>,
    #[serde(rename = "g", default)]
    pub gc: Option<GcMetrics>,
    #[serde(rename = "e", default)]
    pub event_loop: Option<EventLoopMetrics>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CpuMetrics {
    #[serde(rename = "u", default)]
    pub user: Option<f64>,
    #[serde(rename = "s", default)]
    pub system: Option<f64>,
    #[serde(rename = "p", default)]
    pub percentage_used: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct MemoryMetrics {
    #[serde(rename = "r", default)]
    pub rss: Option<f64>,
    #[serde(rename = "ht", default)]
    pub heap_total: Option<f64>,
    #[serde(rename = "hu", default)]
    pub heap_used: Option<f64>,
    #[serde(rename = "e", default)]
    pub external: Option<f64>,
    #[serde(rename = "ab", default)]
    pub array_buffers: Option<f64>,
    #[serde(rename = "p", default)]
    pub percentage_used: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GcMetrics {
    #[serde(rename = "c", default)]
    pub count: Option<f64>,
    #[serde(rename = "td", default)]
    pub total_duration: Option<f64>,
    #[serde(rename = "b", default)]
    pub breakdown: Option<GcBreakdown>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct GcBreakdown {
    #[serde(rename = "m", default)]
    pub minor: Option<GcBucket>,
    /// `major` - keyed `j` because `m` was already taken by `minor`.
    #[serde(rename = "j", default)]
    pub major: Option<GcBucket>,
    #[serde(rename = "i", default)]
    pub incremental: Option<GcBucket>,
}

/// One collection kind's contribution to a window.
///
/// Accepts both shapes the SDK could send. `telemetry-wire-contract.js` declares
/// these as plain numbers, but `NodeRuntimeMetricsService` accumulates
/// `{ count, duration }` objects and the encoder copies them across untouched - so
/// 0.3.5 puts an object on the wire where its own contract expects a number. The
/// richer shape is read when it arrives and a bare number is still accepted, so this
/// keeps working whichever side is eventually corrected.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum GcBucket {
    Count(f64),
    Detailed {
        #[serde(default)]
        count: Option<f64>,
        #[serde(default)]
        duration: Option<f64>,
    },
}

impl GcBucket {
    pub fn count(&self) -> Option<f64> {
        match self {
            GcBucket::Count(value) => Some(*value),
            GcBucket::Detailed { count, .. } => *count,
        }
    }

    pub fn duration_ms(&self) -> Option<f64> {
        match self {
            GcBucket::Count(_) => None,
            GcBucket::Detailed { duration, .. } => *duration,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EventLoopMetrics {
    #[serde(rename = "l", default)]
    pub lag: Option<f64>,
    #[serde(rename = "u", default)]
    pub utilization: Option<f64>,
}

/// An application-declared counter, gauge or summary.
#[derive(Debug, Clone, Deserialize)]
pub struct CustomMetric {
    #[serde(rename = "n")]
    pub name: String,
    /// `counter` | `gauge` | `summary`.
    #[serde(rename = "t", default)]
    pub metric_type: Option<String>,
    /// The current reading *per label set*, not a single number.
    ///
    /// `telemetry-wire-contract.js` declares this a number, but `Counter` and
    /// `Gauge` both hold `{ [stringifiedLabel]: number }` and the encoder copies it
    /// across untouched - an unlabelled metric arrives as `{"default": 42}`. Typing
    /// this `f64` made serde reject the whole batch over one custom metric, so it is
    /// read as a value and flattened by the decoder.
    #[serde(rename = "v", default)]
    pub value: Option<Value>,
    #[serde(rename = "tg", default)]
    pub tags: Option<Map<String, Value>>,
    #[serde(rename = "d", default)]
    pub description: Option<String>,
    #[serde(rename = "l", default)]
    pub labels: Option<Map<String, Value>>,
    #[serde(rename = "lu", default)]
    pub last_updated: Option<f64>,
    #[serde(rename = "k", default)]
    pub kind: Option<String>,
    /// How much each label set rose since the last successful flush. Same per-label
    /// shape as `value`.
    #[serde(rename = "iv", default)]
    pub increase: Option<Value>,
    /// Summary quantiles, each a map of label-set to value.
    #[serde(rename = "q50", default)]
    pub p50: Option<Value>,
    #[serde(rename = "q95", default)]
    pub p95: Option<Value>,
    #[serde(rename = "q99", default)]
    pub p99: Option<Value>,
    #[serde(rename = "ct", default)]
    pub observations: Option<Value>,
    #[serde(rename = "sm", default)]
    pub total: Option<Value>,
    #[serde(rename = "mx", default)]
    pub maximum: Option<Value>,
}

/// An `@Objective()` declaration. Carried for completeness; SLO definitions have no
/// OTLP counterpart, so the mapper drops them rather than inventing one.
#[derive(Debug, Clone, Deserialize)]
pub struct ObjectiveDeclaration {
    pub handler: String,
    #[serde(rename = "operationId")]
    pub operation_id: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    pub objectives: Vec<Value>,
}
