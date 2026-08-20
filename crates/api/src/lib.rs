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
pub mod public;
pub mod response;
pub mod state;

use axum::Router;
use axum::routing::get;
use hydrant_store::Store;

pub use error::ApiError;
pub use response::{PageBody, RecordBody};
pub use state::ApiState;

/// The public read router.
///
/// `/health` is liveness for an orchestrator, not part of the data surface.
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
            "/v1/{source}/{collection}/{id}",
            get(public::get_record::<S>),
        )
        .with_state(state)
}
