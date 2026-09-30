//! Working out which Nest concept a span represents.
//!
//! The wire does not say. A span carries `className`, `methodKey`, and an `origin`
//! that distinguishes only `manual` from `auto` - there is no field anywhere in
//! `@nestjs/observe@0.3.5` that marks a span as a guard rather than a service. The
//! SDK does not need one: it reports to a collector that renders class and method
//! directly, so the distinction never has to be machine-readable.
//!
//! A bridge does need it, because `nestjs.type` is the attribute that makes a trace
//! searchable by the thing the framework was doing ("show me requests where a guard
//! was slow"). So it is inferred here, from the two signals Nest itself makes
//! reliable, in this order:
//!
//! 1. **A driver name.** Outgoing spans are opened against `pg`, `mysql2`,
//!    `mongodb` or `http` rather than a class, and carry `db.system` or
//!    `http.method` tags. Exact names, corroborated by the tags, so an application
//!    class that happens to be called `Http` is not mistaken for one.
//! 2. **The class name's suffix.** Nest's conventions are strong and
//!    near-universal (`AuthGuard`, `UsersController`, `LoggingInterceptor`), and
//!    the enhancer classes the framework itself ships follow them too.
//! 3. **An interface method name.** `canActivate`, `intercept` and `catch` are
//!    declared by `CanActivate`, `NestInterceptor` and `ExceptionFilter`, so a class
//!    with an unconventional name is still recognisable by the hook it implements.
//!
//! `transform` and `use` are deliberately *not* in that third group. They are the
//! `PipeTransform` and `NestMiddleware` hooks, but they are also ordinary method
//! names on ordinary services, and misfiling a service as a pipe is worse than
//! leaving it as `provider`. Those two are recognised by suffix only.
//!
//! Inference can be wrong. It is a labelling convenience layered on top of
//! `nestjs.class.name` and `nestjs.method.name`, which are always exact, and
//! `docs/mapping.md` says so where users will read it.

use crate::observe::model::Component;
use crate::observe::protocol::TraceNode;

/// Driver names the SDK opens outgoing database spans against.
const DATABASE_DRIVERS: [&str; 3] = ["pg", "mysql2", "mongodb"];

/// The class name the SDK opens outgoing HTTP spans against.
const HTTP_DRIVER: &str = "http";

/// Suffix-to-concept, longest first so that `GraphQLResolver` is not caught by a
/// shorter suffix and `ExceptionFilter` is not read as a plain `Filter`.
const SUFFIXES: [(&str, Component); 14] = [
    ("ExceptionFilter", Component::ExceptionFilter),
    ("Interceptor", Component::Interceptor),
    ("Subscriber", Component::QueueConsumer),
    ("Controller", Component::Controller),
    ("Repository", Component::Repository),
    ("Middleware", Component::Middleware),
    ("Processor", Component::QueueConsumer),
    ("Consumer", Component::QueueConsumer),
    ("Resolver", Component::GraphQlResolver),
    ("Service", Component::Service),
    ("Gateway", Component::WebSocketGateway),
    ("Filter", Component::ExceptionFilter),
    ("Guard", Component::Guard),
    ("Pipe", Component::Pipe),
];

/// Interface hooks distinctive enough to identify a class on their own.
const HOOK_METHODS: [(&str, Component); 3] = [
    ("canActivate", Component::Guard),
    ("intercept", Component::Interceptor),
    ("catch", Component::ExceptionFilter),
];

/// Classifies one span.
///
/// `manual` spans are reported as such whatever they are named: the application
/// opened them by hand, so a suffix on the surrounding class says nothing about what
/// the span covers.
pub fn classify(node: &TraceNode) -> Component {
    if node.origin.as_deref() == Some("manual") {
        return Component::Manual;
    }

    let class_name = node.class_name.as_deref().unwrap_or("");

    if let Some(outgoing) = classify_outgoing(class_name, node) {
        return outgoing;
    }

    if let Some(component) = classify_by_suffix(class_name) {
        return component;
    }

    if let Some(method) = node.method_key.as_deref() {
        for (hook, component) in HOOK_METHODS {
            if method == hook {
                return component;
            }
        }
    }

    Component::Provider
}

/// Recognises a span opened by an outgoing-call integration rather than by the
/// instance decorator.
///
/// The tag check is what makes this safe. `http` and `pg` are plausible class names,
/// and a bare name match would relabel an application's own `HttpService` as a
/// client span - losing its `nestjs.*` attributes and giving it a span kind that
/// says the time left the process when it did not.
fn classify_outgoing(class_name: &str, node: &TraceNode) -> Option<Component> {
    let has_tag = |key: &str| {
        node.tags
            .as_ref()
            .is_some_and(|tags| tags.contains_key(key))
    };

    if DATABASE_DRIVERS.contains(&class_name) && has_tag("db.system") {
        return Some(Component::DatabaseClient);
    }
    if class_name == HTTP_DRIVER && has_tag("http.method") {
        return Some(Component::HttpClient);
    }
    None
}

fn classify_by_suffix(class_name: &str) -> Option<Component> {
    SUFFIXES
        .iter()
        .find(|(suffix, _)| class_name.ends_with(suffix))
        .map(|(_, component)| *component)
}

#[cfg(test)]
mod tests {
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
}
