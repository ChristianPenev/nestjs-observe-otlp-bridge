//! Turning Observe identifiers into OTLP ones.
//!
//! OTLP wants a 16-byte trace id and an 8-byte span id, both non-zero; Observe
//! guarantees neither. A UUID keeps its own bytes, anything else is hashed, and a
//! span that arrived without an `s` of its own falls back to its position in the
//! tree. Hashing is a pure function of the id, so two services that saw the same
//! `x-request-id` land on the same trace - the point of the SDK propagating it.
//!
//! Every derivation is deterministic, so re-exporting a batch reproduces the same
//! tree rather than a duplicate with fresh ids. `docs/mapping.md` §Identifiers has
//! the reasoning in full.

use sha2::{Digest, Sha256};

/// Domain separators, so a trace id and a span id derived from the same bytes cannot
/// collide, and so span ids from a future signal type cannot collide with these.
const TRACE_DOMAIN: &[u8] = b"nestjs-observe-oss/trace-id\0";
const SPAN_DOMAIN: &[u8] = b"nestjs-observe-oss/span-id\0";

/// A 16-byte OTLP trace id for an Observe `ti`.
///
/// A UUID contributes its own bytes; anything else is hashed under a domain
/// separator. An all-zero result is invalid, so it is nudged - reachable only from
/// the nil UUID, which a hand-written `x-request-id` can carry.
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
/// Log records carry the same value, so this is what links a log line to the call
/// that wrote it. Hashed rather than truncated: one derivation for every id shape.
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
/// The fallback for a span with no id of its own. `path` is the child-index route
/// from the root - `[0, 2]` is the first root's third child - unique within a trace
/// by construction.
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
/// Hand-rolled so it accepts exactly the 8-4-4-4-12 and bare-32-hex forms and rejects
/// everything else - an `x-request-id` that merely looks uuid-ish must take the
/// hashing path. Version and variant bits are not checked.
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
