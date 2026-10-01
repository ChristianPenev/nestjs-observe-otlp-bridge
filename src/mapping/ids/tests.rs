//! Tests for id derivation.

use super::*;

/// A UUIDv7 of the shape the agent mints for a trace.
const TRACE_UUID: &str = "0198f3a1-2b4c-7d8e-9f01-23456789abcd";
/// A UUIDv7 of the shape the agent mints per span invocation.
const SPAN_UUID: &str = "01a0f4a1-5d24-74d0-9ae8-316500f03919";

#[test]
fn a_uuid_trace_id_keeps_its_own_bytes_in_every_accepted_form() {
    for (label, input) in [
        ("dashed", TRACE_UUID),
        ("dashless", "0198f3a12b4c7d8e9f0123456789abcd"),
        ("uppercase", "0198F3A1-2B4C-7D8E-9F01-23456789ABCD"),
    ] {
        assert_eq!(
            hex::encode(trace_id(input)),
            "0198f3a12b4c7d8e9f0123456789abcd",
            "{label}"
        );
    }
}

#[test]
fn non_uuid_request_id_is_hashed_not_rejected() {
    // The SDK adopts any `x-request-id` matching its pattern, so this is a real
    // trace id, not a malformed one.
    let mapped = trace_id("req_01HV8Z.edge-7:a");
    assert_ne!(mapped, [0u8; 16]);
}

#[test]
fn hashed_trace_ids_are_stable_and_distinct() {
    // Two services that saw the same propagated id must agree, or the trace
    // splits in the backend.
    assert_eq!(trace_id("gateway-42"), trace_id("gateway-42"));
    assert_ne!(trace_id("gateway-42"), trace_id("gateway-43"));
}

#[test]
fn uuid_shaped_but_invalid_ids_take_the_hash_path() {
    // Right length, wrong alphabet.
    assert!(parse_uuid("0198f3a1-2b4c-7d8e-9f01-23456789abcz").is_none());
    // Right length and alphabet, dashes in the wrong places.
    assert!(parse_uuid("0198f3a12b4c-7d8e-9f01-23456789abcd-").is_none());
    assert!(parse_uuid("not-a-uuid").is_none());
    assert!(parse_uuid("").is_none());
}

#[test]
fn nil_uuid_is_nudged_off_zero() {
    // All-zero is reserved by OTLP for "no trace".
    assert_ne!(trace_id("00000000-0000-0000-0000-000000000000"), [0u8; 16]);
}

#[test]
fn a_span_id_is_eight_bytes_stable_and_distinct() {
    assert_eq!(span_id_from(SPAN_UUID).len(), 8);
    // The SDK puts the span's own id on the log record, so the two derivations
    // must agree or log-to-span links break.
    assert_eq!(span_id_from(SPAN_UUID), span_id_from(SPAN_UUID));
    assert_ne!(
        span_id_from(SPAN_UUID),
        span_id_from("01a0f4a1-5d20-7b5c-86b3-200582a1bca1")
    );
}

#[test]
fn a_span_id_and_a_trace_id_from_the_same_text_differ() {
    // Domain separation: a trace and a span that share a source string must not
    // produce related bytes.
    assert_ne!(&trace_id(SPAN_UUID)[..8], &span_id_from(SPAN_UUID)[..]);
}

#[test]
fn positional_span_ids_differ_whenever_their_input_does() {
    for (label, left, right) in [
        // The case the `s` field cannot serve: the same method called twice
        // under one parent carries one label but must become two spans.
        (
            "sibling positions",
            span_id_at("trace-1", &[0, 1]),
            span_id_at("trace-1", &[0, 2]),
        ),
        (
            "same position in different traces",
            span_id_at("trace-1", &[0]),
            span_id_at("trace-2", &[0]),
        ),
        // A decimal join would make both of these "1,11" vs "11,1" -> equal if
        // concatenated carelessly.
        (
            "index paths that a careless join would flatten together",
            span_id_at("t", &[1, 11]),
            span_id_at("t", &[11, 1]),
        ),
        (
            "a nested path against its own root",
            span_id_at("t", &[0]),
            span_id_at("t", &[0, 0]),
        ),
    ] {
        assert_ne!(left, right, "{label}");
    }
}

#[test]
fn span_ids_are_deterministic() {
    // Re-exporting a batch must not fork the trace into a second tree.
    assert_eq!(
        span_id_at("trace-1", &[0, 3, 2]),
        span_id_at("trace-1", &[0, 3, 2])
    );
}
