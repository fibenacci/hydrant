//! Storage for hydrant: the [`Store`] contract and its PostgreSQL implementation.
//!
//! The store holds documents, not entities. It never learns what a collection means, never resolves
//! a reference between records, and never orders one collection's writes against another's. What it
//! does guarantee is the pair of properties everything above it relies on:
//!
//! - **Ingest is idempotent over the content hash.** An identical payload advances no `seq` and
//!   produces no change-feed entry, which is what lets a sender retry blindly.
//! - **Deletes are tombstones.** A consumer replicating from a cursor has to be able to observe a
//!   deletion; a removed row would simply vanish from the feed.
//!
//! Payloads arriving here are already projected. The store does not decide what is public.

pub mod error;
pub mod postgres;
pub mod record;
pub mod store;

pub use error::StoreError;
pub use postgres::PostgresStore;
pub use record::{
    Deletion, Digest, DigestPage, IngestRecord, Manifest, Page, PageLimit, StoredRecord, Upsert,
};
pub use store::Store;
