//! How a failed request looks from the outside.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hydrant_store::StoreError;
use serde::Serialize;

/// Everything a public request can fail with.
///
/// The variants are deliberately few. A public read API has no vocabulary for "not authorised" or
/// "conflict": either the address is malformed, or the record is not there, or the service is
/// broken.
#[derive(Debug)]
pub enum ApiError {
    /// A path segment or query parameter was not usable.
    BadRequest {
        /// Machine-readable reason, stable enough for a client to branch on.
        code: &'static str,
        /// What was wrong, in words. Safe to show: it describes the request, never the store.
        message: String,
    },
    /// No usable ingest credential was presented.
    ///
    /// One variant for "no header", "malformed header" and "unknown token" alike: telling a caller
    /// which of the three it was tells it how to get closer.
    Unauthorized,
    /// No such collection: the name is not declared in any schema.
    ///
    /// Distinct from a missing record on purpose. "This collection does not exist" and "this record
    /// does not exist" send a sender looking in different places.
    UnknownCollection,
    /// No such record, or it has been deleted.
    NotFound,
    /// The store failed. The client learns nothing about why.
    Internal(StoreError),
}

impl ApiError {
    /// A malformed path segment.
    pub fn bad_path(what: &str, reason: &impl std::fmt::Display) -> Self {
        Self::BadRequest {
            code: "invalid_path",
            message: format!("{what}: {reason}"),
        }
    }
}

impl From<StoreError> for ApiError {
    fn from(error: StoreError) -> Self {
        Self::Internal(error)
    }
}

/// The error envelope. One shape for every failure, so a client parses one thing.
#[derive(Debug, Serialize)]
struct ErrorBody {
    error: ErrorDetail,
}

#[derive(Debug, Serialize)]
struct ErrorDetail {
    code: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if matches!(self, Self::Unauthorized) {
            return (
                StatusCode::UNAUTHORIZED,
                [crate::ingest::challenge()],
                Json(ErrorBody {
                    error: ErrorDetail {
                        code: "unauthorized",
                        message: "a valid ingest credential is required".to_owned(),
                    },
                }),
            )
                .into_response();
        }

        let (status, code, message) = match self {
            Self::Unauthorized => unreachable!("handled above"),
            Self::BadRequest { code, message } => (StatusCode::BAD_REQUEST, code, message),
            Self::UnknownCollection => (
                StatusCode::NOT_FOUND,
                "unknown_collection",
                "no collection of that name is served".to_owned(),
            ),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "no such record in this collection".to_owned(),
            ),
            Self::Internal(error) => {
                // The detail belongs in the log, not in the response: it describes the store.
                tracing::error!(error = ?error, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "the service could not answer this request".to_owned(),
                )
            }
        };
        (
            status,
            Json(ErrorBody {
                error: ErrorDetail { code, message },
            }),
        )
            .into_response()
    }
}
