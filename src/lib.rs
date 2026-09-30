//! An open-source `@nestjs/observe` -> OpenTelemetry bridge.
//!
//! The crate is laid out along the one seam that matters: `observe` understands the
//! SDK's wire format and knows nothing about OpenTelemetry, `mapping` understands
//! OpenTelemetry and reads only the normalized model, and neither has to change when
//! the other does.
//!
//! - [`observe::protocol`] - the wire format, transcribed from the SDK.
//! - [`observe::decoder`] - wire format in, [`observe::model`] out.
//! - [`mapping`] - the normalized model into OTLP structures.
//! - [`otlp::exporter`] - shipping them.
//!
//! `docs/protocol.md` and `docs/mapping.md` are the prose versions of the first and
//! third of those.

pub mod config;
pub mod mapping;
pub mod observe;
pub mod otlp;
