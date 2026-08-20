//! Rate limits for the public read surface.
//!
//! Public means hostile traffic, and every other operational limit in this service bounds one
//! request: the page cap bounds how much a page returns, the statement timeout bounds how long a
//! query may run. Neither bounds how many requests arrive. That is what these do.
//!
//! Two limits rather than one, because two costs are not alike. A cached item read is cheap; a page
//! of the change feed can be a thousand records and cannot be served from a cache validator alone.
//! The feed therefore gets its own, lower budget.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use hydrant_store::Store;
use serde::Serialize;
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::KeyExtractor;

use crate::public;
use crate::state::ApiState;

/// How many requests a client may make, and how fast the budget refills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimits {
    /// Sustained rate for ordinary reads, per second per client.
    pub read_per_second: u32,
    /// How many read requests may arrive at once before the sustained rate applies.
    pub read_burst: u32,
    /// Sustained rate for the change feed, per second per client.
    pub feed_per_second: u32,
    /// How many feed requests may arrive at once.
    pub feed_burst: u32,
    /// Whether to key on `X-Forwarded-For` rather than the peer address.
    ///
    /// Off by default, and that default is the safe one: a forwarded header is whatever the client
    /// says it is unless something in front is known to overwrite it. Turn it on when the service
    /// only ever receives traffic through a CDN or an ingress that does — and only then, because
    /// otherwise every client can hand itself a fresh bucket per request.
    pub trust_forwarded_for: bool,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            read_per_second: 20,
            read_burst: 60,
            feed_per_second: 2,
            feed_burst: 10,
            trust_forwarded_for: false,
        }
    }
}

/// A rate limit that cannot be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("rate limit is not usable: {reason}")]
pub struct LimitError {
    /// What was wrong with it.
    pub reason: String,
}

impl RateLimits {
    /// The refill interval the governor takes, in milliseconds.
    ///
    /// Clamped to at least one millisecond: a rate faster than the resolution would otherwise round
    /// to zero, which is a limit that refuses everything.
    fn interval(per_second: u32) -> u64 {
        (1000 / u64::from(per_second.max(1))).max(1)
    }

    /// Checks that neither limit would refuse every request.
    ///
    /// # Errors
    ///
    /// Returns [`LimitError`] if a rate or burst is zero. That is a configuration mistake worth
    /// failing the boot over rather than serving.
    pub fn validate(&self) -> Result<(), LimitError> {
        for (what, per_second, burst) in [
            ("read", self.read_per_second, self.read_burst),
            ("feed", self.feed_per_second, self.feed_burst),
        ] {
            if per_second == 0 || burst == 0 {
                return Err(LimitError {
                    reason: format!("the {what} limit is {per_second}/s with a burst of {burst}"),
                });
            }
        }
        Ok(())
    }
}

/// Builds the public router with its two limiters.
///
/// The configurations are locals rather than struct fields on purpose: naming the governor's
/// middleware type would mean depending on the `governor` crate directly just to spell it, and
/// inference already knows it. Each route gets its own layer over a shared configuration, so routes
/// are layered separately while a client's budget is not.
///
/// # Errors
///
/// Returns [`LimitError`] if a limit is unusable.
pub(crate) fn public_router<S, K>(
    state: ApiState<S>,
    limits: RateLimits,
    key_extractor: K,
) -> Result<Router, LimitError>
where
    S: Store + 'static,
    K: KeyExtractor + Send + Sync + 'static,
    K::Key: Send + Sync + 'static,
{
    limits.validate()?;

    let read = Arc::new(
        GovernorConfigBuilder::default()
            .per_millisecond(RateLimits::interval(limits.read_per_second))
            .burst_size(limits.read_burst)
            .key_extractor(key_extractor.clone())
            .finish()
            .ok_or_else(|| LimitError {
                reason: "the read limit could not be built".to_owned(),
            })?,
    );
    let feed = Arc::new(
        GovernorConfigBuilder::default()
            .per_millisecond(RateLimits::interval(limits.feed_per_second))
            .burst_size(limits.feed_burst)
            .key_extractor(key_extractor)
            .finish()
            .ok_or_else(|| LimitError {
                reason: "the feed limit could not be built".to_owned(),
            })?,
    );

    Ok(Router::new()
        // Liveness is not rate limited. An orchestrator probes it constantly, and throttling that
        // is how a healthy service gets restarted.
        .route("/health", get(public::health))
        .route(
            "/v1/{source}/{collection}",
            get(public::list_collection::<S>)
                .layer(GovernorLayer::new(Arc::clone(&read)).error_handler(refusal)),
        )
        .route(
            "/v1/{source}/{collection}/changes",
            get(public::changes::<S>)
                .layer(GovernorLayer::new(Arc::clone(&feed)).error_handler(refusal)),
        )
        .route(
            "/v1/{source}/{collection}/manifest",
            get(public::manifest::<S>)
                .layer(GovernorLayer::new(Arc::clone(&read)).error_handler(refusal)),
        )
        .route(
            "/v1/{source}/{collection}/{id}",
            get(public::get_record::<S>).layer(GovernorLayer::new(read).error_handler(refusal)),
        )
        .with_state(state))
}

/// Renders a governor error as this service's error shape.
///
/// A 429 with a plain-text body while everything else answers JSON is the kind of inconsistency a
/// client has to special-case, so it says the same thing in the same shape - and carries
/// `Retry-After`, which is the only part a client can act on.
fn refusal(error: tower_governor::GovernorError) -> Response<Body> {
    let (status, code, message, headers) = match error {
        tower_governor::GovernorError::TooManyRequests { wait_time, headers } => {
            // The governor reports whole seconds, so a sub-second wait arrives as zero.
            // `Retry-After: 0` is worse than no hint at all: a client obeys it, comes back
            // immediately, and is refused again.
            let wait_time = wait_time.max(1);
            (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                format!("too many requests; retry in {wait_time}s"),
                Some((headers.unwrap_or_default(), wait_time)),
            )
        }
        // Missing peer address: the service is misconfigured rather than the client misbehaving, and
        // failing closed is the only safe reading of "cannot tell who this is".
        tower_governor::GovernorError::UnableToExtractKey => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "the service could not answer this request".to_owned(),
            None,
        ),
        tower_governor::GovernorError::Other { code, msg, headers } => (
            code,
            "rate_limited",
            msg.unwrap_or_else(|| "request refused".to_owned()),
            headers.map(|headers| (headers, 0)),
        ),
    };

    let body = Json(ErrorBody {
        error: ErrorDetail { code, message },
    });
    let mut response = (status, body).into_response();
    if let Some((extra, wait_time)) = headers {
        merge(response.headers_mut(), &extra, wait_time);
    }
    response
}

fn merge(target: &mut HeaderMap, extra: &HeaderMap, wait_time: u64) {
    for (name, value) in extra {
        target.insert(name, value.clone());
    }
    if let Ok(value) = wait_time.to_string().parse() {
        target.insert(axum::http::header::RETRY_AFTER, value);
    }
}

/// Mirrors the envelope in [`crate::error`]. Kept local because the governor hands back a raw
/// response rather than going through `IntoResponse` on our error type.
#[derive(Debug, Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_never_says_to_retry_immediately() {
        let response = refusal(tower_governor::GovernorError::TooManyRequests {
            wait_time: 0,
            headers: None,
        });
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("1"),
            "a client that obeys Retry-After: 0 is refused again immediately"
        );
    }

    #[test]
    fn the_defaults_are_usable() {
        assert!(RateLimits::default().validate().is_ok());
    }

    #[test]
    fn a_zero_limit_is_refused_rather_than_serving_nothing() {
        let limits = RateLimits {
            read_per_second: 0,
            ..RateLimits::default()
        };
        let error = limits.validate().expect_err("zero rate");
        assert!(error.reason.contains("read"), "{error}");

        let limits = RateLimits {
            feed_burst: 0,
            ..RateLimits::default()
        };
        let error = limits.validate().expect_err("zero burst");
        assert!(error.reason.contains("feed"), "{error}");
    }

    #[test]
    fn a_rate_faster_than_the_resolution_clamps_rather_than_vanishing() {
        // 2000/s is a shorter interval than a millisecond. Rounding down would produce zero, which
        // the governor reads as a limit that refuses everything.
        assert_eq!(RateLimits::interval(2000), 1);
        assert_eq!(RateLimits::interval(20), 50);
        assert_eq!(RateLimits::interval(0), 1000);
    }
}
