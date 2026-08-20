//! Tests of the assembled service.
//!
//! Everything else in this workspace builds its own router with its own state, which leaves one
//! thing uncovered: whether the configured limits actually reach it. Twice during development an
//! edit to that wiring was silently dropped and every test still passed, because no test went
//! through `service`. These do.

// A fixture that cannot be built is a broken test. The libraries deny both lints.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use hydrant_core::schema::CollectionSchema;
use hydrant_core::{RecordKey, SchemaSet, SourceName};
use hydrant_server::config::Config;
use hydrant_server::service;
use hydrant_store::{PostgresStore, Store, Token};
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const SECRET: &str = "an application secret of sufficient length";
const TOKEN: &str = "hyd_0123456789abcdef0123456789abcdef";

fn schemas() -> SchemaSet {
    let product: CollectionSchema = serde_json::from_value(json!({
        "collection": "catalog.product",
        "id": "$.id",
        "fields": { "sku": { "type": "string", "index": true } },
        "filters": ["sku"]
    }))
    .expect("valid schema");
    SchemaSet::new([product]).expect("one collection")
}

fn source() -> SourceName {
    SourceName::new("sap-stage").expect("source")
}

/// A store with a usable credential in it.
async fn store_with_credential(pool: PgPool) -> PostgresStore {
    let store = PostgresStore::from_pool(pool);
    let hash = Token::from_presented(TOKEN)
        .hash(SECRET.as_bytes())
        .expect("hash");
    store
        .store_token(&hash, &source(), "wiring test")
        .await
        .expect("token stored");
    store
}

async fn body_of(response: axum::response::Response) -> Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json")
    }
}

/// A read request, with the peer address the rate limiter is keyed on.
fn read(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4000))))
        .body(Body::empty())
        .expect("request")
}

#[sqlx::test(migrations = "../store/migrations")]
async fn the_configured_payload_limit_reaches_the_ingest_surface(pool: PgPool) {
    let store = store_with_credential(pool).await;
    let config = Config {
        token_secret: SECRET.to_owned(),
        max_payload_bytes: 200,
        ..Config::default()
    };
    let service = service(store, schemas(), &config).expect("service");

    let request = Request::builder()
        .method(Method::POST)
        .uri("/v1/ingest/catalog.product")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!([{
                "op": "upsert",
                "id": "SW1",
                "payload": { "sku": "x".repeat(500) }
            }]))
            .expect("serialise"),
        ))
        .expect("request");

    let response = service.oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        body_of(response).await["error"]["code"],
        "payload_too_large"
    );
}

#[sqlx::test(migrations = "../store/migrations")]
async fn the_configured_response_budget_reaches_the_listing(pool: PgPool) {
    let store = store_with_credential(pool).await;
    for id in ["SW1", "SW2", "SW3"] {
        let key = RecordKey::new(
            source(),
            "catalog.product".parse().expect("collection"),
            id.parse().expect("id"),
        );
        store
            .upsert(&key, &json!({ "sku": "x".repeat(1000) }))
            .await
            .expect("stored");
    }

    let config = Config {
        token_secret: SECRET.to_owned(),
        max_response_bytes: 1500,
        ..Config::default()
    };
    let service = service(store, schemas(), &config).expect("service");

    let response = service
        .oneshot(read("/v1/sap-stage/catalog.product?limit=100"))
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let body = body_of(response).await;
    assert_eq!(
        body["records"].as_array().expect("records").len(),
        1,
        "the budget cut the page short"
    );
    assert!(
        body["next_cursor"].as_u64().is_some(),
        "and offered a cursor"
    );
}

#[sqlx::test(migrations = "../store/migrations")]
async fn the_configured_rate_limit_reaches_the_read_routes(pool: PgPool) {
    let store = store_with_credential(pool).await;
    let config = Config {
        token_secret: SECRET.to_owned(),
        read_per_second: 1,
        read_burst: 1,
        ..Config::default()
    };
    let service = service(store, schemas(), &config).expect("service");

    let first = service
        .clone()
        .oneshot(read("/v1/sap-stage/catalog.product"))
        .await
        .expect("response");
    assert_eq!(first.status(), StatusCode::OK);

    let second = service
        .oneshot(read("/v1/sap-stage/catalog.product"))
        .await
        .expect("response");
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
}
