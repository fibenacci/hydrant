//! What a stored record looks like, and what writing one did.

use hydrant_core::{ContentHash, RecordId, RecordKey, Seq};
use serde_json::Value;
use time::OffsetDateTime;

/// A record as it sits in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRecord {
    /// What addresses this record.
    pub key: RecordKey,
    /// The projected payload. Empty for a tombstone.
    pub payload: Value,
    /// SHA-256 over the payload's canonical form.
    pub content_hash: ContentHash,
    /// Position in the change feed, as of the last change.
    pub seq: Seq,
    /// When the last change was written.
    pub ingested_at: OffsetDateTime,
    /// When the record was deleted, if it was.
    pub deleted_at: Option<OffsetDateTime>,
}

impl StoredRecord {
    /// Whether this record is a tombstone.
    #[must_use]
    pub const fn is_deleted(&self) -> bool {
        self.deleted_at.is_some()
    }
}

/// One record on its way in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestRecord {
    /// The record's identifier, as assigned by the source system.
    pub id: RecordId,
    /// The already-projected payload. Projection happens before the store sees it — the store
    /// never decides what is public.
    pub payload: Value,
}

/// What an upsert did.
///
/// The distinction is the whole point of hashing content: a sender may push the same payload as
/// often as it likes, and only a real change costs a feed position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    /// The payload differed from what was stored — or the record was a tombstone — so it was
    /// written and given a new feed position.
    Stored {
        /// The record's new position in the change feed.
        seq: Seq,
    },
    /// The stored payload already hashed to the same value. Nothing was written, no feed position
    /// was consumed, and no consumer will see an entry.
    Unchanged,
}

impl Upsert {
    /// The new feed position, if anything was written.
    #[must_use]
    pub const fn seq(self) -> Option<Seq> {
        match self {
            Self::Stored { seq } => Some(seq),
            Self::Unchanged => None,
        }
    }
}

/// What a delete did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deletion {
    /// A live record became a tombstone, at a new feed position.
    Tombstoned {
        /// The tombstone's position in the change feed.
        seq: Seq,
    },
    /// There was nothing to delete: no such record, or it was already a tombstone. Deleting twice
    /// is not an error, so a sender's retry costs nothing.
    Unchanged,
}

impl Deletion {
    /// The new feed position, if a tombstone was written.
    #[must_use]
    pub const fn seq(self) -> Option<Seq> {
        match self {
            Self::Tombstoned { seq } => Some(seq),
            Self::Unchanged => None,
        }
    }
}
