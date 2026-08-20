//! The storage contract.

use hydrant_core::{CollectionName, RecordId, RecordKey, SourceName};
use serde_json::Value;

use crate::error::StoreError;
use hydrant_core::Seq;

use crate::record::{
    Applied, Deletion, DigestPage, IngestOp, Manifest, Page, PageLimit, StoredRecord, Upsert,
};
use crate::token::TokenHash;

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

    /// Applies a batch of operations to one collection, in one transaction.
    ///
    /// Outcomes come back in the order the operations were given. Repeating an id inside one batch
    /// is allowed: the later operation sees the earlier one, exactly as two separate requests would,
    /// which is what makes an upsert followed by a delete behave the way a sender wrote it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if a payload cannot be canonicalised or the transaction fails, in
    /// which case nothing in the batch was written.
    fn apply(
        &self,
        source: &SourceName,
        collection: &CollectionName,
        ops: &[IngestOp],
    ) -> impl Future<Output = Result<Vec<Applied>, StoreError>> + Send;

    /// Turns a record into a tombstone.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the database rejects the write.
    fn delete(&self, key: &RecordKey) -> impl Future<Output = Result<Deletion, StoreError>> + Send;

    /// Resolves an ingest credential to the source it may write to.
    ///
    /// Takes the hashed form, never the token: the plaintext has no business below the HTTP layer,
    /// and the lookup is a primary-key hit on a MAC rather than a comparison in application code.
    /// A revoked credential resolves to nothing.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the database rejects the query or the stored source name is not a
    /// usable identifier.
    fn authenticate(
        &self,
        token: &TokenHash,
    ) -> impl Future<Output = Result<Option<SourceName>, StoreError>> + Send;

    /// Records a credential for `source`, under a label that says who holds it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the database rejects the write - including when the same token is
    /// recorded twice.
    fn store_token(
        &self,
        token: &TokenHash,
        source: &SourceName,
        label: &str,
    ) -> impl Future<Output = Result<(), StoreError>> + Send;

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

    /// One page of the change feed: every change after `since`, tombstones included.
    ///
    /// This is the difference from [`Store::list`], and it is the point of keeping tombstones at
    /// all. A consumer replicating from a cursor has to observe a deletion; a removed row would
    /// simply stop appearing and the consumer would keep serving what it last saw.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the database rejects the query or a row cannot be read back as a
    /// record.
    fn changes(
        &self,
        source: &SourceName,
        collection: &CollectionName,
        since: Option<Seq>,
        limit: PageLimit,
    ) -> impl Future<Output = Result<Page, StoreError>> + Send;

    /// The collection's count, checksum and feed position.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the database rejects the query, a row cannot be read back, or the
    /// pairs cannot be canonicalised.
    fn manifest(
        &self,
        source: &SourceName,
        collection: &CollectionName,
    ) -> impl Future<Output = Result<Manifest, StoreError>> + Send;

    /// One page of per-record digests, in id order, tombstones excluded.
    ///
    /// A checksum can only say *that* a collection drifted. These say which record — and comparing
    /// two hashes is what turns "something is wrong" into "re-push this one". Walking by id rather
    /// than by feed position is what lets a sender join the two sides.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the database rejects the query or a row cannot be read back.
    fn digests(
        &self,
        source: &SourceName,
        collection: &CollectionName,
        after: Option<&RecordId>,
        limit: PageLimit,
    ) -> impl Future<Output = Result<DigestPage, StoreError>> + Send;

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
