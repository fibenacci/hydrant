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
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::header::{CACHE_CONTROL, ETAG};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use hydrant_core::schema::CollectionSchema;
use hydrant_core::{CollectionName, Filter, RecordId, RecordKey, Seq, SourceName, content_hash};
use hydrant_store::{PageBudget, PageLimit, Store};
use serde::{Deserialize, Serialize};

use crate::cache::is_fresh;
use crate::error::ApiError;
use crate::query::parse_list_params;
use crate::response::{ChangesBody, ManifestBody, PageBody, RecordBody};
use crate::state::ApiState;

/// Query parameters of the change feed.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangesQuery {
    /// Feed position already seen. Everything after it is returned.
    since: Option<u64>,
    /// Page size, clamped like a listing's.
    limit: Option<u16>,
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
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let source = parse::<SourceName>(&source, "source")?;
    let collection = parse::<CollectionName>(&collection, "collection")?;
    let schema = schema(&state, &collection)?;
    let max_age = schema.cache().shared_max_age;

    let params = parse_list_params(query.as_deref())?;
    let filter =
        Filter::parse(schema, params.filters.iter().map(|(f, v)| (f, v))).map_err(|error| {
            ApiError::BadRequest {
                code: "invalid_filter",
                message: error.to_string(),
            }
        })?;
    let limit = params
        .limit
        .map_or_else(PageLimit::default, PageLimit::clamp);
    let budget = PageBudget {
        records: limit,
        bytes: state.response_bytes,
    };
    let cursor = params.cursor.map(Seq::new);

    let max_seq = state.store.max_seq(&source, &collection).await?;
    let etag = listing_validator(max_seq, cursor, limit, &filter);
    if is_fresh(&headers, &etag) {
        return Ok(not_modified(max_age, &etag));
    }

    let page = state
        .store
        .list(&source, &collection, &filter, cursor, budget)
        .await?;
    let body = PageBody::new(page, max_seq.map_or(0, Seq::get));
    Ok((cache_headers(max_age, &etag), Json(body)).into_response())
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
    let collection = parse::<CollectionName>(&collection, "collection")?;
    let max_age = shared_max_age(&state, &collection)?;
    let key = RecordKey::new(
        parse::<SourceName>(&source, "source")?,
        collection,
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
        return Ok(not_modified(max_age, &etag));
    }
    Ok((
        cache_headers(max_age, &etag),
        Json(RecordBody::from(record)),
    )
        .into_response())
}

/// `GET /v1/{source}/{collection}/changes`
///
/// Every change after `?since=`, tombstones included, in feed order. This is how a consumer
/// replicates rather than polls: it keeps the last `next_cursor` and asks again.
///
/// # Errors
///
/// Returns 400 for a malformed source or collection name and for any undeclared query parameter,
/// and 500 if the store cannot answer.
pub async fn changes<S: Store>(
    State(state): State<ApiState<S>>,
    Path((source, collection)): Path<(String, String)>,
    query: Result<Query<ChangesQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let source = parse::<SourceName>(&source, "source")?;
    let collection = parse::<CollectionName>(&collection, "collection")?;
    let max_age = shared_max_age(&state, &collection)?;
    let Query(query) = query.map_err(|rejection| ApiError::BadRequest {
        code: "invalid_query",
        message: rejection.body_text(),
    })?;

    let limit = query
        .limit
        .map_or_else(PageLimit::default, PageLimit::clamp);
    let budget = PageBudget {
        records: limit,
        bytes: state.response_bytes,
    };
    let since = query.since.map(Seq::new);

    let max_seq = state.store.max_seq(&source, &collection).await?;
    let etag = validator("c", max_seq, since, Some(limit));
    if is_fresh(&headers, &etag) {
        return Ok(not_modified(max_age, &etag));
    }

    let page = state
        .store
        .changes(&source, &collection, since, budget)
        .await?;
    let body = ChangesBody::new(page, max_seq.map_or(0, Seq::get));
    Ok((cache_headers(max_age, &etag), Json(body)).into_response())
}

/// `GET /v1/{source}/{collection}/manifest`
///
/// The collection's count, checksum and feed position — enough for a sender to tell whether its own
/// state matches, in one request. When it does not, the per-record digests say which record.
///
/// # Errors
///
/// Returns 400 for a malformed source or collection name, and 500 if the store cannot answer.
pub async fn manifest<S: Store>(
    State(state): State<ApiState<S>>,
    Path((source, collection)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let source = parse::<SourceName>(&source, "source")?;
    let collection = parse::<CollectionName>(&collection, "collection")?;
    let max_age = shared_max_age(&state, &collection)?;

    let max_seq = state.store.max_seq(&source, &collection).await?;
    // The manifest is a function of the collection's state, and every change moves the feed
    // position - so the position alone determines this representation.
    let etag = validator("m", max_seq, None, None);
    if is_fresh(&headers, &etag) {
        return Ok(not_modified(max_age, &etag));
    }

    let manifest = state.store.manifest(&source, &collection).await?;
    Ok((
        cache_headers(max_age, &etag),
        Json(ManifestBody::from(manifest)),
    )
        .into_response())
}

/// Parses a path segment, turning a grammar violation into a 400 that names the segment.
fn parse<T>(value: &str, what: &str) -> Result<T, ApiError>
where
    T: FromStr,
    T::Err: Display,
{
    T::from_str(value).map_err(|error| ApiError::bad_path(what, &error))
}

/// The validator for a collection-level representation: what the collection is at, plus which slice
/// of it this response is.
///
/// `max_seq` alone would be wrong — two pages of one collection share it while serving different
/// records. The `kind` prefix keeps a listing, a feed page and a manifest from ever computing the
/// same validator, so no cache can answer one with another.
/// The validator for a listing, filters included.
///
/// A filtered page and an unfiltered one share a collection position and page parameters while
/// serving different records, so the filter belongs in the validator as much as the cursor does.
/// The filter is folded in through the canonical form, which is the one rendering of it that two
/// implementations agree on.
fn listing_validator(
    max_seq: Option<Seq>,
    cursor: Option<Seq>,
    limit: PageLimit,
    filter: &Filter,
) -> String {
    let base = validator("l", max_seq, cursor, Some(limit));
    if filter.is_empty() {
        return base;
    }
    let fingerprint = content_hash(&filter.as_json()).map_or_else(
        |_| "unhashable".to_owned(),
        |hash| hash.to_hex()[..16].to_owned(),
    );
    // `base` is quoted; splice the fingerprint in before the closing quote.
    format!("{}.{fingerprint}\"", base.trim_end_matches('"'))
}

fn validator(
    kind: &str,
    max_seq: Option<Seq>,
    cursor: Option<Seq>,
    limit: Option<PageLimit>,
) -> String {
    let position = max_seq.map_or(0, Seq::get);
    // Fixed arity per kind: a paged view always carries a cursor (0 for the first page) and a
    // limit, a manifest carries neither. Otherwise two different slices could render the same
    // validator by dropping an absent field.
    match limit {
        Some(limit) => quoted(&format!(
            "{kind}.{position}.{}.{}",
            cursor.map_or(0, Seq::get),
            limit.get()
        )),
        None => quoted(&format!("{kind}.{position}")),
    }
}

fn quoted(value: &str) -> String {
    format!("\"{value}\"")
}

/// The collection's declared `s-maxage`, or a 404 if the collection is not served at all.
fn shared_max_age<S>(state: &ApiState<S>, collection: &CollectionName) -> Result<u32, ApiError> {
    schema(state, collection).map(|schema| schema.cache().shared_max_age)
}

/// The collection's definition, or a 404. A collection exists because a schema declares it — an
/// undeclared name serving an empty page would tell a consumer it had replicated nothing, rather
/// than that it asked for the wrong thing.
fn schema<'a, S>(
    state: &'a ApiState<S>,
    collection: &CollectionName,
) -> Result<&'a CollectionSchema, ApiError> {
    state
        .schemas
        .get(collection)
        .ok_or(ApiError::UnknownCollection)
}

/// `ETag` plus `Cache-Control`, the pair every cacheable response carries.
fn cache_headers(shared_max_age: u32, etag: &str) -> [(HeaderName, HeaderValue); 2] {
    // Both values are built from a hex digest, decimal digits and a fixed prefix, so they cannot
    // fail to parse as header values. The fallback keeps that assumption from becoming a panic.
    let etag = HeaderValue::from_str(etag).unwrap_or(HeaderValue::from_static("\"\""));
    let cache_control = HeaderValue::from_str(&format!("public, s-maxage={shared_max_age}"))
        .unwrap_or(HeaderValue::from_static("public"));
    [(ETAG, etag), (CACHE_CONTROL, cache_control)]
}

/// A 304 carries the validator and the caching directives, and no body.
fn not_modified(shared_max_age: u32, etag: &str) -> Response {
    (
        StatusCode::NOT_MODIFIED,
        cache_headers(shared_max_age, etag),
    )
        .into_response()
}
