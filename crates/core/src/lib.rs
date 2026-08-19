//! Core domain of hydrant: record identity, canonicalization and projection.
//!
//! This crate is deliberately free of I/O dependencies — no `sqlx`, no `axum`, no `tokio`.
//! Projection and hashing stay pure functions, which makes them exhaustively property-testable
//! and lets the CLI use them without a database.
//!
//! Nothing is implemented yet; see `CONTRIBUTING.md` for the invariants this crate has to hold.
