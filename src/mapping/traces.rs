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
use opentelemetry_proto::tonic::common::v1::{InstrumentationScope, KeyValue};
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
        attributes: root_attributes(operation),
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

fn root_attributes(operation: &Operation) -> Vec<KeyValue> {
    let mut builder = AttributeBuilder::new();
    builder.tags(&operation.tags);

    match &operation.kind {
        OperationKind::Request(request) => {
            builder.str(attrs::NESTJS_PROTOCOL, request.protocol.as_str());
            builder.maybe_str(attrs::NESTJS_OPERATION_ID, request.operation_id.clone());
            builder.maybe_str(attrs::NESTJS_USER_ID, request.user_id.clone());

            match request.protocol {
                Protocol::Http => {
                    builder.maybe_str(attrs::HTTP_REQUEST_METHOD, request.method.clone());
                    builder.maybe_int(attrs::HTTP_RESPONSE_STATUS_CODE, request.status_code);
                    // The SDK's operation id for HTTP is the matched route template,
                    // which is exactly what `http.route` means.
                    builder.maybe_str(attrs::HTTP_ROUTE, request.operation_id.clone());
                    builder.maybe_str(attrs::URL_PATH, request.original_url.clone());
                }
                Protocol::GraphQl => {
                    builder.maybe_str(attrs::GRAPHQL_OPERATION_NAME, request.operation_id.clone());
                    // For GraphQL the SDK puts the sanitized document where a URL
                    // would go, so it is the document that is recorded - under both
                    // the convention's key and the `nestjs.*` one, since the
                    // convention is still experimental.
                    if let Some(document) = &request.original_url {
                        builder.str(attrs::GRAPHQL_DOCUMENT, document.clone());
                        builder.str(attrs::NESTJS_GRAPHQL_DOCUMENT, document.clone());
                    }
                    builder.maybe_int(attrs::HTTP_RESPONSE_STATUS_CODE, request.status_code);
                }
                Protocol::Rpc | Protocol::Grpc => {
                    builder.str(
                        attrs::RPC_SYSTEM,
                        if request.protocol == Protocol::Grpc {
                            "grpc"
                        } else {
                            "nestjs_microservice"
                        },
                    );
                    builder.maybe_str(attrs::RPC_METHOD, request.operation_id.clone());
                }
                Protocol::WebSocket | Protocol::Other(_) => {
                    builder.maybe_str(attrs::URL_PATH, request.original_url.clone());
                }
            }

            if let Some(captured) = &request.captured_request
                && let Some(value) = attrs::to_any_value(captured)
            {
                builder.set(attrs::NESTJS_CAPTURED_REQUEST, value);
            }
        }
        OperationKind::Job(job) => {
            builder.str(attrs::NESTJS_PROTOCOL, "job");
            builder.str(attrs::NESTJS_JOB_ID, job.id.clone());
            builder.maybe_str(attrs::NESTJS_JOB_QUEUE, job.queue_name.clone());

            match job.source {
                JobSource::Queue => {
                    builder.str(attrs::NESTJS_TYPE, Component::QueueConsumer.as_str());
                    builder.str(attrs::MESSAGING_SYSTEM, "nestjs_queue");
                    builder.str(attrs::MESSAGING_OPERATION_NAME, "process");
                    builder.maybe_str(attrs::MESSAGING_DESTINATION_NAME, job.queue_name.clone());
                    builder.str(attrs::MESSAGING_MESSAGE_ID, job.id.clone());
                }
                // A timer firing is not a message being consumed, so the messaging
                // conventions are deliberately not set here - a backend's queue
                // views would otherwise count cron runs as queue traffic.
                JobSource::Scheduled => {
                    builder.str(attrs::NESTJS_TYPE, Component::ScheduledTask.as_str());
                    builder.maybe_str(attrs::NESTJS_SCHEDULE_KIND, job.queue_name.clone());
                }
            }

            builder.maybe_str(attrs::NESTJS_JOB_STATUS, job.status.clone());
            builder.maybe_int(attrs::NESTJS_JOB_ATTEMPTS_MADE, job.attempts_made);
            builder.maybe_int(attrs::NESTJS_JOB_MAX_ATTEMPTS, job.max_attempts);
            builder.maybe_str(attrs::NESTJS_JOB_ENQUEUED_AT, job.enqueued_at.clone());
            builder.maybe_f64(attrs::NESTJS_JOB_WAIT_DURATION, job.wait_duration_ms);
        }
    }

    builder.build()
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
mod tests {
    use super::*;
    use crate::observe::decoder;
    use opentelemetry_proto::tonic::common::v1::any_value;

    fn decode(payload: &str) -> Batch {
        decoder::decode(payload.as_bytes(), false).unwrap()
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn mapped(payload: &str) -> Vec<OtlpSpan> {
        let batch = decode(payload);
        map(&batch, at("2026-10-01T12:00:00Z"))
            .map(|resource| resource.scope_spans[0].spans.clone())
            .unwrap_or_default()
    }

    fn find<'a>(spans: &'a [OtlpSpan], name: &str) -> &'a OtlpSpan {
        spans
            .iter()
            .find(|span| span.name == name)
            .unwrap_or_else(|| panic!("no span named {name}; got {:?}", names(spans)))
    }

    fn names(spans: &[OtlpSpan]) -> Vec<&str> {
        spans.iter().map(|span| span.name.as_str()).collect()
    }

    fn attribute<'a>(span: &'a OtlpSpan, key: &str) -> Option<&'a any_value::Value> {
        span.attributes
            .iter()
            .find(|entry| entry.key == key)?
            .value
            .as_ref()?
            .value
            .as_ref()
    }

    fn string_attribute<'a>(span: &'a OtlpSpan, key: &str) -> Option<&'a str> {
        match attribute(span, key)? {
            any_value::Value::StringValue(value) => Some(value),
            _ => None,
        }
    }

    const NESTED: &str = r#"{"serviceId":"api","serviceVersion":"1.4.0","snapshots":[{
        "ti":"0198f3a1-2b4c-7d8e-9f01-23456789abcd","p":"http","op":"/users/:id",
        "ct":"2026-10-01T10:00:00.000Z","d":120,"a":{"m":"GET","sc":200},
        "t":[{"c":"UsersController","m":"getUser","o":"auto","so":1,"d":118,"ch":[
            {"c":"AuthGuard","m":"canActivate","o":"auto","so":2,"d":5},
            {"c":"UsersService","m":"findUser","o":"auto","so":8,"d":100,"ch":[
                {"c":"pg","m":"SELECT","o":"auto","so":10,"d":90,
                 "t":{"db.system":"postgresql","db.statement":"SELECT 1"}}
            ]}
        ]}]
    }]}"#;

    #[test]
    fn the_nest_call_tree_survives_translation() {
        let spans = mapped(NESTED);
        assert_eq!(spans.len(), 5);

        let root = find(&spans, "GET /users/:id");
        let controller = find(&spans, "UsersController.getUser");
        let guard = find(&spans, "AuthGuard.canActivate");
        let service = find(&spans, "UsersService.findUser");
        let query = find(&spans, "SELECT");

        // The shape from the plan: request > controller > (guard, service > query).
        assert!(root.parent_span_id.is_empty());
        assert_eq!(controller.parent_span_id, root.span_id);
        assert_eq!(guard.parent_span_id, controller.span_id);
        assert_eq!(service.parent_span_id, controller.span_id);
        assert_eq!(query.parent_span_id, service.span_id);
    }

    #[test]
    fn every_span_shares_the_operations_trace_id() {
        let spans = mapped(NESTED);
        let trace_id = &spans[0].trace_id;
        assert_eq!(trace_id.len(), 16);
        assert!(spans.iter().all(|span| &span.trace_id == trace_id));
    }

    #[test]
    fn a_uuid_trace_id_is_preserved_bit_for_bit() {
        let spans = mapped(NESTED);
        assert_eq!(
            hex::encode(&spans[0].trace_id),
            "0198f3a12b4c7d8e9f0123456789abcd"
        );
    }

    #[test]
    fn span_ids_are_unique_and_well_formed() {
        let spans = mapped(NESTED);
        let mut ids: Vec<_> = spans.iter().map(|span| span.span_id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), spans.len());
        assert!(spans.iter().all(|span| span.span_id.len() == 8));
    }

    #[test]
    fn repeated_calls_to_one_method_become_separate_spans() {
        // The SDK mints an id per invocation, so two calls to one method are two
        // spans with two ids.
        let payload = r#"{"serviceId":"api","snapshots":[{
            "ti":"t1","p":"http","op":"/x","ct":"2026-10-01T10:00:00.000Z","d":10,
            "t":[{"c":"S","m":"find","o":"auto","s":"01a0f4a1-5d20-7b5c-86b3-200582a1bca1","so":1,"d":2},
                 {"c":"S","m":"find","o":"auto","s":"01a0f4a1-5d24-74d0-9ae8-316500f03919","so":4,"d":2}]
        }]}"#;
        let spans = mapped(payload);
        let calls: Vec<_> = spans.iter().filter(|span| span.name == "S.find").collect();
        assert_eq!(calls.len(), 2);
        assert_ne!(calls[0].span_id, calls[1].span_id);
    }

    #[test]
    fn a_spans_own_id_is_used_so_logs_can_reference_it() {
        let payload = r#"{"serviceId":"api","snapshots":[{
            "ti":"t1","p":"http","op":"/x","d":10,
            "t":[{"c":"S","m":"find","o":"auto","s":"01a0f4a1-5d24-74d0-9ae8-316500f03919","d":2}]
        }]}"#;
        let spans = mapped(payload);
        let call = find(&spans, "S.find");
        assert_eq!(
            call.span_id,
            ids::span_id_from("01a0f4a1-5d24-74d0-9ae8-316500f03919").to_vec()
        );
    }

    #[test]
    fn duplicate_span_ids_do_not_collide_into_one_span() {
        // Should not happen - but a shared id would make the parent of everything
        // below ambiguous, so the second falls back to its position.
        let payload = r#"{"serviceId":"api","snapshots":[{
            "ti":"t1","p":"http","op":"/x","d":10,
            "t":[{"c":"S","m":"find","o":"auto","s":"same","so":1,"d":2},
                 {"c":"S","m":"find","o":"auto","s":"same","so":4,"d":2}]
        }]}"#;
        let spans = mapped(payload);
        let calls: Vec<_> = spans.iter().filter(|span| span.name == "S.find").collect();
        assert_ne!(calls[0].span_id, calls[1].span_id);
    }

    #[test]
    fn a_span_without_an_id_still_gets_one() {
        let payload = r#"{"serviceId":"api","snapshots":[{
            "ti":"t1","p":"http","op":"/x","d":10,
            "t":[{"c":"S","m":"find","o":"auto","d":2}]
        }]}"#;
        let spans = mapped(payload);
        let call = find(&spans, "S.find");
        assert_eq!(call.span_id.len(), 8);
        assert_ne!(call.span_id, vec![0u8; 8]);
    }

    #[test]
    fn timestamps_are_rebuilt_from_called_at_plus_offset() {
        let spans = mapped(NESTED);
        let root = find(&spans, "GET /users/:id");
        let base = at("2026-10-01T10:00:00.000Z")
            .timestamp_nanos_opt()
            .unwrap() as u64;
        assert_eq!(root.start_time_unix_nano, base);
        assert_eq!(root.end_time_unix_nano, base + 120_000_000);

        // The guard starts 2ms in and runs for 5ms.
        let guard = find(&spans, "AuthGuard.canActivate");
        assert_eq!(guard.start_time_unix_nano, base + 2_000_000);
        assert_eq!(guard.end_time_unix_nano, base + 7_000_000);
    }

    #[test]
    fn an_operation_without_called_at_is_dated_at_receipt() {
        let payload = r#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/x","d":5}
        ]}"#;
        let spans = mapped(payload);
        let expected = at("2026-10-01T12:00:00Z").timestamp_nanos_opt().unwrap() as u64;
        assert_eq!(spans[0].start_time_unix_nano, expected);
    }

    #[test]
    fn nest_metadata_is_preserved_on_every_span() {
        let spans = mapped(NESTED);
        let guard = find(&spans, "AuthGuard.canActivate");
        assert_eq!(string_attribute(guard, attrs::NESTJS_TYPE), Some("guard"));
        assert_eq!(
            string_attribute(guard, attrs::NESTJS_CLASS_NAME),
            Some("AuthGuard")
        );
        assert_eq!(
            string_attribute(guard, attrs::NESTJS_METHOD_NAME),
            Some("canActivate")
        );

        let controller = find(&spans, "UsersController.getUser");
        assert_eq!(
            string_attribute(controller, attrs::NESTJS_TYPE),
            Some("controller")
        );
    }

    #[test]
    fn http_conventions_are_set_on_the_root_span() {
        let spans = mapped(NESTED);
        let root = find(&spans, "GET /users/:id");
        assert_eq!(
            string_attribute(root, attrs::HTTP_REQUEST_METHOD),
            Some("GET")
        );
        assert_eq!(
            string_attribute(root, attrs::HTTP_ROUTE),
            Some("/users/:id")
        );
        assert!(matches!(
            attribute(root, attrs::HTTP_RESPONSE_STATUS_CODE),
            Some(any_value::Value::IntValue(200))
        ));
        assert_eq!(root.kind, SpanKind::Server as i32);
    }

    #[test]
    fn a_database_span_is_a_client_span_with_db_conventions() {
        let spans = mapped(NESTED);
        let query = find(&spans, "SELECT");
        assert_eq!(query.kind, SpanKind::Client as i32);
        assert_eq!(
            string_attribute(query, attrs::DB_SYSTEM_NAME),
            Some("postgresql")
        );
        assert_eq!(
            string_attribute(query, attrs::DB_QUERY_TEXT),
            Some("SELECT 1")
        );
        assert_eq!(
            string_attribute(query, attrs::DB_OPERATION_NAME),
            Some("SELECT")
        );
    }

    #[test]
    fn a_thrown_4xx_is_recorded_without_failing_the_span() {
        // Nest answers "not found" by throwing, so a captured `NotFoundException`
        // sits on an ordinary 404. Letting it set an error status would make a
        // service's error rate meaningless.
        let payload = r#"{"serviceId":"api","snapshots":[{
            "ti":"t1","p":"http","op":"/users/:id","d":10,"a":{"m":"GET","sc":404},
            "e":{"cls":"NotFoundException","message":"no such user"}
        }]}"#;
        let spans = mapped(payload);
        assert_eq!(
            spans[0].status.as_ref().unwrap().code,
            StatusCode::Unset as i32
        );
        // The exception still happened and is still reported.
        assert_eq!(spans[0].events[0].name, "exception");
    }

    #[test]
    fn a_graphql_error_fails_the_span_despite_a_200() {
        // GraphQL answers 200 with an `errors` array, so the transport status says
        // nothing and the captured error has to decide.
        let payload = r#"{"serviceId":"api","snapshots":[{
            "ti":"t1","p":"graphql","op":"Query.orders","d":10,"a":{"m":"POST","sc":200},
            "e":{"cls":"GraphQLError","message":"boom"}
        }]}"#;
        let spans = mapped(payload);
        assert_eq!(
            spans[0].status.as_ref().unwrap().code,
            StatusCode::Error as i32
        );
    }

    #[test]
    fn a_failed_job_is_an_error_even_though_it_has_no_status_code() {
        let payload = r#"{"serviceId":"api","jobs":[{
            "i":"job-1","ti":"t1","q":"email","d":5,
            "e":{"cls":"Error","message":"smtp down"}
        }]}"#;
        let spans = mapped(payload);
        assert_eq!(
            spans[0].status.as_ref().unwrap().code,
            StatusCode::Error as i32
        );
        assert_eq!(spans[0].status.as_ref().unwrap().message, "smtp down");
    }

    #[test]
    fn an_exception_sets_the_status_and_records_an_event() {
        let payload = r#"{"serviceId":"api","snapshots":[{
            "ti":"t1","p":"http","op":"/u","ct":"2026-10-01T10:00:00.000Z","d":10,
            "a":{"m":"GET","sc":500},
            "e":{"cls":"TypeError","message":"boom","stack":"at x"}
        }]}"#;
        let spans = mapped(payload);
        let root = &spans[0];
        assert_eq!(root.status.as_ref().unwrap().code, StatusCode::Error as i32);
        assert_eq!(root.status.as_ref().unwrap().message, "boom");

        let event = &root.events[0];
        assert_eq!(event.name, "exception");
        let kind = event
            .attributes
            .iter()
            .find(|entry| entry.key == attrs::EXCEPTION_TYPE)
            .unwrap();
        assert!(matches!(
            kind.value.as_ref().unwrap().value.as_ref().unwrap(),
            any_value::Value::StringValue(value) if value == "TypeError"
        ));
        // The event is dated at the span's end, the only time the wire allows.
        assert_eq!(event.time_unix_nano, root.end_time_unix_nano);
    }

    #[test]
    fn a_5xx_is_an_error_even_with_nothing_thrown() {
        let payload = r#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/u","a":{"m":"GET","sc":503}}
        ]}"#;
        let spans = mapped(payload);
        assert_eq!(
            spans[0].status.as_ref().unwrap().code,
            StatusCode::Error as i32
        );
    }

    #[test]
    fn a_4xx_is_not_an_error_on_a_server_span() {
        let payload = r#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/u","a":{"m":"GET","sc":404}}
        ]}"#;
        let spans = mapped(payload);
        assert_eq!(
            spans[0].status.as_ref().unwrap().code,
            StatusCode::Unset as i32
        );
    }

    #[test]
    fn a_scheduled_run_is_named_after_its_handler_not_its_timer() {
        // `rollup interval` would read like a queue called "interval".
        let payload = r#"{"serviceId":"api","jobs":[
            {"i":"r1","ti":"t1","n":"rollup","q":"interval","d":5}
        ]}"#;
        let spans = mapped(payload);
        assert_eq!(spans[0].name, "rollup");
    }

    #[test]
    fn an_uncaptured_span_failure_still_sets_an_error_status() {
        let payload = r#"{"serviceId":"api","snapshots":[{
            "ti":"t1","p":"http","op":"/u","d":5,
            "t":[{"c":"S","m":"m","o":"auto","e":true}]
        }]}"#;
        let spans = mapped(payload);
        let inner = find(&spans, "S.m");
        assert_eq!(
            inner.status.as_ref().unwrap().code,
            StatusCode::Error as i32
        );
        // Nothing was captured, so there is no exception event to record.
        assert!(inner.events.is_empty());
    }

    #[test]
    fn a_job_becomes_a_consumer_span_with_messaging_conventions() {
        let payload = r#"{"serviceId":"api","jobs":[{
            "i":"job-7","ti":"t1","n":"send","q":"email","s":"completed",
            "c":"2026-10-01T10:00:00.000Z","d":40,"am":1,"ma":3,"wd":12.5
        }]}"#;
        let spans = mapped(payload);
        let root = &spans[0];
        assert_eq!(root.name, "send email");
        assert_eq!(root.kind, SpanKind::Consumer as i32);
        assert_eq!(
            string_attribute(root, attrs::MESSAGING_DESTINATION_NAME),
            Some("email")
        );
        assert_eq!(
            string_attribute(root, attrs::MESSAGING_MESSAGE_ID),
            Some("job-7")
        );
        assert_eq!(
            string_attribute(root, attrs::NESTJS_JOB_STATUS),
            Some("completed")
        );
    }

    #[test]
    fn a_scheduled_run_is_not_reported_as_queue_traffic() {
        // `@nestjs/schedule` puts the scheduler kind where a queue name would go.
        let payload = r#"{"serviceId":"api","jobs":[{
            "i":"run-1","ti":"t1","n":"ReportsService.nightly","q":"cron","d":900
        }]}"#;
        let spans = mapped(payload);
        let root = &spans[0];
        assert_eq!(
            string_attribute(root, attrs::NESTJS_TYPE),
            Some("scheduled_task")
        );
        assert_eq!(
            string_attribute(root, attrs::NESTJS_SCHEDULE_KIND),
            Some("cron")
        );
        // A timer firing is not a message, so it must not land in queue views.
        assert!(string_attribute(root, attrs::MESSAGING_SYSTEM).is_none());
        assert!(string_attribute(root, attrs::MESSAGING_DESTINATION_NAME).is_none());
    }

    #[test]
    fn a_queue_job_is_still_reported_as_messaging() {
        let payload = r#"{"serviceId":"api","jobs":[
            {"i":"job-1","ti":"t1","n":"send","q":"email","d":5}
        ]}"#;
        let spans = mapped(payload);
        assert_eq!(
            string_attribute(&spans[0], attrs::NESTJS_TYPE),
            Some("queue_consumer")
        );
        assert_eq!(
            string_attribute(&spans[0], attrs::MESSAGING_SYSTEM),
            Some("nestjs_queue")
        );
    }

    #[test]
    fn a_job_enqueued_by_a_request_joins_that_request_trace() {
        // The SDK propagates the request's trace id onto the job, and that is what
        // makes the two one trace in the backend.
        let payload = r#"{"serviceId":"api",
            "snapshots":[{"ti":"shared-trace","p":"http","op":"/checkout","d":10}],
            "jobs":[{"i":"job-1","ti":"shared-trace","q":"email","d":5}]}"#;
        let spans = mapped(payload);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].trace_id, spans[1].trace_id);
        // Both are roots of their own: Observe does not say which span enqueued the
        // job, so inventing a parent link would be a guess.
        assert!(spans[1].parent_span_id.is_empty());
    }

    #[test]
    fn graphql_records_its_document_and_operation() {
        let payload = r#"{"serviceId":"api","snapshots":[{
            "ti":"t1","p":"graphql","op":"Query.orders","d":10,
            "a":{"m":"POST","sc":200,"ou":"{ orders { id } }"}
        }]}"#;
        let spans = mapped(payload);
        let root = &spans[0];
        assert_eq!(root.name, "Query.orders");
        assert_eq!(
            string_attribute(root, attrs::GRAPHQL_OPERATION_NAME),
            Some("Query.orders")
        );
        assert_eq!(
            string_attribute(root, attrs::GRAPHQL_DOCUMENT),
            Some("{ orders { id } }")
        );
    }

    #[test]
    fn a_collapsed_node_keeps_its_count() {
        let payload = r#"{"serviceId":"api","snapshots":[{
            "ti":"t1","p":"http","op":"/u","d":100,
            "t":[{"n":"ValidationPipe.transform x27","c":"ValidationPipe",
                  "m":"transform","o":"auto","so":1,"d":54,"t":{"observe.collapsed":27}}]
        }]}"#;
        let spans = mapped(payload);
        let collapsed = find(&spans, "ValidationPipe.transform x27");
        assert!(matches!(
            attribute(collapsed, attrs::NESTJS_COLLAPSED_CALLS),
            Some(any_value::Value::IntValue(27))
        ));
        assert_eq!(
            string_attribute(collapsed, attrs::NESTJS_TYPE),
            Some("pipe")
        );
    }

    #[test]
    fn the_resource_names_the_service_and_its_version() {
        let batch = decode(NESTED);
        let resource = map(&batch, at("2026-10-01T12:00:00Z")).unwrap();
        let attributes = resource.resource.unwrap().attributes;
        let name = attributes
            .iter()
            .find(|entry| entry.key == attrs::SERVICE_NAME)
            .unwrap();
        assert!(matches!(
            name.value.as_ref().unwrap().value.as_ref().unwrap(),
            any_value::Value::StringValue(value) if value == "api"
        ));
        assert!(
            attributes
                .iter()
                .any(|entry| entry.key == attrs::SERVICE_VERSION)
        );
    }

    #[test]
    fn a_batch_with_no_operations_maps_to_nothing() {
        let batch = decode(r#"{"serviceId":"api"}"#);
        assert!(map(&batch, at("2026-10-01T12:00:00Z")).is_none());
    }

    #[test]
    fn a_negative_duration_does_not_wrap_into_a_huge_span() {
        let payload = r#"{"serviceId":"api","snapshots":[
            {"ti":"t1","p":"http","op":"/u","ct":"2026-10-01T10:00:00.000Z","d":-5}
        ]}"#;
        let spans = mapped(payload);
        assert_eq!(spans[0].end_time_unix_nano, spans[0].start_time_unix_nano);
    }

    #[test]
    fn mapping_is_deterministic() {
        // A retried batch must land on the same tree, not a duplicate one.
        assert_eq!(mapped(NESTED), mapped(NESTED));
    }
}
