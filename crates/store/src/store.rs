//! The storage contract.

use hydrant_core::{CollectionName, RecordKey, SourceName};
use serde_json::Value;

use crate::error::StoreError;
use hydrant_core::Seq;

use crate::record::{Deletion, IngestRecord, Page, PageLimit, StoredRecord, Upsert};

/// What hydrant needs from a store.
///
/// The trait exists so the PostgreSQL implementation stays replaceable, not because a second one is
/// planned. Nothing should be added here that a document store cannot answer: no joins, no
/// aggregation, no resolution of references between records.
///
/// Every method returns a `Send` future, so the HTTP layer can hold a store across an await without
/// the trait forcing a boxing dance on it.
pub trait Store: Send + Sync {
    /// Writes one record, if its payload differs from what is stored.
    ///
    /// The content hash is computed here rather than accepted from the caller: a hash that does not
    /// match its payload would make idempotency and drift detection lie, and the only way to rule
    /// that out is to not offer the choice.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the payload cannot be canonicalised or the database rejects the
    /// write.
    fn upsert(
        &self,
        key: &RecordKey,
        payload: &Value,
    ) -> impl Future<Output = Result<Upsert, StoreError>> + Send;

    /// Writes a batch of records of one collection, in one transaction.
    ///
    /// Outcomes come back in the order the records were given. Repeating an id inside one batch is
    /// allowed: the later entry sees the earlier one, exactly as two separate requests would.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if any payload cannot be canonicalised or the transaction fails, in
    /// which case nothing in the batch was written.
    fn upsert_batch(
        &self,
        source: &SourceName,
        collection: &CollectionName,
        records: &[IngestRecord],
    ) -> impl Future<Output = Result<Vec<Upsert>, StoreError>> + Send;

    /// Turns a record into a tombstone.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the database rejects the write.
    fn delete(&self, key: &RecordKey) -> impl Future<Output = Result<Deletion, StoreError>> + Send;

    /// Reads one page of a collection in feed order, tombstones excluded.
    ///
    /// Tombstones are left out because a listing serves what is public now; a consumer that needs
    /// to observe deletions replicates the change feed instead.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the database rejects the query or a row cannot be read back as a
    /// record.
    fn list(
        &self,
        source: &SourceName,
        collection: &CollectionName,
        after: Option<Seq>,
        limit: PageLimit,
    ) -> impl Future<Output = Result<Page, StoreError>> + Send;

    /// The highest feed position in a collection, tombstones included.
    ///
    /// This is the collection's cache validator. Tombstones count: a deletion changes what the
    /// collection serves, so it has to invalidate a cached listing.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the database rejects the query.
    fn max_seq(
        &self,
        source: &SourceName,
        collection: &CollectionName,
    ) -> impl Future<Output = Result<Option<Seq>, StoreError>> + Send;

    /// Reads one record, tombstones included — a caller replicating the feed has to see them.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the database rejects the query or the row cannot be read back as a
    /// record.
    fn get(
        &self,
        key: &RecordKey,
    ) -> impl Future<Output = Result<Option<StoredRecord>, StoreError>> + Send;
}
