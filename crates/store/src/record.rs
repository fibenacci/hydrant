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

/// How many records a listing may return.
///
/// The cap is enforced here rather than at the HTTP edge, and it is a clamp rather than a
/// rejection: a public endpoint that returns whatever `?limit=` asked for is a denial-of-service
/// vector, and one that 400s on a large limit only moves the problem into the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PageLimit(u16);

impl PageLimit {
    /// The largest page the store will ever return, whatever the request asked for.
    pub const MAX: u16 = 1000;

    /// The page size used when a request does not ask for one.
    pub const DEFAULT: u16 = 100;

    /// Clamps `requested` into `1..=MAX`.
    #[must_use]
    pub const fn clamp(requested: u16) -> Self {
        if requested == 0 {
            Self(1)
        } else if requested > Self::MAX {
            Self(Self::MAX)
        } else {
            Self(requested)
        }
    }

    /// The clamped page size.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl Default for PageLimit {
    fn default() -> Self {
        Self(Self::DEFAULT)
    }
}

/// One page of a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The records in feed order.
    pub records: Vec<StoredRecord>,
    /// The cursor to pass as `after` for the next page, or `None` when the page was not full.
    ///
    /// Pagination is cursor-based on `seq`, never offset-based: an offset over a table that is
    /// being written to produces duplicates and gaps.
    pub next: Option<Seq>,
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

#[cfg(test)]
mod tests {
    use super::PageLimit;

    #[test]
    fn a_limit_is_clamped_rather_than_rejected() {
        assert_eq!(PageLimit::clamp(50).get(), 50);
        assert_eq!(PageLimit::clamp(0).get(), 1);
        assert_eq!(PageLimit::clamp(u16::MAX).get(), PageLimit::MAX);
        assert_eq!(PageLimit::default().get(), PageLimit::DEFAULT);
    }
}
