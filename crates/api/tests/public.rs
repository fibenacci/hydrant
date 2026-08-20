//! End-to-end tests of the public read API against a real database.
//!
//! The router is driven directly through `tower::ServiceExt::oneshot`, so there is no socket in the
//! way, but everything below the handler is real: a PostgreSQL store, real migrations, real `ETag` handling.

// A fixture that cannot be built is a broken test. The library denies both lints.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use axum::Router;
use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, ETAG, IF_NONE_MATCH};
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use hydrant_api::{ApiState, router};
use hydrant_core::{CollectionName, RecordId, RecordKey, SourceName};
use hydrant_store::{PostgresStore, Store};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

fn key(id: &str) -> RecordKey {
    RecordKey::new(
        SourceName::new("sap-stage").expect("source"),
        CollectionName::new("catalog.product").expect("collection"),
        RecordId::new(id).expect("id"),
    )
}

fn app(pool: PgPool) -> Router {
    router(ApiState::new(PostgresStore::from_pool(pool), 300))
}

/// One request. Returns status, the `ETag` if there was one, and the parsed body.
async fn get(
    app: &Router,
    uri: &str,
    if_none_match: Option<&str>,
) -> (StatusCode, Option<String>, Value) {
    let mut request = Request::builder().uri(uri);
    if let Some(validator) = if_none_match {
        request = request.header(IF_NONE_MATCH, validator);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).expect("request"))
        .await
        .expect("response");

    let status = response.status();
    let etag = response
        .headers()
        .get(ETAG)
        .map(|value| value.to_str().expect("ascii etag").to_owned());
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, etag, body)
}

#[sqlx::test(migrations = "../store/migrations")]
async fn an_item_carries_its_content_hash_as_the_validator(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "sku": "SW-1" }))
        .await
        .expect("stored");
    let app = app(pool);

    let (status, etag, body) = get(&app, "/v1/sap-stage/catalog.product/SW1", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], "SW1");
    assert_eq!(body["payload"], json!({ "sku": "SW-1" }));

    let hash = hydrant_core::content_hash(&json!({ "sku": "SW-1" }))
        .expect("hash")
        .to_hex();
    assert_eq!(etag.expect("etag"), format!("\"{hash}\""));
    assert_eq!(body["content_hash"], hash);
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_matching_validator_answers_304_without_a_body(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "sku": "SW-1" }))
        .await
        .expect("stored");
    let app = app(pool);

    let (_, etag, _) = get(&app, "/v1/sap-stage/catalog.product/SW1", None).await;
    let etag = etag.expect("etag");

    let (status, returned, body) =
        get(&app, "/v1/sap-stage/catalog.product/SW1", Some(&etag)).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(returned.expect("etag on a 304"), etag);
    assert_eq!(body, Value::Null, "a 304 carries no body");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_changed_record_invalidates_the_validator(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "sku": "SW-1" }))
        .await
        .expect("stored");
    let app = app(pool);
    let (_, stale, _) = get(&app, "/v1/sap-stage/catalog.product/SW1", None).await;

    store
        .upsert(&key("SW1"), &json!({ "sku": "SW-2" }))
        .await
        .expect("stored");
    let (status, fresh, body) = get(
        &app,
        "/v1/sap-stage/catalog.product/SW1",
        Some(&stale.clone().expect("etag")),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::OK,
        "the old validator must not answer 304"
    );
    assert_ne!(fresh, stale);
    assert_eq!(body["payload"], json!({ "sku": "SW-2" }));
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_deleted_record_is_gone_rather_than_stale(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "secret": "was public once" }))
        .await
        .expect("stored");
    store.delete(&key("SW1")).await.expect("deleted");
    let app = app(pool);

    let (status, _, body) = get(&app, "/v1/sap-stage/catalog.product/SW1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    assert!(
        !body.to_string().contains("was public once"),
        "a tombstone must not serve its last known state"
    );
}

#[sqlx::test(migrations = "../store/migrations")]
async fn an_unknown_record_is_404(pool: PgPool) {
    let (status, _, body) = get(&app(pool), "/v1/sap-stage/catalog.product/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_listing_pages_with_a_cursor(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    for id in ["SW1", "SW2", "SW3"] {
        store
            .upsert(&key(id), &json!({ "sku": id }))
            .await
            .expect("stored");
    }
    let app = app(pool);

    let (status, _, first) = get(&app, "/v1/sap-stage/catalog.product?limit=2", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["records"].as_array().expect("records").len(), 2);
    assert_eq!(first["records"][0]["id"], "SW1");
    let cursor = first["next_cursor"]
        .as_u64()
        .expect("a full page offers a cursor");

    let (_, _, second) = get(
        &app,
        &format!("/v1/sap-stage/catalog.product?limit=2&cursor={cursor}"),
        None,
    )
    .await;
    assert_eq!(second["records"].as_array().expect("records").len(), 1);
    assert_eq!(second["records"][0]["id"], "SW3");
    assert_eq!(
        second["next_cursor"],
        Value::Null,
        "a short page ends the walk"
    );
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_listing_validator_moves_when_a_record_is_deleted(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "sku": "SW-1" }))
        .await
        .expect("stored");
    store
        .upsert(&key("SW2"), &json!({ "sku": "SW-2" }))
        .await
        .expect("stored");
    let app = app(pool);

    let (_, etag, body) = get(&app, "/v1/sap-stage/catalog.product", None).await;
    let etag = etag.expect("etag");
    assert!(body["max_seq"].as_u64().expect("max_seq") > 0);

    // Deleting SW2 removes it from the listing; the cached page must not be served as fresh.
    store.delete(&key("SW2")).await.expect("deleted");
    let (status, _, body) = get(&app, "/v1/sap-stage/catalog.product", Some(&etag)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["records"].as_array().expect("records").len(), 1);
}

#[sqlx::test(migrations = "../store/migrations")]
async fn an_unknown_query_parameter_is_refused(pool: PgPool) {
    let app = app(pool);
    for uri in [
        "/v1/sap-stage/catalog.product?filter[sku]=SW-1",
        "/v1/sap-stage/catalog.product?limit=10&offset=20",
    ] {
        let (status, _, body) = get(&app, uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri} should be refused");
        assert_eq!(body["error"]["code"], "invalid_query");
    }
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_malformed_address_is_refused(pool: PgPool) {
    let app = app(pool);
    let (status, _, body) = get(&app, "/v1/SAP-Stage/catalog.product", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_path");
    assert!(
        body["error"]["message"]
            .as_str()
            .expect("message")
            .contains("source")
    );
}

#[sqlx::test(migrations = "../store/migrations")]
async fn an_oversized_limit_is_clamped_rather_than_refused(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "sku": "SW-1" }))
        .await
        .expect("stored");

    let (status, _, body) = get(
        &app(pool),
        "/v1/sap-stage/catalog.product?limit=65535",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["records"].as_array().expect("records").len(), 1);
}

#[sqlx::test(migrations = "../store/migrations")]
async fn every_cacheable_response_says_it_is_public(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "sku": "SW-1" }))
        .await
        .expect("stored");
    let app = app(pool);

    for uri in [
        "/v1/sap-stage/catalog.product",
        "/v1/sap-stage/catalog.product/SW1",
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        let cache_control = response
            .headers()
            .get(CACHE_CONTROL)
            .expect("cache-control")
            .to_str()
            .expect("ascii");
        assert_eq!(cache_control, "public, s-maxage=300", "{uri}");
    }
}

#[sqlx::test(migrations = "../store/migrations")]
async fn health_reports_liveness(pool: PgPool) {
    let (status, _, body) = get(&app(pool), "/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
}
