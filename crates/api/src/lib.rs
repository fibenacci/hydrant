//! The HTTP surface of hydrant. This crate carries the public read API.
//!
//! Everything here is read-only and unauthenticated, which is a storage decision rather than a
//! routing one: what reaches a response was allow-listed at ingest, so there is no filtering left
//! to do here and no bug in this crate can leak a field the store never took.
//!
//! Two properties matter more than the routes themselves:
//!
//! - **Cacheability.** A CDN in front is the scaling plan, so every response carries a validator
//!   and `Cache-Control`. An item's validator is its content hash; a listing's is the collection's
//!   highest feed position together with the page parameters.
//! - **No silent defaults.** An unknown query parameter is a 400. A parameter that looks like it
//!   worked is worse than one that was refused.

pub mod cache;
pub mod error;
pub mod ingest;
pub mod metrics;
pub mod public;
pub mod query;
pub mod response;
pub mod state;

use axum::Router;
use axum::routing::{delete, get, post};
use hydrant_store::Store;

pub use error::ApiError;
pub use ingest::IngestState;
pub use response::{ChangeBody, ChangesBody, ManifestBody, PageBody, RecordBody};
pub use state::ApiState;

/// The public read router.
///
/// `/health` is liveness for an orchestrator, not part of the data surface.
///
/// `changes` and `manifest` are static segments under a collection, so they take precedence over the
/// item route: a record whose id is literally `changes` or `manifest` is not addressable. That is a
/// consequence of the URL shape rather than a decision, and it is recorded as an open question.
pub fn router<S>(state: ApiState<S>) -> Router
where
    S: Store + 'static,
{
    Router::new()
        .route("/health", get(public::health))
        .route(
            "/v1/{source}/{collection}",
            get(public::list_collection::<S>),
        )
        .route(
            "/v1/{source}/{collection}/changes",
            get(public::changes::<S>),
        )
        .route(
            "/v1/{source}/{collection}/manifest",
            get(public::manifest::<S>),
        )
        .route(
            "/v1/{source}/{collection}/{id}",
            get(public::get_record::<S>),
        )
        .with_state(state)
}

/// The authenticated ingest router.
///
/// Kept separate from the public router, with its own state: the application secret lives here and
/// nowhere else, so no read-path handler can reach credential material even by accident.
pub fn ingest_router<S>(state: IngestState<S>) -> Router
where
    S: Store + 'static,
{
    Router::new()
        .route("/v1/ingest/{collection}", post(ingest::ingest::<S>))
        .route("/v1/ingest/{collection}/digests", get(ingest::digests::<S>))
        .route(
            "/v1/ingest/{collection}/{id}",
            delete(ingest::delete_record::<S>),
        )
        .with_state(state)
}
