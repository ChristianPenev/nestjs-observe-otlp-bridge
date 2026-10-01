//! Shipping OTLP over HTTP.
//!
//! Deliberately thin: no queue, no retry, no buffer. The bridge is stateless and a
//! failed export is reported, not held - buffering and backpressure belong in the
//! collector this sits in front of, not in two places.

use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::logs::v1::ResourceLogs;
use opentelemetry_proto::tonic::metrics::v1::ResourceMetrics;
use opentelemetry_proto::tonic::trace::v1::ResourceSpans;
use prost::Message;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};

use crate::config::{OtlpConfig, OtlpProtocol, Signal};

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("could not reach the OTLP endpoint at {url}: {source}")]
    Transport {
        url: String,
        #[source]
        source: reqwest::Error,
    },
    /// The endpoint answered, and refused.
    #[error("the OTLP endpoint at {url} answered {status}{}", detail(.body))]
    Rejected {
        url: String,
        status: u16,
        body: String,
    },
    #[error("could not build the OTLP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("could not serialize the OTLP request: {0}")]
    Serialize(#[source] serde_json::Error),
}

fn detail(body: &str) -> String {
    if body.is_empty() {
        String::new()
    } else {
        format!(" - {}", body.chars().take(500).collect::<String>())
    }
}

/// Posts OTLP payloads to a configured endpoint.
#[derive(Clone)]
pub struct Exporter {
    client: reqwest::Client,
    config: OtlpConfig,
    /// The configured headers, parsed once at start-up so a malformed one is
    /// reported then rather than on every export.
    headers: HeaderMap,
}

/// Installs the TLS backend, once per process.
///
/// rustls has no compiled-in default here and `reqwest` *panics* if a client is built
/// before one is installed. Doing it here means the exporter cannot be constructed in
/// the wrong order - the mistake a test, or a second entry point, would make.
fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // An error means something else installed one first, which is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl Exporter {
    pub fn new(config: OtlpConfig) -> Result<Self, ExportError> {
        install_crypto_provider();

        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(ExportError::Client)?;

        let mut headers = HeaderMap::new();
        for (key, value) in &config.headers {
            // Dropped with a warning rather than failing start-up: an exporter that
            // refuses to start takes the application's telemetry with it.
            match (
                HeaderName::try_from(key.as_str()),
                HeaderValue::from_str(value),
            ) {
                (Ok(name), Ok(value)) => {
                    headers.insert(name, value);
                }
                _ => {
                    tracing::warn!(header = %key, "ignoring an OTLP header that is not valid HTTP")
                }
            }
        }

        Ok(Self {
            client,
            config,
            headers,
        })
    }

    pub async fn export_traces(&self, spans: ResourceSpans) -> Result<(), ExportError> {
        let request = ExportTraceServiceRequest {
            resource_spans: vec![spans],
        };
        self.send(Signal::Traces, request.encode_to_vec(), || {
            serde_json::to_vec(&request)
        })
        .await
    }

    pub async fn export_logs(&self, logs: ResourceLogs) -> Result<(), ExportError> {
        let request = ExportLogsServiceRequest {
            resource_logs: vec![logs],
        };
        self.send(Signal::Logs, request.encode_to_vec(), || {
            serde_json::to_vec(&request)
        })
        .await
    }

    pub async fn export_metrics(&self, metrics: ResourceMetrics) -> Result<(), ExportError> {
        let request = ExportMetricsServiceRequest {
            resource_metrics: vec![metrics],
        };
        self.send(Signal::Metrics, request.encode_to_vec(), || {
            serde_json::to_vec(&request)
        })
        .await
    }

    /// Posts one payload.
    ///
    /// The JSON body is produced by a closure so that the protobuf path - the
    /// default - never pays to serialize a second representation it will not send.
    async fn send(
        &self,
        signal: Signal,
        protobuf: Vec<u8>,
        json: impl FnOnce() -> Result<Vec<u8>, serde_json::Error>,
    ) -> Result<(), ExportError> {
        let url = self.config.signal_url(signal);

        let (body, content_type) = match self.config.protocol {
            OtlpProtocol::HttpProtobuf => (protobuf, "application/x-protobuf"),
            OtlpProtocol::HttpJson => (json().map_err(ExportError::Serialize)?, "application/json"),
        };

        let length = body.len();
        let response = self
            .client
            .post(&url)
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, content_type)
            .body(body)
            .send()
            .await
            .map_err(|source| ExportError::Transport {
                url: url.clone(),
                source,
            })?;

        let status = response.status();
        if status.is_success() {
            tracing::debug!(signal = signal.name(), bytes = length, "exported");
            return Ok(());
        }

        // Read defensively: an error body is not guaranteed to be readable, and
        // failing here would report a reachable endpoint as unreachable.
        let body = response.text().await.unwrap_or_default();
        Err(ExportError::Rejected {
            url,
            status: status.as_u16(),
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OtlpProtocol;
    use std::collections::HashMap;
    use std::time::Duration;

    fn config(endpoint: &str, protocol: OtlpProtocol) -> OtlpConfig {
        OtlpConfig {
            endpoint: endpoint.to_string(),
            protocol,
            headers: HashMap::new(),
            timeout: Duration::from_millis(250),
        }
    }

    #[test]
    fn an_unrepresentable_header_is_dropped_rather_than_fatal() {
        let mut headers = HashMap::new();
        headers.insert("valid".to_string(), "1".to_string());
        // A newline cannot appear in a header value.
        headers.insert("bad".to_string(), "a\nb".to_string());
        let exporter = Exporter::new(OtlpConfig {
            headers,
            ..config("http://localhost:4318", OtlpProtocol::HttpProtobuf)
        })
        .unwrap();
        assert!(exporter.headers.contains_key("valid"));
        assert!(!exporter.headers.contains_key("bad"));
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_a_transport_error_naming_the_url() {
        // Port 1 is reserved and nothing listens on it.
        let exporter =
            Exporter::new(config("http://127.0.0.1:1", OtlpProtocol::HttpProtobuf)).unwrap();
        let error = exporter
            .export_traces(ResourceSpans::default())
            .await
            .unwrap_err();
        assert!(matches!(error, ExportError::Transport { .. }));
        assert!(error.to_string().contains("127.0.0.1:1/v1/traces"));
    }
}
