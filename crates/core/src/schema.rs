//! Collection definitions: what a collection releases, and how it may be queried.
//!
//! A schema is the operator's declaration of everything that becomes public. It lives in the
//! operator's git repository rather than in an admin UI, so releasing a field is a pull request
//! with a reviewer — exactly the friction that decision deserves.
//!
//! Loading a schema from YAML arrives with a later milestone; these types are the model such a
//! loader produces, and the validation it has to satisfy.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde_json::Value;

use crate::ident::{CollectionName, IdentError, MAX_NAME_LEN, RecordId};

/// Default `s-maxage` for shared caches, in seconds.
pub const DEFAULT_SHARED_MAX_AGE: u32 = 300;

/// Why a collection definition is not usable.
///
/// Every variant is refused at load time. The service validates schemas at boot and refuses to
/// start on an invalid one: there is no partial-load mode, because a half-loaded schema would
/// mean a collection serving fields nobody reviewed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SchemaError {
    /// A field name was not usable.
    #[error("invalid field name: {0}")]
    FieldName(#[from] IdentError),
    /// The collection declared no fields at all.
    #[error("a collection must declare at least one field")]
    NoFields,
    /// An object field carried no allow list.
    #[error("object field `{path}` has no allow list; `type: object` is not a wildcard")]
    ObjectWithoutAllowList {
        /// Dotted path of the offending field.
        path: String,
    },
    /// A filter or sort key referred to a field that was never declared.
    #[error("{usage} refers to `{field}`, which is not a declared field")]
    UnknownField {
        /// Either `filters` or `sort`.
        usage: &'static str,
        /// The name that could not be resolved.
        field: String,
    },
    /// A filter or sort key referred to a field that is not indexed.
    #[error("{usage} refers to `{field}`, which is not an indexed scalar field")]
    FieldNotIndexed {
        /// Either `filters` or `sort`.
        usage: &'static str,
        /// The offending name.
        field: String,
    },
    /// The same key appeared twice in `filters` or `sort`.
    #[error("{usage} lists `{field}` twice")]
    Duplicate {
        /// Either `filters` or `sort`.
        usage: &'static str,
        /// The repeated name.
        field: String,
    },
}

/// The name of a field inside a payload.
///
/// Dots are refused so that a dotted path in a drop report — `attributes.color` — has exactly one
/// reading.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FieldName(String);

impl FieldName {
    /// Validates `value` and wraps it.
    ///
    /// # Errors
    ///
    /// Returns [`IdentError`] if the name is empty, longer than [`MAX_NAME_LEN`], contains a
    /// control character, or contains a dot.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentError> {
        const KIND: &str = "field name";
        let value = value.into();
        if value.is_empty() {
            return Err(IdentError::Empty { kind: KIND });
        }
        if value.len() > MAX_NAME_LEN {
            return Err(IdentError::TooLong {
                kind: KIND,
                len: value.len(),
                max: MAX_NAME_LEN,
            });
        }
        if let Some(ch) = value.chars().find(|ch| ch.is_control() || *ch == '.') {
            return Err(IdentError::Char { kind: KIND, ch });
        }
        Ok(Self(value))
    }

    /// Borrows the name as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for FieldName {
    /// Lets an allow list be looked up by a payload key without allocating a [`FieldName`] for
    /// every key in every incoming document.
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FieldName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for FieldName {
    type Err = IdentError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

/// The type a scalar field must have to be kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ScalarType {
    /// A JSON string.
    String,
    /// A JSON number.
    Number,
    /// A JSON boolean.
    Boolean,
    /// Any JSON scalar, including `null`. This is what an `allow: [a, b]` entry expands to: the
    /// key is released, its scalar type is not constrained.
    Any,
}

impl ScalarType {
    /// Whether `value` satisfies this type.
    #[must_use]
    pub fn accepts(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Number => value.is_number(),
            Self::Boolean => value.is_boolean(),
            Self::Any => !(value.is_object() || value.is_array()),
        }
    }

    /// The name used in drop reports and error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Any => "scalar",
        }
    }
}

impl fmt::Display for ScalarType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What a declared field may contain.
///
/// There is no variant that passes a subtree through unexamined. Nesting is allow-listed at every
/// level, because a wildcard one level down releases whatever the source system adds there next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldSpec {
    /// A scalar, optionally indexed so it may be filtered and sorted on.
    Scalar {
        /// The accepted scalar type.
        ty: ScalarType,
        /// Whether the store maintains an index for this field.
        index: bool,
    },
    /// A nested object, with its own allow list.
    Object {
        /// The keys this object releases, each with its own specification.
        allow: BTreeMap<FieldName, FieldSpec>,
    },
    /// An array whose elements all satisfy one specification.
    Array {
        /// The specification every element must satisfy.
        items: Box<FieldSpec>,
    },
}

impl FieldSpec {
    /// An unindexed scalar of type `ty`.
    #[must_use]
    pub const fn scalar(ty: ScalarType) -> Self {
        Self::Scalar { ty, index: false }
    }

    /// An indexed scalar of type `ty`, usable in `filters` and `sort`.
    #[must_use]
    pub const fn indexed(ty: ScalarType) -> Self {
        Self::Scalar { ty, index: true }
    }

    /// A nested object releasing exactly `allow`.
    #[must_use]
    pub const fn object(allow: BTreeMap<FieldName, FieldSpec>) -> Self {
        Self::Object { allow }
    }

    /// An array of `items`.
    #[must_use]
    pub fn array(items: Self) -> Self {
        Self::Array {
            items: Box::new(items),
        }
    }

    /// The object form of `allow: [a, b, c]`: named keys, unconstrained scalar values.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError::FieldName`] if one of the names is not a valid field name, and
    /// [`SchemaError::ObjectWithoutAllowList`] if `names` is empty.
    pub fn allow_scalars<I, S>(names: I) -> Result<Self, SchemaError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut allow = BTreeMap::new();
        for name in names {
            allow.insert(FieldName::new(name)?, Self::scalar(ScalarType::Any));
        }
        if allow.is_empty() {
            return Err(SchemaError::ObjectWithoutAllowList {
                path: String::new(),
            });
        }
        Ok(Self::Object { allow })
    }

    /// Whether this field is an indexed scalar, and therefore usable in `filters` and `sort`.
    #[must_use]
    pub const fn is_indexed(&self) -> bool {
        matches!(self, Self::Scalar { index: true, .. })
    }

    /// The name used in drop reports.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Scalar { ty, .. } => ty.as_str(),
            Self::Object { .. } => "object",
            Self::Array { .. } => "array",
        }
    }

    /// Rejects wildcards anywhere in the tree.
    fn validate(&self, path: &str) -> Result<(), SchemaError> {
        match self {
            Self::Scalar { .. } => Ok(()),
            Self::Object { allow } => {
                if allow.is_empty() {
                    return Err(SchemaError::ObjectWithoutAllowList {
                        path: path.to_owned(),
                    });
                }
                for (name, spec) in allow {
                    spec.validate(&format!("{path}.{name}"))?;
                }
                Ok(())
            }
            Self::Array { items } => items.validate(&format!("{path}[]")),
        }
    }
}

/// Where a record's identifier sits inside the incoming payload.
///
/// Written `$.id` or `$.meta.id` in a schema file. The identifier is lifted out of the payload
/// before projection and stored as part of the record's key, so it does not need to be — and
/// usually is not — a released field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdPath(Vec<FieldName>);

impl IdPath {
    /// Builds a path from its segments.
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError::NoFields`] if `segments` is empty.
    pub fn new(segments: Vec<FieldName>) -> Result<Self, SchemaError> {
        if segments.is_empty() {
            return Err(SchemaError::NoFields);
        }
        Ok(Self(segments))
    }

    /// The path's segments, outermost first.
    #[must_use]
    pub fn segments(&self) -> &[FieldName] {
        &self.0
    }

    /// Follows the path into `payload`.
    #[must_use]
    pub fn resolve<'a>(&self, payload: &'a Value) -> Option<&'a Value> {
        let mut current = payload;
        for segment in &self.0 {
            current = current.as_object()?.get(segment.as_str())?;
        }
        Some(current)
    }

    /// Reads the record identifier out of `payload`.
    ///
    /// A string is taken as it stands; an integer is rendered in decimal, because source systems
    /// routinely key records by an auto-increment column and refusing that would push a string
    /// cast into every sender. Anything else — a float, a bool, an object, a missing key — is not
    /// an identifier.
    ///
    /// # Errors
    ///
    /// Returns [`IdError::Missing`] if the path does not resolve, [`IdError::NotAnIdentifier`] if
    /// the value is of an unusable type, and [`IdError::Invalid`] if the value does not satisfy
    /// [`RecordId`]'s grammar.
    pub fn extract(&self, payload: &Value) -> Result<RecordId, IdError> {
        let value = self.resolve(payload).ok_or_else(|| IdError::Missing {
            path: self.to_string(),
        })?;
        let raw = match value {
            Value::String(text) => text.clone(),
            Value::Number(number) if number.is_i64() || number.is_u64() => number.to_string(),
            other => {
                return Err(IdError::NotAnIdentifier {
                    path: self.to_string(),
                    found: kind_of(other),
                });
            }
        };
        RecordId::new(raw).map_err(|source| IdError::Invalid {
            path: self.to_string(),
            source,
        })
    }
}

impl fmt::Display for IdPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("$")?;
        for segment in &self.0 {
            write!(f, ".{segment}")?;
        }
        Ok(())
    }
}

impl FromStr for IdPath {
    type Err = SchemaError;

    /// Accepts `$.a.b` and the equivalent `a.b`.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.strip_prefix("$.").unwrap_or(value);
        let segments = trimmed
            .split('.')
            .map(FieldName::new)
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(segments)
    }
}

/// Why a payload yielded no usable record identifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    /// The path did not resolve to anything.
    #[error("payload has no value at {path}")]
    Missing {
        /// The path that was tried.
        path: String,
    },
    /// The value at the path cannot be an identifier.
    #[error("value at {path} is a {found}, which cannot be a record id")]
    NotAnIdentifier {
        /// The path that was tried.
        path: String,
        /// The JSON type found there.
        found: &'static str,
    },
    /// The value was of a usable type but not a valid identifier.
    #[error("value at {path} is not a usable record id")]
    Invalid {
        /// The path that was tried.
        path: String,
        /// The grammar violation.
        #[source]
        source: IdentError,
    },
}

/// Cache directives a collection's responses carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CacheSpec {
    /// `s-maxage` for shared caches, in seconds. A CDN in front is the scaling plan for a public
    /// read service, so this is a first-class part of the collection definition.
    pub shared_max_age: u32,
}

impl Default for CacheSpec {
    fn default() -> Self {
        Self {
            shared_max_age: DEFAULT_SHARED_MAX_AGE,
        }
    }
}

/// A key a collection may be sorted by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SortKey {
    /// The change-feed position. Always available, and the only stable order under concurrent
    /// ingest.
    Seq,
    /// An indexed scalar field.
    Field(FieldName),
}

impl fmt::Display for SortKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Seq => f.write_str("seq"),
            Self::Field(name) => f.write_str(name.as_str()),
        }
    }
}

/// A validated collection definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionSchema {
    collection: CollectionName,
    id: IdPath,
    fields: BTreeMap<FieldName, FieldSpec>,
    filters: Vec<FieldName>,
    sort: Vec<SortKey>,
    cache: CacheSpec,
}

impl CollectionSchema {
    /// Validates a collection definition.
    ///
    /// Beyond rejecting wildcards, this enforces the operational limits the read API depends on:
    /// a field may only be filtered or sorted on if it is an indexed scalar. A filter on an
    /// unindexed field is a sequential scan on a public endpoint, which is a denial-of-service
    /// vector rather than a slow query.
    ///
    /// # Errors
    ///
    /// Returns the [`SchemaError`] describing the first problem found.
    pub fn new(
        collection: CollectionName,
        id: IdPath,
        fields: BTreeMap<FieldName, FieldSpec>,
        filters: Vec<FieldName>,
        sort: Vec<SortKey>,
        cache: CacheSpec,
    ) -> Result<Self, SchemaError> {
        if fields.is_empty() {
            return Err(SchemaError::NoFields);
        }
        for (name, spec) in &fields {
            spec.validate(name.as_str())?;
        }

        let mut seen = Vec::new();
        for field in &filters {
            if seen.contains(&field) {
                return Err(SchemaError::Duplicate {
                    usage: "filters",
                    field: field.to_string(),
                });
            }
            seen.push(field);
            Self::require_indexed(&fields, "filters", field)?;
        }

        let mut seen_sort = Vec::new();
        for key in &sort {
            if seen_sort.contains(&key) {
                return Err(SchemaError::Duplicate {
                    usage: "sort",
                    field: key.to_string(),
                });
            }
            seen_sort.push(key);
            if let SortKey::Field(field) = key {
                Self::require_indexed(&fields, "sort", field)?;
            }
        }

        Ok(Self {
            collection,
            id,
            fields,
            filters,
            sort,
            cache,
        })
    }

    fn require_indexed(
        fields: &BTreeMap<FieldName, FieldSpec>,
        usage: &'static str,
        field: &FieldName,
    ) -> Result<(), SchemaError> {
        let spec = fields.get(field).ok_or_else(|| SchemaError::UnknownField {
            usage,
            field: field.to_string(),
        })?;
        if spec.is_indexed() {
            Ok(())
        } else {
            Err(SchemaError::FieldNotIndexed {
                usage,
                field: field.to_string(),
            })
        }
    }

    /// The collection this definition describes.
    #[must_use]
    pub const fn collection(&self) -> &CollectionName {
        &self.collection
    }

    /// Where the record identifier sits in an incoming payload.
    #[must_use]
    pub const fn id(&self) -> &IdPath {
        &self.id
    }

    /// The released fields, by name.
    #[must_use]
    pub const fn fields(&self) -> &BTreeMap<FieldName, FieldSpec> {
        &self.fields
    }

    /// The fields that may appear as query parameters. Anything else is a bad request, not a
    /// silently ignored parameter.
    #[must_use]
    pub fn filters(&self) -> &[FieldName] {
        &self.filters
    }

    /// The keys a listing may be sorted by.
    #[must_use]
    pub fn sort(&self) -> &[SortKey] {
        &self.sort
    }

    /// The cache directives responses carry.
    #[must_use]
    pub const fn cache(&self) -> CacheSpec {
        self.cache
    }
}

/// The JSON type of `value`, for error and report messages.
pub(crate) const fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn name(value: &str) -> FieldName {
        FieldName::new(value).expect("valid field name")
    }

    fn collection() -> CollectionName {
        CollectionName::new("catalog.product").expect("valid collection")
    }

    fn fields() -> BTreeMap<FieldName, FieldSpec> {
        BTreeMap::from([
            (name("sku"), FieldSpec::indexed(ScalarType::String)),
            (name("price"), FieldSpec::scalar(ScalarType::Number)),
        ])
    }

    #[test]
    fn field_names_refuse_dots_so_report_paths_stay_unambiguous() {
        assert!(FieldName::new("attributes.color").is_err());
        assert!(FieldName::new("attributes").is_ok());
    }

    #[test]
    fn object_without_allow_list_is_a_schema_error() {
        let fields = BTreeMap::from([(name("attributes"), FieldSpec::object(BTreeMap::new()))]);
        let error = CollectionSchema::new(
            collection(),
            "$.id".parse().expect("path"),
            fields,
            Vec::new(),
            Vec::new(),
            CacheSpec::default(),
        )
        .expect_err("must be rejected");
        assert_eq!(
            error,
            SchemaError::ObjectWithoutAllowList {
                path: "attributes".to_owned()
            }
        );
    }

    #[test]
    fn nested_wildcards_are_caught_at_every_level() {
        let inner = BTreeMap::from([(name("deep"), FieldSpec::object(BTreeMap::new()))]);
        let fields = BTreeMap::from([(
            name("attributes"),
            FieldSpec::array(FieldSpec::object(inner)),
        )]);
        let error = CollectionSchema::new(
            collection(),
            "$.id".parse().expect("path"),
            fields,
            Vec::new(),
            Vec::new(),
            CacheSpec::default(),
        )
        .expect_err("must be rejected");
        assert_eq!(
            error,
            SchemaError::ObjectWithoutAllowList {
                path: "attributes[].deep".to_owned()
            }
        );
    }

    #[test]
    fn filters_must_be_declared_and_indexed() {
        let unknown = CollectionSchema::new(
            collection(),
            "$.id".parse().expect("path"),
            fields(),
            vec![name("colour")],
            Vec::new(),
            CacheSpec::default(),
        )
        .expect_err("unknown filter");
        assert_eq!(
            unknown,
            SchemaError::UnknownField {
                usage: "filters",
                field: "colour".to_owned()
            }
        );

        let unindexed = CollectionSchema::new(
            collection(),
            "$.id".parse().expect("path"),
            fields(),
            vec![name("price")],
            Vec::new(),
            CacheSpec::default(),
        )
        .expect_err("unindexed filter");
        assert_eq!(
            unindexed,
            SchemaError::FieldNotIndexed {
                usage: "filters",
                field: "price".to_owned()
            }
        );
    }

    #[test]
    fn seq_is_always_sortable_and_duplicates_are_refused() {
        assert!(
            CollectionSchema::new(
                collection(),
                "$.id".parse().expect("path"),
                fields(),
                Vec::new(),
                vec![SortKey::Seq, SortKey::Field(name("sku"))],
                CacheSpec::default(),
            )
            .is_ok()
        );

        let duplicate = CollectionSchema::new(
            collection(),
            "$.id".parse().expect("path"),
            fields(),
            Vec::new(),
            vec![SortKey::Seq, SortKey::Seq],
            CacheSpec::default(),
        )
        .expect_err("duplicate sort key");
        assert_eq!(
            duplicate,
            SchemaError::Duplicate {
                usage: "sort",
                field: "seq".to_owned()
            }
        );
    }

    #[test]
    fn id_paths_round_trip_and_resolve() {
        let path: IdPath = "$.meta.id".parse().expect("path");
        assert_eq!(path.to_string(), "$.meta.id");
        let payload = json!({ "meta": { "id": "SW1" } });
        assert_eq!(path.extract(&payload).expect("id").as_str(), "SW1");
    }

    #[test]
    fn integer_ids_are_accepted_as_written() {
        let path: IdPath = "$.id".parse().expect("path");
        assert_eq!(
            path.extract(&json!({ "id": 42 })).expect("id").as_str(),
            "42"
        );
    }

    #[test]
    fn unusable_id_values_are_named_precisely() {
        let path: IdPath = "$.id".parse().expect("path");
        assert_eq!(
            path.extract(&json!({})),
            Err(IdError::Missing {
                path: "$.id".to_owned()
            })
        );
        assert_eq!(
            path.extract(&json!({ "id": 4.2 })),
            Err(IdError::NotAnIdentifier {
                path: "$.id".to_owned(),
                found: "number"
            })
        );
        assert!(matches!(
            path.extract(&json!({ "id": "a/b" })),
            Err(IdError::Invalid { .. })
        ));
    }

    #[test]
    fn allow_scalars_builds_the_yaml_list_form() {
        let spec = FieldSpec::allow_scalars(["color", "size"]).expect("spec");
        let FieldSpec::Object { allow } = spec else {
            panic!("expected an object spec")
        };
        assert_eq!(allow.len(), 2);
        assert_eq!(allow[&name("color")], FieldSpec::scalar(ScalarType::Any));
    }
}
