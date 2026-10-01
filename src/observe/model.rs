//! The normalized telemetry model.
//!
//! Shaped by neither the wire format nor OTLP, so the two can change independently:
//! an SDK key rename touches the decoder, a semantic-convention revision touches the
//! mapper, and neither reaches the other through here. Everything the wire leaves
//! implicit is already resolved by the time it lands in these types.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::BTreeMap;

/// Attribute values, narrowed to what OTLP can carry.
pub type Attributes = BTreeMap<String, Value>;

/// One decoded batch.
#[derive(Debug, Clone)]
pub struct Batch {
    pub service: Service,
    /// Requests and jobs, unified: both are an entry point with a span forest, and
    /// every difference between them lives in `Operation::kind`.
    pub operations: Vec<Operation>,
    pub logs: Vec<LogRecord>,
    pub metrics: Vec<Metric>,
}

impl Batch {
    /// Whether the batch carried nothing worth exporting.
    ///
    /// A batch of only `objectives` is empty by this measure, which is correct: SLO
    /// declarations have no OTLP counterpart and are dropped.
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty() && self.logs.is_empty() && self.metrics.is_empty()
    }

    /// Total spans, counted the way the SDK's own meter counts them - every node,
    /// children included. Used for logging throughput, not for billing.
    pub fn span_count(&self) -> usize {
        self.operations
            .iter()
            .map(|operation| 1 + count_spans(&operation.spans))
            .sum()
    }
}

fn count_spans(spans: &[Span]) -> usize {
    spans
        .iter()
        .map(|span| 1 + count_spans(&span.children))
        .sum()
}

/// What the telemetry is about. Becomes the OTLP `Resource`.
#[derive(Debug, Clone)]
pub struct Service {
    pub id: String,
    pub version: Option<String>,
}

/// An entry-point execution and the tree of calls it made.
#[derive(Debug, Clone)]
pub struct Operation {
    /// The Observe trace id, still in its original form. Converting it to OTLP's 16
    /// bytes is the mapper's job, and the raw value is kept so log records can be
    /// correlated against it by the same rule.
    pub trace_id: String,
    pub kind: OperationKind,
    /// What the root span is called.
    pub name: String,
    /// Absolute start, from the snapshot's `calledAt`. `None` when the SDK sent no
    /// timestamp, which the mapper has to resolve against its own clock.
    pub started_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<f64>,
    pub tags: Attributes,
    pub error: Option<ErrorInfo>,
    /// The span forest. Usually one root, but the wire permits several.
    pub spans: Vec<Span>,
}

#[derive(Debug, Clone)]
pub enum OperationKind {
    Request(RequestInfo),
    Job(JobInfo),
}

#[derive(Debug, Clone)]
pub struct RequestInfo {
    pub protocol: Protocol,
    /// The matched route for HTTP (`/users/:id`), `Type.field` for GraphQL,
    /// `gateway:pattern` for WebSockets, the pattern for RPC.
    pub operation_id: Option<String>,
    pub method: Option<String>,
    pub status_code: Option<i64>,
    /// The transport URL, or the sanitized document for GraphQL.
    pub original_url: Option<String>,
    pub user_id: Option<String>,
    /// Captured headers and body, when `http.capture` asked for them. Passed through
    /// as JSON: it is already redacted by the SDK, and its shape is the SDK's.
    pub captured_request: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: String,
    pub name: Option<String>,
    pub queue_name: Option<String>,
    pub status: Option<String>,
    pub enqueued_at: Option<String>,
    pub wait_duration_ms: Option<f64>,
    pub attempts_made: Option<i64>,
    pub max_attempts: Option<i64>,
    pub source: JobSource,
}

/// What made a job run.
///
/// `@nestjs/schedule` and BullMQ share the `jobs` section with nothing to tell them
/// apart, except that the schedule instrumentation puts the scheduler kind - `cron`,
/// `interval`, `timeout`, `schedule` - where a queue name would go. Enough: a timer
/// firing is not a message being consumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobSource {
    Queue,
    Scheduled,
}

/// The transport an operation arrived on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Protocol {
    Http,
    GraphQl,
    Rpc,
    Grpc,
    WebSocket,
    /// Something a later SDK introduced. Carried verbatim rather than dropped, so a
    /// new transport still exports - unmapped, but present.
    Other(String),
}

impl Protocol {
    pub fn as_str(&self) -> &str {
        match self {
            Protocol::Http => "http",
            Protocol::GraphQl => "graphql",
            Protocol::Rpc => "rpc",
            Protocol::Grpc => "grpc",
            Protocol::WebSocket => "ws",
            Protocol::Other(value) => value,
        }
    }
}

/// One call within an operation.
#[derive(Debug, Clone)]
pub struct Span {
    /// What the span is called in a waterfall.
    pub name: String,
    /// Which Nest concept ran, as far as it can be established. See
    /// `observe::component` for how it is inferred and why it has to be - the wire
    /// does not carry it.
    pub component: Component,
    /// `None` for an outgoing call, whose "class" is a driver name rather than a
    /// Nest class.
    pub class_name: Option<String>,
    pub method_name: Option<String>,
    /// The SDK's `s`: a UUIDv7 minted per invocation. Kept in its original form
    /// because log records carry the same value, and because it is the id the hosted
    /// collector would have stored. Documented as optional, occasionally absent.
    pub observe_span_id: Option<String>,
    /// A span the application opened itself, rather than one instrumentation did.
    pub manual: bool,
    /// Milliseconds from the start of the enclosing operation.
    pub start_offset_ms: Option<f64>,
    pub duration_ms: Option<f64>,
    pub tags: Attributes,
    pub error: Option<SpanOutcome>,
    /// How many identical sibling calls this node stands for, when the SDK collapsed
    /// a repeated frame. Its `duration_ms` is then their sum, not a wall-clock span.
    pub collapsed_count: Option<i64>,
    pub children: Vec<Span>,
}

/// Which Nest concept a span represents.
///
/// Inferred, not received. Anything unrecognised stays `Provider`, which is what a
/// plain injectable is anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    Controller,
    Service,
    Repository,
    Guard,
    Interceptor,
    Pipe,
    ExceptionFilter,
    Middleware,
    GraphQlResolver,
    WebSocketGateway,
    QueueConsumer,
    ScheduledTask,
    /// An outbound database query, recorded against a driver rather than a class.
    DatabaseClient,
    /// An outbound HTTP call.
    HttpClient,
    /// A span the application opened by hand.
    Manual,
    /// An injectable that matched nothing more specific.
    Provider,
}

impl Component {
    /// The value written to `nestjs.type`. Snake case, matching the namespace's own
    /// style rather than Nest's class names.
    pub fn as_str(&self) -> &'static str {
        match self {
            Component::Controller => "controller",
            Component::Service => "service",
            Component::Repository => "repository",
            Component::Guard => "guard",
            Component::Interceptor => "interceptor",
            Component::Pipe => "pipe",
            Component::ExceptionFilter => "exception_filter",
            Component::Middleware => "middleware",
            Component::GraphQlResolver => "graphql_resolver",
            Component::WebSocketGateway => "websocket_gateway",
            Component::QueueConsumer => "queue_consumer",
            Component::ScheduledTask => "scheduled_task",
            Component::DatabaseClient => "database_client",
            Component::HttpClient => "http_client",
            Component::Manual => "manual",
            Component::Provider => "provider",
        }
    }

    /// Whether this span represents a call leaving the process, which OTLP models as
    /// a client span rather than an internal one.
    pub fn is_outgoing(&self) -> bool {
        matches!(self, Component::DatabaseClient | Component::HttpClient)
    }
}

/// What happened to a span that did not succeed.
#[derive(Debug, Clone)]
pub enum SpanOutcome {
    /// The SDK saw a failure but captured no detail - every failed non-root span
    /// reports this. It still has to set the span's status, or a failing call looks
    /// successful in the backend.
    Failed,
    Error(ErrorInfo),
}

#[derive(Debug, Clone)]
pub struct ErrorInfo {
    /// The exception's constructor name, e.g. `NotFoundException`.
    pub class_name: Option<String>,
    pub message: Option<String>,
    pub stack: Option<String>,
    pub tags: Attributes,
}

/// A log line, already correlated to its trace by the SDK.
#[derive(Debug, Clone)]
pub struct LogRecord {
    pub timestamp: DateTime<Utc>,
    pub body: String,
    pub severity: Severity,
    /// The level exactly as the SDK sent it, for `nestjs.log.level`. Nest's own
    /// levels do not all have an OTLP equivalent, so the original is preserved
    /// alongside the mapped one.
    pub raw_level: Option<String>,
    pub trace_id: Option<String>,
    /// The id of the span that wrote the line, matching some
    /// `Span::observe_span_id` - though not necessarily one in this batch, since
    /// logs and traces flush independently.
    pub observe_span_id: Option<String>,
    /// Nest's logger context, usually the emitting class.
    pub context: Option<String>,
    pub attributes: Attributes,
}

/// OTLP severity, as both a number and a name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
    Unspecified,
}

impl Severity {
    /// The OTLP `SeverityNumber`.
    pub fn number(&self) -> i32 {
        match self {
            Severity::Trace => 1,
            Severity::Debug => 5,
            Severity::Info => 9,
            Severity::Warn => 13,
            Severity::Error => 17,
            Severity::Fatal => 21,
            Severity::Unspecified => 0,
        }
    }

    /// The OTLP `SeverityText`.
    pub fn text(&self) -> Option<&'static str> {
        match self {
            Severity::Trace => Some("TRACE"),
            Severity::Debug => Some("DEBUG"),
            Severity::Info => Some("INFO"),
            Severity::Warn => Some("WARN"),
            Severity::Error => Some("ERROR"),
            Severity::Fatal => Some("FATAL"),
            Severity::Unspecified => None,
        }
    }
}

/// A single metric data point.
///
/// Flattened deliberately: the wire sends two differently-shaped sections, OTLP wants
/// one stream per measurement, and splitting in the decoder keeps the mapper from
/// having to know which section a number came from.
#[derive(Debug, Clone)]
pub struct Metric {
    /// A dotted OTLP metric name, e.g. `nestjs.runtime.memory.heap.used`.
    pub name: String,
    pub description: Option<String>,
    /// A UCUM unit, or `None` when the measurement is dimensionless.
    pub unit: Option<&'static str>,
    pub kind: MetricKind,
    pub value: MetricValue,
    pub attributes: Attributes,
    /// When the value was observed. `None` means the mapper stamps it with receipt
    /// time, which is the best available for a gauge the SDK did not date.
    pub timestamp: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetricKind {
    /// Monotonic cumulative total.
    Counter,
    /// A value that can move in either direction.
    Gauge,
}

#[derive(Debug, Clone)]
pub enum MetricValue {
    F64(f64),
}
