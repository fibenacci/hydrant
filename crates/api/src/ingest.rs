//! The ingest surface: authenticated, write-only, and the place projection happens.
//!
//! Everything a sender pushes passes the collection's allow-list here, before persistence. A field
//! the schema does not name is dropped and reported back, so the sender learns what was not
//! released rather than discovering it missing later. This is the asymmetry the whole design rests
//! on: filtering on write makes a bug a missing field, filtering on read would make it a leak.
//!
//! The state of this router carries the application secret, and the public router's does not. That
//! is a boundary rather than a convention: nothing on the read side can reach the credential
//! material even by mistake.

use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use hydrant_core::schema::CollectionSchema;
use hydrant_core::{
    CollectionName, DropReason, RecordId, RecordKey, SchemaSet, SourceName, project,
};
use hydrant_store::token::Token;
use hydrant_store::{Applied, Deletion, IngestOp, PageLimit, Store};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ApiError;

/// The largest batch one request may carry.
///
/// A cap is not politeness: a batch is applied in one transaction, and an unbounded one holds a
/// connection and a write lock for as long as the sender feels like.
pub const MAX_BATCH: usize = 1000;

/// State behind the ingest routes.
#[derive(Debug)]
pub struct IngestState<S> {
    /// The store to write to.
    pub store: Arc<S>,
    /// Every collection the service serves. A collection that is not declared cannot be written to.
    pub schemas: Arc<SchemaSet>,
    /// The application secret ingest tokens are hashed with. Never leaves this crate.
    secret: Arc<Vec<u8>>,
}

impl<S> IngestState<S> {
    /// Wraps a store, the collections it serves, and the secret credentials are keyed with.
    #[must_use]
    pub fn new(store: S, schemas: SchemaSet, secret: impl Into<Vec<u8>>) -> Self {
        Self {
            store: Arc::new(store),
            schemas: Arc::new(schemas),
            secret: Arc::new(secret.into()),
        }
    }
}

impl<S> Clone for IngestState<S> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            schemas: Arc::clone(&self.schemas),
            secret: Arc::clone(&self.secret),
        }
    }
}

/// The source a request is allowed to write to.
///
/// Produced by the extractor below, so a handler cannot be written that forgets to authenticate:
/// there is no way to name the source without having resolved a credential first.
#[derive(Debug, Clone)]
pub struct Authenticated(pub SourceName);

impl<S> FromRequestParts<IngestState<S>> for Authenticated
where
    S: Store + 'static,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &IngestState<S>,
    ) -> Result<Self, Self::Rejection> {
        let presented = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(ApiError::Unauthorized)?;

        // The token is hashed, never compared: the lookup is a primary-key hit on a MAC.
        let hash = Token::from_presented(presented)
            .hash(&state.secret)
            .map_err(|_| ApiError::Unauthorized)?;

        state
            .store
            .authenticate(&hash)
            .await?
            .map(Self)
            .ok_or(ApiError::Unauthorized)
    }
}

/// One operation as a sender writes it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// What to do with the record.
    op: Operation,
    /// The record's identifier. Optional for an upsert, where the schema's id path can supply it.
    #[serde(default)]
    id: Option<RecordId>,
    /// The payload, before projection. Required for an upsert, refused for a delete.
    #[serde(default)]
    payload: Option<Value>,
}

/// The two things a sender can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    /// Write the record if its projected payload differs from what is stored.
    Upsert,
    /// Turn the record into a tombstone.
    Delete,
}

/// What happened to one operation.
#[derive(Debug, Serialize)]
pub struct OperationResult {
    /// The record the operation addressed.
    pub id: String,
    /// What was asked for.
    pub op: Operation,
    /// `stored`, `tombstoned` or `unchanged`.
    pub outcome: &'static str,
    /// The new feed position, absent when nothing was written.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Fields the collection's schema did not release. Empty is the normal case.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<DroppedBody>,
}

/// One field that did not survive projection.
#[derive(Debug, Serialize)]
pub struct DroppedBody {
    /// Dotted path of the key, with array indices.
    pub path: String,
    /// `unknown_key` or `type_mismatch`.
    pub reason: &'static str,
    /// What the schema declared, for a type mismatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<&'static str>,
    /// What arrived instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub found: Option<&'static str>,
}

impl From<hydrant_core::DroppedField> for DroppedBody {
    fn from(dropped: hydrant_core::DroppedField) -> Self {
        match dropped.reason {
            DropReason::UnknownKey => Self {
                path: dropped.path,
                reason: "unknown_key",
                expected: None,
                found: None,
            },
            DropReason::TypeMismatch { expected, found } => Self {
                path: dropped.path,
                reason: "type_mismatch",
                expected: Some(expected),
                found: Some(found),
            },
        }
    }
}

/// The response to a batch.
#[derive(Debug, Serialize)]
pub struct BatchResult {
    /// One result per operation, in the order they were sent.
    pub results: Vec<OperationResult>,
}

/// One page of per-record digests.
#[derive(Debug, Serialize)]
pub struct DigestsBody {
    /// The digests, in id order.
    pub digests: Vec<DigestBody>,
    /// The id to pass back as `?after=` to continue, or `null` when the walk is complete.
    pub next_cursor: Option<String>,
}

/// One record's identity and content hash.
#[derive(Debug, Serialize)]
pub struct DigestBody {
    /// The record's identifier.
    pub id: String,
    /// SHA-256 over its payload's canonical form, hex.
    pub content_hash: String,
}

/// Query parameters of the digest listing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DigestsQuery {
    /// Id already seen. The walk continues after it.
    after: Option<String>,
    /// Page size, clamped like every other page.
    limit: Option<u16>,
}

/// `POST /v1/ingest/{collection}`
///
/// Applies a batch in one transaction. Every payload is projected first, so what is written is what
/// the schema releases and nothing else, and what was dropped comes back in the response.
///
/// # Errors
///
/// Returns 401 without a usable credential, 404 for a collection no schema declares, 400 for a
/// malformed body, an oversized batch, an operation missing what it needs, or a payload with no
/// usable identifier, and 500 if the store cannot answer.
pub async fn ingest<S: Store>(
    State(state): State<IngestState<S>>,
    Authenticated(source): Authenticated,
    Path(collection): Path<String>,
    body: Result<Json<Vec<Envelope>>, JsonRejection>,
) -> Result<Response, ApiError> {
    let collection = collection
        .parse::<CollectionName>()
        .map_err(|error| ApiError::bad_path("collection", &error))?;
    let schema = state
        .schemas
        .get(&collection)
        .ok_or(ApiError::UnknownCollection)?;

    let Json(envelopes) = body.map_err(|rejection| ApiError::BadRequest {
        code: "invalid_body",
        message: rejection.body_text(),
    })?;
    if envelopes.len() > MAX_BATCH {
        return Err(ApiError::BadRequest {
            code: "batch_too_large",
            message: format!("a batch may carry at most {MAX_BATCH} operations"),
        });
    }

    let mut ops = Vec::with_capacity(envelopes.len());
    let mut dropped_per_op = Vec::with_capacity(envelopes.len());
    for envelope in envelopes {
        let (op, dropped) = prepare(schema, envelope)?;
        ops.push(op);
        dropped_per_op.push(dropped);
    }

    let outcomes = state.store.apply(&source, &collection, &ops).await?;
    crate::metrics::record_outcomes(&collection, &outcomes);
    let results = ops
        .iter()
        .zip(outcomes)
        .zip(dropped_per_op)
        .map(|((op, outcome), dropped)| result_of(op, outcome, dropped))
        .collect();

    Ok(Json(BatchResult { results }).into_response())
}

/// `DELETE /v1/ingest/{collection}/{id}`
///
/// # Errors
///
/// Returns 401 without a usable credential, 404 for an undeclared collection, 400 for a malformed
/// address, and 500 if the store cannot answer.
pub async fn delete_record<S: Store>(
    State(state): State<IngestState<S>>,
    Authenticated(source): Authenticated,
    Path((collection, id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let collection = collection
        .parse::<CollectionName>()
        .map_err(|error| ApiError::bad_path("collection", &error))?;
    if !state.schemas.contains(&collection) {
        return Err(ApiError::UnknownCollection);
    }
    let id = id
        .parse::<RecordId>()
        .map_err(|error| ApiError::bad_path("record id", &error))?;

    let key = RecordKey::new(source, collection, id);
    let outcome = state.store.delete(&key).await?;
    let (outcome, seq) = match outcome {
        Deletion::Tombstoned { seq } => ("tombstoned", Some(seq.get())),
        Deletion::Unchanged => ("unchanged", None),
    };

    Ok(Json(OperationResult {
        id: key.id.to_string(),
        op: Operation::Delete,
        outcome,
        seq,
        dropped: Vec::new(),
    })
    .into_response())
}

/// `GET /v1/ingest/{collection}/digests`
///
/// Per-record hashes, walked by id. A collection checksum can only say *that* something drifted;
/// these say which record, which is the difference between re-pushing one and re-pushing everything.
///
/// # Errors
///
/// Returns 401 without a usable credential, 404 for an undeclared collection, 400 for a malformed
/// address or an undeclared parameter, and 500 if the store cannot answer.
pub async fn digests<S: Store>(
    State(state): State<IngestState<S>>,
    Authenticated(source): Authenticated,
    Path(collection): Path<String>,
    query: Result<Query<DigestsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let collection = collection
        .parse::<CollectionName>()
        .map_err(|error| ApiError::bad_path("collection", &error))?;
    if !state.schemas.contains(&collection) {
        return Err(ApiError::UnknownCollection);
    }
    let Query(query) = query.map_err(|rejection| ApiError::BadRequest {
        code: "invalid_query",
        message: rejection.body_text(),
    })?;

    let after = query
        .after
        .as_deref()
        .map(str::parse::<RecordId>)
        .transpose()
        .map_err(|error| ApiError::bad_path("after", &error))?;
    let limit = query
        .limit
        .map_or_else(PageLimit::default, PageLimit::clamp);

    let page = state
        .store
        .digests(&source, &collection, after.as_ref(), limit)
        .await?;
    Ok(Json(DigestsBody {
        digests: page
            .entries
            .into_iter()
            .map(|entry| DigestBody {
                id: entry.id.to_string(),
                content_hash: entry.content_hash.to_hex(),
            })
            .collect(),
        next_cursor: page.next.map(|id| id.to_string()),
    })
    .into_response())
}

/// Turns one envelope into a store operation, projecting an upsert's payload on the way.
fn prepare(
    schema: &CollectionSchema,
    envelope: Envelope,
) -> Result<(IngestOp, Vec<DroppedBody>), ApiError> {
    match envelope.op {
        Operation::Upsert => {
            let payload = envelope.payload.ok_or_else(|| ApiError::BadRequest {
                code: "missing_payload",
                message: "an upsert needs a payload".to_owned(),
            })?;

            // The id may be stated in the envelope or lifted out of the payload by the schema's id
            // path. Stating it wins: a sender that knows its own key should not have to embed it.
            let id = match envelope.id {
                Some(id) => id,
                None => schema
                    .id()
                    .extract(&payload)
                    .map_err(|error| ApiError::BadRequest {
                        code: "missing_id",
                        message: error.to_string(),
                    })?,
            };

            let projected = project(schema, &payload).map_err(|error| ApiError::BadRequest {
                code: "invalid_payload",
                message: error.to_string(),
            })?;
            if !projected.dropped.is_empty() {
                // Both, on purpose: the counter is what a dashboard notices, the log line is what
                // names the record once someone is looking.
                crate::metrics::record_dropped(schema.collection(), &projected.dropped);
                tracing::info!(
                    collection = %schema.collection(),
                    record = %id,
                    dropped = projected.dropped.len(),
                    "fields dropped at ingest"
                );
            }

            let dropped = projected
                .dropped
                .into_iter()
                .map(DroppedBody::from)
                .collect();
            Ok((
                IngestOp::Upsert {
                    id,
                    payload: projected.payload,
                },
                dropped,
            ))
        }
        Operation::Delete => {
            if envelope.payload.is_some() {
                return Err(ApiError::BadRequest {
                    code: "payload_on_delete",
                    message: "a delete carries no payload".to_owned(),
                });
            }
            let id = envelope.id.ok_or_else(|| ApiError::BadRequest {
                code: "missing_id",
                message: "a delete needs an explicit id".to_owned(),
            })?;
            Ok((IngestOp::Delete { id }, Vec::new()))
        }
    }
}

/// Renders one outcome.
fn result_of(op: &IngestOp, outcome: Applied, dropped: Vec<DroppedBody>) -> OperationResult {
    let (label, seq) = match outcome {
        Applied::Stored { seq } => ("stored", Some(seq.get())),
        Applied::Tombstoned { seq } => ("tombstoned", Some(seq.get())),
        Applied::Unchanged => ("unchanged", None),
    };
    OperationResult {
        id: op.id().to_string(),
        op: match op {
            IngestOp::Upsert { .. } => Operation::Upsert,
            IngestOp::Delete { .. } => Operation::Delete,
        },
        outcome: label,
        seq,
        dropped,
    }
}

/// The `WWW-Authenticate` challenge a 401 carries.
pub(crate) fn challenge() -> (axum::http::HeaderName, axum::http::HeaderValue) {
    (
        WWW_AUTHENTICATE,
        axum::http::HeaderValue::from_static("Bearer"),
    )
}
