//! Rate limits, exercised through the router.
//!
//! Every case here is about what happens to the *next* request, so the tests are written as
//! sequences rather than single calls.

// A fixture that cannot be built is a broken test. The library denies both lints.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::net::{IpAddr, SocketAddr};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::header::RETRY_AFTER;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use hydrant_api::{ApiState, RateLimits, router};
use hydrant_core::SchemaSet;
use hydrant_core::schema::CollectionSchema;
use hydrant_store::PostgresStore;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

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

fn app(pool: PgPool, limits: RateLimits) -> Router {
    router(
        ApiState::new(PostgresStore::from_pool(pool), schemas()),
        limits,
    )
    .expect("usable limits")
}

/// One request from `client`, returning the status and the `Retry-After` header if there was one.
async fn get(app: &Router, uri: &str, client: [u8; 4]) -> (StatusCode, Option<String>, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .extension(ConnectInfo(SocketAddr::from((IpAddr::from(client), 4000))))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    let status = response.status();
    let retry_after = response
        .headers()
        .get(RETRY_AFTER)
        .map(|value| value.to_str().expect("ascii").to_owned());
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json")
    };
    (status, retry_after, body)
}

const CLIENT: [u8; 4] = [10, 0, 0, 1];

#[sqlx::test(migrations = "../store/migrations")]
async fn a_client_past_its_burst_is_refused_in_the_usual_envelope(pool: PgPool) {
    let limits = RateLimits {
        read_per_second: 1,
        read_burst: 2,
        feed_per_second: 1,
        feed_burst: 1,
        trust_forwarded_for: false,
    };
    let app = app(pool, limits);

    for attempt in 1..=2 {
        let (status, _, _) = get(&app, "/v1/sap-stage/catalog.product", CLIENT).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "request {attempt} is within the burst"
        );
    }

    let (status, retry_after, body) = get(&app, "/v1/sap-stage/catalog.product", CLIENT).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        body["error"]["code"], "rate_limited",
        "the same envelope as every other error"
    );
    assert!(retry_after.is_some(), "a 429 has to say when to come back");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn the_change_feed_has_its_own_budget(pool: PgPool) {
    // A feed page can be a thousand records and cannot be answered from a validator alone, so it
    // gets a lower budget than an item read - and exhausting it must not close the read path.
    let limits = RateLimits {
        read_per_second: 100,
        read_burst: 100,
        feed_per_second: 1,
        feed_burst: 1,
        trust_forwarded_for: false,
    };
    let app = app(pool, limits);

    let (first, _, _) = get(&app, "/v1/sap-stage/catalog.product/changes", CLIENT).await;
    assert_eq!(first, StatusCode::OK);
    let (second, _, _) = get(&app, "/v1/sap-stage/catalog.product/changes", CLIENT).await;
    assert_eq!(second, StatusCode::TOO_MANY_REQUESTS);

    let (listing, _, _) = get(&app, "/v1/sap-stage/catalog.product", CLIENT).await;
    assert_eq!(listing, StatusCode::OK, "the read budget is separate");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn one_client_cannot_exhaust_anothers_budget(pool: PgPool) {
    let limits = RateLimits {
        read_per_second: 1,
        read_burst: 1,
        feed_per_second: 1,
        feed_burst: 1,
        trust_forwarded_for: false,
    };
    let app = app(pool, limits);

    let (first, _, _) = get(&app, "/v1/sap-stage/catalog.product", [10, 0, 0, 1]).await;
    assert_eq!(first, StatusCode::OK);
    let (again, _, _) = get(&app, "/v1/sap-stage/catalog.product", [10, 0, 0, 1]).await;
    assert_eq!(again, StatusCode::TOO_MANY_REQUESTS);

    let (other, _, _) = get(&app, "/v1/sap-stage/catalog.product", [10, 0, 0, 2]).await;
    assert_eq!(other, StatusCode::OK, "the limit is per client, not global");
}

#[sqlx::test(migrations = "../store/migrations")]
async fn liveness_is_never_refused(pool: PgPool) {
    // An orchestrator probes this constantly. Throttling it is how a healthy service gets restarted.
    let limits = RateLimits {
        read_per_second: 1,
        read_burst: 1,
        feed_per_second: 1,
        feed_burst: 1,
        trust_forwarded_for: false,
    };
    let app = app(pool, limits);

    for attempt in 1..=10 {
        let (status, _, _) = get(&app, "/health", CLIENT).await;
        assert_eq!(status, StatusCode::OK, "probe {attempt}");
    }
}

#[sqlx::test(migrations = "../store/migrations")]
async fn a_limit_that_would_refuse_everything_is_refused_at_startup(pool: PgPool) {
    let limits = RateLimits {
        read_burst: 0,
        ..RateLimits::default()
    };
    let error = router(
        ApiState::new(PostgresStore::from_pool(pool), schemas()),
        limits,
    )
    .expect_err("a zero burst is not a rate limit");
    assert!(error.reason.contains("read"), "{error}");
}
