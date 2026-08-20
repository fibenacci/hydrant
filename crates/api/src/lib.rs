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
pub mod limits;
pub mod metrics;
pub mod public;
pub mod query;
pub mod response;
pub mod state;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::{delete, get, post};
use hydrant_store::Store;
use tower_governor::key_extractor::{PeerIpKeyExtractor, SmartIpKeyExtractor};

pub use error::ApiError;
pub use ingest::IngestState;
pub use limits::{LimitError, RateLimits};
pub use response::{ChangeBody, ChangesBody, ManifestBody, PageBody, RecordBody};
pub use state::ApiState;

/// The public read router, with its rate limits.
///
/// `/health` is liveness for an orchestrator, not part of the data surface, and is not rate limited.
///
/// `changes` and `manifest` are static segments under a collection, so they take precedence over the
/// item route: a record whose id is literally `changes` or `manifest` is not addressable. That is a
/// consequence of the URL shape rather than a decision, and it is recorded as an open question.
///
/// # Errors
///
/// Returns [`limits::LimitError`] if a configured limit would refuse every request.
pub fn router<S>(state: ApiState<S>, limits: RateLimits) -> Result<Router, LimitError>
where
    S: Store + 'static,
{
    // The key is what a limit is per. Trusting a forwarded header when nothing overwrites it hands
    // every client a fresh bucket per request, so the peer address is the default.
    if limits.trust_forwarded_for {
        limits::public_router(state, limits, SmartIpKeyExtractor)
    } else {
        limits::public_router(state, limits, PeerIpKeyExtractor)
    }
}

/// The authenticated ingest router.
///
/// Kept separate from the public router, with its own state: the application secret lives here and
/// nowhere else, so no read-path handler can reach credential material even by accident.
pub fn ingest_router<S>(state: IngestState<S>) -> Router
where
    S: Store + 'static,
{
    // The body limit belongs on this router only: a read request has no body worth bounding, and a
    // global limit would be a number nobody could explain.
    let max_body_bytes = state.max_body_bytes();
    Router::new()
        .route("/v1/ingest/{collection}", post(ingest::ingest::<S>))
        .route("/v1/ingest/{collection}/digests", get(ingest::digests::<S>))
        .route(
            "/v1/ingest/{collection}/{id}",
            delete(ingest::delete_record::<S>),
        )
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(state)
}
