//! The public read routes.
//!
//! Unauthenticated by design: everything in the store is public, and a record that needs an
//! audience check does not belong in it. What the handlers do owe the caller is precision — a
//! malformed address is a 400, an unknown parameter is a 400 rather than a silent default, and a
//! deleted record is a 404 rather than its last known state.

use std::fmt::Display;
use std::str::FromStr;

use axum::Json;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::header::{CACHE_CONTROL, ETAG};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use hydrant_core::{CollectionName, RecordId, RecordKey, Seq, SourceName};
use hydrant_store::{PageLimit, Store};
use serde::{Deserialize, Serialize};

use crate::cache::is_fresh;
use crate::error::ApiError;
use crate::response::{PageBody, RecordBody};
use crate::state::ApiState;

/// Query parameters of a collection listing.
///
/// `deny_unknown_fields` is the point: an undeclared parameter is a 400, never something quietly
/// ignored. A client that misspells `cursor` has to find out, and a filter that is not declared in
/// the collection schema must not look like it worked.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    /// Page size. Clamped server-side to [`PageLimit::MAX`], whatever is asked for.
    limit: Option<u16>,
    /// Feed position to continue after, from a previous page's `next_cursor`.
    cursor: Option<u64>,
}

/// Liveness only: it says the process is up and serving, not that the database is reachable.
/// Anything more belongs behind an operator endpoint, not on the public surface.
#[derive(Debug, Serialize)]
struct Health {
    status: &'static str,
}

/// `GET /health`
pub async fn health() -> Response {
    Json(Health { status: "ok" }).into_response()
}

/// `GET /v1/{source}/{collection}`
///
/// One page of a collection in feed order, tombstones excluded. The validator comes from the
/// collection's highest feed position together with the page parameters, because those decide the
/// representation just as much.
///
/// # Errors
///
/// Returns 400 for a malformed source or collection name and for any query parameter the listing
/// does not declare, and 500 if the store cannot answer.
pub async fn list_collection<S: Store>(
    State(state): State<ApiState<S>>,
    Path((source, collection)): Path<(String, String)>,
    query: Result<Query<ListQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let source = parse::<SourceName>(&source, "source")?;
    let collection = parse::<CollectionName>(&collection, "collection")?;
    let Query(query) = query.map_err(|rejection| ApiError::BadRequest {
        code: "invalid_query",
        message: rejection.body_text(),
    })?;

    let limit = query
        .limit
        .map_or_else(PageLimit::default, PageLimit::clamp);
    let cursor = query.cursor.map(Seq::new);

    let max_seq = state.store.max_seq(&source, &collection).await?;
    let etag = listing_etag(max_seq, cursor, limit);
    if is_fresh(&headers, &etag) {
        return Ok(not_modified(&state, &etag));
    }

    let page = state
        .store
        .list(&source, &collection, cursor, limit)
        .await?;
    let body = PageBody::new(page, max_seq.map_or(0, Seq::get));
    Ok((cache_headers(&state, &etag), Json(body)).into_response())
}

/// `GET /v1/{source}/{collection}/{id}`
///
/// One record, with its content hash as the validator — the same hash ingest deduplicates on, so a
/// conditional request answers with what the store actually knows rather than with a timestamp.
///
/// # Errors
///
/// Returns 400 for a malformed address, 404 if there is no such record or it has been deleted, and
/// 500 if the store cannot answer.
pub async fn get_record<S: Store>(
    State(state): State<ApiState<S>>,
    Path((source, collection, id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let key = RecordKey::new(
        parse::<SourceName>(&source, "source")?,
        parse::<CollectionName>(&collection, "collection")?,
        parse::<RecordId>(&id, "record id")?,
    );

    let record = state.store.get(&key).await?.ok_or(ApiError::NotFound)?;
    // A tombstone is readable through the change feed, never here: this endpoint serves what is
    // public now, and a deleted record is not.
    if record.is_deleted() {
        return Err(ApiError::NotFound);
    }

    let etag = quoted(&record.content_hash.to_hex());
    if is_fresh(&headers, &etag) {
        return Ok(not_modified(&state, &etag));
    }
    Ok((cache_headers(&state, &etag), Json(RecordBody::from(record))).into_response())
}

/// Parses a path segment, turning a grammar violation into a 400 that names the segment.
fn parse<T>(value: &str, what: &str) -> Result<T, ApiError>
where
    T: FromStr,
    T::Err: Display,
{
    T::from_str(value).map_err(|error| ApiError::bad_path(what, &error))
}

/// The validator for a listing: the collection's position plus the page it describes.
///
/// `max_seq` alone would be wrong — two pages of one collection share it while serving different
/// records, and a cache keyed on the URL would still be allowed to answer a stale 304.
fn listing_etag(max_seq: Option<Seq>, cursor: Option<Seq>, limit: PageLimit) -> String {
    quoted(&format!(
        "{}.{}.{}",
        max_seq.map_or(0, Seq::get),
        cursor.map_or(0, Seq::get),
        limit.get()
    ))
}

fn quoted(value: &str) -> String {
    format!("\"{value}\"")
}

/// `ETag` plus `Cache-Control`, the pair every cacheable response carries.
fn cache_headers<S>(state: &ApiState<S>, etag: &str) -> [(HeaderName, HeaderValue); 2] {
    // Both values are built from a hex digest, decimal digits and a fixed prefix, so they cannot
    // fail to parse as header values. The fallback keeps that assumption from becoming a panic.
    let etag = HeaderValue::from_str(etag).unwrap_or(HeaderValue::from_static("\"\""));
    let cache_control =
        HeaderValue::from_str(&format!("public, s-maxage={}", state.shared_max_age))
            .unwrap_or(HeaderValue::from_static("public"));
    [(ETAG, etag), (CACHE_CONTROL, cache_control)]
}

/// A 304 carries the validator and the caching directives, and no body.
fn not_modified<S>(state: &ApiState<S>, etag: &str) -> Response {
    (StatusCode::NOT_MODIFIED, cache_headers(state, etag)).into_response()
}
