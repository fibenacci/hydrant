//! The projection engine: the only way a payload becomes storable.
//!
//! Projection runs at ingest and drops every key the collection schema does not name. It never
//! runs on read. That asymmetry is the whole design: filtering on read makes every bug in the read
//! path a data leak, while filtering on write makes a bug a missing field. A field that was never
//! stored cannot be served by a broken query, a debug endpoint, or a future feature.
//!
//! Nothing is dropped silently. Every removed key is reported with its path and a reason, so a
//! caller can raise `ingest_dropped_field_total{collection,field}` and a source system that starts
//! sending a new field shows up in monitoring rather than in a response.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::schema::{CollectionSchema, FieldName, FieldSpec, kind_of};

/// Why a key did not survive projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    /// The schema does not name this key. This is the ordinary case, and the one that must stay
    /// visible: it is how a new field in the source system announces itself.
    UnknownKey,
    /// The key is declared, but the value is not of the declared shape.
    TypeMismatch {
        /// What the schema declared.
        expected: &'static str,
        /// The JSON type that arrived.
        found: &'static str,
    },
}

impl std::fmt::Display for DropReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownKey => f.write_str("not declared by the schema"),
            Self::TypeMismatch { expected, found } => {
                write!(f, "declared as {expected}, but a {found} arrived")
            }
        }
    }
}

/// One key that was removed from a payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedField {
    /// Dotted path of the key, with array indices — `attributes.color`, `images[2]`.
    pub path: String,
    /// Why it was removed.
    pub reason: DropReason,
}

/// The result of projecting one payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Projection {
    /// The payload as it will be persisted: a JSON object containing only released fields.
    pub payload: Value,
    /// Every key that was removed, in the order encountered.
    pub dropped: Vec<DroppedField>,
}

impl Projection {
    /// Whether anything was dropped.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.dropped.is_empty()
    }
}

/// A payload could not be projected at all.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProjectionError {
    /// The payload was not a JSON object. A record is a document; a bare array or scalar has no
    /// fields to release.
    #[error("a payload must be a JSON object, this one is a {found}")]
    PayloadNotAnObject {
        /// The JSON type that arrived.
        found: &'static str,
    },
}

/// Projects `payload` through `schema`.
///
/// Keys the schema names are kept if the value matches the declared shape. Everything else is
/// removed and reported. A declared field that is simply absent is absent — projection releases
/// fields, it does not require them.
///
/// # Errors
///
/// Returns [`ProjectionError::PayloadNotAnObject`] if the payload is not a JSON object.
pub fn project(schema: &CollectionSchema, payload: &Value) -> Result<Projection, ProjectionError> {
    let object = payload
        .as_object()
        .ok_or(ProjectionError::PayloadNotAnObject {
            found: kind_of(payload),
        })?;

    let mut dropped = Vec::new();
    let kept = project_object(schema.fields(), object, "", &mut dropped);
    Ok(Projection {
        payload: Value::Object(kept),
        dropped,
    })
}

/// Projects one object level against its allow list.
fn project_object(
    allow: &BTreeMap<FieldName, FieldSpec>,
    input: &Map<String, Value>,
    prefix: &str,
    dropped: &mut Vec<DroppedField>,
) -> Map<String, Value> {
    let mut kept = Map::new();
    for (key, value) in input {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        // A key containing a dot can never match a declared field name, so it lands here as an
        // unknown key rather than being split into a path.
        let Some(spec) = allow.get(key.as_str()) else {
            dropped.push(DroppedField {
                path,
                reason: DropReason::UnknownKey,
            });
            continue;
        };
        if let Some(value) = project_value(spec, value, &path, dropped) {
            kept.insert(key.clone(), value);
        }
    }
    kept
}

/// Projects one value against its specification, or reports why it cannot be kept.
fn project_value(
    spec: &FieldSpec,
    value: &Value,
    path: &str,
    dropped: &mut Vec<DroppedField>,
) -> Option<Value> {
    match spec {
        FieldSpec::Scalar { ty, .. } => {
            if ty.accepts(value) {
                Some(value.clone())
            } else {
                dropped.push(mismatch(path, spec.kind(), value));
                None
            }
        }
        FieldSpec::Object { allow } => {
            let Some(object) = value.as_object() else {
                dropped.push(mismatch(path, spec.kind(), value));
                return None;
            };
            Some(Value::Object(project_object(allow, object, path, dropped)))
        }
        FieldSpec::Array { items } => {
            let Some(elements) = value.as_array() else {
                dropped.push(mismatch(path, spec.kind(), value));
                return None;
            };
            // A mismatching element is dropped on its own rather than taking the array with it:
            // one unusable entry should cost that entry, not the whole list.
            let mut kept = Vec::with_capacity(elements.len());
            for (index, element) in elements.iter().enumerate() {
                let element_path = format!("{path}[{index}]");
                if let Some(value) = project_value(items, element, &element_path, dropped) {
                    kept.push(value);
                }
            }
            Some(Value::Array(kept))
        }
    }
}

fn mismatch(path: &str, expected: &'static str, value: &Value) -> DroppedField {
    DroppedField {
        path: path.to_owned(),
        reason: DropReason::TypeMismatch {
            expected,
            found: kind_of(value),
        },
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::ident::CollectionName;
    use crate::schema::{CacheSpec, ScalarType, SortKey};

    /// The schema from `schemas/example.catalog.product.yaml`, built by hand until the loader
    /// exists.
    fn example_schema() -> CollectionSchema {
        let field = |name: &str| FieldName::new(name).expect("valid field name");
        let fields = BTreeMap::from([
            (field("name"), FieldSpec::indexed(ScalarType::String)),
            (field("sku"), FieldSpec::indexed(ScalarType::String)),
            (field("price"), FieldSpec::scalar(ScalarType::Number)),
            (field("currency"), FieldSpec::scalar(ScalarType::String)),
            (
                field("attributes"),
                FieldSpec::allow_scalars(["color", "size", "material"]).expect("allow list"),
            ),
            (
                field("images"),
                FieldSpec::array(FieldSpec::scalar(ScalarType::String)),
            ),
        ]);
        CollectionSchema::new(
            CollectionName::new("catalog.product").expect("valid collection"),
            "$.id".parse().expect("id path"),
            fields,
            vec![field("sku"), field("name")],
            vec![SortKey::Seq, SortKey::Field(field("name"))],
            CacheSpec::default(),
        )
        .expect("valid schema")
    }

    #[test]
    fn keeps_declared_fields_and_drops_everything_else() {
        let payload = json!({
            "id": "SW1",
            "name": "Chair",
            "sku": "SW-1",
            "price": 49.9,
            "cost_price": 12.5,
            "internal_note": "do not publish"
        });
        let projected = project(&example_schema(), &payload).expect("object payload");

        assert_eq!(
            projected.payload,
            json!({ "name": "Chair", "sku": "SW-1", "price": 49.9 })
        );
        let mut paths: Vec<&str> = projected.dropped.iter().map(|d| d.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, ["cost_price", "id", "internal_note"]);
        assert!(
            projected
                .dropped
                .iter()
                .all(|d| d.reason == DropReason::UnknownKey)
        );
    }

    #[test]
    fn the_id_is_not_a_released_field_unless_declared() {
        // `id: $.id` lifts the identifier into the record key; the payload itself keeps it only if
        // the schema also declares it as a field. Here it does not.
        let projected = project(&example_schema(), &json!({ "id": "SW1" })).expect("payload");
        assert_eq!(projected.payload, json!({}));
    }

    #[test]
    fn nested_objects_are_allow_listed_at_their_own_level() {
        let payload = json!({
            "attributes": { "color": "red", "size": "M", "supplier_cost": 3.5 }
        });
        let projected = project(&example_schema(), &payload).expect("payload");
        assert_eq!(
            projected.payload,
            json!({ "attributes": { "color": "red", "size": "M" } })
        );
        assert_eq!(
            projected.dropped,
            vec![DroppedField {
                path: "attributes.supplier_cost".to_owned(),
                reason: DropReason::UnknownKey,
            }]
        );
    }

    #[test]
    fn a_type_mismatch_is_a_missing_field_not_a_rejected_payload() {
        let projected = project(&example_schema(), &json!({ "price": "49.90" })).expect("payload");
        assert_eq!(projected.payload, json!({}));
        assert_eq!(
            projected.dropped,
            vec![DroppedField {
                path: "price".to_owned(),
                reason: DropReason::TypeMismatch {
                    expected: "number",
                    found: "string"
                },
            }]
        );
    }

    #[test]
    fn array_elements_are_checked_individually() {
        let payload = json!({ "images": ["a.jpg", 7, "b.jpg", { "url": "c.jpg" }] });
        let projected = project(&example_schema(), &payload).expect("payload");
        assert_eq!(projected.payload, json!({ "images": ["a.jpg", "b.jpg"] }));
        assert_eq!(
            projected.dropped,
            vec![
                DroppedField {
                    path: "images[1]".to_owned(),
                    reason: DropReason::TypeMismatch {
                        expected: "string",
                        found: "number"
                    },
                },
                DroppedField {
                    path: "images[3]".to_owned(),
                    reason: DropReason::TypeMismatch {
                        expected: "string",
                        found: "object"
                    },
                },
            ]
        );
    }

    #[test]
    fn an_object_where_a_scalar_was_declared_is_dropped_whole() {
        let projected =
            project(&example_schema(), &json!({ "sku": { "value": "SW-1" } })).expect("payload");
        assert_eq!(projected.payload, json!({}));
        assert_eq!(
            projected.dropped,
            vec![DroppedField {
                path: "sku".to_owned(),
                reason: DropReason::TypeMismatch {
                    expected: "string",
                    found: "object"
                },
            }]
        );
    }

    #[test]
    fn an_allow_listed_key_takes_any_scalar_including_null() {
        let payload = json!({ "attributes": { "color": 5, "size": null, "material": true } });
        let projected = project(&example_schema(), &payload).expect("payload");
        assert_eq!(projected.payload, payload);
        assert!(projected.is_complete());
    }

    #[test]
    fn a_dotted_payload_key_is_unknown_rather_than_a_path() {
        let projected =
            project(&example_schema(), &json!({ "attributes.color": "red" })).expect("payload");
        assert_eq!(projected.payload, json!({}));
        assert_eq!(
            projected.dropped,
            vec![DroppedField {
                path: "attributes.color".to_owned(),
                reason: DropReason::UnknownKey,
            }]
        );
    }

    #[test]
    fn a_payload_that_is_not_an_object_is_refused() {
        assert_eq!(
            project(&example_schema(), &json!([{ "sku": "SW-1" }])),
            Err(ProjectionError::PayloadNotAnObject { found: "array" })
        );
    }
}
