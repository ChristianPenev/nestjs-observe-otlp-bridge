//! Tests for component inference.

use super::*;
use serde_json::{Map, Value};

fn node(class_name: &str, method_key: &str) -> TraceNode {
    TraceNode {
        class_name: Some(class_name.to_string()),
        method_key: Some(method_key.to_string()),
        origin: Some("auto".to_string()),
        ..Default::default()
    }
}

fn tagged(class_name: &str, method_key: &str, tag: &str) -> TraceNode {
    let mut tags = Map::new();
    tags.insert(tag.to_string(), Value::String("x".to_string()));
    TraceNode {
        tags: Some(tags),
        ..node(class_name, method_key)
    }
}

#[test]
fn suffixes_identify_the_common_concepts() {
    assert_eq!(
        classify(&node("UsersController", "getUser")),
        Component::Controller
    );
    assert_eq!(
        classify(&node("UsersService", "findUser")),
        Component::Service
    );
    assert_eq!(
        classify(&node("AuthGuard", "canActivate")),
        Component::Guard
    );
    assert_eq!(
        classify(&node("LoggingInterceptor", "intercept")),
        Component::Interceptor
    );
    assert_eq!(
        classify(&node("UsersResolver", "user")),
        Component::GraphQlResolver
    );
    assert_eq!(
        classify(&node("ValidationPipe", "transform")),
        Component::Pipe
    );
    assert_eq!(
        classify(&node("EventsGateway", "handleMessage")),
        Component::WebSocketGateway
    );
    assert_eq!(
        classify(&node("UserRepository", "findOne")),
        Component::Repository
    );
    assert_eq!(
        classify(&node("AuditMiddleware", "use")),
        Component::Middleware
    );
}

#[test]
fn longer_suffixes_win() {
    // `ExceptionFilter` must not be read as `Filter` - both map to the same
    // concept here, but the ordering is what keeps `Resolver` from being
    // shadowed by a shorter entry as the table grows.
    assert_eq!(
        classify(&node("AllExceptionsFilter", "catch")),
        Component::ExceptionFilter
    );
    assert_eq!(
        classify(&node("HttpExceptionFilter", "catch")),
        Component::ExceptionFilter
    );
}

#[test]
fn queue_consumers_have_several_conventional_names() {
    assert_eq!(
        classify(&node("EmailProcessor", "process")),
        Component::QueueConsumer
    );
    assert_eq!(
        classify(&node("EmailConsumer", "handle")),
        Component::QueueConsumer
    );
}

#[test]
fn interface_hooks_identify_an_unconventionally_named_class() {
    // No recognised suffix, but it implements `CanActivate`.
    assert_eq!(
        classify(&node("OnlyAdmins", "canActivate")),
        Component::Guard
    );
    assert_eq!(
        classify(&node("Retry", "intercept")),
        Component::Interceptor
    );
    assert_eq!(
        classify(&node("Catcher", "catch")),
        Component::ExceptionFilter
    );
}

#[test]
fn ambiguous_hooks_do_not_relabel_a_plain_provider() {
    // `transform` and `use` are ordinary method names; a service that has one is
    // not a pipe or middleware.
    assert_eq!(
        classify(&node("ImageTools", "transform")),
        Component::Provider
    );
    assert_eq!(classify(&node("FeatureFlags", "use")), Component::Provider);
}

#[test]
fn unrecognised_classes_stay_providers() {
    assert_eq!(classify(&node("Mailer", "send")), Component::Provider);
}

#[test]
fn driver_spans_with_their_tags_are_outgoing() {
    assert_eq!(
        classify(&tagged("pg", "SELECT", "db.system")),
        Component::DatabaseClient
    );
    assert_eq!(
        classify(&tagged("mysql2", "INSERT", "db.system")),
        Component::DatabaseClient
    );
    assert_eq!(
        classify(&tagged("mongodb", "find", "db.system")),
        Component::DatabaseClient
    );
    assert_eq!(
        classify(&tagged("http", "GET api.stripe.com", "http.method")),
        Component::HttpClient
    );
}

#[test]
fn a_driver_name_without_its_tags_is_not_outgoing() {
    // An application class really called `http` or `pg`. Without the tag the
    // integration always sets, this is the application's own code.
    assert_eq!(classify(&node("http", "get")), Component::Provider);
    assert_eq!(classify(&node("pg", "connect")), Component::Provider);
}

#[test]
fn an_application_class_is_not_shadowed_by_a_driver_name() {
    // Ends with `Service`, so it classifies as one - the driver check requires an
    // exact name and would not have matched anyway.
    assert_eq!(
        classify(&node("HttpService", "request")),
        Component::Service
    );
}

#[test]
fn manual_spans_are_reported_as_manual() {
    let mut manual = node("UsersService", "findUser");
    manual.origin = Some("manual".to_string());
    assert_eq!(classify(&manual), Component::Manual);
}

#[test]
fn a_manual_span_in_a_controller_is_still_manual() {
    // The surrounding class says nothing about what the application chose to
    // wrap, so the suffix must not win here.
    let mut manual = node("UsersController", "getUser");
    manual.origin = Some("manual".to_string());
    assert_eq!(classify(&manual), Component::Manual);
}

#[test]
fn a_span_with_no_class_is_a_provider() {
    assert_eq!(classify(&TraceNode::default()), Component::Provider);
}

#[test]
fn standalone_functions_are_providers() {
    // The SDK labels instrumented standalone functions with the class name
    // `Function`, which matches no suffix.
    assert_eq!(
        classify(&node("Function", "computeTotals")),
        Component::Provider
    );
}
