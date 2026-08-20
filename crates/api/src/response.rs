//! The public JSON shapes.
//!
//! These are a contract: a consumer replicates against them. Adding a field is safe, renaming or
//! removing one is not.

use hydrant_store::{Page, StoredRecord};
use serde::Serialize;
use serde_json::Value;
use time::format_description::well_known::Rfc3339;

/// One record as served.
///
/// The payload is nested rather than merged into the envelope, so a field a source system happens
/// to call `seq` cannot collide with the record's own metadata.
#[derive(Debug, Serialize)]
pub struct RecordBody {
    /// The record's identifier within its source and collection.
    pub id: String,
    /// The record's position in the change feed, as of its last change.
    pub seq: u64,
    /// When that change was ingested, RFC 3339 in UTC.
    pub ingested_at: String,
    /// SHA-256 over the payload's RFC 8785 canonical form, hex. Also the record's `ETag`.
    pub content_hash: String,
    /// The released fields, and nothing else.
    pub payload: Value,
}

impl From<StoredRecord> for RecordBody {
    fn from(record: StoredRecord) -> Self {
        Self {
            id: record.key.id.to_string(),
            seq: record.seq.get(),
            // An unformattable timestamp cannot come out of PostgreSQL; an empty string is still
            // preferable to a panic on a public endpoint.
            ingested_at: record.ingested_at.format(&Rfc3339).unwrap_or_default(),
            content_hash: record.content_hash.to_hex(),
            payload: record.payload,
        }
    }
}

/// One page of a collection.
#[derive(Debug, Serialize)]
pub struct PageBody {
    /// The records, in feed order.
    pub records: Vec<RecordBody>,
    /// Pass this back as `?cursor=` to continue. `null` means the walk is complete.
    pub next_cursor: Option<u64>,
    /// The collection's highest feed position, tombstones included. A consumer that has reached
    /// this has seen everything the collection currently serves.
    pub max_seq: u64,
}

impl PageBody {
    /// Builds the body from a store page and the collection's cache validator.
    #[must_use]
    pub fn new(page: Page, max_seq: u64) -> Self {
        Self {
            records: page.records.into_iter().map(RecordBody::from).collect(),
            next_cursor: page.next.map(hydrant_core::Seq::get),
            max_seq,
        }
    }
}
