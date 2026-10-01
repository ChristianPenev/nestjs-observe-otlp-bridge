//! Tests for the attribute scheme.

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
fn legacy_convention_keys_are_renamed_forward_and_nothing_else_is() {
    for (from, to) in [
        ("db.statement", "db.query.text"),
        ("db.system", "db.system.name"),
        ("http.method", "http.request.method"),
        ("http.url", "url.full"),
        // Application tags are the user's own keys. Renaming them, or hiding
        // them behind a prefix, would break the dashboards they were added for.
        ("tenant.id", "tenant.id"),
        ("db.shard", "db.shard"),
    ] {
        assert_eq!(rename_tag(from), to, "rename_tag({from})");
    }
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
fn a_json_tag_keeps_its_type_in_otlp() {
    // `null` is dropped rather than emptied: OTLP has no null, and a key present
    // with an empty string reads as a real measurement of nothing.
    assert!(
        to_any_value(&Value::Null).is_none(),
        "null must not become a value at all"
    );
    assert!(
        matches!(
            to_any_value(&json!(3)).unwrap().value,
            Some(any_value::Value::IntValue(3))
        ),
        "an integer must not arrive as a float"
    );
    // Objects and arrays are kept structurally - OTLP models both - so a nested
    // tag survives instead of being flattened into a string nobody can filter on.
    assert!(
        matches!(
            to_any_value(&json!({"a": [1, 2]})).unwrap().value,
            Some(any_value::Value::KvlistValue(_))
        ),
        "a nested object must keep its structure"
    );
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
