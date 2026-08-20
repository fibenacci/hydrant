//! Configuration, from the environment only.
//!
//! No config file: the service is deployed in containers, and a file would mean a second place to
//! look when a setting is wrong. `DATABASE_URL` is read unprefixed as well, because that is the
//! name every PostgreSQL tool already uses.

use std::net::SocketAddr;
use std::path::PathBuf;

use figment::providers::{Env, Serialized};
use figment::{Figment, Provider};
use serde::{Deserialize, Serialize};

/// Everything the service needs to start.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// PostgreSQL connection string. `DATABASE_URL` or `HYDRANT_DATABASE_URL`.
    pub database_url: String,
    /// Secret that ingest tokens are hashed with, `HYDRANT_TOKEN_SECRET`.
    ///
    /// Not in the database on purpose: a dump without it yields no working credentials. Changing it
    /// invalidates every existing token, which is also how a suspected leak is handled.
    pub token_secret: String,
    /// Address to listen on. Defaults to all interfaces on 8080, which is what a container wants.
    pub listen: SocketAddr,
    /// Upper bound on pooled database connections.
    pub max_connections: u32,
    /// Directory of collection definitions, read at startup.
    ///
    /// Cache directives are per collection and come from these files, which is why there is no
    /// global `s-maxage` setting to disagree with them.
    pub schemas_dir: PathBuf,
    /// Whether to apply pending migrations at startup.
    ///
    /// On by default: hydrant is a single-writer service, and a first run against an empty database
    /// that serves 500s until someone remembers to migrate is worse than the coordination cost.
    pub migrate_on_start: bool,
    /// Hard limit on how long a request may take, in seconds. A public endpoint needs one.
    pub request_timeout: u64,
    /// Hard limit on how long a single database statement may take, in seconds.
    ///
    /// Bounds the worst case of a filter the planner cannot serve from an index. Lower than the
    /// request timeout on purpose: the query should give up before the request does, so the response
    /// says what happened.
    pub statement_timeout: u64,
    /// `tracing` filter, e.g. `info` or `hydrant_api=debug,info`.
    pub log: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database_url: String::new(),
            token_secret: String::new(),
            listen: SocketAddr::from(([0, 0, 0, 0], 8080)),
            max_connections: 10,
            schemas_dir: PathBuf::from("schemas"),
            migrate_on_start: true,
            request_timeout: 10,
            statement_timeout: 5,
            log: "info".to_owned(),
        }
    }
}

/// Why the configuration could not be assembled.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A value was missing or of the wrong shape.
    ///
    /// Boxed because `figment::Error` is large and this enum's other variant is a unit: an error
    /// type should not be the widest thing a startup path moves around.
    #[error("configuration could not be read")]
    Figment(#[source] Box<figment::Error>),
    /// No database connection string was given.
    #[error("no database connection string: set DATABASE_URL or HYDRANT_DATABASE_URL")]
    NoDatabaseUrl,
    /// No secret for hashing ingest tokens was given.
    #[error("no token secret: set HYDRANT_TOKEN_SECRET (32 or more characters)")]
    NoTokenSecret,
}

impl From<figment::Error> for ConfigError {
    fn from(error: figment::Error) -> Self {
        Self::Figment(Box::new(error))
    }
}

impl Config {
    /// Reads the configuration from the environment, over the defaults.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a value cannot be parsed or the database URL is absent.
    pub fn load() -> Result<Self, ConfigError> {
        Self::from_provider(Env::prefixed("HYDRANT_"))
    }

    /// The same, with the prefixed provider injected — which is what makes this testable.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a value cannot be parsed or the database URL is absent.
    pub fn from_provider(prefixed: impl Provider) -> Result<Self, ConfigError> {
        let config: Self = Figment::from(Serialized::defaults(Self::default()))
            .merge(Env::raw().only(&["DATABASE_URL"]))
            .merge(prefixed)
            .extract()?;

        if config.database_url.trim().is_empty() {
            return Err(ConfigError::NoDatabaseUrl);
        }
        // A short secret is worse than an obvious error at startup: it would key every credential
        // in the system.
        if config.token_secret.trim().len() < 32 {
            return Err(ConfigError::NoTokenSecret);
        }
        Ok(config)
    }
}

#[cfg(test)]
// `Jail::expect_with` dictates the closure's error type, and figment's own error is the large one.
#[allow(
    clippy::result_large_err,
    reason = "the closure signature comes from figment"
)]
mod tests {
    use figment::Jail;
    use figment::providers::Env;

    use super::*;

    #[test]
    fn the_defaults_need_only_a_database_url() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("HYDRANT_DATABASE_URL", "postgres://localhost/hydrant");
            jail.set_env("HYDRANT_TOKEN_SECRET", "x".repeat(32));
            let config = Config::from_provider(Env::prefixed("HYDRANT_")).expect("config");
            assert_eq!(config.listen.port(), 8080);
            assert_eq!(config.schemas_dir, PathBuf::from("schemas"));
            assert!(config.migrate_on_start);
            Ok(())
        });
    }

    #[test]
    fn a_short_token_secret_is_refused() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("HYDRANT_DATABASE_URL", "postgres://localhost/hydrant");
            jail.set_env("HYDRANT_TOKEN_SECRET", "too short");
            let error = Config::from_provider(Env::prefixed("HYDRANT_")).expect_err("too short");
            assert!(matches!(error, ConfigError::NoTokenSecret));
            Ok(())
        });
    }

    #[test]
    fn an_absent_database_url_is_named_rather_than_defaulted() {
        Jail::expect_with(|jail| {
            // Without this the ambient DATABASE_URL - which the integration tests need - would
            // decide the outcome, and the test would pass or fail depending on the shell it ran in.
            jail.clear_env();
            let error = Config::from_provider(Env::prefixed("HYDRANT_")).expect_err("no url");
            assert!(matches!(error, ConfigError::NoDatabaseUrl));
            Ok(())
        });
    }

    #[test]
    fn the_unprefixed_database_url_is_honoured() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://localhost/from-plain");
            jail.set_env("HYDRANT_TOKEN_SECRET", "x".repeat(32));
            let config = Config::from_provider(Env::prefixed("HYDRANT_")).expect("config");
            assert_eq!(config.database_url, "postgres://localhost/from-plain");
            Ok(())
        });
    }

    #[test]
    fn a_prefixed_value_wins_over_the_plain_one() {
        Jail::expect_with(|jail| {
            jail.clear_env();
            jail.set_env("DATABASE_URL", "postgres://localhost/plain");
            jail.set_env("HYDRANT_DATABASE_URL", "postgres://localhost/prefixed");
            jail.set_env("HYDRANT_TOKEN_SECRET", "x".repeat(32));
            jail.set_env("HYDRANT_LISTEN", "127.0.0.1:9000");
            let config = Config::from_provider(Env::prefixed("HYDRANT_")).expect("config");
            assert_eq!(config.database_url, "postgres://localhost/prefixed");
            assert_eq!(config.listen.to_string(), "127.0.0.1:9000");
            Ok(())
        });
    }
}
