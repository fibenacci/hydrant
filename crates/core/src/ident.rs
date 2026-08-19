//! Identifiers that address a record: its source, its collection, and its own id.
//!
//! All three appear verbatim in public URLs — `/v1/{source}/{collection}/{id}` — so they are
//! validated on construction rather than at the HTTP edge. An identifier that cannot be
//! addressed is not an identifier, and finding that out at request time would mean the store
//! already holds a record nobody can read.
//!
//! `source` is a partitioning label, never a security boundary: everything in the store is
//! public, and no isolation may be built on this type.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Maximum length of a source or collection name, in bytes.
pub const MAX_NAME_LEN: usize = 128;

/// Maximum length of a record identifier, in bytes.
pub const MAX_ID_LEN: usize = 512;

/// Why a string cannot be used as an identifier.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentError {
    /// The value was empty.
    #[error("a {kind} must not be empty")]
    Empty {
        /// The identifier kind that rejected the value.
        kind: &'static str,
    },
    /// The value was longer than the identifier permits.
    #[error("a {kind} may be at most {max} bytes, this one is {len}")]
    TooLong {
        /// The identifier kind that rejected the value.
        kind: &'static str,
        /// Length of the offending value, in bytes.
        len: usize,
        /// Maximum length for this kind.
        max: usize,
    },
    /// The value contained a character this identifier does not permit.
    #[error("a {kind} must not contain {ch:?}")]
    Char {
        /// The identifier kind that rejected the value.
        kind: &'static str,
        /// The first offending character.
        ch: char,
    },
    /// The value started or ended with a separator.
    #[error("a {kind} must start and end with a lower-case letter or a digit")]
    Boundary {
        /// The identifier kind that rejected the value.
        kind: &'static str,
    },
}

/// Source and collection names share one grammar: lower-case ASCII letters, digits, and the
/// separators `.`, `-` and `_`, starting and ending on a letter or digit.
///
/// Lower case is enforced rather than normalised. Accepting `Catalog.Product` and storing it as
/// `catalog.product` would make two spellings address one collection, and the sender would have
/// no way to tell which one the store believes in.
fn validate_name(kind: &'static str, value: &str) -> Result<(), IdentError> {
    if value.is_empty() {
        return Err(IdentError::Empty { kind });
    }
    if value.len() > MAX_NAME_LEN {
        return Err(IdentError::TooLong {
            kind,
            len: value.len(),
            max: MAX_NAME_LEN,
        });
    }
    if let Some(ch) = value.chars().find(|ch| {
        !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '-' | '_'))
    }) {
        return Err(IdentError::Char { kind, ch });
    }
    let alnum = |ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit();
    if !value.starts_with(alnum) || !value.ends_with(alnum) {
        return Err(IdentError::Boundary { kind });
    }
    Ok(())
}

/// Record ids come from the source system, so the grammar is deliberately wide: any printable
/// UTF-8 that survives a URL path segment. Control characters and `/` are refused because they
/// would change the shape of the request, and surrounding whitespace because two ids that look
/// identical in a log must not be different records.
fn validate_id(kind: &'static str, value: &str) -> Result<(), IdentError> {
    if value.is_empty() {
        return Err(IdentError::Empty { kind });
    }
    if value.len() > MAX_ID_LEN {
        return Err(IdentError::TooLong {
            kind,
            len: value.len(),
            max: MAX_ID_LEN,
        });
    }
    if let Some(ch) = value
        .chars()
        .find(|ch| ch.is_control() || matches!(ch, '/' | '?' | '#'))
    {
        return Err(IdentError::Char { kind, ch });
    }
    if value.trim() != value {
        return Err(IdentError::Boundary { kind });
    }
    Ok(())
}

/// Declares a validated string newtype.
///
/// The five conversions below are the whole reason this is a macro: every identifier needs the
/// same `FromStr`, `TryFrom<String>`, `Display` and serde plumbing, and three hand-written
/// copies of it would drift.
macro_rules! ident_type {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $validate:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            /// The kind name used in error messages.
            pub const KIND: &'static str = $kind;

            /// Validates `value` and wraps it.
            ///
            /// # Errors
            ///
            /// Returns [`IdentError`] if `value` does not satisfy the grammar documented on this
            /// type.
            pub fn new(value: impl Into<String>) -> Result<Self, IdentError> {
                let value = value.into();
                $validate($kind, &value)?;
                Ok(Self(value))
            }

            /// Borrows the identifier as a string slice.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdentError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdentError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

ident_type!(
    /// One source-system instance, such as `orkestra-prod` or `sap-stage`.
    ///
    /// Grammar: lower-case ASCII letters, digits, `.`, `-`, `_`; first and last character a
    /// letter or a digit; at most [`MAX_NAME_LEN`] bytes.
    SourceName,
    "source name",
    validate_name
);

ident_type!(
    /// A logical record type, such as `catalog.product`.
    ///
    /// Same grammar as [`SourceName`]. The dot carries no meaning to the service — it is a
    /// naming convention of the operator, not a hierarchy the store understands.
    CollectionName,
    "collection name",
    validate_name
);

ident_type!(
    /// A record's stable identifier, as assigned by the source system.
    ///
    /// Grammar: printable UTF-8 without control characters, `/`, `?` or `#`, no surrounding
    /// whitespace, at most [`MAX_ID_LEN`] bytes. The service never generates one: identifiers
    /// come from the sender, which is what keeps re-ingesting a record idempotent.
    RecordId,
    "record id",
    validate_id
);

/// A record's position in the global change feed.
///
/// Monotonic and never reused, so `?since=<seq>` is an index scan and a consumer can replicate
/// rather than poll. Assigned by the store, never by a sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(u64);

impl Seq {
    /// The position before any record has been written.
    pub const ZERO: Self = Self(0);

    /// Wraps a raw sequence number.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The raw sequence number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Everything needed to address exactly one record.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RecordKey {
    /// The source system instance the record was ingested from.
    pub source: SourceName,
    /// The collection the record belongs to.
    pub collection: CollectionName,
    /// The record's identifier within that source and collection.
    pub id: RecordId,
}

impl RecordKey {
    /// Assembles a key from its three parts.
    #[must_use]
    pub const fn new(source: SourceName, collection: CollectionName, id: RecordId) -> Self {
        Self {
            source,
            collection,
            id,
        }
    }
}

impl fmt::Display for RecordKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.source, self.collection, self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_documented_grammar() {
        assert!(SourceName::new("orkestra-prod").is_ok());
        assert!(CollectionName::new("catalog.product").is_ok());
        assert!(RecordId::new("SW10001").is_ok());
        assert!(RecordId::new("018f3e1c-9b7d-7a3e-8c21-0d9a5f6b2e11").is_ok());
    }

    #[test]
    fn rejects_upper_case_rather_than_normalising_it() {
        assert_eq!(
            CollectionName::new("Catalog.Product"),
            Err(IdentError::Char {
                kind: CollectionName::KIND,
                ch: 'C'
            })
        );
    }

    #[test]
    fn rejects_separator_boundaries() {
        assert_eq!(
            SourceName::new(".prod"),
            Err(IdentError::Boundary {
                kind: SourceName::KIND
            })
        );
        assert_eq!(
            SourceName::new("prod-"),
            Err(IdentError::Boundary {
                kind: SourceName::KIND
            })
        );
    }

    #[test]
    fn rejects_ids_that_would_change_the_request_shape() {
        for bad in ["a/b", "a?b", "a#b", "a\nb"] {
            assert!(RecordId::new(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn rejects_surrounding_whitespace_in_ids() {
        assert_eq!(
            RecordId::new(" SW10001"),
            Err(IdentError::Boundary {
                kind: RecordId::KIND
            })
        );
    }

    #[test]
    fn rejects_empty_and_oversized_values() {
        assert_eq!(
            SourceName::new(""),
            Err(IdentError::Empty {
                kind: SourceName::KIND
            })
        );
        let long = "a".repeat(MAX_NAME_LEN + 1);
        assert_eq!(
            SourceName::new(long),
            Err(IdentError::TooLong {
                kind: SourceName::KIND,
                len: MAX_NAME_LEN + 1,
                max: MAX_NAME_LEN,
            })
        );
    }

    #[test]
    fn deserialisation_validates() {
        let ok: Result<CollectionName, _> = serde_json::from_str("\"catalog.product\"");
        assert!(ok.is_ok());
        let bad: Result<CollectionName, _> = serde_json::from_str("\"Catalog\"");
        assert!(bad.is_err());
    }

    #[test]
    fn key_renders_as_its_url_path() {
        let key = RecordKey::new(
            SourceName::new("sap-stage").expect("valid source"),
            CollectionName::new("catalog.product").expect("valid collection"),
            RecordId::new("SW10001").expect("valid id"),
        );
        assert_eq!(key.to_string(), "sap-stage/catalog.product/SW10001");
    }
}
