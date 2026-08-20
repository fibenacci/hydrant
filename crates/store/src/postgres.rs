//! PostgreSQL implementation of [`Store`].

use hydrant_core::{
    CollectionName, ContentHash, Filter, RecordId, RecordKey, Seq, SourceName, collection_checksum,
    content_hash,
};
use std::time::Duration;

use serde_json::Value;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgExecutor, PgPool};
use time::OffsetDateTime;

use crate::error::StoreError;
use crate::record::{
    Applied, Deletion, Digest, DigestPage, IngestOp, Manifest, Page, PageLimit, StoredRecord,
    Upsert,
};
use crate::store::Store;
use crate::token::TokenHash;

/// A store backed by PostgreSQL.
#[derive(Debug, Clone)]
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    /// Connects to `url` with at most `max_connections` pooled connections, and a statement timeout
    /// on every one of them.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Database`] if `url` cannot be parsed or the connection fails.
    pub async fn connect(
        url: &str,
        max_connections: u32,
        statement_timeout: Duration,
    ) -> Result<Self, StoreError> {
        let options: PgConnectOptions = url.parse()?;
        Self::connect_with(options, max_connections, statement_timeout).await
    }

    /// The same, from already-parsed connection options.
    ///
    /// The statement timeout is not a tuning knob: this service answers unauthenticated requests,
    /// and a query that can run without bound is a denial-of-service vector rather than a slow page.
    /// Setting it on the connection rather than per query means no code path can forget it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Database`] if the connection cannot be established or the timeout
    /// cannot be set.
    pub async fn connect_with(
        options: PgConnectOptions,
        max_connections: u32,
        statement_timeout: Duration,
    ) -> Result<Self, StoreError> {
        let millis = statement_timeout.as_millis().min(u128::from(u32::MAX));
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .after_connect(move |connection, _| {
                Box::pin(async move {
                    sqlx::query(&format!("SET statement_timeout = {millis}"))
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await?;
        Ok(Self { pool })
    }

    /// Wraps an existing pool, which is what the tests and the server's own wiring use.
    #[must_use]
    pub const fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The underlying pool.
    #[must_use]
    pub const fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Brings the schema up to date.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Migration`] if a migration fails or the recorded checksums do not
    /// match the migrations on disk.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }
}

impl Store for PostgresStore {
    async fn upsert(&self, key: &RecordKey, payload: &Value) -> Result<Upsert, StoreError> {
        let hash = content_hash(payload)?;
        write_upsert(&self.pool, key, payload, &hash).await
    }

    async fn apply(
        &self,
        source: &SourceName,
        collection: &CollectionName,
        ops: &[IngestOp],
    ) -> Result<Vec<Applied>, StoreError> {
        // Hash before opening the transaction: a payload that cannot be canonicalised should not
        // hold a connection, and it fails the whole batch either way.
        let mut hashed = Vec::with_capacity(ops.len());
        for op in ops {
            let hash = match op {
                IngestOp::Upsert { payload, .. } => Some(content_hash(payload)?),
                IngestOp::Delete { .. } => None,
            };
            hashed.push((op, hash));
        }

        let mut transaction = self.pool.begin().await?;
        let mut outcomes = Vec::with_capacity(ops.len());
        for (op, hash) in hashed {
            let key = RecordKey::new(source.clone(), collection.clone(), op.id().clone());
            let outcome = match (op, hash) {
                (IngestOp::Upsert { payload, .. }, Some(hash)) => {
                    match write_upsert(&mut *transaction, &key, payload, &hash).await? {
                        Upsert::Stored { seq } => Applied::Stored { seq },
                        Upsert::Unchanged => Applied::Unchanged,
                    }
                }
                (IngestOp::Delete { .. }, _) => {
                    match write_delete(&mut *transaction, &key).await? {
                        Deletion::Tombstoned { seq } => Applied::Tombstoned { seq },
                        Deletion::Unchanged => Applied::Unchanged,
                    }
                }
                // An upsert always carries a hash; the pairing above is what guarantees it.
                (IngestOp::Upsert { .. }, None) => {
                    return Err(StoreError::Corrupt {
                        reason: "an upsert reached the store without a content hash".to_owned(),
                    });
                }
            };
            outcomes.push(outcome);
        }
        transaction.commit().await?;
        Ok(outcomes)
    }

    async fn delete(&self, key: &RecordKey) -> Result<Deletion, StoreError> {
        write_delete(&self.pool, key).await
    }

    async fn list(
        &self,
        source: &SourceName,
        collection: &CollectionName,
        filter: &Filter,
        after: Option<Seq>,
        limit: PageLimit,
    ) -> Result<Page, StoreError> {
        let rows = sqlx::query_as!(
            RawRecord,
            r#"
            SELECT id, seq, payload, content_hash, ingested_at, deleted_at
              FROM record
             WHERE source = $1
               AND collection = $2
               AND deleted_at IS NULL
               AND seq > $3
               AND payload @> $5
             ORDER BY seq
             LIMIT $4
            "#,
            source.as_str(),
            collection.as_str(),
            cursor(after)?,
            i64::from(limit.get()),
            filter.as_json(),
        )
        .fetch_all(&self.pool)
        .await?;

        page(rows, source, collection, limit)
    }

    async fn changes(
        &self,
        source: &SourceName,
        collection: &CollectionName,
        since: Option<Seq>,
        limit: PageLimit,
    ) -> Result<Page, StoreError> {
        // The one difference from `list`: no `deleted_at IS NULL`. A consumer replicating from a
        // cursor has to see the tombstone, or it keeps serving a record that was deleted.
        let rows = sqlx::query_as!(
            RawRecord,
            r#"
            SELECT id, seq, payload, content_hash, ingested_at, deleted_at
              FROM record
             WHERE source = $1
               AND collection = $2
               AND seq > $3
             ORDER BY seq
             LIMIT $4
            "#,
            source.as_str(),
            collection.as_str(),
            cursor(since)?,
            i64::from(limit.get()),
        )
        .fetch_all(&self.pool)
        .await?;

        page(rows, source, collection, limit)
    }

    async fn manifest(
        &self,
        source: &SourceName,
        collection: &CollectionName,
    ) -> Result<Manifest, StoreError> {
        // A full scan of the live rows. That is the honest cost of a checksum that is defined over
        // the collection's contents; a maintained aggregate would be a cache with its own drift.
        let rows = sqlx::query!(
            r#"
            SELECT id, content_hash
              FROM record
             WHERE source = $1
               AND collection = $2
               AND deleted_at IS NULL
            "#,
            source.as_str(),
            collection.as_str(),
        )
        .fetch_all(&self.pool)
        .await?;

        let mut pairs = Vec::with_capacity(rows.len());
        for row in rows {
            pairs.push((row.id, to_content_hash(&row.content_hash)?));
        }
        let checksum = collection_checksum(pairs.iter().map(|(id, hash)| (id.as_str(), *hash)))?;
        let count = u64::try_from(pairs.len()).map_err(|_| StoreError::Corrupt {
            reason: "collection holds more records than a u64 can count".to_owned(),
        })?;

        Ok(Manifest {
            count,
            checksum,
            max_seq: self.max_seq(source, collection).await?,
        })
    }

    async fn digests(
        &self,
        source: &SourceName,
        collection: &CollectionName,
        after: Option<&RecordId>,
        limit: PageLimit,
    ) -> Result<DigestPage, StoreError> {
        // Walked by id, not by feed position: a sender joins its own records to these by id, and a
        // record that changed mid-walk would otherwise move past the cursor and be missed. Ordering
        // and comparison both use the database's collation, so the walk stays consistent whatever
        // that collation is.
        let rows = sqlx::query!(
            r#"
            SELECT id, content_hash
              FROM record
             WHERE source = $1
               AND collection = $2
               AND deleted_at IS NULL
               AND id > $3
             ORDER BY id
             LIMIT $4
            "#,
            source.as_str(),
            collection.as_str(),
            after.map_or("", RecordId::as_str),
            i64::from(limit.get()),
        )
        .fetch_all(&self.pool)
        .await?;

        let full_page = rows.len() == usize::from(limit.get());
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(Digest {
                id: RecordId::new(row.id).map_err(|error| StoreError::Corrupt {
                    reason: format!("stored id is not a usable identifier: {error}"),
                })?,
                content_hash: to_content_hash(&row.content_hash)?,
            });
        }

        let next = if full_page {
            entries.last().map(|entry| entry.id.clone())
        } else {
            None
        };
        Ok(DigestPage { entries, next })
    }

    async fn max_seq(
        &self,
        source: &SourceName,
        collection: &CollectionName,
    ) -> Result<Option<Seq>, StoreError> {
        let max = sqlx::query_scalar!(
            r#"
            SELECT max(seq) FROM record WHERE source = $1 AND collection = $2
            "#,
            source.as_str(),
            collection.as_str(),
        )
        .fetch_one(&self.pool)
        .await?;

        max.map(to_seq).transpose()
    }

    async fn authenticate(&self, token: &TokenHash) -> Result<Option<SourceName>, StoreError> {
        let source = sqlx::query_scalar!(
            r#"
            SELECT source
              FROM ingest_token
             WHERE token_hash = $1
               AND revoked_at IS NULL
            "#,
            token.as_bytes().as_slice(),
        )
        .fetch_optional(&self.pool)
        .await?;

        source
            .map(|source| {
                SourceName::new(source).map_err(|error| StoreError::Corrupt {
                    reason: format!("stored source name is not usable: {error}"),
                })
            })
            .transpose()
    }

    async fn store_token(
        &self,
        token: &TokenHash,
        source: &SourceName,
        label: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            r#"
            INSERT INTO ingest_token (token_hash, source, label)
            VALUES ($1, $2, $3)
            "#,
            token.as_bytes().as_slice(),
            source.as_str(),
            label,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(&self, key: &RecordKey) -> Result<Option<StoredRecord>, StoreError> {
        let row = sqlx::query!(
            r#"
            SELECT seq, payload, content_hash, ingested_at, deleted_at
              FROM record
             WHERE source = $1
               AND collection = $2
               AND id = $3
            "#,
            key.source.as_str(),
            key.collection.as_str(),
            key.id.as_str(),
        )
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else { return Ok(None) };
        Ok(Some(StoredRecord {
            key: key.clone(),
            payload: row.payload,
            content_hash: to_content_hash(&row.content_hash)?,
            seq: to_seq(row.seq)?,
            ingested_at: row.ingested_at,
            deleted_at: row.deleted_at,
        }))
    }
}

/// The upsert, written once so the single and the batch path cannot drift apart.
///
/// The `WHERE` clause on `DO UPDATE` is what makes ingest idempotent: an identical payload updates
/// no row, so no feed position is consumed and `RETURNING` yields nothing. A tombstone is treated
/// as a change even when the payload matches, because a resurrection is something a consumer has to
/// see.
async fn write_upsert<'e, E>(
    executor: E,
    key: &RecordKey,
    payload: &Value,
    hash: &ContentHash,
) -> Result<Upsert, StoreError>
where
    E: PgExecutor<'e>,
{
    let seq = sqlx::query_scalar!(
        r#"
        INSERT INTO record (source, collection, id, payload, content_hash)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (source, collection, id) DO UPDATE
           SET payload = excluded.payload,
               content_hash = excluded.content_hash,
               seq = nextval('record_seq'),
               ingested_at = now(),
               deleted_at = NULL
         WHERE record.content_hash <> excluded.content_hash
            OR record.deleted_at IS NOT NULL
        RETURNING seq
        "#,
        key.source.as_str(),
        key.collection.as_str(),
        key.id.as_str(),
        payload,
        hash.as_bytes().as_slice(),
    )
    .fetch_optional(executor)
    .await?;

    match seq {
        Some(seq) => Ok(Upsert::Stored { seq: to_seq(seq)? }),
        None => Ok(Upsert::Unchanged),
    }
}

/// The delete, written once so the single and the batch path cannot drift apart.
///
/// A tombstone keeps the record addressable but empties it: the payload becomes the empty document
/// and the hash becomes that document's hash, which is what the schema's constraint requires. The
/// `deleted_at IS NULL` clause makes deleting twice a no-op rather than a second feed entry.
async fn write_delete<'e, E>(executor: E, key: &RecordKey) -> Result<Deletion, StoreError>
where
    E: PgExecutor<'e>,
{
    let seq = sqlx::query_scalar!(
        r#"
        UPDATE record
           SET payload = '{}'::jsonb,
               content_hash = $4,
               seq = nextval('record_seq'),
               ingested_at = now(),
               deleted_at = now()
         WHERE source = $1
           AND collection = $2
           AND id = $3
           AND deleted_at IS NULL
        RETURNING seq
        "#,
        key.source.as_str(),
        key.collection.as_str(),
        key.id.as_str(),
        ContentHash::EMPTY_DOCUMENT.as_bytes().as_slice(),
    )
    .fetch_optional(executor)
    .await?;

    match seq {
        Some(seq) => Ok(Deletion::Tombstoned { seq: to_seq(seq)? }),
        None => Ok(Deletion::Unchanged),
    }
}

/// A row of the record table, before it becomes a [`StoredRecord`].
///
/// Named rather than anonymous so the listing and the change feed share one mapping: two copies of
/// it would be two places for a column to be read wrongly.
struct RawRecord {
    id: String,
    seq: i64,
    payload: Value,
    content_hash: Vec<u8>,
    ingested_at: OffsetDateTime,
    deleted_at: Option<OffsetDateTime>,
}

impl RawRecord {
    fn into_record(
        self,
        source: &SourceName,
        collection: &CollectionName,
    ) -> Result<StoredRecord, StoreError> {
        let id = RecordId::new(self.id).map_err(|error| StoreError::Corrupt {
            reason: format!("stored id is not a usable identifier: {error}"),
        })?;
        Ok(StoredRecord {
            key: RecordKey::new(source.clone(), collection.clone(), id),
            payload: self.payload,
            content_hash: to_content_hash(&self.content_hash)?,
            seq: to_seq(self.seq)?,
            ingested_at: self.ingested_at,
            deleted_at: self.deleted_at,
        })
    }
}

/// Turns rows into a page.
///
/// Only a full page promises there may be more. A short page ends the walk, which is what lets a
/// consumer stop without a second request.
fn page(
    rows: Vec<RawRecord>,
    source: &SourceName,
    collection: &CollectionName,
    limit: PageLimit,
) -> Result<Page, StoreError> {
    let full_page = rows.len() == usize::from(limit.get());
    let records = rows
        .into_iter()
        .map(|row| row.into_record(source, collection))
        .collect::<Result<Vec<_>, _>>()?;
    let next = if full_page {
        records.last().map(|record| record.seq)
    } else {
        None
    };
    Ok(Page { records, next })
}

/// A feed cursor as the database wants it.
fn cursor(after: Option<Seq>) -> Result<i64, StoreError> {
    i64::try_from(after.map_or(0, Seq::get)).map_err(|_| StoreError::Corrupt {
        reason: "cursor is beyond the range of a bigint".to_owned(),
    })
}

/// `seq` is a positive bigint in the schema; a negative one means the sequence was tampered with.
fn to_seq(raw: i64) -> Result<Seq, StoreError> {
    u64::try_from(raw)
        .map(Seq::new)
        .map_err(|_| StoreError::Corrupt {
            reason: format!("seq {raw} is negative"),
        })
}

/// The `record_content_hash_is_sha256` constraint makes this unreachable, which is exactly why it
/// is checked rather than assumed.
fn to_content_hash(raw: &[u8]) -> Result<ContentHash, StoreError> {
    let bytes: [u8; 32] = raw.try_into().map_err(|_| StoreError::Corrupt {
        reason: format!("content hash is {} bytes, expected 32", raw.len()),
    })?;
    Ok(ContentHash::from_bytes(bytes))
}
