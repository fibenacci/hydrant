//! Core domain of hydrant: record identity, canonicalisation and projection.
//!
//! This crate is deliberately free of I/O dependencies — no `sqlx`, no `axum`, no `tokio`.
//! Projection and hashing stay pure functions, which makes them exhaustively property-testable
//! and lets the CLI use them without a database.
//!
//! Three invariants live here, and code that breaks one is wrong even if it passes every test:
//!
//! - **Projection happens at ingest, never at read.** [`projection::project`] is the only way a
//!   payload becomes storable, and it drops everything the schema does not name. Filtering on
//!   read would make every bug in the read path a data leak; filtering on write makes a bug a
//!   missing field.
//! - **Deny by default, with no wildcard.** An object without an allow list is a schema error,
//!   not a pass-through, and every dropped key is reported rather than discarded silently.
//! - **Canonicalisation is a wire contract.** [`hash::content_hash`] is RFC 8785 plus SHA-256,
//!   and it may never change.
//!
//! The service knows nothing about the source domain: payloads are documents, references between
//! them are opaque strings, and resolving those is the consumer's job.

pub mod filter;
pub mod hash;
pub mod ident;
pub mod projection;
pub mod schema;

pub use filter::{Filter, FilterError};
pub use hash::{ContentHash, canonicalize, collection_checksum, content_hash};
pub use ident::{CollectionName, RecordId, RecordKey, Seq, SourceName};
pub use projection::{DropReason, DroppedField, Projection, ProjectionError, project};
pub use schema::{
    CacheSpec, CollectionSchema, FieldName, FieldSpec, IdPath, ScalarType, SchemaError, SchemaSet,
    SortKey,
};
