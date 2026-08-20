//! The public JSON shapes.
//!
//! These are a contract: a consumer replicates against them. Adding a field is safe, renaming or
//! removing one is not.

use hydrant_store::{Manifest, Page, StoredRecord};
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

/// One entry of the change feed.
///
/// A tombstone appears here with `deleted: true` and an empty payload. That is the entry a consumer
/// replicating from a cursor needs: without it, a deleted record would simply stop appearing and the
/// consumer would keep serving what it last saw.
#[derive(Debug, Serialize)]
pub struct ChangeBody {
    /// The record's identifier.
    pub id: String,
    /// The feed position of this change.
    pub seq: u64,
    /// Whether this change is a deletion.
    pub deleted: bool,
    /// When the change was ingested, RFC 3339 in UTC.
    pub ingested_at: String,
    /// SHA-256 over the payload's canonical form. For a tombstone, the hash of the empty document.
    pub content_hash: String,
    /// The released fields, or `{}` for a tombstone.
    pub payload: Value,
}

impl From<StoredRecord> for ChangeBody {
    fn from(record: StoredRecord) -> Self {
        Self {
            id: record.key.id.to_string(),
            seq: record.seq.get(),
            deleted: record.is_deleted(),
            ingested_at: record.ingested_at.format(&Rfc3339).unwrap_or_default(),
            content_hash: record.content_hash.to_hex(),
            payload: record.payload,
        }
    }
}

/// One page of the change feed.
#[derive(Debug, Serialize)]
pub struct ChangesBody {
    /// The changes, in feed order.
    pub changes: Vec<ChangeBody>,
    /// Pass this back as `?since=` to continue. `null` means the consumer is caught up.
    pub next_cursor: Option<u64>,
    /// The collection's highest feed position.
    pub max_seq: u64,
}

impl ChangesBody {
    /// Builds the body from a store page and the collection's feed position.
    #[must_use]
    pub fn new(page: Page, max_seq: u64) -> Self {
        Self {
            changes: page.records.into_iter().map(ChangeBody::from).collect(),
            next_cursor: page.next.map(hydrant_core::Seq::get),
            max_seq,
        }
    }
}

/// A collection summarised: the cheap comparison a sender uses to detect drift.
#[derive(Debug, Serialize)]
pub struct ManifestBody {
    /// How many live records the collection holds. Tombstones are not counted.
    pub count: u64,
    /// SHA-256 over the canonical form of the live `[id, hash]` pairs, hex.
    pub checksum: String,
    /// The highest feed position, tombstones included. A sender that has replicated up to this has
    /// seen every change.
    pub max_seq: u64,
}

impl From<Manifest> for ManifestBody {
    fn from(manifest: Manifest) -> Self {
        Self {
            count: manifest.count,
            checksum: manifest.checksum.to_hex(),
            max_seq: manifest.max_seq.map_or(0, hydrant_core::Seq::get),
        }
    }
}
