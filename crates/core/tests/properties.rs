//! Property tests for the two operations the design leans on hardest.
//!
//! The unit tests next to the code state what projection does for cases someone thought of. These
//! state what must hold for payloads nobody thought of, which is where a projection engine actually
//! fails: a nested object, a key that looks like a path, an array of the wrong thing.

// `expect_used` is denied for the library, because a panic in the projection path is a dropped
// record. In a test binary the opposite holds: a fixture that cannot be built is a broken test, and
// panicking names the line. clippy's `allow-expect-in-tests` only covers `#[cfg(test)]` code, which
// an integration test's helpers are not.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use hydrant_core::schema::{CacheSpec, FieldName, FieldSpec, ScalarType, SortKey};
use hydrant_core::{CollectionName, CollectionSchema, content_hash, project};
use proptest::prelude::*;
use serde_json::{Map, Value};

fn field(name: &str) -> FieldName {
    FieldName::new(name).expect("valid field name")
}

/// The example `catalog.product` collection: two indexed strings, an unindexed number and string,
/// a nested allow list, and an array of strings.
fn schema() -> CollectionSchema {
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

/// Keys are drawn from the declared names, the nested names, and arbitrary noise, so a generated
/// payload has a real chance of matching, half-matching, or missing the schema entirely.
fn key() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("name".to_owned()),
        Just("sku".to_owned()),
        Just("price".to_owned()),
        Just("currency".to_owned()),
        Just("attributes".to_owned()),
        Just("images".to_owned()),
        Just("color".to_owned()),
        Just("size".to_owned()),
        Just("id".to_owned()),
        Just("attributes.color".to_owned()),
        "[a-z_]{1,6}",
    ]
}

fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i32>().prop_map(|n| Value::from(i64::from(n))),
        (-1.0e6_f64..1.0e6)
            .prop_map(|n| serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)),
        "[a-zA-Z0-9 .-]{0,12}".prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 32, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map(key(), inner, 0..4)
                .prop_map(|entries| Value::Object(entries.into_iter().collect())),
        ]
    })
}

fn json_object() -> impl Strategy<Value = Value> {
    prop::collection::btree_map(key(), json_value(), 0..8)
        .prop_map(|entries| Value::Object(entries.into_iter().collect()))
}

/// Walks a projected payload and asserts every key at every level is one the schema declares.
fn assert_only_declared(
    allow: &BTreeMap<FieldName, FieldSpec>,
    object: &Map<String, Value>,
    path: &str,
) {
    for (key, value) in object {
        let spec = allow
            .get(key.as_str())
            .unwrap_or_else(|| panic!("{path}{key} survived projection but is not declared"));
        match spec {
            FieldSpec::Scalar { ty, .. } => assert!(
                ty.accepts(value),
                "{path}{key} survived as {value} but is declared {ty}"
            ),
            FieldSpec::Object { allow } => {
                let nested = value
                    .as_object()
                    .unwrap_or_else(|| panic!("{path}{key} is declared an object"));
                assert_only_declared(allow, nested, &format!("{path}{key}."));
            }
            FieldSpec::Array { items } => {
                let elements = value
                    .as_array()
                    .unwrap_or_else(|| panic!("{path}{key} is declared an array"));
                for (index, element) in elements.iter().enumerate() {
                    match &**items {
                        FieldSpec::Scalar { ty, .. } => assert!(
                            ty.accepts(element),
                            "{path}{key}[{index}] survived as {element} but is declared {ty}"
                        ),
                        FieldSpec::Object { allow } => {
                            let nested = element.as_object().unwrap_or_else(|| {
                                panic!("{path}{key}[{index}] is declared an object")
                            });
                            assert_only_declared(allow, nested, &format!("{path}{key}[{index}]."));
                        }
                        FieldSpec::Array { .. } => {
                            assert!(element.is_array(), "nested array expected");
                        }
                    }
                }
            }
        }
    }
}

/// Object-key paths, not descending into arrays: an array element's index shifts when an earlier
/// element is dropped, so its path is not stable enough to compare across a projection.
fn object_key_paths(object: &Map<String, Value>, prefix: &str, out: &mut BTreeSet<String>) {
    for (key, value) in object {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        out.insert(path.clone());
        if let Value::Object(nested) = value {
            object_key_paths(nested, &path, out);
        }
    }
}

fn keys_of(value: &Value) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    if let Value::Object(object) = value {
        object_key_paths(object, "", &mut paths);
    }
    paths
}

/// Whether a path that did not survive projection is named in the report, either directly or
/// through an ancestor that was dropped whole. Dropping `images` because it arrived as an object
/// accounts for `images.name` as well: the subtree went with its root.
fn is_accounted_for(path: &str, reported: &BTreeSet<&str>) -> bool {
    if reported.contains(path) {
        return true;
    }
    let mut prefix = path;
    while let Some((parent, _)) = prefix.rsplit_once('.') {
        if reported.contains(parent) {
            return true;
        }
        prefix = parent;
    }
    false
}

/// Writes a JSON object with the keys in the order given, rather than in the order a sorted map
/// would produce.
fn write_object<'a, I>(pairs: I) -> String
where
    I: Iterator<Item = &'a (String, Value)>,
{
    let members: Vec<String> = pairs
        .map(|(key, value)| {
            let key = serde_json::to_string(key).expect("serialise key");
            let value = serde_json::to_string(value).expect("serialise value");
            format!("{key}:{value}")
        })
        .collect();
    format!("{{{}}}", members.join(","))
}

proptest! {
    /// The invariant the store depends on: nothing reaches persistence that the schema did not
    /// name, at any depth.
    #[test]
    fn projection_yields_only_declared_fields(payload in json_object()) {
        let schema = schema();
        let projected = project(&schema, &payload).expect("object payload");
        let object = projected.payload.as_object().expect("projection returns an object");
        assert_only_declared(schema.fields(), object, "");
    }

    /// Projecting a projected payload changes nothing and drops nothing. Without this, a
    /// re-projection after a schema change - or a second pass anywhere in the pipeline - could
    /// quietly alter stored data.
    #[test]
    fn projection_is_idempotent(payload in json_object()) {
        let schema = schema();
        let once = project(&schema, &payload).expect("object payload");
        let twice = project(&schema, &once.payload).expect("object payload");
        prop_assert_eq!(&once.payload, &twice.payload);
        prop_assert!(twice.dropped.is_empty(), "second pass dropped {:?}", twice.dropped);
    }

    /// Projection removes; it never invents. Every surviving key existed in the input.
    #[test]
    fn projection_never_adds_a_key(payload in json_object()) {
        let projected = project(&schema(), &payload).expect("object payload");
        let before = keys_of(&payload);
        let after = keys_of(&projected.payload);
        prop_assert!(
            after.is_subset(&before),
            "invented {:?}",
            after.difference(&before).collect::<Vec<_>>()
        );
    }

    /// Nothing disappears silently: every object key that did not survive is accounted for, either
    /// by name or by an ancestor that was dropped whole. This is what makes
    /// `ingest_dropped_field_total` trustworthy - a source system adding a field must show up in
    /// monitoring rather than in a response.
    #[test]
    fn every_dropped_key_is_reported(payload in json_object()) {
        let projected = project(&schema(), &payload).expect("object payload");
        let reported: BTreeSet<&str> =
            projected.dropped.iter().map(|dropped| dropped.path.as_str()).collect();
        let before = keys_of(&payload);
        let after = keys_of(&projected.payload);
        for missing in before.difference(&after) {
            prop_assert!(
                is_accounted_for(missing, &reported),
                "{missing} vanished without a report; reported: {reported:?}"
            );
        }
    }

    /// The wire contract: the hash depends on the document, never on the order its keys arrived
    /// in. A sender whose map iterates in a different order must still compute the same hash.
    ///
    /// The two documents are written out as JSON text by hand, because `serde_json`'s map is
    /// already sorted - serialising it would erase the very difference under test.
    #[test]
    fn content_hash_ignores_key_order(
        entries in prop::collection::btree_map(key(), json_value(), 1..6)
    ) {
        let pairs: Vec<(String, Value)> = entries.into_iter().collect();
        let forward: Value = serde_json::from_str(&write_object(pairs.iter()))
            .expect("parse hand-written object");
        let reverse: Value = serde_json::from_str(&write_object(pairs.iter().rev()))
            .expect("parse hand-written object");

        prop_assert_eq!(&forward, &reverse);
        prop_assert_eq!(
            content_hash(&forward).expect("hash"),
            content_hash(&reverse).expect("hash")
        );
    }

    /// Canonicalisation is a re-serialisation, not a transformation: the canonical bytes parse back
    /// to the value they came from.
    #[test]
    fn canonical_form_round_trips(value in json_value()) {
        let canonical = hydrant_core::canonicalize(&value).expect("canonicalise");
        let parsed: Value = serde_json::from_slice(&canonical).expect("parse canonical form");
        prop_assert_eq!(parsed, value);
    }

    /// Different documents hash differently. Not a proof of collision resistance - that is
    /// SHA-256's job - but it catches a canonicalisation that discards information.
    #[test]
    fn distinct_documents_hash_differently(a in json_object(), b in json_object()) {
        prop_assume!(a != b);
        prop_assert_ne!(content_hash(&a).expect("hash"), content_hash(&b).expect("hash"));
    }
}
