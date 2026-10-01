//! Working out which Nest concept a span represents.
//!
//! The wire does not say - nothing in `@nestjs/observe@0.3.5` marks a span as a guard
//! rather than a service, because the hosted collector renders class and method
//! directly and never needs the distinction machine-readable. A bridge does need it:
//! `nestjs.type` is what makes a trace searchable by what the framework was doing.
//!
//! So it is inferred, in order: manual origin, driver name corroborated by its tags,
//! class-name suffix, interface hook, then `provider`. It is a heuristic and it can
//! be wrong; `nestjs.class.name` and `nestjs.method.name` are always exact.
//! `docs/mapping.md` records where it fails.

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
/// `manual` spans are reported as such whatever they are named: the application chose
/// what to wrap, so the surrounding class says nothing.
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
/// The tag check is what makes it safe: `http` and `pg` are plausible class names, and
/// a bare match would relabel an application's own `HttpService` as a client span.
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
mod tests;
