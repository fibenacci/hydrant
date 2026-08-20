//! The hydrant binary: configuration, telemetry, the public read API and the ingest surface.

mod config;
mod schemas;

use std::error::Error;
use std::time::Duration;

use axum::Router;
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::http::{Method, StatusCode};
use clap::{Parser, Subcommand};
use hydrant_api::{ApiState, IngestState, ingest_router, router};
use hydrant_core::{SchemaSet, SourceName};
use hydrant_store::{PostgresStore, Store, Token};
use tokio::net::TcpListener;
use tokio::signal;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

use crate::config::Config;

/// Command-line surface. With no subcommand, the binary serves.
#[derive(Debug, Parser)]
#[command(
    name = "hydrant",
    version,
    about = "Source-agnostic ingest service",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

/// What the binary can be asked to do.
#[derive(Debug, Subcommand)]
enum Command {
    /// Serve the public read API and the ingest surface. The default.
    Serve,
    /// Manage ingest credentials.
    Token {
        #[command(subcommand)]
        action: TokenCommand,
    },
}

/// Credential management.
#[derive(Debug, Subcommand)]
enum TokenCommand {
    /// Mint a credential for one source. The token is printed once and cannot be recovered.
    Mint {
        /// The source the credential may write to.
        #[arg(long)]
        source: String,
        /// Who or what holds it, for revocation later.
        #[arg(long)]
        label: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();
    let config = Config::load()?;
    init_telemetry(&config.log);

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(config).await,
        Command::Token {
            action: TokenCommand::Mint { source, label },
        } => mint(&config, &source, &label).await,
    }
}

/// Loads the schemas, connects, and serves until a signal arrives.
async fn serve(config: Config) -> Result<(), Box<dyn Error>> {
    // Schemas first: a service that cannot read them serves nothing, and finding that out after
    // binding the port would mean answering requests with 404s that look like missing data.
    let schemas = schemas::load(&config.schemas_dir)?;
    tracing::info!(collections = schemas.len(), "collection definitions loaded");

    let store = PostgresStore::connect(
        &config.database_url,
        config.max_connections,
        Duration::from_secs(config.statement_timeout),
    )
    .await?;
    if config.migrate_on_start {
        store.migrate().await?;
        tracing::info!("migrations are up to date");
    }

    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(address = %listener.local_addr()?, "serving");

    axum::serve(listener, service(store, schemas, &config))
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// Mints a credential and prints it once.
///
/// The token is never stored in plaintext, so there is no second chance to read it: what goes into
/// the database is its HMAC under the application secret.
async fn mint(config: &Config, source: &str, label: &str) -> Result<(), Box<dyn Error>> {
    let source: SourceName = source.parse()?;
    let store = PostgresStore::connect(
        &config.database_url,
        1,
        Duration::from_secs(config.statement_timeout),
    )
    .await?;
    if config.migrate_on_start {
        store.migrate().await?;
    }

    let token = Token::generate()?;
    let hash = token.hash(config.token_secret.as_bytes())?;
    store.store_token(&hash, &source, label).await?;

    // Deliberately stdout rather than the log: a credential should not end up in a log aggregator
    // because someone left the level at info.
    println!("source: {source}");
    println!("label:  {label}");
    println!("token:  {}", token.expose());
    println!();
    println!("Store it now - it is not recoverable. Revoke with:");
    println!("  UPDATE ingest_token SET revoked_at = now() WHERE label = '{label}';");
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

    // Two routers with two states: the ingest one holds the application secret, the public one
    // cannot see it. That is a boundary rather than a comment.
    let public = router(ApiState::new(store.clone(), schemas.clone()));
    let ingest = ingest_router(IngestState::new(
        store,
        schemas,
        config.token_secret.as_bytes(),
    ));

    public
        .merge(ingest)
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
