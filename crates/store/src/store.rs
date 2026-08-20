//! The storage contract.

use hydrant_core::{CollectionName, RecordKey, SourceName};
use serde_json::Value;

use crate::error::StoreError;
use crate::record::{Deletion, IngestRecord, StoredRecord, Upsert};

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
