//! The hydrant binary: configuration, telemetry, the public read API and the ingest surface.

use std::error::Error;
use std::time::Duration;

use clap::{Parser, Subcommand};
use hydrant_core::SourceName;
use hydrant_server::config::Config;
use hydrant_server::{init_metrics, init_telemetry, schemas, service, shutdown};
use hydrant_store::{PostgresStore, Store, Token};
use tokio::net::TcpListener;

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

    init_metrics(config.metrics_listen)?;
    tracing::info!(address = %config.metrics_listen, "exporting metrics");

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

    // `into_make_service_with_connect_info` is what puts the peer address in the request, and the
    // rate limiter is keyed on it. Without it every limited route fails closed.
    let service = service(store, schemas, &config)?
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, service)
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
