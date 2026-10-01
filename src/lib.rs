//! An open-source `@nestjs/observe` -> OpenTelemetry bridge.
//!
//! One seam matters: `observe` knows the SDK's wire format and nothing about
//! OpenTelemetry, `mapping` knows OpenTelemetry and reads only the normalized model.
//! `docs/protocol.md` and `docs/mapping.md` are the prose versions.

pub mod config;
pub mod mapping;
pub mod observe;
pub mod otlp;
