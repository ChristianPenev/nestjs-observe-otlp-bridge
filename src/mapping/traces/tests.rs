//! Tests for trace mapping.

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
    // A UUID trace id is carried through bit for bit, so the same trace can be
    // found here and in the hosted collector.
    assert_eq!(hex::encode(trace_id), "0198f3a12b4c7d8e9f0123456789abcd");
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
fn a_spans_own_id_is_used_so_logs_can_reference_it() {
    // The SDK mints a UUIDv7 per invocation, so two calls to one method are two
    // spans with two ids - each derived from its own `s`, which is the value a
    // log line carries when it names the span that wrote it.
    let payload = r#"{"serviceId":"api","snapshots":[{
            "ti":"t1","p":"http","op":"/x","ct":"2026-10-01T10:00:00.000Z","d":10,
            "t":[{"c":"S","m":"find","o":"auto","s":"01a0f4a1-5d20-7b5c-86b3-200582a1bca1","so":1,"d":2},
                 {"c":"S","m":"find","o":"auto","s":"01a0f4a1-5d24-74d0-9ae8-316500f03919","so":4,"d":2}]
        }]}"#;
    let spans = mapped(payload);
    let calls: Vec<_> = spans.iter().filter(|span| span.name == "S.find").collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].span_id,
        ids::span_id_from("01a0f4a1-5d20-7b5c-86b3-200582a1bca1").to_vec()
    );
    assert_eq!(
        calls[1].span_id,
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
fn root_span_status_follows_the_rule_for_its_protocol() {
    // For HTTP the response code decides and nothing else. For everything else
    // the captured error decides, because there is no status code worth
    // trusting. Either way a captured exception is still recorded as an event.
    let cases: [(&str, &str, i32, &str, bool); 5] = [
        (
            "a 5xx is an error even with nothing thrown",
            r#"{"serviceId":"api","snapshots":[
                {"ti":"t1","p":"http","op":"/u","a":{"m":"GET","sc":503}}
            ]}"#,
            StatusCode::Error as i32,
            "",
            false,
        ),
        (
            "a 4xx is not an error on a server span",
            r#"{"serviceId":"api","snapshots":[
                {"ti":"t1","p":"http","op":"/u","a":{"m":"GET","sc":404}}
            ]}"#,
            StatusCode::Unset as i32,
            "",
            false,
        ),
        (
            // Nest answers "not found" by throwing, so a captured
            // `NotFoundException` sits on an ordinary 404. Letting it set an
            // error status would make a service's error rate meaningless.
            "a thrown 4xx is recorded without failing the span",
            r#"{"serviceId":"api","snapshots":[{
                "ti":"t1","p":"http","op":"/users/:id","d":10,"a":{"m":"GET","sc":404},
                "e":{"cls":"NotFoundException","message":"no such user"}
            }]}"#,
            StatusCode::Unset as i32,
            "",
            true,
        ),
        (
            // GraphQL answers 200 with an `errors` array, so the transport
            // status says nothing and the captured error has to decide.
            "a graphql error fails the span despite a 200",
            r#"{"serviceId":"api","snapshots":[{
                "ti":"t1","p":"graphql","op":"Query.orders","d":10,"a":{"m":"POST","sc":200},
                "e":{"cls":"GraphQLError","message":"boom"}
            }]}"#,
            StatusCode::Error as i32,
            "boom",
            true,
        ),
        (
            "a failed job is an error even though it has no status code",
            r#"{"serviceId":"api","jobs":[{
                "i":"job-1","ti":"t1","q":"email","d":5,
                "e":{"cls":"Error","message":"smtp down"}
            }]}"#,
            StatusCode::Error as i32,
            "smtp down",
            true,
        ),
    ];

    for (label, payload, expected_code, expected_message, expects_event) in cases {
        let spans = mapped(payload);
        let status = spans[0].status.as_ref().unwrap();
        assert_eq!(status.code, expected_code, "{label}: status code");
        assert_eq!(status.message, expected_message, "{label}: status message");
        assert_eq!(
            spans[0]
                .events
                .iter()
                .any(|event| event.name == "exception"),
            expects_event,
            "{label}: exception event"
        );
    }
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
        string_attribute(root, attrs::NESTJS_TYPE),
        Some("queue_consumer")
    );
    assert_eq!(
        string_attribute(root, attrs::MESSAGING_SYSTEM),
        Some("nestjs_queue")
    );
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
fn a_scheduled_run_is_named_after_its_handler_and_is_not_queue_traffic() {
    // `@nestjs/schedule` puts the scheduler kind where a queue name would go, so
    // naming the span after both would give "rollup interval" - which reads like
    // a queue called "interval".
    for (label, payload, expected_name, expected_kind) in [
        (
            "interval",
            r#"{"serviceId":"api","jobs":[
                {"i":"r1","ti":"t1","n":"rollup","q":"interval","d":5}
            ]}"#,
            "rollup",
            "interval",
        ),
        (
            "cron",
            r#"{"serviceId":"api","jobs":[
                {"i":"run-1","ti":"t1","n":"ReportsService.nightly","q":"cron","d":900}
            ]}"#,
            "ReportsService.nightly",
            "cron",
        ),
    ] {
        let spans = mapped(payload);
        let root = &spans[0];
        assert_eq!(root.name, expected_name, "{label}: span name");
        assert_eq!(
            string_attribute(root, attrs::NESTJS_TYPE),
            Some("scheduled_task"),
            "{label}: nestjs.type"
        );
        assert_eq!(
            string_attribute(root, attrs::NESTJS_SCHEDULE_KIND),
            Some(expected_kind),
            "{label}: schedule kind"
        );
        // A timer firing is not a message, so it must not land in queue views.
        assert!(
            string_attribute(root, attrs::MESSAGING_SYSTEM).is_none(),
            "{label}: messaging.system must be unset"
        );
        assert!(
            string_attribute(root, attrs::MESSAGING_DESTINATION_NAME).is_none(),
            "{label}: messaging.destination.name must be unset"
        );
    }
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
fn a_non_http_transport_sets_only_the_conventions_it_owns() {
    // `root_attributes` has one arm per protocol, so the failure worth catching is
    // a transport falling through to the wrong arm - an RPC message picking up
    // `http.route`, or a gateway picking up `rpc.*`. Each row therefore asserts
    // both what the transport does set and what it must not.
    struct Case {
        label: &'static str,
        payload: &'static str,
        name: &'static str,
        protocol: &'static str,
        rpc_system: Option<&'static str>,
        rpc_method: Option<&'static str>,
        url_path: Option<&'static str>,
    }

    let cases = [
        Case {
            // A Nest microservice pattern. `rpc.system` has no registered value
            // for Nest's own transports, so the namespaced one is used rather
            // than claiming a system the conventions define differently.
            label: "an rpc message",
            payload: r#"{"serviceId":"api","snapshots":[
                {"ti":"t1","p":"rpc","op":"orders.created","d":10}
            ]}"#,
            name: "orders.created",
            protocol: "rpc",
            rpc_system: Some("nestjs_microservice"),
            rpc_method: Some("orders.created"),
            url_path: None,
        },
        Case {
            // gRPC is a registered `rpc.system` value, so it is reported as itself.
            label: "a grpc call",
            payload: r#"{"serviceId":"api","snapshots":[
                {"ti":"t1","p":"grpc","op":"/orders.Orders/Get","d":10}
            ]}"#,
            name: "/orders.Orders/Get",
            protocol: "grpc",
            rpc_system: Some("grpc"),
            rpc_method: Some("/orders.Orders/Get"),
            url_path: None,
        },
        Case {
            // The SDK sends `gateway:pattern` as the operation id for a gateway
            // handler. OpenTelemetry has no WebSocket conventions, so the transport
            // URL is all there is to record under a standard key.
            label: "a websocket message",
            payload: r#"{"serviceId":"api","snapshots":[
                {"ti":"t1","p":"ws","op":"chat:message","d":10,"a":{"ou":"/socket.io"}}
            ]}"#,
            name: "chat:message",
            protocol: "ws",
            rpc_system: None,
            rpc_method: None,
            url_path: Some("/socket.io"),
        },
    ];

    for case in cases {
        let spans = mapped(case.payload);
        let root = &spans[0];
        let label = case.label;

        assert_eq!(root.name, case.name, "{label}: root span name");
        assert_eq!(root.kind, SpanKind::Server as i32, "{label}: span kind");
        assert_eq!(
            string_attribute(root, attrs::NESTJS_PROTOCOL),
            Some(case.protocol),
            "{label}: nestjs.protocol"
        );
        assert_eq!(
            string_attribute(root, attrs::NESTJS_OPERATION_ID),
            Some(case.name),
            "{label}: nestjs.operation.id"
        );
        assert_eq!(
            string_attribute(root, attrs::RPC_SYSTEM),
            case.rpc_system,
            "{label}: rpc.system"
        );
        assert_eq!(
            string_attribute(root, attrs::RPC_METHOD),
            case.rpc_method,
            "{label}: rpc.method"
        );
        assert_eq!(
            string_attribute(root, attrs::URL_PATH),
            case.url_path,
            "{label}: url.path"
        );

        // None of these is HTTP, so the HTTP conventions must stay off the span -
        // a backend keys its request views off exactly these.
        assert!(
            string_attribute(root, attrs::HTTP_ROUTE).is_none(),
            "{label}: http.route must not be set"
        );
        assert!(
            string_attribute(root, attrs::HTTP_REQUEST_METHOD).is_none(),
            "{label}: http.request.method must not be set"
        );
        assert!(
            attribute(root, attrs::HTTP_RESPONSE_STATUS_CODE).is_none(),
            "{label}: http.response.status_code must not be set"
        );
    }
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
