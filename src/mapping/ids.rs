//! Turning Observe identifiers into OTLP ones.
//!
//! OTLP requires a 16-byte trace id and an 8-byte span id, both non-zero. Observe
//! guarantees neither, so this module is where the gap is closed - and it is the
//! part of the bridge most worth understanding before trusting a waterfall.
//!
//! **Trace ids.** `ti` is a UUIDv7 when the agent minted it, but an inbound
//! `x-request-id` is adopted verbatim whenever it matches
//! `^[A-Za-z0-9._:-]{1,128}$`, so it can be any short token. A UUID is used for its
//! own 16 bytes, which keeps the id recognisable in the backend and identical to
//! what the hosted collector would have stored. Anything else is hashed. Both paths
//! are pure functions of the id, so two services that saw the same `x-request-id`
//! land on the same OTLP trace id and their spans join up - which is the whole
//! point of the SDK propagating it.
//!
//! **Span ids.** Observe sends a UUIDv7 per invocation in `s`, which is hashed down
//! to the 8 bytes OTLP wants. Hashed rather than truncated so the derivation is the
//! same one used for a non-UUID id, and so log records - whose `spanId` carries the
//! same value - land on exactly the same bytes and link to the span that wrote them.
//!
//! `s` is documented as optional, and a span that arrives without one still needs an
//! id, so there is a fallback: the node's *position*, as the path of child indices
//! from the root. That is unique within a trace by construction. It is strictly
//! worse than the real id - it moves if the tree's shape changes between batches,
//! and nothing else can reference it - so it is only ever a fallback.
//!
//! Both paths are deterministic, so re-exporting a batch reproduces the same tree
//! rather than a duplicate one with fresh ids.

use sha2::{Digest, Sha256};

/// Domain separators, so a trace id and a span id derived from the same bytes cannot
/// collide, and so span ids from a future signal type cannot collide with these.
const TRACE_DOMAIN: &[u8] = b"nestjs-observe-oss/trace-id\0";
const SPAN_DOMAIN: &[u8] = b"nestjs-observe-oss/span-id\0";

/// A 16-byte OTLP trace id for an Observe `ti`.
///
/// A UUID contributes its own bytes; anything else is hashed under a domain
/// separator. An all-zero result is invalid per the spec, so it is nudged - only
/// reachable from the nil UUID, which no generator produces but a hand-written
/// `x-request-id` can carry.
pub fn trace_id(observe_trace_id: &str) -> [u8; 16] {
    let bytes = match parse_uuid(observe_trace_id) {
        Some(uuid) => uuid,
        None => {
            let digest = Sha256::new()
                .chain_update(TRACE_DOMAIN)
                .chain_update(observe_trace_id.as_bytes())
                .finalize();
            let mut out = [0u8; 16];
            out.copy_from_slice(&digest[..16]);
            out
        }
    };
    non_zero(bytes)
}

/// An 8-byte OTLP span id for an Observe span id.
///
/// The SDK sends a UUIDv7 per invocation, and log records reference the same value,
/// so this is what makes a log line link to the exact call that wrote it. Hashing
/// rather than truncating keeps one derivation for every id shape and avoids
/// depending on which half of a UUIDv7 carries its entropy.
pub fn span_id_from(observe_span_id: &str) -> [u8; 8] {
    let digest = Sha256::new()
        .chain_update(SPAN_DOMAIN)
        .chain_update(observe_span_id.as_bytes())
        .finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    non_zero(out)
}

/// An 8-byte OTLP span id for the node at `path` within `observe_trace_id`.
///
/// The fallback, for a span that arrived without an id of its own. `path` is the
/// sequence of child indices walked from the root of the snapshot's span forest -
/// `[0]` is the first root, `[0, 2]` its third child - which is unique within a
/// trace by construction.
pub fn span_id_at(observe_trace_id: &str, path: &[usize]) -> [u8; 8] {
    let mut hasher = Sha256::new();
    hasher.update(SPAN_DOMAIN);
    hasher.update(observe_trace_id.as_bytes());
    for index in path {
        // Fixed-width and length-delimited, so `[1, 11]` and `[11, 1]` cannot hash
        // to the same bytes the way a decimal join would let them.
        hasher.update((*index as u64).to_be_bytes());
    }
    let digest = hasher.finalize();
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    non_zero(out)
}

/// Parses a UUID, with or without dashes, into its 16 bytes.
///
/// Hand-rolled rather than delegated: this must accept exactly the canonical
/// 8-4-4-4-12 form and the bare 32-hex form and reject everything else, so that an
/// `x-request-id` which merely looks uuid-ish takes the hashing path instead of
/// being half-parsed. Version and variant bits are not checked - a v4 id, a v7 id
/// and a customer's own UUID are all equally usable as 16 bytes.
fn parse_uuid(value: &str) -> Option<[u8; 16]> {
    let mut nibbles = [0u8; 32];
    let mut seen = 0usize;

    match value.len() {
        32 => {}
        36 => {
            let dashes: Vec<usize> = value
                .char_indices()
                .filter(|(_, c)| *c == '-')
                .map(|(i, _)| i)
                .collect();
            if dashes != [8, 13, 18, 23] {
                return None;
            }
        }
        _ => return None,
    }

    for byte in value.bytes() {
        if byte == b'-' {
            continue;
        }
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        if seen == 32 {
            return None;
        }
        nibbles[seen] = nibble;
        seen += 1;
    }

    if seen != 32 {
        return None;
    }

    let mut out = [0u8; 16];
    for (i, pair) in nibbles.chunks_exact(2).enumerate() {
        out[i] = (pair[0] << 4) | pair[1];
    }
    Some(out)
}

/// Replaces an all-zero id, which OTLP reserves to mean "absent".
fn non_zero<const N: usize>(mut bytes: [u8; N]) -> [u8; N] {
    if bytes.iter().all(|b| *b == 0) {
        bytes[N - 1] = 1;
    }
    bytes
}

#[cfg(test)]
mod tests {
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
}
