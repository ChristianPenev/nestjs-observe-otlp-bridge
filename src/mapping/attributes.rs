//! The attribute scheme, in one place.
//!
//! Two rules decide where anything lands, and `docs/mapping.md` is generated from
//! the same names:
//!
//! 1. If OpenTelemetry has a convention for it, use the convention's name. The SDK
//!    emits its outgoing-call tags under *older* semantic-convention names
//!    (`db.statement`, `http.method`, `http.url`) which have since been renamed, so
//!    those are translated forward here rather than passed through - a backend's
//!    built-in database and HTTP views key off the current names.
//! 2. If it is a Nest concept OpenTelemetry has no word for, put it under `nestjs.*`.
//!    Nothing else invents a namespace.
//!
//! Application tags are passed through untouched. They are the user's own keys and
//! renaming them, or hiding them behind a prefix, would break the dashboards they
//! were added for.

use opentelemetry_proto::tonic::common::v1::{AnyValue, ArrayValue, KeyValue, any_value};
use serde_json::Value;

use crate::observe::model::{Attributes, Component, ErrorInfo, Span};

// --- the `nestjs.*` namespace -------------------------------------------------

/// Which Nest concept a span represents - `controller`, `guard`, `service`, ...
///
/// Inferred rather than received; see `observe::component`.
pub const NESTJS_TYPE: &str = "nestjs.type";
pub const NESTJS_CLASS_NAME: &str = "nestjs.class.name";
pub const NESTJS_METHOD_NAME: &str = "nestjs.method.name";
/// `manual` for a span the application opened, `auto` for an instrumented one.
pub const NESTJS_SPAN_ORIGIN: &str = "nestjs.span.origin";
/// How many identical sibling calls a collapsed node stands for.
pub const NESTJS_COLLAPSED_CALLS: &str = "nestjs.collapsed.calls";
/// The transport an operation arrived on.
pub const NESTJS_PROTOCOL: &str = "nestjs.protocol";
/// The SDK's own operation identifier: a route, `Type.field`, or `gateway:pattern`.
pub const NESTJS_OPERATION_ID: &str = "nestjs.operation.id";
pub const NESTJS_USER_ID: &str = "nestjs.user.id";
pub const NESTJS_JOB_ID: &str = "nestjs.job.id";
/// The queue a job came off, or the scheduler kind that fired it - the SDK sends
/// both in the same field.
pub const NESTJS_JOB_QUEUE: &str = "nestjs.job.queue";
/// `cron`, `interval`, `timeout` or `schedule`, for a `@nestjs/schedule` run.
pub const NESTJS_SCHEDULE_KIND: &str = "nestjs.schedule.kind";
pub const NESTJS_JOB_STATUS: &str = "nestjs.job.status";
pub const NESTJS_JOB_ATTEMPTS_MADE: &str = "nestjs.job.attempts_made";
pub const NESTJS_JOB_MAX_ATTEMPTS: &str = "nestjs.job.max_attempts";
pub const NESTJS_JOB_ENQUEUED_AT: &str = "nestjs.job.enqueued_at";
pub const NESTJS_JOB_WAIT_DURATION: &str = "nestjs.job.wait_duration_ms";
/// Nest's logger context, usually the emitting class.
pub const NESTJS_LOG_CONTEXT: &str = "nestjs.log.context";
/// The level as Nest wrote it, kept because `verbose` has no OTLP severity.
pub const NESTJS_LOG_LEVEL: &str = "nestjs.log.level";
/// The Observe span id, kept in its original UUID form alongside the derived OTLP
/// one so a trace here can be cross-referenced with the hosted collector's.
pub const NESTJS_SPAN_ID: &str = "nestjs.span.id";
/// The GraphQL document, which the SDK sends in place of a URL for GraphQL.
pub const NESTJS_GRAPHQL_DOCUMENT: &str = "nestjs.graphql.document";
/// The request's captured headers and body, when `http.capture` asked for them.
pub const NESTJS_CAPTURED_REQUEST: &str = "nestjs.request.captured";

// --- semantic conventions -----------------------------------------------------

pub const HTTP_REQUEST_METHOD: &str = "http.request.method";
pub const HTTP_RESPONSE_STATUS_CODE: &str = "http.response.status_code";
pub const HTTP_ROUTE: &str = "http.route";
pub const URL_FULL: &str = "url.full";
pub const URL_PATH: &str = "url.path";
pub const SERVER_ADDRESS: &str = "server.address";
pub const DB_SYSTEM_NAME: &str = "db.system.name";
pub const DB_QUERY_TEXT: &str = "db.query.text";
pub const DB_OPERATION_NAME: &str = "db.operation.name";
pub const MESSAGING_SYSTEM: &str = "messaging.system";
pub const MESSAGING_DESTINATION_NAME: &str = "messaging.destination.name";
pub const MESSAGING_MESSAGE_ID: &str = "messaging.message.id";
pub const MESSAGING_OPERATION_NAME: &str = "messaging.operation.name";
pub const RPC_SYSTEM: &str = "rpc.system";
pub const RPC_METHOD: &str = "rpc.method";
pub const GRAPHQL_OPERATION_NAME: &str = "graphql.operation.name";
pub const GRAPHQL_DOCUMENT: &str = "graphql.document";
pub const EXCEPTION_TYPE: &str = "exception.type";
pub const EXCEPTION_MESSAGE: &str = "exception.message";
pub const EXCEPTION_STACKTRACE: &str = "exception.stacktrace";
pub const SERVICE_NAME: &str = "service.name";
pub const SERVICE_VERSION: &str = "service.version";
pub const TELEMETRY_SDK_NAME: &str = "telemetry.sdk.name";
pub const TELEMETRY_SDK_LANGUAGE: &str = "telemetry.sdk.language";

/// Older semantic-convention names the SDK still emits, and what they are called now.
///
/// Applied to outgoing-span tags only. A rename is not a reinterpretation: each pair
/// here is the same measurement under the name the current conventions give it.
const RENAMED_TAGS: [(&str, &str); 4] = [
    ("db.system", DB_SYSTEM_NAME),
    ("db.statement", DB_QUERY_TEXT),
    ("http.method", HTTP_REQUEST_METHOD),
    ("http.url", URL_FULL),
];

/// Translates a tag key the SDK set, leaving anything unrecognised alone.
pub fn rename_tag(key: &str) -> &str {
    RENAMED_TAGS
        .iter()
        .find(|(old, _)| *old == key)
        .map(|(_, new)| *new)
        .unwrap_or(key)
}

// --- building OTLP attributes --------------------------------------------------

/// Accumulates attributes, keeping the last write for a key.
///
/// Last-write-wins matters: application tags are applied first and the conventions
/// derived from structured fields afterwards, so a user tag called `http.route`
/// cannot displace the route the framework actually matched.
#[derive(Default)]
pub struct AttributeBuilder {
    entries: Vec<KeyValue>,
}

impl AttributeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets a key, replacing any existing value for it.
    pub fn set(&mut self, key: impl Into<String>, value: AnyValue) -> &mut Self {
        let key = key.into();
        if let Some(existing) = self.entries.iter_mut().find(|entry| entry.key == key) {
            existing.value = Some(value);
        } else {
            self.entries.push(key_value(key, value));
        }
        self
    }

    pub fn str(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.set(key, string_value(value))
    }

    pub fn int(&mut self, key: impl Into<String>, value: i64) -> &mut Self {
        self.set(key, int_value(value))
    }

    pub fn f64(&mut self, key: impl Into<String>, value: f64) -> &mut Self {
        self.set(key, double_value(value))
    }

    /// Sets a key only when the value is present.
    pub fn maybe_str(
        &mut self,
        key: impl Into<String>,
        value: Option<impl Into<String>>,
    ) -> &mut Self {
        if let Some(value) = value {
            self.str(key, value);
        }
        self
    }

    pub fn maybe_int(&mut self, key: impl Into<String>, value: Option<i64>) -> &mut Self {
        if let Some(value) = value {
            self.int(key, value);
        }
        self
    }

    pub fn maybe_f64(&mut self, key: impl Into<String>, value: Option<f64>) -> &mut Self {
        if let Some(value) = value {
            self.f64(key, value);
        }
        self
    }

    /// Copies application tags across, renaming only the SDK's own convention keys.
    pub fn tags(&mut self, tags: &Attributes) -> &mut Self {
        for (key, value) in tags {
            if let Some(value) = to_any_value(value) {
                self.set(rename_tag(key), value);
            }
        }
        self
    }

    pub fn build(self) -> Vec<KeyValue> {
        self.entries
    }
}

/// Builds a `KeyValue`, leaving `key_strindex` unset.
///
/// That field belongs to the Profiling signal and interns the key into a dictionary
/// this bridge does not send. The spec tells receivers to treat it as a non-fatal
/// oddity on other signals, but setting it alongside `key` is explicitly disallowed,
/// so every attribute is built here rather than by hand.
pub fn key_value(key: impl Into<String>, value: AnyValue) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(value),
        ..Default::default()
    }
}

pub fn string_value(value: impl Into<String>) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::StringValue(value.into())),
    }
}

pub fn int_value(value: i64) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::IntValue(value)),
    }
}

pub fn double_value(value: f64) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::DoubleValue(value)),
    }
}

#[allow(dead_code)] // part of the value-constructor set; kept for completeness
pub fn bool_value(value: bool) -> AnyValue {
    AnyValue {
        value: Some(any_value::Value::BoolValue(value)),
    }
}

/// Converts a JSON tag value into an OTLP one.
///
/// `null` returns `None` rather than an empty value: OTLP has no null, and a key
/// present with an empty string reads as a real measurement of nothing. Objects and
/// arrays are kept structurally - OTLP models both - so a nested tag survives instead
/// of being flattened into a string a backend cannot filter on.
pub fn to_any_value(value: &Value) -> Option<AnyValue> {
    let inner = match value {
        Value::Null => return None,
        Value::Bool(value) => any_value::Value::BoolValue(*value),
        Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                any_value::Value::IntValue(value)
            } else {
                any_value::Value::DoubleValue(number.as_f64()?)
            }
        }
        Value::String(value) => any_value::Value::StringValue(value.clone()),
        Value::Array(items) => any_value::Value::ArrayValue(ArrayValue {
            values: items.iter().filter_map(to_any_value).collect(),
        }),
        Value::Object(map) => {
            any_value::Value::KvlistValue(opentelemetry_proto::tonic::common::v1::KeyValueList {
                values: map
                    .iter()
                    .filter_map(|(key, value)| Some(key_value(key.clone(), to_any_value(value)?)))
                    .collect(),
            })
        }
    };
    Some(AnyValue { value: Some(inner) })
}

/// The attributes for one span, other than those derived from its operation.
///
/// Application tags go on first so that the derived conventions below cannot be
/// overwritten by a tag that happens to share a key.
pub fn span_attributes(span: &Span) -> Vec<KeyValue> {
    let mut builder = AttributeBuilder::new();
    builder.tags(&span.tags);
    builder.str(NESTJS_TYPE, span.component.as_str());
    builder.str(
        NESTJS_SPAN_ORIGIN,
        if span.manual { "manual" } else { "auto" },
    );

    if span.component.is_outgoing() {
        // The "class" is a driver name and the "method" is already carried by a
        // convention key, so neither earns a `nestjs.*` attribute here.
        if span.component == Component::DatabaseClient {
            // The SDK sends the operation as the method: `SELECT`, `find`, ...
            builder.maybe_str(DB_OPERATION_NAME, span.method_name.clone());
        }
        if span.component == Component::HttpClient {
            // `methodKey` is `GET api.stripe.com`; the host after the verb is the
            // peer. Parsed off the label because the tags carry the full URL but not
            // the host on its own.
            if let Some((method, host)) = span
                .method_name
                .as_deref()
                .and_then(|label| label.split_once(' '))
            {
                builder.str(HTTP_REQUEST_METHOD, method);
                builder.str(SERVER_ADDRESS, host);
            }
        }
    } else {
        builder.maybe_str(NESTJS_CLASS_NAME, span.class_name.clone());
        builder.maybe_str(NESTJS_METHOD_NAME, span.method_name.clone());
    }

    if let Some(count) = span.collapsed_count {
        builder.int(NESTJS_COLLAPSED_CALLS, count);
    }

    if let Some(id) = &span.observe_span_id {
        builder.str(NESTJS_SPAN_ID, id.clone());
    }

    builder.build()
}

/// The `exception.*` attributes for an error, used both on a span's status and on the
/// `exception` event recorded beside it.
pub fn exception_attributes(error: &ErrorInfo) -> Vec<KeyValue> {
    let mut builder = AttributeBuilder::new();
    builder.tags(&error.tags);
    builder.maybe_str(EXCEPTION_TYPE, error.class_name.clone());
    builder.maybe_str(EXCEPTION_MESSAGE, error.message.clone());
    builder.maybe_str(EXCEPTION_STACKTRACE, error.stack.clone());
    builder.build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::model::Component;
    use serde_json::json;

    fn span(component: Component) -> Span {
        Span {
            name: "x".to_string(),
            component,
            class_name: None,
            method_name: None,
            observe_span_id: None,
            manual: false,
            start_offset_ms: None,
            duration_ms: None,
            tags: Attributes::new(),
            error: None,
            collapsed_count: None,
            children: Vec::new(),
        }
    }

    fn get<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a AnyValue> {
        attributes
            .iter()
            .find(|entry| entry.key == key)
            .and_then(|entry| entry.value.as_ref())
    }

    fn as_string(value: &AnyValue) -> Option<&str> {
        match value.value.as_ref()? {
            any_value::Value::StringValue(value) => Some(value),
            _ => None,
        }
    }

    #[test]
    fn legacy_convention_keys_are_renamed_forward() {
        assert_eq!(rename_tag("db.statement"), "db.query.text");
        assert_eq!(rename_tag("db.system"), "db.system.name");
        assert_eq!(rename_tag("http.method"), "http.request.method");
        assert_eq!(rename_tag("http.url"), "url.full");
    }

    #[test]
    fn application_tags_are_left_alone() {
        assert_eq!(rename_tag("tenant.id"), "tenant.id");
        assert_eq!(rename_tag("db.shard"), "db.shard");
    }

    #[test]
    fn a_nest_span_carries_its_class_and_method() {
        let mut node = span(Component::Controller);
        node.class_name = Some("UsersController".to_string());
        node.method_name = Some("getUser".to_string());
        let attributes = span_attributes(&node);
        assert_eq!(
            as_string(get(&attributes, NESTJS_TYPE).unwrap()),
            Some("controller")
        );
        assert_eq!(
            as_string(get(&attributes, NESTJS_CLASS_NAME).unwrap()),
            Some("UsersController")
        );
        assert_eq!(
            as_string(get(&attributes, NESTJS_METHOD_NAME).unwrap()),
            Some("getUser")
        );
    }

    #[test]
    fn a_database_span_reports_its_operation_not_a_nest_class() {
        let mut node = span(Component::DatabaseClient);
        node.class_name = Some("pg".to_string());
        node.method_name = Some("SELECT".to_string());
        node.tags.insert(
            "db.statement".to_string(),
            json!("SELECT id FROM orders WHERE customer_id = $1"),
        );
        node.tags
            .insert("db.system".to_string(), json!("postgresql"));

        let attributes = span_attributes(&node);
        assert_eq!(
            as_string(get(&attributes, DB_OPERATION_NAME).unwrap()),
            Some("SELECT")
        );
        assert_eq!(
            as_string(get(&attributes, DB_SYSTEM_NAME).unwrap()),
            Some("postgresql")
        );
        assert!(get(&attributes, DB_QUERY_TEXT).is_some());
        // The driver is not a class anyone wants to read in a trace view.
        assert!(get(&attributes, NESTJS_CLASS_NAME).is_none());
        // And the old names are gone rather than duplicated.
        assert!(get(&attributes, "db.statement").is_none());
    }

    #[test]
    fn an_http_client_span_yields_a_peer_address() {
        let mut node = span(Component::HttpClient);
        node.class_name = Some("http".to_string());
        node.method_name = Some("POST api.stripe.com".to_string());
        node.tags
            .insert("http.url".to_string(), json!("https://api.stripe.com/v1/x"));

        let attributes = span_attributes(&node);
        assert_eq!(
            as_string(get(&attributes, SERVER_ADDRESS).unwrap()),
            Some("api.stripe.com")
        );
        assert_eq!(
            as_string(get(&attributes, HTTP_REQUEST_METHOD).unwrap()),
            Some("POST")
        );
        assert_eq!(
            as_string(get(&attributes, URL_FULL).unwrap()),
            Some("https://api.stripe.com/v1/x")
        );
    }

    #[test]
    fn a_derived_convention_outranks_a_colliding_user_tag() {
        // A user tag must not be able to misreport what the framework observed.
        let mut node = span(Component::DatabaseClient);
        node.method_name = Some("SELECT".to_string());
        node.tags
            .insert("db.operation.name".to_string(), json!("nonsense"));
        let attributes = span_attributes(&node);
        assert_eq!(
            as_string(get(&attributes, DB_OPERATION_NAME).unwrap()),
            Some("SELECT")
        );
    }

    #[test]
    fn a_collapsed_node_reports_how_many_calls_it_stands_for() {
        let mut node = span(Component::Pipe);
        node.collapsed_count = Some(27);
        let attributes = span_attributes(&node);
        assert!(matches!(
            get(&attributes, NESTJS_COLLAPSED_CALLS).unwrap().value,
            Some(any_value::Value::IntValue(27))
        ));
    }

    #[test]
    fn null_tags_are_dropped_rather_than_emptied() {
        assert!(to_any_value(&Value::Null).is_none());
    }

    #[test]
    fn nested_tags_keep_their_structure() {
        let value = to_any_value(&json!({"a": [1, 2]})).unwrap();
        assert!(matches!(
            value.value,
            Some(any_value::Value::KvlistValue(_))
        ));
    }

    #[test]
    fn integers_do_not_become_floats() {
        assert!(matches!(
            to_any_value(&json!(3)).unwrap().value,
            Some(any_value::Value::IntValue(3))
        ));
    }

    #[test]
    fn setting_a_key_twice_keeps_the_last_value() {
        let mut builder = AttributeBuilder::new();
        builder.str("k", "first").str("k", "second");
        let attributes = builder.build();
        assert_eq!(attributes.len(), 1);
        assert_eq!(as_string(get(&attributes, "k").unwrap()), Some("second"));
    }

    #[test]
    fn exception_attributes_carry_type_message_and_stack() {
        let error = ErrorInfo {
            class_name: Some("NotFoundException".to_string()),
            message: Some("no such user".to_string()),
            stack: Some("at UsersService.findUser".to_string()),
            tags: Attributes::new(),
        };
        let attributes = exception_attributes(&error);
        assert_eq!(
            as_string(get(&attributes, EXCEPTION_TYPE).unwrap()),
            Some("NotFoundException")
        );
        assert_eq!(
            as_string(get(&attributes, EXCEPTION_MESSAGE).unwrap()),
            Some("no such user")
        );
        assert!(get(&attributes, EXCEPTION_STACKTRACE).is_some());
    }
}
