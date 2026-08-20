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
use hydrant_core::schema::CollectionSchema;
use hydrant_core::{CollectionName, RecordId, RecordKey, SchemaSet, SourceName};
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

/// Two collections: the example one, and a second that keeps a longer shared cache so the
/// per-collection directive is actually observable.
fn schemas() -> SchemaSet {
    let product: CollectionSchema = serde_json::from_value(json!({
        "collection": "catalog.product",
        "id": "$.id",
        "fields": {
            "sku": { "type": "string", "index": true },
            "name": { "type": "string", "index": true },
            "price": { "type": "number" },
            "attributes": { "type": "object", "allow": ["color", "size"] }
        },
        "filters": ["sku"],
        "sort": ["seq", "name"],
        "cache": { "shared_max_age": 300 }
    }))
    .expect("valid product schema");

    let category: CollectionSchema = serde_json::from_value(json!({
        "collection": "catalog.category",
        "id": "$.id",
        "fields": { "name": { "type": "string" } },
        "cache": { "shared_max_age": 600 }
    }))
    .expect("valid category schema");

    SchemaSet::new([product, category]).expect("distinct collections")
}

fn app(pool: PgPool) -> Router {
    router(ApiState::new(PostgresStore::from_pool(pool), schemas()))
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

#[sqlx::test(migrations = "../store/migrations")]
async fn the_change_feed_carries_a_tombstone(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "sku": "SW-1" }))
        .await
        .expect("stored");
    store
        .upsert(&key("SW2"), &json!({ "sku": "SW-2" }))
        .await
        .expect("stored");
    store.delete(&key("SW1")).await.expect("deleted");

    let app = app(pool);
    let (status, _, body) = get(&app, "/v1/sap-stage/catalog.product/changes", None).await;
    assert_eq!(status, StatusCode::OK);

    let changes = body["changes"].as_array().expect("changes");
    assert_eq!(changes.len(), 2);
    let tombstone = changes.last().expect("the deletion");
    assert_eq!(tombstone["id"], "SW1");
    assert_eq!(tombstone["deleted"], true);
    assert_eq!(tombstone["payload"], json!({}));
    // The listing does not show it; the feed must.
    let (_, _, listing) = get(&app, "/v1/sap-stage/catalog.product", None).await;
    assert_eq!(listing["records"].as_array().expect("records").len(), 1);
}

#[sqlx::test(migrations = "../store/migrations")]
async fn the_change_feed_resumes_from_a_cursor(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "a": 1 }))
        .await
        .expect("stored");
    store
        .upsert(&key("SW2"), &json!({ "a": 2 }))
        .await
        .expect("stored");
    let app = app(pool);

    let (_, _, first) = get(&app, "/v1/sap-stage/catalog.product/changes?limit=1", None).await;
    let cursor = first["next_cursor"]
        .as_u64()
        .expect("a full page offers a cursor");
    assert_eq!(first["changes"][0]["id"], "SW1");

    let (_, _, second) = get(
        &app,
        &format!("/v1/sap-stage/catalog.product/changes?since={cursor}&limit=1"),
        None,
    )
    .await;
    assert_eq!(second["changes"][0]["id"], "SW2");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_caught_up_consumer_gets_304_until_something_changes(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "a": 1 }))
        .await
        .expect("stored");
    let app = app(pool);

    let (_, etag, _) = get(&app, "/v1/sap-stage/catalog.product/changes", None).await;
    let etag = etag.expect("etag");

    let (status, _, _) = get(&app, "/v1/sap-stage/catalog.product/changes", Some(&etag)).await;
    assert_eq!(
        status,
        StatusCode::NOT_MODIFIED,
        "nothing changed, so nothing to send"
    );

    store
        .upsert(&key("SW2"), &json!({ "a": 2 }))
        .await
        .expect("stored");
    let (status, _, body) = get(&app, "/v1/sap-stage/catalog.product/changes", Some(&etag)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["changes"].as_array().expect("changes").len(), 2);
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_manifest_reports_what_a_sender_can_reproduce(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    let live = json!({ "sku": "SW-2" });
    store
        .upsert(&key("SW1"), &json!({ "sku": "SW-1" }))
        .await
        .expect("stored");
    store.upsert(&key("SW2"), &live).await.expect("stored");
    store.delete(&key("SW1")).await.expect("deleted");

    let (status, etag, body) =
        get(&app(pool), "/v1/sap-stage/catalog.product/manifest", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["count"], 1,
        "a tombstone is not a record the collection holds"
    );
    assert!(body["max_seq"].as_u64().expect("max_seq") > 0);
    assert!(etag.is_some());

    let expected = hydrant_core::collection_checksum([(
        "SW2",
        hydrant_core::content_hash(&live).expect("hash"),
    )])
    .expect("checksum");
    assert_eq!(body["checksum"], expected.to_hex());
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_manifest_is_conditional_too(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "a": 1 }))
        .await
        .expect("stored");
    let app = app(pool);

    let (_, etag, _) = get(&app, "/v1/sap-stage/catalog.product/manifest", None).await;
    let (status, _, _) = get(
        &app,
        "/v1/sap-stage/catalog.product/manifest",
        Some(&etag.expect("etag")),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
}

#[sqlx::test(migrations = "../store/migrations")]
async fn an_unknown_feed_parameter_is_refused(pool: PgPool) {
    let (status, _, body) = get(
        &app(pool),
        "/v1/sap-stage/catalog.product/changes?cursor=3",
        None,
    )
    .await;
    // The feed's parameter is `since`, not `cursor`. A silently ignored one would leave a consumer
    // convinced it had replicated from a position it never asked for.
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_query");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn the_three_collection_views_never_share_a_validator(pool: PgPool) {
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("SW1"), &json!({ "a": 1 }))
        .await
        .expect("stored");
    let app = app(pool);

    let (_, listing, _) = get(&app, "/v1/sap-stage/catalog.product", None).await;
    let (_, feed, _) = get(&app, "/v1/sap-stage/catalog.product/changes", None).await;
    let (_, manifest, _) = get(&app, "/v1/sap-stage/catalog.product/manifest", None).await;

    assert_ne!(listing, feed);
    assert_ne!(listing, manifest);
    assert_ne!(feed, manifest);
}

#[sqlx::test(migrations = "../store/migrations")]
async fn the_feed_route_shadows_a_record_of_the_same_name(pool: PgPool) {
    // Documents a consequence of the URL shape rather than a decision: `changes` and `manifest` are
    // static segments, so a record with one of those ids is not addressable. Recorded as an open
    // question rather than silently accepted.
    let store = PostgresStore::from_pool(pool.clone());
    store
        .upsert(&key("changes"), &json!({ "sku": "unreachable" }))
        .await
        .expect("stored");

    let (status, _, body) = get(&app(pool), "/v1/sap-stage/catalog.product/changes", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["changes"].is_array(),
        "the feed answers, not the record"
    );
    assert!(!body.to_string().contains("unreachable") || body["changes"][0]["id"] == "changes");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn an_undeclared_collection_is_absent_rather_than_empty(pool: PgPool) {
    // An empty page would tell a consumer it had replicated a collection that does not exist. The
    // schema set is what makes a collection real, so an undeclared name is a 404 everywhere.
    let app = app(pool);
    for uri in [
        "/v1/sap-stage/catalog.nothing",
        "/v1/sap-stage/catalog.nothing/SW1",
        "/v1/sap-stage/catalog.nothing/changes",
        "/v1/sap-stage/catalog.nothing/manifest",
    ] {
        let (status, _, body) = get(&app, uri, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        assert_eq!(body["error"]["code"], "unknown_collection", "{uri}");
    }
}

#[sqlx::test(migrations = "../store/migrations")]
async fn cache_directives_come_from_the_collection(pool: PgPool) {
    let app = app(pool);
    let cache_control = |uri: &'static str| {
        let app = app.clone();
        async move {
            let response = app
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            response
                .headers()
                .get(CACHE_CONTROL)
                .expect("cache-control")
                .to_str()
                .expect("ascii")
                .to_owned()
        }
    };

    assert_eq!(
        cache_control("/v1/sap-stage/catalog.product").await,
        "public, s-maxage=300"
    );
    assert_eq!(
        cache_control("/v1/sap-stage/catalog.category").await,
        "public, s-maxage=600"
    );
}
