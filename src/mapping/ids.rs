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
mod tests;
