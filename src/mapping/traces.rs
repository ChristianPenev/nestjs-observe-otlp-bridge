//! Observe operations into OTLP spans.
//!
//! Each operation becomes a tree: a synthesized root span for the entry point - the
//! request, the job - with the SDK's span forest beneath it. The root is synthesized
//! because Observe does not send one; a snapshot *is* the root, carrying the route,
//! status and duration as fields of its own, while `t` holds only the calls made
//! underneath it.
//!
//! Timing is reconstructed rather than read. The only absolute timestamp on the wire
//! is the snapshot's `calledAt` - the SDK deletes `startTimestamp` before encoding -
//! and every span underneath carries `so`, its offset in milliseconds from the start
//! of the operation. So a span's start is `calledAt + so`, and its end is that plus
//! `d`.

use chrono::{DateTime, Utc};
use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
use opentelemetry_proto::tonic::resource::v1::Resource;
use opentelemetry_proto::tonic::trace::v1::{
    ResourceSpans, ScopeSpans, Span as OtlpSpan, Status,
    span::{Event, SpanKind},
    status::StatusCode,
};
use std::collections::HashSet;

use crate::mapping::attributes::{self as attrs, AttributeBuilder};
use crate::mapping::ids;
use crate::observe::model::*;

/// Status codes at or above this are the server's own failure, which the HTTP
/// conventions say must set an error status even when nothing threw.
const SERVER_ERROR_FLOOR: i64 = 500;

/// Builds the `ResourceSpans` for a batch.
///
/// `received_at` stands in for any operation the SDK sent without a `calledAt`.
/// Dropping those would lose real traces; dating them at receipt is off by at most
/// one flush interval, and the batch is the only clock available.
pub fn map(batch: &Batch, received_at: DateTime<Utc>) -> Option<ResourceSpans> {
    let spans: Vec<OtlpSpan> = batch
        .operations
        .iter()
        .flat_map(|operation| map_operation(operation, received_at))
        .collect();

    if spans.is_empty() {
        return None;
    }

    Some(ResourceSpans {
        resource: Some(resource(&batch.service)),
        scope_spans: vec![ScopeSpans {
            scope: Some(scope()),
            spans,
            schema_url: String::new(),
        }],
        schema_url: String::new(),
    })
}

/// The OTLP resource for a service.
pub fn resource(service: &Service) -> Resource {
    let mut builder = AttributeBuilder::new();
    builder.str(attrs::SERVICE_NAME, service.id.clone());
    builder.maybe_str(attrs::SERVICE_VERSION, service.version.clone());
    // Named after the SDK that produced the telemetry, not after this bridge: the
    // instrumentation really is `@nestjs/observe`, and a backend grouping by SDK
    // should see that.
    builder.str(attrs::TELEMETRY_SDK_NAME, "nestjs-observe");
    builder.str(attrs::TELEMETRY_SDK_LANGUAGE, "nodejs");
    Resource {
        attributes: builder.build(),
        ..Default::default()
    }
}

/// The scope every span and log from this bridge is attributed to.
pub fn scope() -> InstrumentationScope {
    InstrumentationScope {
        name: env!("CARGO_PKG_NAME").to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        ..Default::default()
    }
}

fn map_operation(operation: &Operation, received_at: DateTime<Utc>) -> Vec<OtlpSpan> {
    let trace_id = ids::trace_id(&operation.trace_id).to_vec();
    // The synthesized root has no Observe span of its own - the snapshot *is* the
    // root - so it always takes the positional id for the empty path.
    let root_id = ids::span_id_at(&operation.trace_id, &[]).to_vec();

    let start = operation.started_at.unwrap_or(received_at);
    let start_nanos = to_nanos(start);
    let end_nanos = start_nanos + millis_to_nanos(operation.duration_ms.unwrap_or(0.0));

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    out.push(OtlpSpan {
        trace_id: trace_id.clone(),
        span_id: root_id.clone(),
        parent_span_id: Vec::new(),
        name: operation.name.clone(),
        kind: root_kind(&operation.kind) as i32,
        start_time_unix_nano: start_nanos,
        end_time_unix_nano: end_nanos,
        attributes: attrs::root_attributes(operation),
        events: error_events(operation.error.as_ref(), end_nanos),
        status: Some(root_status(operation)),
        ..Default::default()
    });

    let mut walk = Walk {
        trace_id: &trace_id,
        observe_trace_id: &operation.trace_id,
        operation_start_nanos: start_nanos,
        seen: &mut seen,
        out: &mut out,
    };
    for (index, span) in operation.spans.iter().enumerate() {
        walk.visit(span, &root_id, &mut vec![index]);
    }

    out
}

/// The state carried down a span tree while it is being translated.
///
/// Everything here is fixed for one operation except `seen` and `out`, which
/// accumulate. Bundled rather than passed as eight parameters, so recursing reads as
/// "visit this child" instead of restating the whole context at each level.
struct Walk<'a> {
    trace_id: &'a [u8],
    observe_trace_id: &'a str,
    operation_start_nanos: u64,
    /// Span ids already issued for this operation, so a duplicate cannot silently
    /// re-parent everything beneath it.
    seen: &'a mut HashSet<[u8; 8]>,
    out: &'a mut Vec<OtlpSpan>,
}

impl Walk<'_> {
    /// Translates one span and everything below it.
    ///
    /// `path` is the child-index route from the root, used only as a fallback id
    /// for a span that arrived without one of its own.
    fn visit(&mut self, span: &Span, parent_id: &[u8], path: &mut Vec<usize>) {
        // The span's own id when it sent one, so a log line naming it links to
        // exactly this span; its position otherwise.
        //
        // The `seen` check is a guard, not an expectation: the SDK mints a UUIDv7
        // per invocation, so ids are unique in practice. But two spans sharing one
        // would make the parent of everything below them ambiguous and quietly
        // corrupt the waterfall, and falling back to position avoids that.
        let span_id = match span.observe_span_id.as_deref() {
            Some(id) => {
                let derived = ids::span_id_from(id);
                if self.seen.insert(derived) {
                    derived.to_vec()
                } else {
                    ids::span_id_at(self.observe_trace_id, path).to_vec()
                }
            }
            None => ids::span_id_at(self.observe_trace_id, path).to_vec(),
        };
        let start_nanos =
            self.operation_start_nanos + millis_to_nanos(span.start_offset_ms.unwrap_or(0.0));
        let end_nanos = start_nanos + millis_to_nanos(span.duration_ms.unwrap_or(0.0));

        let error = match &span.error {
            Some(SpanOutcome::Error(error)) => Some(error),
            _ => None,
        };

        self.out.push(OtlpSpan {
            trace_id: self.trace_id.to_vec(),
            span_id: span_id.clone(),
            parent_span_id: parent_id.to_vec(),
            name: span.name.clone(),
            kind: span_kind(span) as i32,
            start_time_unix_nano: start_nanos,
            end_time_unix_nano: end_nanos,
            attributes: attrs::span_attributes(span),
            events: error_events(error, end_nanos),
            status: Some(span_status(span)),
            ..Default::default()
        });

        for (index, child) in span.children.iter().enumerate() {
            path.push(index);
            self.visit(child, &span_id, path);
            path.pop();
        }
    }
}

fn root_kind(kind: &OperationKind) -> SpanKind {
    match kind {
        // A job is work taken off a queue, which the messaging conventions model as
        // a consumer rather than a server.
        OperationKind::Job(_) => SpanKind::Consumer,
        OperationKind::Request(_) => SpanKind::Server,
    }
}

fn span_kind(span: &Span) -> SpanKind {
    if span.component.is_outgoing() {
        SpanKind::Client
    } else {
        SpanKind::Internal
    }
}

/// The root span's status.
///
/// For an **HTTP** request the response code decides, and nothing else: the HTTP
/// conventions say a server span is an error only on 5xx. That rule matters more in
/// a Nest application than in most, because Nest answers "not found", "forbidden"
/// and "invalid input" by *throwing* - so a captured `NotFoundException` sits on a
/// perfectly ordinary 404. Letting the exception decide would mark every such
/// request failed and make a service's error rate meaningless. The exception is
/// still recorded as an event either way; only the status is withheld.
///
/// For **everything else** the error decides, because there is no status code worth
/// trusting. A GraphQL operation answers 200 with an `errors` array, and jobs and
/// RPC messages have no status of their own.
fn root_status(operation: &Operation) -> Status {
    let error_status = || Status {
        code: StatusCode::Error as i32,
        message: operation
            .error
            .as_ref()
            .map(error_message)
            .unwrap_or_default(),
    };

    let is_http = matches!(
        &operation.kind,
        OperationKind::Request(request) if request.protocol == Protocol::Http
    );

    let status_code = match &operation.kind {
        OperationKind::Request(request) => request.status_code,
        OperationKind::Job(_) => None,
    };

    if is_http {
        return if status_code.is_some_and(|code| code >= SERVER_ERROR_FLOOR) {
            error_status()
        } else {
            Status {
                code: StatusCode::Unset as i32,
                message: String::new(),
            }
        };
    }

    if operation.error.is_some() || status_code.is_some_and(|code| code >= SERVER_ERROR_FLOOR) {
        return error_status();
    }

    Status {
        code: StatusCode::Unset as i32,
        message: String::new(),
    }
}

fn span_status(span: &Span) -> Status {
    match &span.error {
        // The SDK's "something failed here but was not captured" form. It still has
        // to set an error status, or a failing call reads as a successful one.
        Some(SpanOutcome::Failed) => Status {
            code: StatusCode::Error as i32,
            message: String::new(),
        },
        Some(SpanOutcome::Error(error)) => Status {
            code: StatusCode::Error as i32,
            message: error_message(error),
        },
        None => Status {
            code: StatusCode::Unset as i32,
            message: String::new(),
        },
    }
}

fn error_message(error: &ErrorInfo) -> String {
    error
        .message
        .clone()
        .or_else(|| error.class_name.clone())
        .unwrap_or_default()
}

/// The `exception` event for a captured error.
///
/// Recorded at the span's end, which is the closest the wire allows: the SDK sends
/// no timestamp with an error, only the span it belonged to.
fn error_events(error: Option<&ErrorInfo>, end_nanos: u64) -> Vec<Event> {
    let Some(error) = error else {
        return Vec::new();
    };
    vec![Event {
        time_unix_nano: end_nanos,
        name: "exception".to_string(),
        attributes: attrs::exception_attributes(error),
        ..Default::default()
    }]
}

fn to_nanos(timestamp: DateTime<Utc>) -> u64 {
    timestamp.timestamp_nanos_opt().unwrap_or(0).max(0) as u64
}

/// Milliseconds to nanoseconds, saturating at zero.
///
/// A negative duration is nonsense the wire permits; clamping keeps it from
/// wrapping into an enormous unsigned value and producing a span that appears to
/// last centuries.
fn millis_to_nanos(millis: f64) -> u64 {
    if !millis.is_finite() || millis <= 0.0 {
        return 0;
    }
    (millis * 1_000_000.0) as u64
}

#[cfg(test)]
mod tests;
