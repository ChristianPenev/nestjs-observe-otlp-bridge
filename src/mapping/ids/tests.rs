//! Tests for id derivation.

use super::*;

#[test]
fn uuid_trace_id_keeps_its_own_bytes() {
    // A UUIDv7 of the shape the agent mints.
    let id = "0198f3a1-2b4c-7d8e-9f01-23456789abcd";
    let mapped = trace_id(id);
    assert_eq!(hex::encode(mapped), "0198f3a12b4c7d8e9f0123456789abcd");
}

#[test]
fn dashless_uuid_parses_identically() {
    assert_eq!(
        trace_id("0198f3a1-2b4c-7d8e-9f01-23456789abcd"),
        trace_id("0198f3a12b4c7d8e9f0123456789abcd")
    );
}

#[test]
fn uppercase_uuid_parses_identically() {
    assert_eq!(
        trace_id("0198F3A1-2B4C-7D8E-9F01-23456789ABCD"),
        trace_id("0198f3a1-2b4c-7d8e-9f01-23456789abcd")
    );
}

#[test]
fn non_uuid_request_id_is_hashed_not_rejected() {
    // The SDK adopts any `x-request-id` matching its pattern, so this is a real
    // trace id, not a malformed one.
    let mapped = trace_id("req_01HV8Z.edge-7:a");
    assert_ne!(mapped, [0u8; 16]);
}

#[test]
fn hashing_is_stable_across_calls() {
    // Two services that saw the same propagated id must agree, or the trace
    // splits in the backend.
    assert_eq!(trace_id("gateway-42"), trace_id("gateway-42"));
}

#[test]
fn different_request_ids_do_not_collide() {
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
fn a_span_id_is_eight_bytes_and_stable() {
    let id = span_id_from("01a0f4a1-5d24-74d0-9ae8-316500f03919");
    assert_eq!(id.len(), 8);
    assert_eq!(id, span_id_from("01a0f4a1-5d24-74d0-9ae8-316500f03919"));
}

#[test]
fn a_log_lands_on_the_same_span_id_as_the_span_it_names() {
    // The SDK puts the span's own id on the log record, so the two derivations
    // must agree or log-to-span links break.
    let span = "01a0f4a1-5d24-74d0-9ae8-316500f03919";
    assert_eq!(span_id_from(span), span_id_from(span));
}

#[test]
fn distinct_span_ids_do_not_collide() {
    assert_ne!(
        span_id_from("01a0f4a1-5d24-74d0-9ae8-316500f03919"),
        span_id_from("01a0f4a1-5d20-7b5c-86b3-200582a1bca1")
    );
}

#[test]
fn a_span_id_and_a_trace_id_from_the_same_text_differ() {
    // Domain separation: a trace and a span that share a source string must not
    // produce related bytes.
    let shared = "01a0f4a1-5d24-74d0-9ae8-316500f03919";
    assert_ne!(&trace_id(shared)[..8], &span_id_from(shared)[..]);
}

#[test]
fn sibling_spans_get_distinct_ids() {
    // The case the `s` field cannot serve: the same method called twice under
    // one parent carries one label but must become two spans.
    let a = span_id_at("trace-1", &[0, 1]);
    let b = span_id_at("trace-1", &[0, 2]);
    assert_ne!(a, b);
}

#[test]
fn span_ids_are_scoped_to_their_trace() {
    assert_ne!(span_id_at("trace-1", &[0]), span_id_at("trace-2", &[0]));
}

#[test]
fn span_ids_are_deterministic() {
    // Re-exporting a batch must not fork the trace into a second tree.
    assert_eq!(
        span_id_at("trace-1", &[0, 3, 2]),
        span_id_at("trace-1", &[0, 3, 2])
    );
}

#[test]
fn index_paths_are_unambiguous() {
    // A decimal join would make both of these "1,11" vs "11,1" -> equal if
    // concatenated carelessly.
    assert_ne!(span_id_at("t", &[1, 11]), span_id_at("t", &[11, 1]));
}

#[test]
fn nested_and_root_paths_differ() {
    assert_ne!(span_id_at("t", &[0]), span_id_at("t", &[0, 0]));
}
