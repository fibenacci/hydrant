//! The hydrant binary: configuration, telemetry, and the public read API.

mod config;
mod schemas;

use std::error::Error;
use std::time::Duration;

use axum::Router;
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::http::{Method, StatusCode};
use hydrant_api::{ApiState, router};
use hydrant_core::SchemaSet;
use hydrant_store::PostgresStore;
use tokio::net::TcpListener;
use tokio::signal;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::load()?;
    init_telemetry(&config.log);

    // Schemas first: a service that cannot read them serves nothing, and finding that out after
    // binding the port would mean answering requests with 404s that look like missing data.
    let schemas = schemas::load(&config.schemas_dir)?;
    tracing::info!(collections = schemas.len(), "collection definitions loaded");

    let store = PostgresStore::connect(&config.database_url, config.max_connections).await?;
    if config.migrate_on_start {
        store.migrate().await?;
        tracing::info!("migrations are up to date");
    }

    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(address = %listener.local_addr()?, "serving the public read API");

    axum::serve(listener, service(store, schemas, &config))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// The router with the layers a public endpoint needs.
fn service(store: PostgresStore, schemas: SchemaSet, config: &Config) -> Router {
    // CORS is wide open on purpose: the data is public, and a browser consumer is as legitimate as
    // any other. `expose_headers` is the part that is easy to miss — without it a browser can send
    // a conditional request but cannot read the ETag to build the next one.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::HEAD])
        .allow_headers([IF_NONE_MATCH])
        .expose_headers([ETAG]);

    router(ApiState::new(store, schemas))
        // 503 rather than the default 408: the client was not slow, the service gave up. A CDN
        // reads that as retriable, which is what it is.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            Duration::from_secs(config.request_timeout),
        ))
        .layer(CompressionLayer::new())
        .layer(cors)
        .layer(TraceLayer::new_for_http())
}

/// Reads the filter from configuration, letting `RUST_LOG` override it — the usual expectation when
/// something has to be debugged in place.
fn init_telemetry(filter: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

/// Resolves on SIGTERM or Ctrl-C, so in-flight requests finish instead of being cut off.
async fn shutdown() {
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
