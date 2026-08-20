//! The hydrant service: configuration, schema loading, and the assembled HTTP service.
//!
//! Split from the binary so the wiring is reachable from a test. The limits and layers this module
//! puts together are exactly the sort of thing a silently dropped edit breaks without any unit test
//! noticing, because every other test builds its own router.

pub mod config;
pub mod schemas;

use std::error::Error;
use std::time::Duration;

use axum::Router;
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::http::{Method, StatusCode};
use axum::middleware;
use hydrant_api::{ApiState, IngestState, RateLimits, ingest_router, metrics, router};
use hydrant_core::SchemaSet;
use hydrant_store::{ByteBudget, PostgresStore};
use metrics_exporter_prometheus::PrometheusBuilder;
use tokio::signal;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

/// Installs the Prometheus exporter on its own listener.
///
/// The buckets are explicit because the defaults are not shaped like this service: a cached read is
/// sub-millisecond and a manifest over a large collection is not, so the range has to cover both
/// without spending resolution where nothing happens.
///
/// # Errors
///
/// Returns an error if the bucket definition is invalid or the exporter cannot bind its listener.
pub fn init_metrics(address: std::net::SocketAddr) -> Result<(), Box<dyn Error>> {
    PrometheusBuilder::new()
        .with_http_listener(address)
        .set_buckets(&[
            0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
        ])?
        .install()?;
    Ok(())
}

/// The router with the layers a public endpoint needs.
///
/// # Errors
///
/// Returns an error if a configured rate limit would refuse every request.
pub fn service(
    store: PostgresStore,
    schemas: SchemaSet,
    config: &Config,
) -> Result<Router, Box<dyn Error>> {
    // CORS is wide open on purpose: the data is public, and a browser consumer is as legitimate as
    // any other. `expose_headers` is the part that is easy to miss — without it a browser can send
    // a conditional request but cannot read the ETag to build the next one.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::HEAD])
        .allow_headers([IF_NONE_MATCH])
        .expose_headers([ETAG]);

    // Two routers with two states: the ingest one holds the application secret, the public one
    // cannot see it. That is a boundary rather than a comment.
    let limits = RateLimits {
        read_per_second: config.read_per_second,
        read_burst: config.read_burst,
        feed_per_second: config.feed_per_second,
        feed_burst: config.feed_burst,
        trust_forwarded_for: config.trust_forwarded_for,
    };
    let public = router(
        ApiState::new(store.clone(), schemas.clone())
            .with_response_bytes(ByteBudget::clamp(config.max_response_bytes)),
        limits,
    )?;
    let ingest = ingest_router(
        IngestState::new(store, schemas, config.token_secret.as_bytes())
            .with_limits(config.max_payload_bytes, config.max_body_bytes),
    );

    Ok(public
        .merge(ingest)
        // Outermost, so it sees the status a caller actually gets - including the one the timeout
        // layer below produces.
        .layer(middleware::from_fn(metrics::track))
        // 503 rather than the default 408: the client was not slow, the service gave up. A CDN
        // reads that as retriable, which is what it is.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(config.request_timeout),
        ))
        .layer(CompressionLayer::new())
        .layer(cors)
        .layer(TraceLayer::new_for_http()))
}

/// Reads the filter from configuration, letting `RUST_LOG` override it — the usual expectation when
/// something has to be debugged in place.
pub fn init_telemetry(filter: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

/// Resolves on SIGTERM or Ctrl-C, so in-flight requests finish instead of being cut off.
pub async fn shutdown() {
    let interrupt = async {
        let _ = signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut stream) => {
                stream.recv().await;
            }
            Err(error) => tracing::warn!(%error, "cannot listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => tracing::info!("interrupted, shutting down"),
        () = terminate => tracing::info!("terminated, shutting down"),
    }
}
