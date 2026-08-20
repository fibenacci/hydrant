//! Integration tests against a real PostgreSQL.
//!
//! `#[sqlx::test]` creates a fresh database per test and applies the migrations, so every test sees
//! an empty store and a fresh sequence. They need a running database — `make db-up`.

// A fixture that cannot be built is a broken test, and panicking names the line. The library itself
// denies both lints.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use hydrant_core::{
    CollectionName, ContentHash, RecordId, RecordKey, SourceName, collection_checksum,
};
use hydrant_store::{
    Applied, Deletion, IngestOp, PageLimit, PostgresStore, Store, StoreError, Token, Upsert,
};
use serde_json::{Value, json};
use sqlx::PgPool;

fn source() -> SourceName {
    SourceName::new("sap-stage").expect("valid source")
}

fn collection(name: &str) -> CollectionName {
    CollectionName::new(name).expect("valid collection")
}

fn key(id: &str) -> RecordKey {
    RecordKey::new(
        source(),
        collection("catalog.product"),
        RecordId::new(id).expect("valid id"),
    )
}

fn upsert(id: &str, payload: Value) -> IngestOp {
    IngestOp::Upsert {
        id: RecordId::new(id).expect("valid id"),
        payload,
    }
}

fn delete(id: &str) -> IngestOp {
    IngestOp::Delete {
        id: RecordId::new(id).expect("valid id"),
    }
}

#[sqlx::test]
async fn upsert_stores_a_record_and_returns_its_feed_position(
    pool: PgPool,
) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let key = key("SW1");

    let outcome = store.upsert(&key, &json!({ "sku": "SW-1" })).await?;
    let Upsert::Stored { seq } = outcome else {
        panic!("expected a write, got {outcome:?}")
    };

    let stored = store.get(&key).await?.expect("record is there");
    assert_eq!(stored.key, key);
    assert_eq!(stored.payload, json!({ "sku": "SW-1" }));
    assert_eq!(stored.seq, seq);
    assert_eq!(stored.deleted_at, None);
    assert_eq!(
        stored.content_hash,
        hydrant_core::content_hash(&json!({ "sku": "SW-1" })).expect("hash")
    );
    Ok(())
}

#[sqlx::test]
async fn an_identical_payload_advances_nothing(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let key = key("SW1");
    let payload = json!({ "sku": "SW-1", "price": 49.9 });

    let first = store.upsert(&key, &payload).await?;
    // Key order differs, so this is the same document arriving from a sender that iterates its map
    // differently. The canonical form is what decides, not the byte order on the wire.
    let again = store
        .upsert(&key, &json!({ "price": 49.9, "sku": "SW-1" }))
        .await?;

    assert_eq!(again, Upsert::Unchanged);
    assert_eq!(
        store.get(&key).await?.expect("record").seq,
        first.seq().expect("first seq")
    );
    Ok(())
}

#[sqlx::test]
async fn a_changed_payload_advances_the_feed_position(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let key = key("SW1");

    let first = store
        .upsert(&key, &json!({ "sku": "SW-1" }))
        .await?
        .seq()
        .expect("stored");
    let second = store
        .upsert(&key, &json!({ "sku": "SW-2" }))
        .await?
        .seq()
        .expect("stored");

    assert!(second > first, "{second:?} should follow {first:?}");
    assert_eq!(
        store.get(&key).await?.expect("record").payload,
        json!({ "sku": "SW-2" })
    );
    Ok(())
}

#[sqlx::test]
async fn delete_writes_a_tombstone(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let key = key("SW1");
    let stored = store
        .upsert(&key, &json!({ "sku": "SW-1" }))
        .await?
        .seq()
        .expect("stored");

    let outcome = store.delete(&key).await?;
    let Deletion::Tombstoned { seq } = outcome else {
        panic!("expected a tombstone")
    };
    assert!(seq > stored);

    // The record is still readable: a consumer replicating from a cursor has to see the deletion.
    let tombstone = store.get(&key).await?.expect("tombstone is readable");
    assert!(tombstone.is_deleted());
    assert_eq!(tombstone.payload, json!({}));
    assert_eq!(tombstone.content_hash, ContentHash::EMPTY_DOCUMENT);
    Ok(())
}

#[sqlx::test]
async fn deleting_twice_costs_nothing(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let key = key("SW1");
    store.upsert(&key, &json!({ "sku": "SW-1" })).await?;
    let first = store.delete(&key).await?.seq().expect("tombstoned");

    assert_eq!(store.delete(&key).await?, Deletion::Unchanged);
    assert_eq!(store.get(&key).await?.expect("tombstone").seq, first);
    Ok(())
}

#[sqlx::test]
async fn deleting_an_unknown_record_is_not_an_error(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    assert_eq!(store.delete(&key("nope")).await?, Deletion::Unchanged);
    assert!(store.get(&key("nope")).await?.is_none());
    Ok(())
}

#[sqlx::test]
async fn re_ingesting_a_deleted_record_revives_it(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let key = key("SW1");
    let payload = json!({ "sku": "SW-1" });

    store.upsert(&key, &payload).await?;
    let tombstone = store.delete(&key).await?.seq().expect("tombstoned");

    // Same payload as before the delete. The content hash matches what the tombstone replaced, so
    // only the deleted state makes this a change - and it must, or the record would stay invisible.
    let revived = store.upsert(&key, &payload).await?.seq().expect("revived");
    assert!(revived > tombstone);

    let stored = store.get(&key).await?.expect("record");
    assert!(!stored.is_deleted());
    assert_eq!(stored.payload, payload);
    Ok(())
}

#[sqlx::test]
async fn feed_positions_are_global_across_collections(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let product = RecordKey::new(
        source(),
        collection("catalog.product"),
        RecordId::new("SW1").expect("id"),
    );
    let category = RecordKey::new(
        source(),
        collection("catalog.category"),
        RecordId::new("SW1").expect("id"),
    );

    let first = store
        .upsert(&product, &json!({ "a": 1 }))
        .await?
        .seq()
        .expect("stored");
    let second = store
        .upsert(&category, &json!({ "a": 1 }))
        .await?
        .seq()
        .expect("stored");

    assert!(second > first, "one sequence serves every collection");
    Ok(())
}

#[sqlx::test]
async fn a_batch_reports_one_outcome_per_record_in_order(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let records = vec![
        upsert("SW1", json!({ "sku": "SW-1" })),
        upsert("SW2", json!({ "sku": "SW-2" })),
        upsert("SW3", json!({ "sku": "SW-3" })),
    ];

    let first = store
        .apply(&source(), &collection("catalog.product"), &records)
        .await?;
    assert_eq!(first.len(), 3);
    assert!(
        first
            .iter()
            .all(|outcome| matches!(outcome, Applied::Stored { .. }))
    );

    // The same batch again: every payload is unchanged, so nothing is written at all.
    let again = store
        .apply(&source(), &collection("catalog.product"), &records)
        .await?;
    assert_eq!(again, vec![Applied::Unchanged; 3]);
    Ok(())
}

#[sqlx::test]
async fn a_repeated_id_inside_one_batch_behaves_like_two_requests(
    pool: PgPool,
) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let records = vec![
        upsert("SW1", json!({ "sku": "first" })),
        upsert("SW1", json!({ "sku": "second" })),
    ];

    let outcomes = store
        .apply(&source(), &collection("catalog.product"), &records)
        .await?;
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, Applied::Stored { .. }))
    );
    assert_eq!(
        store.get(&key("SW1")).await?.expect("record").payload,
        json!({ "sku": "second" })
    );
    Ok(())
}

#[sqlx::test]
async fn a_failed_batch_writes_nothing(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool.clone());
    let records = vec![upsert("SW1", json!({ "sku": "SW-1" }))];

    // Drop the table out from under the transaction so the batch fails mid-flight rather than at
    // the boundary; the point is that a partial batch leaves nothing behind.
    sqlx::query("ALTER TABLE record RENAME TO record_moved")
        .execute(&pool)
        .await?;
    let failed = store
        .apply(&source(), &collection("catalog.product"), &records)
        .await;
    assert!(
        matches!(failed, Err(StoreError::Database(_))),
        "expected a database error"
    );

    sqlx::query("ALTER TABLE record_moved RENAME TO record")
        .execute(&pool)
        .await?;
    assert!(
        store.get(&key("SW1")).await?.is_none(),
        "the batch must have left nothing"
    );
    Ok(())
}

#[sqlx::test]
async fn the_schema_refuses_a_tombstone_that_kept_its_payload(pool: PgPool) -> sqlx::Result<()> {
    // The invariant lives in the database as well as in the code: a deleted record may not keep
    // serving its last known state, whatever writes to the table.
    let result = sqlx::query(
        "INSERT INTO record (source, collection, id, payload, content_hash, deleted_at)
         VALUES ('s', 'c', 'i', '{\"secret\": 1}'::jsonb, decode(repeat('00', 32), 'hex'), now())",
    )
    .execute(&pool)
    .await;

    let error = result.expect_err("the check constraint must reject this");
    assert!(
        error
            .to_string()
            .contains("record_tombstone_has_no_payload"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[sqlx::test]
async fn the_schema_refuses_a_hash_of_the_wrong_width(pool: PgPool) -> sqlx::Result<()> {
    let result = sqlx::query(
        "INSERT INTO record (source, collection, id, payload, content_hash)
         VALUES ('s', 'c', 'i', '{}'::jsonb, decode('ff', 'hex'))",
    )
    .execute(&pool)
    .await;

    let error = result.expect_err("the check constraint must reject this");
    assert!(
        error.to_string().contains("record_content_hash_is_sha256"),
        "unexpected error: {error}"
    );
    Ok(())
}

#[sqlx::test]
async fn a_listing_walks_the_collection_in_feed_order(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    for id in ["SW1", "SW2", "SW3"] {
        store.upsert(&key(id), &json!({ "sku": id })).await?;
    }

    let first = store
        .list(
            &source(),
            &collection("catalog.product"),
            None,
            PageLimit::clamp(2),
        )
        .await?;
    assert_eq!(first.records.len(), 2);
    assert_eq!(first.records[0].key.id.as_str(), "SW1");
    assert_eq!(first.records[1].key.id.as_str(), "SW2");
    let cursor = first.next.expect("a full page offers a cursor");

    let second = store
        .list(
            &source(),
            &collection("catalog.product"),
            Some(cursor),
            PageLimit::clamp(2),
        )
        .await?;
    assert_eq!(second.records.len(), 1);
    assert_eq!(second.records[0].key.id.as_str(), "SW3");
    assert_eq!(second.next, None, "a short page ends the walk");
    Ok(())
}

#[sqlx::test]
async fn a_listing_does_not_serve_tombstones(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    store.upsert(&key("SW1"), &json!({ "sku": "SW-1" })).await?;
    store.upsert(&key("SW2"), &json!({ "sku": "SW-2" })).await?;
    store.delete(&key("SW1")).await?;

    let page = store
        .list(
            &source(),
            &collection("catalog.product"),
            None,
            PageLimit::default(),
        )
        .await?;
    let ids: Vec<&str> = page
        .records
        .iter()
        .map(|record| record.key.id.as_str())
        .collect();
    assert_eq!(ids, ["SW2"]);
    Ok(())
}

#[sqlx::test]
async fn an_unknown_collection_lists_empty(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let page = store
        .list(
            &source(),
            &collection("catalog.nothing"),
            None,
            PageLimit::default(),
        )
        .await?;
    assert!(page.records.is_empty());
    assert_eq!(page.next, None);
    Ok(())
}

#[sqlx::test]
async fn the_cache_validator_moves_when_a_record_is_deleted(
    pool: PgPool,
) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let products = collection("catalog.product");
    assert_eq!(store.max_seq(&source(), &products).await?, None);

    store.upsert(&key("SW1"), &json!({ "sku": "SW-1" })).await?;
    let after_write = store
        .max_seq(&source(), &products)
        .await?
        .expect("a position");

    // A deletion changes what the collection serves, so it has to invalidate a cached listing -
    // even though the record itself is no longer listed.
    let tombstone = store.delete(&key("SW1")).await?.seq().expect("tombstoned");
    let after_delete = store
        .max_seq(&source(), &products)
        .await?
        .expect("a position");

    assert!(after_delete > after_write);
    assert_eq!(after_delete, tombstone);
    Ok(())
}

#[sqlx::test]
async fn the_change_feed_carries_tombstones(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    store.upsert(&key("SW1"), &json!({ "sku": "SW-1" })).await?;
    store.upsert(&key("SW2"), &json!({ "sku": "SW-2" })).await?;
    store.delete(&key("SW1")).await?;

    let feed = store
        .changes(
            &source(),
            &collection("catalog.product"),
            None,
            PageLimit::default(),
        )
        .await?;

    // SW2 first (written second, but SW1 moved to the end when it was deleted).
    let ids: Vec<&str> = feed
        .records
        .iter()
        .map(|record| record.key.id.as_str())
        .collect();
    assert_eq!(ids, ["SW2", "SW1"]);

    let tombstone = feed.records.last().expect("the deletion");
    assert!(
        tombstone.is_deleted(),
        "a consumer has to be able to observe the deletion"
    );
    assert_eq!(tombstone.payload, json!({}));
    Ok(())
}

#[sqlx::test]
async fn the_change_feed_resumes_after_a_cursor(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let first = store
        .upsert(&key("SW1"), &json!({ "a": 1 }))
        .await?
        .seq()
        .expect("stored");
    store.upsert(&key("SW2"), &json!({ "a": 2 })).await?;

    let feed = store
        .changes(
            &source(),
            &collection("catalog.product"),
            Some(first),
            PageLimit::default(),
        )
        .await?;
    let ids: Vec<&str> = feed
        .records
        .iter()
        .map(|record| record.key.id.as_str())
        .collect();
    assert_eq!(ids, ["SW2"], "everything up to the cursor is already known");
    Ok(())
}

#[sqlx::test]
async fn an_unchanged_upsert_produces_no_feed_entry(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let payload = json!({ "sku": "SW-1" });
    store.upsert(&key("SW1"), &payload).await?;
    store.upsert(&key("SW1"), &payload).await?;
    store.upsert(&key("SW1"), &payload).await?;

    let feed = store
        .changes(
            &source(),
            &collection("catalog.product"),
            None,
            PageLimit::default(),
        )
        .await?;
    assert_eq!(feed.records.len(), 1, "three pushes, one change");
    Ok(())
}

#[sqlx::test]
async fn a_manifest_describes_the_live_records(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let live = json!({ "sku": "SW-2" });
    store.upsert(&key("SW1"), &json!({ "sku": "SW-1" })).await?;
    store.upsert(&key("SW2"), &live).await?;
    let tombstone = store.delete(&key("SW1")).await?.seq().expect("tombstoned");

    let manifest = store
        .manifest(&source(), &collection("catalog.product"))
        .await?;
    assert_eq!(
        manifest.count, 1,
        "a tombstone is not a record the collection holds"
    );
    assert_eq!(
        manifest.max_seq,
        Some(tombstone),
        "but it does move the feed position, because it changed what is served"
    );

    // The checksum a sender computes from its own side has to match, or drift detection is
    // meaningless. This is that computation, done independently of the store.
    let expected = collection_checksum([("SW2", hydrant_core::content_hash(&live).expect("hash"))])
        .expect("checksum");
    assert_eq!(manifest.checksum, expected);
    Ok(())
}

#[sqlx::test]
async fn a_manifest_of_an_untouched_collection_is_stable(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let manifest = store
        .manifest(&source(), &collection("catalog.nothing"))
        .await?;

    assert_eq!(manifest.count, 0);
    assert_eq!(manifest.max_seq, None);
    assert_eq!(
        manifest.checksum,
        collection_checksum(std::iter::empty()).expect("checksum")
    );
    Ok(())
}

#[sqlx::test]
async fn digests_walk_by_id_and_skip_tombstones(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    for id in ["SW3", "SW1", "SW2"] {
        store.upsert(&key(id), &json!({ "sku": id })).await?;
    }
    store.delete(&key("SW2")).await?;

    let page = store
        .digests(
            &source(),
            &collection("catalog.product"),
            None,
            PageLimit::default(),
        )
        .await?;
    let ids: Vec<&str> = page.entries.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(ids, ["SW1", "SW3"], "id order, and no tombstones");
    assert_eq!(
        page.entries[0].content_hash,
        hydrant_core::content_hash(&json!({ "sku": "SW1" })).expect("hash")
    );
    Ok(())
}

#[sqlx::test]
async fn digests_page_from_the_last_id_seen(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    for id in ["SW1", "SW2", "SW3"] {
        store.upsert(&key(id), &json!({ "sku": id })).await?;
    }

    let first = store
        .digests(
            &source(),
            &collection("catalog.product"),
            None,
            PageLimit::clamp(2),
        )
        .await?;
    assert_eq!(first.entries.len(), 2);
    let cursor = first.next.expect("a full page offers a cursor");
    assert_eq!(cursor.as_str(), "SW2");

    let second = store
        .digests(
            &source(),
            &collection("catalog.product"),
            Some(&cursor),
            PageLimit::clamp(2),
        )
        .await?;
    let ids: Vec<&str> = second
        .entries
        .iter()
        .map(|entry| entry.id.as_str())
        .collect();
    assert_eq!(ids, ["SW3"]);
    assert_eq!(second.next, None);
    Ok(())
}

#[sqlx::test]
async fn a_batch_applies_writes_and_deletions_in_the_order_given(
    pool: PgPool,
) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let ops = vec![
        upsert("SW1", json!({ "sku": "SW-1" })),
        upsert("SW2", json!({ "sku": "SW-2" })),
        delete("SW1"),
        delete("SW3"),
    ];

    let outcomes = store
        .apply(&source(), &collection("catalog.product"), &ops)
        .await?;
    assert!(matches!(outcomes[0], Applied::Stored { .. }));
    assert!(matches!(outcomes[1], Applied::Stored { .. }));
    assert!(
        matches!(outcomes[2], Applied::Tombstoned { .. }),
        "written, then deleted"
    );
    assert_eq!(
        outcomes[3],
        Applied::Unchanged,
        "there was no SW3 to delete"
    );

    assert!(store.get(&key("SW1")).await?.expect("record").is_deleted());
    assert!(!store.get(&key("SW2")).await?.expect("record").is_deleted());
    Ok(())
}

#[sqlx::test]
async fn a_credential_resolves_to_its_source(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let token = Token::generate().expect("randomness");
    let hash = token.hash(b"application secret").expect("usable secret");

    store
        .store_token(&hash, &source(), "sap-prod outbox")
        .await?;

    assert_eq!(store.authenticate(&hash).await?, Some(source()));
    Ok(())
}

#[sqlx::test]
async fn an_unknown_credential_resolves_to_nothing(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let hash = Token::generate()
        .expect("randomness")
        .hash(b"secret")
        .expect("usable secret");
    assert_eq!(store.authenticate(&hash).await?, None);
    Ok(())
}

#[sqlx::test]
async fn the_same_token_under_a_different_secret_does_not_authenticate(
    pool: PgPool,
) -> Result<(), StoreError> {
    // The stored form is keyed by the application secret, which is what makes a database dump
    // useless on its own: the same token hashed under another key is another credential.
    let store = PostgresStore::from_pool(pool);
    let token = Token::generate().expect("randomness");
    store
        .store_token(
            &token.hash(b"real secret").expect("hash"),
            &source(),
            "outbox",
        )
        .await?;

    let elsewhere = token.hash(b"guessed secret").expect("hash");
    assert_eq!(store.authenticate(&elsewhere).await?, None);
    Ok(())
}

#[sqlx::test]
async fn a_revoked_credential_stops_working_but_stays_on_record(
    pool: PgPool,
) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool.clone());
    let hash = Token::generate()
        .expect("randomness")
        .hash(b"secret")
        .expect("hash");
    store
        .store_token(&hash, &source(), "laptop of the integrator")
        .await?;

    sqlx::query("UPDATE ingest_token SET revoked_at = now()")
        .execute(&pool)
        .await?;
    assert_eq!(store.authenticate(&hash).await?, None);

    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM ingest_token")
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        remaining, 1,
        "knowing a credential existed is worth the row"
    );
    Ok(())
}

#[sqlx::test]
async fn a_token_cannot_be_recorded_twice(pool: PgPool) -> Result<(), StoreError> {
    let store = PostgresStore::from_pool(pool);
    let hash = Token::generate()
        .expect("randomness")
        .hash(b"secret")
        .expect("hash");
    store.store_token(&hash, &source(), "first").await?;

    let again = store.store_token(&hash, &source(), "second").await;
    assert!(matches!(again, Err(StoreError::Database(_))));
    Ok(())
}
