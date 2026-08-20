//! PostgreSQL implementation of [`Store`].

use hydrant_core::{
    CollectionName, ContentHash, RecordId, RecordKey, Seq, SourceName, content_hash,
};
use serde_json::Value;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgExecutor, PgPool};

use crate::error::StoreError;
use crate::record::{Deletion, IngestRecord, Page, PageLimit, StoredRecord, Upsert};
use crate::store::Store;

/// A store backed by PostgreSQL.
#[derive(Debug, Clone)]
pub struct PostgresStore {
    pool: PgPool,
}

impl PostgresStore {
    /// Connects to `url` with at most `max_connections` pooled connections.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Database`] if the connection cannot be established.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
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

    async fn upsert_batch(
        &self,
        source: &SourceName,
        collection: &CollectionName,
        records: &[IngestRecord],
    ) -> Result<Vec<Upsert>, StoreError> {
        // Hash before opening the transaction: a payload that cannot be canonicalised should not
        // hold a connection, and it fails the whole batch either way.
        let mut hashed = Vec::with_capacity(records.len());
        for record in records {
            hashed.push((record, content_hash(&record.payload)?));
        }

        let mut transaction = self.pool.begin().await?;
        let mut outcomes = Vec::with_capacity(records.len());
        for (record, hash) in hashed {
            let key = RecordKey::new(source.clone(), collection.clone(), record.id.clone());
            outcomes.push(write_upsert(&mut *transaction, &key, &record.payload, &hash).await?);
        }
        transaction.commit().await?;
        Ok(outcomes)
    }

    async fn delete(&self, key: &RecordKey) -> Result<Deletion, StoreError> {
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
        .fetch_optional(&self.pool)
        .await?;

        match seq {
            Some(seq) => Ok(Deletion::Tombstoned { seq: to_seq(seq)? }),
            None => Ok(Deletion::Unchanged),
        }
    }

    async fn list(
        &self,
        source: &SourceName,
        collection: &CollectionName,
        after: Option<Seq>,
        limit: PageLimit,
    ) -> Result<Page, StoreError> {
        let after = i64::try_from(after.map_or(0, Seq::get)).map_err(|_| StoreError::Corrupt {
            reason: "cursor is beyond the range of a bigint".to_owned(),
        })?;
        let rows = sqlx::query!(
            r#"
            SELECT id, seq, payload, content_hash, ingested_at, deleted_at
              FROM record
             WHERE source = $1
               AND collection = $2
               AND deleted_at IS NULL
               AND seq > $3
             ORDER BY seq
             LIMIT $4
            "#,
            source.as_str(),
            collection.as_str(),
            after,
            i64::from(limit.get()),
        )
        .fetch_all(&self.pool)
        .await?;

        let full_page = rows.len() == usize::from(limit.get());
        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let id = RecordId::new(row.id).map_err(|error| StoreError::Corrupt {
                reason: format!("stored id is not a usable identifier: {error}"),
            })?;
            records.push(StoredRecord {
                key: RecordKey::new(source.clone(), collection.clone(), id),
                payload: row.payload,
                content_hash: to_content_hash(&row.content_hash)?,
                seq: to_seq(row.seq)?,
                ingested_at: row.ingested_at,
                deleted_at: row.deleted_at,
            });
        }

        // Only a full page promises there may be more. A short page ends the walk, which is what
        // lets a consumer stop without a second request.
        let next = if full_page {
            records.last().map(|record| record.seq)
        } else {
            None
        };
        Ok(Page { records, next })
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
