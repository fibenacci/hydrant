//! Instrumentation.
//!
//! One metric here is not optional: `ingest_dropped_field_total`. Deny-by-default only works if a
//! source system that starts sending a new field shows up somewhere, and a log line nobody greps is
//! not somewhere. That counter is what turns "the field is missing" into "the field was dropped, at
//! this rate, from this collection, starting at this time".
//!
//! Label values are chosen to stay bounded. A schema's field names are a closed set and so are its
//! collections, but an array index is not — `images[2]` and `images[7]` would be two series, and a
//! payload with a thousand-element array would be a thousand. Paths are therefore normalised before
//! they become labels.

use std::time::Instant;

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use hydrant_core::{CollectionName, DropReason, DroppedField};
use hydrant_store::Applied;
use metrics::{counter, histogram};

/// Fields dropped at ingest, by collection, field and reason.
pub const INGEST_DROPPED_FIELD: &str = "ingest_dropped_field_total";

/// Records applied at ingest, by collection and outcome.
pub const INGEST_RECORDS: &str = "ingest_records_total";

/// Requests served, by method, matched route and status.
pub const HTTP_REQUESTS: &str = "http_requests_total";

/// Request duration in seconds, by method and matched route.
pub const HTTP_DURATION: &str = "http_request_duration_seconds";

/// Counts every field projection removed.
///
/// One increment per dropped field, not one per record: the question this answers is "which field is
/// arriving that we do not release", and that is per field by construction.
pub fn record_dropped(collection: &CollectionName, dropped: &[DroppedField]) {
    for field in dropped {
        let reason = match field.reason {
            DropReason::UnknownKey => "unknown_key",
            DropReason::TypeMismatch { .. } => "type_mismatch",
        };
        counter!(
            INGEST_DROPPED_FIELD,
            "collection" => collection.to_string(),
            "field" => label_path(&field.path),
            "reason" => reason,
        )
        .increment(1);
    }
}

/// Counts what a batch did.
pub fn record_outcomes(collection: &CollectionName, outcomes: &[Applied]) {
    for outcome in outcomes {
        let label = match outcome {
            Applied::Stored { .. } => "stored",
            Applied::Tombstoned { .. } => "tombstoned",
            Applied::Unchanged => "unchanged",
        };
        counter!(
            INGEST_RECORDS,
            "collection" => collection.to_string(),
            "outcome" => label,
        )
        .increment(1);
    }
}

/// Middleware that counts and times every request.
///
/// The label is the matched route pattern, never the request path: `/v1/{source}/{collection}/{id}`
/// is one series, while the concrete paths are as many series as there are records.
pub async fn track(request: Request, next: Next) -> Response {
    let method = request.method().to_string();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |path| path.as_str().to_owned());

    let start = Instant::now();
    let response = next.run(request).await;
    let elapsed = start.elapsed().as_secs_f64();

    counter!(
        HTTP_REQUESTS,
        "method" => method.clone(),
        "route" => route.clone(),
        "status" => response.status().as_u16().to_string(),
    )
    .increment(1);
    histogram!(HTTP_DURATION, "method" => method, "route" => route).record(elapsed);

    response
}

/// Replaces array indices in a drop path with `[]`, so one array is one series.
fn label_path(path: &str) -> String {
    let mut label = String::with_capacity(path.len());
    let mut in_index = false;
    for character in path.chars() {
        match character {
            '[' => {
                in_index = true;
                label.push('[');
            }
            ']' => {
                in_index = false;
                label.push(']');
            }
            _ if in_index => {}
            _ => label.push(character),
        }
    }
    label
}

#[cfg(test)]
mod tests {
    use hydrant_core::DropReason;
    use metrics::with_local_recorder;
    use metrics_util::debugging::{DebuggingRecorder, Snapshotter};

    use super::*;

    fn collection() -> CollectionName {
        "catalog.product".parse().expect("valid collection")
    }

    /// Records `f`'s metrics into a snapshotter instead of the global recorder.
    fn snapshot(f: impl FnOnce()) -> Snapshotter {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        with_local_recorder(&recorder, f);
        snapshotter
    }

    fn labels_of(snapshotter: &Snapshotter, metric: &str) -> Vec<Vec<String>> {
        snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .filter(|(key, _, _, _)| key.key().name() == metric)
            .map(|(key, _, _, _)| {
                key.key()
                    .labels()
                    .map(|label| format!("{}={}", label.key(), label.value()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn every_dropped_field_is_counted_once() {
        let dropped = vec![
            DroppedField {
                path: "cost_price".to_owned(),
                reason: DropReason::UnknownKey,
            },
            DroppedField {
                path: "attributes.supplier".to_owned(),
                reason: DropReason::UnknownKey,
            },
        ];
        let snapshotter = snapshot(|| record_dropped(&collection(), &dropped));

        let labels = labels_of(&snapshotter, INGEST_DROPPED_FIELD);
        assert_eq!(labels.len(), 2, "one series per field: {labels:?}");
        assert!(
            labels
                .iter()
                .any(|l| l.contains(&"field=cost_price".to_owned())),
            "{labels:?}"
        );
        assert!(
            labels
                .iter()
                .any(|l| l.contains(&"field=attributes.supplier".to_owned())),
            "{labels:?}"
        );
        assert!(
            labels
                .iter()
                .all(|l| l.contains(&"collection=catalog.product".to_owned()))
        );
        assert!(
            labels
                .iter()
                .all(|l| l.contains(&"reason=unknown_key".to_owned()))
        );
    }

    #[test]
    fn a_type_mismatch_is_counted_under_its_own_reason() {
        let dropped = vec![DroppedField {
            path: "price".to_owned(),
            reason: DropReason::TypeMismatch {
                expected: "number",
                found: "string",
            },
        }];
        let snapshotter = snapshot(|| record_dropped(&collection(), &dropped));

        let labels = labels_of(&snapshotter, INGEST_DROPPED_FIELD);
        assert_eq!(labels.len(), 1);
        assert!(
            labels[0].contains(&"reason=type_mismatch".to_owned()),
            "{labels:?}"
        );
    }

    #[test]
    fn array_indices_collapse_into_one_series() {
        // Without this, a payload with a thousand-element array would create a thousand series, and
        // the metric meant to make a dropped field visible would take the monitoring down with it.
        let dropped = (0..50)
            .map(|index| DroppedField {
                path: format!("images[{index}]"),
                reason: DropReason::TypeMismatch {
                    expected: "string",
                    found: "number",
                },
            })
            .collect::<Vec<_>>();
        let snapshotter = snapshot(|| record_dropped(&collection(), &dropped));

        let labels = labels_of(&snapshotter, INGEST_DROPPED_FIELD);
        assert_eq!(labels.len(), 1, "fifty elements, one series: {labels:?}");
        assert!(
            labels[0].contains(&"field=images[]".to_owned()),
            "{labels:?}"
        );
    }

    #[test]
    fn nested_array_paths_normalise_too() {
        assert_eq!(label_path("variants[3].images[12]"), "variants[].images[]");
        assert_eq!(label_path("attributes.colour"), "attributes.colour");
    }

    #[test]
    fn outcomes_are_counted_by_kind() {
        let outcomes = vec![
            Applied::Stored {
                seq: hydrant_core::Seq::new(1),
            },
            Applied::Stored {
                seq: hydrant_core::Seq::new(2),
            },
            Applied::Unchanged,
            Applied::Tombstoned {
                seq: hydrant_core::Seq::new(3),
            },
        ];
        let snapshotter = snapshot(|| record_outcomes(&collection(), &outcomes));

        let labels = labels_of(&snapshotter, INGEST_RECORDS);
        assert_eq!(labels.len(), 3, "three distinct outcomes: {labels:?}");
    }
}
