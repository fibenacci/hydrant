//! Query filters, validated against a collection's schema.
//!
//! Filtering is deliberately narrow: equality on fields the schema declares as filterable, and
//! nothing else. No ranges, no arbitrary JSON paths, no free-form predicates. A public read endpoint
//! that accepts an arbitrary predicate is an invitation to write the one query that scans everything,
//! and every filter a schema does not name is a query nobody reviewed.
//!
//! The comparison is by value on the projected payload, so a filter can only ever see fields that
//! were released — there is no path from a filter to a field that projection dropped.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::schema::{CollectionSchema, FieldName, FieldSpec, ScalarType};

/// Why a filter could not be accepted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilterError {
    /// The field is not declared in the collection's schema at all.
    #[error("`{field}` is not a field of this collection")]
    UnknownField {
        /// The name that was asked for.
        field: String,
    },
    /// The field exists but the schema does not list it as filterable.
    #[error("`{field}` is not declared as a filter for this collection")]
    NotFilterable {
        /// The name that was asked for.
        field: String,
    },
    /// The value does not fit the field's declared type.
    #[error("`{value}` is not a valid {expected} for `{field}`")]
    NotTheDeclaredType {
        /// The field being filtered on.
        field: String,
        /// The declared type.
        expected: &'static str,
        /// What arrived.
        value: String,
    },
    /// The same field was filtered on twice.
    #[error("`{field}` is filtered on more than once")]
    Duplicate {
        /// The repeated field.
        field: String,
    },
}

/// A validated set of equality filters.
///
/// Rendered as a JSON object and applied by containment, so one filter and five compose the same
/// way and the store needs no query builder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter(BTreeMap<String, Value>);

impl Filter {
    /// Validates raw `field = value` pairs against `schema`.
    ///
    /// Values arrive as strings, because that is what a query string carries, and are converted to
    /// the field's declared type. A number field filtered with a word is a bad request rather than a
    /// filter that silently matches nothing — the difference matters to whoever is debugging it.
    ///
    /// # Errors
    ///
    /// Returns [`FilterError`] if a field is unknown, not declared filterable, given twice, or given
    /// a value of the wrong type.
    pub fn parse<I, K, V>(schema: &CollectionSchema, pairs: I) -> Result<Self, FilterError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let mut filters = BTreeMap::new();
        for (field, value) in pairs {
            let (field, value) = (field.as_ref(), value.as_ref());

            let name = FieldName::new(field).map_err(|_| FilterError::UnknownField {
                field: field.to_owned(),
            })?;
            let spec = schema
                .fields()
                .get(field)
                .ok_or_else(|| FilterError::UnknownField {
                    field: field.to_owned(),
                })?;
            if !schema.filters().contains(&name) {
                return Err(FilterError::NotFilterable {
                    field: field.to_owned(),
                });
            }

            let value = coerce(field, spec, value)?;
            if filters.insert(field.to_owned(), value).is_some() {
                return Err(FilterError::Duplicate {
                    field: field.to_owned(),
                });
            }
        }
        Ok(Self(filters))
    }

    /// Whether anything is being filtered on.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many fields are being filtered on.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The filter as a JSON object, for containment against a payload.
    ///
    /// An empty filter renders as `{}`, which every object contains — so the store needs no special
    /// case for "no filter".
    #[must_use]
    pub fn as_json(&self) -> Value {
        Value::Object(
            self.0
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )
    }

    /// The filtered fields and their values, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }
}

/// Turns a query-string value into the field's declared type.
fn coerce(field: &str, spec: &FieldSpec, value: &str) -> Result<Value, FilterError> {
    let mismatch = |expected: &'static str| FilterError::NotTheDeclaredType {
        field: field.to_owned(),
        expected,
        value: value.to_owned(),
    };

    match spec {
        // A schema file cannot declare a top-level scalar of unconstrained type, but an allow-list
        // entry is one. Both compare as strings, because a query string offers nothing else.
        FieldSpec::Scalar {
            ty: ScalarType::String | ScalarType::Any,
            ..
        } => Ok(Value::String(value.to_owned())),
        FieldSpec::Scalar {
            ty: ScalarType::Boolean,
            ..
        } => match value {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(mismatch("boolean")),
        },
        FieldSpec::Scalar {
            ty: ScalarType::Number,
            ..
        } => {
            // Integers first, so `?filter[stock]=3` matches a stored 3 rather than 3.0. In jsonb the
            // two are the same value, but the canonical form is not, and the filter should not be
            // the place that decides.
            if let Ok(integer) = value.parse::<i64>() {
                return Ok(Value::from(integer));
            }
            value
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| mismatch("number"))
        }
        // Neither can be declared filterable - `filters` requires an indexed scalar - so this is
        // unreachable through validation, and refusing it here keeps it that way.
        FieldSpec::Object { .. } => Err(mismatch("object")),
        FieldSpec::Array { .. } => Err(mismatch("array")),
    }
}

/// The filter as a `Map`, for callers that want to build a payload from it.
impl From<&Filter> for Map<String, Value> {
    fn from(filter: &Filter) -> Self {
        filter
            .0
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema() -> CollectionSchema {
        serde_json::from_value(json!({
            "collection": "catalog.product",
            "id": "$.id",
            "fields": {
                "sku": { "type": "string", "index": true },
                "stock": { "type": "number", "index": true },
                "active": { "type": "boolean", "index": true },
                "price": { "type": "number" },
                "attributes": { "type": "object", "allow": ["color"] }
            },
            "filters": ["sku", "stock", "active"]
        }))
        .expect("valid schema")
    }

    #[test]
    fn a_declared_filter_becomes_a_containment_object() {
        let filter = Filter::parse(&schema(), [("sku", "SW-1")]).expect("valid filter");
        assert_eq!(filter.as_json(), json!({ "sku": "SW-1" }));
        assert_eq!(filter.len(), 1);
    }

    #[test]
    fn several_filters_compose() {
        let filter =
            Filter::parse(&schema(), [("sku", "SW-1"), ("stock", "3")]).expect("valid filter");
        assert_eq!(filter.as_json(), json!({ "sku": "SW-1", "stock": 3 }));
    }

    #[test]
    fn an_empty_filter_matches_everything() {
        let filter = Filter::parse(&schema(), Vec::<(&str, &str)>::new()).expect("valid filter");
        assert!(filter.is_empty());
        assert_eq!(filter.as_json(), json!({}));
    }

    #[test]
    fn a_field_that_is_not_declared_filterable_is_refused() {
        // `price` is a declared field, but the schema does not list it as a filter: filtering on it
        // would be a query nobody reviewed, on a column nobody indexed.
        assert_eq!(
            Filter::parse(&schema(), [("price", "9.99")]),
            Err(FilterError::NotFilterable {
                field: "price".to_owned()
            })
        );
    }

    #[test]
    fn a_field_that_does_not_exist_is_refused() {
        assert_eq!(
            Filter::parse(&schema(), [("colour", "red")]),
            Err(FilterError::UnknownField {
                field: "colour".to_owned()
            })
        );
    }

    #[test]
    fn a_nested_path_is_not_a_field() {
        // Filtering on a JSON path is what this design refuses; the dot makes it not a field name.
        assert_eq!(
            Filter::parse(&schema(), [("attributes.color", "red")]),
            Err(FilterError::UnknownField {
                field: "attributes.color".to_owned()
            })
        );
    }

    #[test]
    fn values_take_the_declared_type() {
        let filter =
            Filter::parse(&schema(), [("stock", "3"), ("active", "true")]).expect("valid filter");
        assert_eq!(filter.as_json(), json!({ "stock": 3, "active": true }));
    }

    #[test]
    fn a_value_of_the_wrong_type_is_a_bad_request_not_an_empty_result() {
        assert_eq!(
            Filter::parse(&schema(), [("stock", "plenty")]),
            Err(FilterError::NotTheDeclaredType {
                field: "stock".to_owned(),
                expected: "number",
                value: "plenty".to_owned(),
            })
        );
        assert!(matches!(
            Filter::parse(&schema(), [("active", "yes")]),
            Err(FilterError::NotTheDeclaredType { .. })
        ));
    }

    #[test]
    fn a_fractional_number_is_accepted_as_one() {
        let filter = Filter::parse(&schema(), [("stock", "2.5")]).expect("valid filter");
        assert_eq!(filter.as_json(), json!({ "stock": 2.5 }));
    }

    #[test]
    fn filtering_on_one_field_twice_is_refused() {
        assert_eq!(
            Filter::parse(&schema(), [("sku", "SW-1"), ("sku", "SW-2")]),
            Err(FilterError::Duplicate {
                field: "sku".to_owned()
            })
        );
    }
}
