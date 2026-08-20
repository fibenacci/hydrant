//! End-to-end tests of the ingest surface against a real database.
//!
//! The interesting cases are the ones where a sender is wrong: no credential, a field the schema
//! does not release, a delete with a payload, a batch that is too large. Ingest is where the
//! service decides what becomes public, so its refusals matter more than its successes.

// A fixture that cannot be built is a broken test. The library denies both lints.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use hydrant_api::{IngestState, ingest_router};
use hydrant_core::schema::CollectionSchema;
use hydrant_core::{CollectionName, RecordId, RecordKey, SchemaSet, SourceName};
use hydrant_store::{PostgresStore, Store, Token};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const SECRET: &[u8] = b"an application secret of sufficient length";
const TOKEN: &str = "hyd_0123456789abcdef0123456789abcdef";

fn source() -> SourceName {
    SourceName::new("sap-stage").expect("source")
}

fn key(id: &str) -> RecordKey {
    RecordKey::new(
        source(),
        CollectionName::new("catalog.product").expect("collection"),
        RecordId::new(id).expect("id"),
    )
}

fn schemas() -> SchemaSet {
    let product: CollectionSchema = serde_json::from_value(json!({
        "collection": "catalog.product",
        "id": "$.id",
        "fields": {
            "sku": { "type": "string", "index": true },
            "price": { "type": "number" },
            "attributes": { "type": "object", "allow": ["color", "size"] }
        },
        "filters": ["sku"]
    }))
    .expect("valid schema");
    SchemaSet::new([product]).expect("one collection")
}

/// A router plus a credential that is already in the database.
async fn app(pool: PgPool) -> (Router, PostgresStore) {
    app_with_limits(pool, None).await
}

/// The same, with a per-record payload limit.
async fn app_with_limits(pool: PgPool, max_payload: Option<usize>) -> (Router, PostgresStore) {
    let store = PostgresStore::from_pool(pool);
    let hash = Token::from_presented(TOKEN).hash(SECRET).expect("hash");
    store
        .store_token(&hash, &source(), "test sender")
        .await
        .expect("token stored");
    let mut state = IngestState::new(store.clone(), schemas(), SECRET);
    if let Some(max_payload) = max_payload {
        state = state.with_limits(max_payload, 4 * 1024 * 1024);
    }
    (ingest_router(state), store)
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value, Option<String>) {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).expect("serialise")))
            .expect("request"),
        None => request.body(Body::empty()).expect("request"),
    };

    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let challenge = response
        .headers()
        .get(WWW_AUTHENTICATE)
        .map(|value| value.to_str().expect("ascii").to_owned());
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let parsed = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json")
    };
    (status, parsed, challenge)
}

async fn post(app: &Router, body: Value) -> (StatusCode, Value) {
    let (status, parsed, _) = send(
        app,
        Method::POST,
        "/v1/ingest/catalog.product",
        Some(TOKEN),
        Some(body),
    )
    .await;
    (status, parsed)
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_request_without_a_credential_is_refused(pool: PgPool) {
    let (app, _) = app(pool).await;
    let (status, body, challenge) = send(
        &app,
        Method::POST,
        "/v1/ingest/catalog.product",
        None,
        Some(json!([{ "op": "upsert", "id": "SW1", "payload": { "sku": "SW-1" } }])),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert_eq!(
        challenge.as_deref(),
        Some("Bearer"),
        "a 401 has to say how to authenticate"
    );
}

#[sqlx::test(migrations = "../store/migrations")]
async fn an_unknown_credential_is_refused_the_same_way(pool: PgPool) {
    let (app, _) = app(pool).await;
    let (status, body, _) = send(
        &app,
        Method::POST,
        "/v1/ingest/catalog.product",
        Some("hyd_wrong"),
        Some(json!([])),
    )
    .await;

    // Same code, same message as a missing header: telling a caller which of the two it was tells
    // it how to get closer.
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_payload_is_projected_before_it_is_stored(pool: PgPool) {
    let (app, store) = app(pool).await;
    let (status, body) = post(
        &app,
        json!([{
            "op": "upsert",
            "id": "SW1",
            "payload": {
                "sku": "SW-1",
                "price": 49.9,
                "cost_price": 12.5,
                "internal_note": "do not publish",
                "attributes": { "color": "red", "supplier": "ACME" }
            }
        }]),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let result = &body["results"][0];
    assert_eq!(result["outcome"], "stored");
    assert!(result["seq"].as_u64().expect("seq") > 0);

    let dropped: Vec<&str> = result["dropped"]
        .as_array()
        .expect("dropped")
        .iter()
        .map(|d| d["path"].as_str().expect("path"))
        .collect();
    assert!(dropped.contains(&"cost_price"), "{dropped:?}");
    assert!(dropped.contains(&"internal_note"), "{dropped:?}");
    assert!(dropped.contains(&"attributes.supplier"), "{dropped:?}");

    // What matters is not the report but the store: the fields were never persisted.
    let stored = store
        .get(&key("SW1"))
        .await
        .expect("query")
        .expect("record");
    assert_eq!(
        stored.payload,
        json!({ "sku": "SW-1", "price": 49.9, "attributes": { "color": "red" } })
    );
    assert!(!stored.payload.to_string().contains("do not publish"));
}

#[sqlx::test(migrations = "../store/migrations")]
async fn pushing_the_same_payload_twice_costs_nothing(pool: PgPool) {
    let (app, _) = app(pool).await;
    let batch = json!([{ "op": "upsert", "id": "SW1", "payload": { "sku": "SW-1" } }]);

    let (_, first) = post(&app, batch.clone()).await;
    let (_, second) = post(&app, batch).await;

    assert_eq!(first["results"][0]["outcome"], "stored");
    assert_eq!(second["results"][0]["outcome"], "unchanged");
    assert_eq!(
        second["results"][0]["seq"],
        Value::Null,
        "no feed position was consumed"
    );
}

#[sqlx::test(migrations = "../store/migrations")]
async fn an_upsert_may_leave_the_id_to_the_schema(pool: PgPool) {
    let (app, store) = app(pool).await;
    // No `id` in the envelope: the schema's `id: $.id` lifts it out of the payload. The id itself is
    // not a declared field, so it does not survive projection - it belongs to the record's key.
    let (status, body) = post(
        &app,
        json!([{ "op": "upsert", "payload": { "id": "SW9", "sku": "SW-9" } }]),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"][0]["id"], "SW9");
    let stored = store
        .get(&key("SW9"))
        .await
        .expect("query")
        .expect("record");
    assert_eq!(stored.payload, json!({ "sku": "SW-9" }));
}

#[sqlx::test(migrations = "../store/migrations")]
async fn an_upsert_without_an_identifier_anywhere_is_refused(pool: PgPool) {
    let (app, _) = app(pool).await;
    let (status, body) = post(
        &app,
        json!([{ "op": "upsert", "payload": { "sku": "SW-1" } }]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "missing_id");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_batch_applies_writes_and_deletions_in_order(pool: PgPool) {
    let (app, store) = app(pool).await;
    let (status, body) = post(
        &app,
        json!([
            { "op": "upsert", "id": "SW1", "payload": { "sku": "SW-1" } },
            { "op": "upsert", "id": "SW2", "payload": { "sku": "SW-2" } },
            { "op": "delete", "id": "SW1" }
        ]),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let results = body["results"].as_array().expect("results");
    assert_eq!(results[0]["outcome"], "stored");
    assert_eq!(results[2]["outcome"], "tombstoned");
    assert!(
        store
            .get(&key("SW1"))
            .await
            .expect("query")
            .expect("record")
            .is_deleted()
    );
    assert!(
        !store
            .get(&key("SW2"))
            .await
            .expect("query")
            .expect("record")
            .is_deleted()
    );
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_malformed_operation_is_refused(pool: PgPool) {
    let (app, _) = app(pool).await;

    let (status, body) = post(&app, json!([{ "op": "upsert", "id": "SW1" }])).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "missing_payload");

    let (status, body) = post(
        &app,
        json!([{ "op": "delete", "id": "SW1", "payload": { "sku": "x" } }]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "payload_on_delete");

    // A misspelled envelope key would otherwise be ignored, and the sender would believe it sent
    // something it did not.
    let (status, body) = post(
        &app,
        json!([{ "op": "upsert", "id": "SW1", "data": { "sku": "x" } }]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_body");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn an_oversized_batch_is_refused(pool: PgPool) {
    let (app, _) = app(pool).await;
    let batch: Vec<Value> = (0..=1000)
        .map(|n| json!({ "op": "upsert", "id": format!("SW{n}"), "payload": { "sku": "x" } }))
        .collect();

    let (status, body) = post(&app, Value::Array(batch)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "batch_too_large");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn an_undeclared_collection_cannot_be_written_to(pool: PgPool) {
    let (app, _) = app(pool).await;
    let (status, body, _) = send(
        &app,
        Method::POST,
        "/v1/ingest/catalog.nothing",
        Some(TOKEN),
        Some(json!([{ "op": "upsert", "id": "SW1", "payload": {} }])),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "unknown_collection");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn the_source_comes_from_the_credential_not_from_the_request(pool: PgPool) {
    let (app, store) = app(pool).await;
    post(
        &app,
        json!([{ "op": "upsert", "id": "SW1", "payload": { "sku": "SW-1" } }]),
    )
    .await;

    // The request never names a source. It cannot: the credential decides, which is what keeps one
    // sender from writing into another's partition.
    assert!(store.get(&key("SW1")).await.expect("query").is_some());
    let elsewhere = RecordKey::new(
        SourceName::new("other-source").expect("source"),
        CollectionName::new("catalog.product").expect("collection"),
        RecordId::new("SW1").expect("id"),
    );
    assert!(store.get(&elsewhere).await.expect("query").is_none());
}

#[sqlx::test(migrations = "../store/migrations")]
async fn the_delete_endpoint_writes_a_tombstone(pool: PgPool) {
    let (app, store) = app(pool).await;
    post(
        &app,
        json!([{ "op": "upsert", "id": "SW1", "payload": { "sku": "SW-1" } }]),
    )
    .await;

    let (status, body, _) = send(
        &app,
        Method::DELETE,
        "/v1/ingest/catalog.product/SW1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["outcome"], "tombstoned");
    assert!(
        store
            .get(&key("SW1"))
            .await
            .expect("query")
            .expect("record")
            .is_deleted()
    );

    let (_, body, _) = send(
        &app,
        Method::DELETE,
        "/v1/ingest/catalog.product/SW1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(body["outcome"], "unchanged", "deleting twice is a no-op");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn digests_are_listed_for_the_credentials_source(pool: PgPool) {
    let (app, _) = app(pool).await;
    post(
        &app,
        json!([
            { "op": "upsert", "id": "SW1", "payload": { "sku": "SW-1" } },
            { "op": "upsert", "id": "SW2", "payload": { "sku": "SW-2" } }
        ]),
    )
    .await;

    let (status, body, _) = send(
        &app,
        Method::GET,
        "/v1/ingest/catalog.product/digests?limit=1",
        Some(TOKEN),
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let digests = body["digests"].as_array().expect("digests");
    assert_eq!(digests.len(), 1);
    assert_eq!(digests[0]["id"], "SW1");
    assert_eq!(
        digests[0]["content_hash"],
        hydrant_core::content_hash(&json!({ "sku": "SW-1" }))
            .expect("hash")
            .to_hex()
    );
    assert_eq!(body["next_cursor"], "SW1");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn the_digest_listing_needs_a_credential_too(pool: PgPool) {
    let (app, _) = app(pool).await;
    let (status, _, _) = send(
        &app,
        Method::GET,
        "/v1/ingest/catalog.product/digests",
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_record_larger_than_the_limit_is_refused(pool: PgPool) {
    let (app, store) = app_with_limits(pool, Some(200)).await;
    let (status, body, _) = send(
        &app,
        Method::POST,
        "/v1/ingest/catalog.product",
        Some(TOKEN),
        Some(json!([{ "op": "upsert", "id": "SW1", "payload": { "sku": "x".repeat(500) } }])),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body["error"]["code"], "payload_too_large");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("SW1"),
        "the refusal names the record: {body}"
    );
    assert!(
        store.get(&key("SW1")).await.expect("query").is_none(),
        "and nothing was stored"
    );
}

#[sqlx::test(migrations = "../store/migrations")]
async fn size_is_measured_on_what_would_be_stored(pool: PgPool) {
    // The payload is far over the limit as sent, but almost all of it is fields the schema does not
    // release. What counts is what would be stored and served, so this is accepted.
    let (app, store) = app_with_limits(pool, Some(200)).await;
    let (status, body) = post(
        &app,
        json!([{
            "op": "upsert",
            "id": "SW1",
            "payload": { "sku": "SW-1", "internal_note": "y".repeat(2000) }
        }]),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["results"][0]["outcome"], "stored");
    assert_eq!(
        store
            .get(&key("SW1"))
            .await
            .expect("query")
            .expect("record")
            .payload,
        json!({ "sku": "SW-1" })
    );
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_whole_batch_is_refused_for_one_oversized_record(pool: PgPool) {
    // The batch is one transaction, so a record that cannot be stored refuses the batch rather than
    // leaving the sender to work out which half landed.
    let (app, store) = app_with_limits(pool, Some(200)).await;
    let (status, _, _) = send(
        &app,
        Method::POST,
        "/v1/ingest/catalog.product",
        Some(TOKEN),
        Some(json!([
            { "op": "upsert", "id": "SW1", "payload": { "sku": "small" } },
            { "op": "upsert", "id": "SW2", "payload": { "sku": "x".repeat(500) } }
        ])),
    )
    .await;

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        store.get(&key("SW1")).await.expect("query").is_none(),
        "nothing was written"
    );
}
