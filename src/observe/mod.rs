//! Receiving and understanding `@nestjs/observe` telemetry.
//!
//! Split so that nothing about OpenTelemetry reaches this side: `protocol` is the
//! wire format, `decoder` turns it into `model`, `component` supplies the one piece
//! of Nest semantics the wire leaves out, and `ingest` is the HTTP surface.

pub mod component;
pub mod decoder;
pub mod ingest;
pub mod model;
pub mod protocol;
