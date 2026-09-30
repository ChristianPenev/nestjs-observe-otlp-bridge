//! An open-source `@nestjs/observe` -> OpenTelemetry bridge.
//!
//! Accepts telemetry from the official NestJS Observe agent and forwards it to any
//! OTLP-capable backend. See `docs/protocol.md` for the wire format this speaks and
//! `docs/mapping.md` for what it turns that into.

use std::sync::Arc;

use nestjs_observe_oss::config::Config;
use nestjs_observe_oss::observe::ingest::{self, AppState};
use nestjs_observe_oss::otlp::exporter::Exporter;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env()?;
    let exporter = Exporter::new(config.otlp.clone())?;

    tracing::info!(
        listen = %config.listen_addr,
        otlp_endpoint = %config.otlp.endpoint,
        otlp_protocol = ?config.otlp.protocol,
        otlp_headers = config.otlp.headers.len(),
        auth = config.ingest_auth.is_some(),
        "starting nestjs-observe-oss"
    );
    if config.ingest_auth.is_none() {
        tracing::warn!(
            "no OBSERVE_APP_KEY/OBSERVE_APP_SECRET set - every batch is accepted; \
             set both if this listener is reachable beyond your own network"
        );
    }

    let app = ingest::router(Arc::new(AppState {
        exporter,
        auth: config.ingest_auth,
    }))
    .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    tracing::info!("listening on {}", listener.local_addr()?);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

/// Waits for the process to be asked to stop.
///
/// A container runtime sends SIGTERM and then waits; without handling it the
/// process is killed outright and whatever batch was mid-export is lost for no
/// reason. In-flight requests are allowed to finish.
async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let interrupt = async {
        tokio::signal::ctrl_c().await.ok();
    };

    let terminate = async {
        match signal(SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            // Nothing to wait on; the ctrl-c branch still applies.
            Err(_) => std::future::pending::<()>().await,
        }
    };

    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
    }

    tracing::info!("shutting down");
}
