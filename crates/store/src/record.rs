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

/// One operation on its way in.
///
/// Payloads arriving here are already projected: the store never decides what is public.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOp {
    /// Write the record if its payload differs from what is stored.
    Upsert {
        /// The record's identifier, as assigned by the source system.
        id: RecordId,
        /// The already-projected payload.
        payload: Value,
    },
    /// Turn the record into a tombstone.
    Delete {
        /// The record's identifier.
        id: RecordId,
    },
}

impl IngestOp {
    /// The record this operation addresses.
    #[must_use]
    pub const fn id(&self) -> &RecordId {
        match self {
            Self::Upsert { id, .. } | Self::Delete { id } => id,
        }
    }
}

/// What one operation of a batch did.
///
/// A batch mixes writes and deletions, so its outcome needs a vocabulary that covers both. The
/// single-record methods keep their own narrower types: an upsert cannot produce a tombstone, and a
/// type that says so is worth more than one fewer enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    /// A payload was written, at a new feed position.
    Stored {
        /// The record's new position in the change feed.
        seq: Seq,
    },
    /// A live record became a tombstone, at a new feed position.
    Tombstoned {
        /// The tombstone's position in the change feed.
        seq: Seq,
    },
    /// Nothing was written: the payload already hashed to the same value, or there was nothing to
    /// delete. No feed position was consumed.
    Unchanged,
}

impl Applied {
    /// The new feed position, if anything was written.
    #[must_use]
    pub const fn seq(self) -> Option<Seq> {
        match self {
            Self::Stored { seq } | Self::Tombstoned { seq } => Some(seq),
            Self::Unchanged => None,
        }
    }
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

/// How many bytes of payload a page may carry.
///
/// The record count alone does not bound a response: a thousand records of sixty kilobytes each is
/// sixty megabytes. This bounds the answer in the unit that actually costs something to produce, to
/// transfer and to cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteBudget(usize);

impl ByteBudget {
    /// The largest budget a request can ask for.
    pub const MAX: usize = 4 * 1024 * 1024;

    /// The budget used when a request does not ask for one.
    pub const DEFAULT: usize = 1024 * 1024;

    /// Clamps `requested` into `1..=MAX`.
    #[must_use]
    pub const fn clamp(requested: usize) -> Self {
        if requested == 0 {
            Self(1)
        } else if requested > Self::MAX {
            Self(Self::MAX)
        } else {
            Self(requested)
        }
    }

    /// The clamped budget.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl Default for ByteBudget {
    fn default() -> Self {
        Self(Self::DEFAULT)
    }
}

/// What a page may cost: a record count and a payload budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PageBudget {
    /// The most records to return.
    pub records: PageLimit,
    /// The most payload bytes to return.
    pub bytes: ByteBudget,
}

impl PageBudget {
    /// A budget of `records` and the default byte allowance.
    #[must_use]
    pub const fn of(records: PageLimit) -> Self {
        Self {
            records,
            bytes: ByteBudget(ByteBudget::DEFAULT),
        }
    }
}

/// One page of a listing or of the change feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// The records in feed order.
    pub records: Vec<StoredRecord>,
    /// The cursor to pass as `after` for the next page, or `None` when the walk is complete.
    ///
    /// Pagination is cursor-based on `seq`, never offset-based: an offset over a table that is
    /// being written to produces duplicates and gaps. A cursor is offered when the record count was
    /// reached *or* the byte budget cut the page short — from the caller's side the two are the same
    /// thing, which is why it is one field.
    pub next: Option<Seq>,
}

/// A collection summarised for drift detection.
///
/// `count` and `checksum` describe the live records; `max_seq` includes tombstones, because it is
/// also the cache validator and a deletion changes what the collection serves. The asymmetry is
/// deliberate: a sender compares its own live set against `count` and `checksum`, and a deleted
/// record is not in either side's live set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// How many live records the collection holds.
    pub count: u64,
    /// SHA-256 over the canonical form of the live `[id, hash]` pairs.
    pub checksum: ContentHash,
    /// The highest feed position, tombstones included, or `None` for an untouched collection.
    pub max_seq: Option<Seq>,
}

/// One record's identity and content hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    /// The record's identifier.
    pub id: RecordId,
    /// SHA-256 over its payload's canonical form.
    pub content_hash: ContentHash,
}

/// One page of digests, walked by id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestPage {
    /// The digests, in id order.
    pub entries: Vec<Digest>,
    /// The id to pass as `after` for the next page, or `None` when the page was not full.
    pub next: Option<RecordId>,
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
