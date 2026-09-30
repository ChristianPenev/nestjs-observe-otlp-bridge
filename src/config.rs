//! Configuration, entirely from the environment.
//!
//! Nothing here knows about a particular backend. Grafana Cloud, Better Stack,
//! SigNoz, Honeycomb, Datadog and New Relic all differ only in their endpoint and
//! the headers they want, so both are free-form and neither is special-cased.

use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::time::Duration;

/// Where the bridge listens for `@nestjs/observe` traffic.
///
/// 4319 rather than 4317/4318, which are OTLP's own ports - the bridge frequently
/// runs beside a collector, and colliding with it by default would be unhelpful.
const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:4319";

/// Where telemetry is forwarded when nothing says otherwise: a collector on the
/// same host, on OTLP/HTTP's standard port.
const DEFAULT_OTLP_ENDPOINT: &str = "http://localhost:4318";

const DEFAULT_TIMEOUT_SECONDS: u64 = 10;

#[derive(Debug, Clone)]
pub struct Config {
    pub listen_addr: SocketAddr,
    pub otlp: OtlpConfig,
    /// Credentials the agent must present, when the deployment wants them checked.
    pub ingest_auth: Option<IngestAuth>,
}

#[derive(Debug, Clone)]
pub struct OtlpConfig {
    /// The base URL, without a signal path.
    pub endpoint: String,
    pub protocol: OtlpProtocol,
    pub headers: HashMap<String, String>,
    pub timeout: Duration,
}

impl OtlpConfig {
    /// The URL for one signal.
    ///
    /// OTLP/HTTP appends `/v1/traces`, `/v1/logs` or `/v1/metrics` to the base, and
    /// a trailing slash on the base would produce a double slash that some
    /// receivers route differently.
    pub fn signal_url(&self, signal: Signal) -> String {
        format!("{}/{}", self.endpoint.trim_end_matches('/'), signal.path())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Traces,
    Logs,
    Metrics,
}

impl Signal {
    pub fn path(&self) -> &'static str {
        match self {
            Signal::Traces => "v1/traces",
            Signal::Logs => "v1/logs",
            Signal::Metrics => "v1/metrics",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Signal::Traces => "traces",
            Signal::Logs => "logs",
            Signal::Metrics => "metrics",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtlpProtocol {
    /// OTLP over HTTP with a protobuf body. The most widely accepted form, and the
    /// default.
    HttpProtobuf,
    /// OTLP over HTTP with a JSON body. Accepted by fewer backends, but far easier
    /// to read while working out why something is not arriving.
    HttpJson,
}

#[derive(Debug, Clone)]
pub struct IngestAuth {
    pub app_key: String,
    pub app_secret: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is not a valid socket address: {1}")]
    ListenAddr(String, String),
    #[error(
        "OTLP_PROTOCOL must be one of http/protobuf, http/json (or the aliases http, grpc); got {0:?}"
    )]
    Protocol(String),
    #[error("OTLP over gRPC is not implemented yet; use OTLP_PROTOCOL=http/protobuf")]
    GrpcUnsupported,
    #[error("OTLP_ENDPOINT must start with http:// or https://; got {0:?}")]
    Endpoint(String),
    #[error("OBSERVE_APP_KEY and OBSERVE_APP_SECRET must be set together, or neither")]
    PartialAuth,
    #[error("{0} must be a whole number of seconds; got {1:?}")]
    Duration(&'static str, String),
}

impl Config {
    /// Reads the configuration from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_source(|key| env::var(key).ok())
    }

    /// Reads the configuration from an arbitrary source, so it can be tested without
    /// mutating the process environment - which is shared by every test in the
    /// binary and cannot be set safely from several at once.
    pub fn from_source(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let listen = get("LISTEN_ADDR").unwrap_or_else(|| DEFAULT_LISTEN_ADDR.to_string());
        let listen_addr = listen.parse().map_err(|error: std::net::AddrParseError| {
            ConfigError::ListenAddr(listen.clone(), error.to_string())
        })?;

        let endpoint = get("OTLP_ENDPOINT").unwrap_or_else(|| DEFAULT_OTLP_ENDPOINT.to_string());
        if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
            return Err(ConfigError::Endpoint(endpoint));
        }

        let protocol = parse_protocol(get("OTLP_PROTOCOL").as_deref())?;
        let headers = parse_headers(get("OTLP_HEADERS").as_deref());

        let timeout = match get("OTLP_TIMEOUT_SECONDS") {
            Some(raw) => Duration::from_secs(
                raw.trim()
                    .parse()
                    .map_err(|_| ConfigError::Duration("OTLP_TIMEOUT_SECONDS", raw.clone()))?,
            ),
            None => Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
        };

        let ingest_auth = match (get("OBSERVE_APP_KEY"), get("OBSERVE_APP_SECRET")) {
            (Some(app_key), Some(app_secret)) => Some(IngestAuth {
                app_key,
                app_secret,
            }),
            (None, None) => None,
            // Half-configured auth is the dangerous case: it reads as "authentication
            // is on" while accepting everything.
            _ => return Err(ConfigError::PartialAuth),
        };

        Ok(Config {
            listen_addr,
            otlp: OtlpConfig {
                endpoint,
                protocol,
                headers,
                timeout,
            },
            ingest_auth,
        })
    }
}

fn parse_protocol(value: Option<&str>) -> Result<OtlpProtocol, ConfigError> {
    match value.map(str::trim) {
        None | Some("") => Ok(OtlpProtocol::HttpProtobuf),
        // `http` is what the OTel environment-variable spec and most documentation
        // use as shorthand for the protobuf form.
        Some("http") | Some("http/protobuf") => Ok(OtlpProtocol::HttpProtobuf),
        Some("http/json") | Some("json") => Ok(OtlpProtocol::HttpJson),
        // Named specifically so the message can say what to do instead of listing
        // this as an unknown value.
        Some("grpc") | Some("http/grpc") => Err(ConfigError::GrpcUnsupported),
        Some(other) => Err(ConfigError::Protocol(other.to_string())),
    }
}

/// Parses `OTLP_HEADERS`, in the W3C Baggage form the OTel spec uses:
/// `key1=value1,key2=value2`.
///
/// A value may itself contain `=` - a bearer token or a base64 credential often
/// does - so only the first one separates the pair. Commas cannot appear in a value;
/// that is the spec's limitation, not this one's.
fn parse_headers(value: Option<&str>) -> HashMap<String, String> {
    let Some(value) = value else {
        return HashMap::new();
    };

    value
        .split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let owned: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        Config::from_source(|key| owned.get(key).cloned())
    }

    #[test]
    fn defaults_point_at_a_local_collector() {
        let config = from(&[]).unwrap();
        assert_eq!(config.otlp.endpoint, "http://localhost:4318");
        assert_eq!(config.otlp.protocol, OtlpProtocol::HttpProtobuf);
        assert_eq!(config.listen_addr.port(), 4319);
        assert!(config.ingest_auth.is_none());
    }

    #[test]
    fn signal_paths_are_appended_to_the_base() {
        let config = from(&[("OTLP_ENDPOINT", "http://collector:4318")]).unwrap();
        assert_eq!(
            config.otlp.signal_url(Signal::Traces),
            "http://collector:4318/v1/traces"
        );
        assert_eq!(
            config.otlp.signal_url(Signal::Logs),
            "http://collector:4318/v1/logs"
        );
    }

    #[test]
    fn a_trailing_slash_does_not_double_up() {
        let config = from(&[("OTLP_ENDPOINT", "https://otlp.example.com/")]).unwrap();
        assert_eq!(
            config.otlp.signal_url(Signal::Metrics),
            "https://otlp.example.com/v1/metrics"
        );
    }

    #[test]
    fn headers_parse_in_the_documented_form() {
        let config = from(&[("OTLP_HEADERS", "x-api-key=abc,x-tenant=eu")]).unwrap();
        assert_eq!(config.otlp.headers.get("x-api-key").unwrap(), "abc");
        assert_eq!(config.otlp.headers.get("x-tenant").unwrap(), "eu");
    }

    #[test]
    fn a_header_value_may_contain_equals_signs() {
        // A bearer token or a base64 credential routinely does.
        let config = from(&[("OTLP_HEADERS", "Authorization=Bearer abc==")]).unwrap();
        assert_eq!(
            config.otlp.headers.get("Authorization").unwrap(),
            "Bearer abc=="
        );
    }

    #[test]
    fn surrounding_whitespace_in_headers_is_ignored() {
        let config = from(&[("OTLP_HEADERS", " a = 1 , b = 2 ")]).unwrap();
        assert_eq!(config.otlp.headers.get("a").unwrap(), "1");
        assert_eq!(config.otlp.headers.get("b").unwrap(), "2");
    }

    #[test]
    fn malformed_header_entries_are_skipped_not_fatal() {
        let config = from(&[("OTLP_HEADERS", "good=1,,nonsense,=2")]).unwrap();
        assert_eq!(config.otlp.headers.len(), 1);
        assert_eq!(config.otlp.headers.get("good").unwrap(), "1");
    }

    #[test]
    fn protocol_aliases_are_accepted() {
        assert_eq!(
            from(&[("OTLP_PROTOCOL", "http")]).unwrap().otlp.protocol,
            OtlpProtocol::HttpProtobuf
        );
        assert_eq!(
            from(&[("OTLP_PROTOCOL", "http/protobuf")])
                .unwrap()
                .otlp
                .protocol,
            OtlpProtocol::HttpProtobuf
        );
        assert_eq!(
            from(&[("OTLP_PROTOCOL", "http/json")])
                .unwrap()
                .otlp
                .protocol,
            OtlpProtocol::HttpJson
        );
    }

    #[test]
    fn grpc_is_refused_with_a_message_that_says_what_to_do() {
        let error = from(&[("OTLP_PROTOCOL", "grpc")]).unwrap_err();
        assert!(matches!(error, ConfigError::GrpcUnsupported));
        assert!(error.to_string().contains("http/protobuf"));
    }

    #[test]
    fn an_endpoint_without_a_scheme_is_refused() {
        // `fetch`-style relative URLs are the single most common misconfiguration.
        assert!(matches!(
            from(&[("OTLP_ENDPOINT", "collector:4318")]).unwrap_err(),
            ConfigError::Endpoint(_)
        ));
    }

    #[test]
    fn half_configured_auth_is_refused() {
        // Otherwise it reads as "authentication is on" while accepting everything.
        assert!(matches!(
            from(&[("OBSERVE_APP_KEY", "k")]).unwrap_err(),
            ConfigError::PartialAuth
        ));
        assert!(matches!(
            from(&[("OBSERVE_APP_SECRET", "s")]).unwrap_err(),
            ConfigError::PartialAuth
        ));
    }

    #[test]
    fn both_credentials_together_enable_checking() {
        let config = from(&[("OBSERVE_APP_KEY", "k"), ("OBSERVE_APP_SECRET", "s")]).unwrap();
        let auth = config.ingest_auth.unwrap();
        assert_eq!(auth.app_key, "k");
        assert_eq!(auth.app_secret, "s");
    }

    #[test]
    fn a_bad_listen_address_is_refused() {
        assert!(matches!(
            from(&[("LISTEN_ADDR", "not-an-address")]).unwrap_err(),
            ConfigError::ListenAddr(_, _)
        ));
    }

    #[test]
    fn a_bad_timeout_is_refused() {
        assert!(matches!(
            from(&[("OTLP_TIMEOUT_SECONDS", "soon")]).unwrap_err(),
            ConfigError::Duration(_, _)
        ));
    }
}
