//! Canonicalisation and content hashing.
//!
//! This module is a wire contract, not an implementation detail. A sender in any language
//! computes the same hash for the same document, which is what makes ingest idempotent and drift
//! detection meaningful. Two rules follow from that:
//!
//! 1. The canonical form is [RFC 8785](https://www.rfc-editor.org/rfc/rfc8785) (JCS), never a
//!    hand-rolled one, and the digest is SHA-256 over those bytes.
//! 2. Neither may change. Changing the canonical form makes every collection report drift at
//!    once and forces a full re-export from every source system.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Length of a content hash in bytes.
pub const HASH_LEN: usize = 32;

/// A value could not be brought into canonical form.
#[derive(Debug, thiserror::Error)]
#[error("value cannot be canonicalised as RFC 8785 JSON")]
pub struct CanonicalError(#[from] serde_json::Error);

/// A hexadecimal string could not be read as a content hash.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HashParseError {
    /// The string did not have the length of a hex-encoded SHA-256 digest.
    #[error("a content hash is 64 hexadecimal characters, this one has {len}")]
    Length {
        /// Length of the offending string.
        len: usize,
    },
    /// The string was not valid hexadecimal.
    #[error("a content hash must be hexadecimal: {reason}")]
    NotHex {
        /// What the hex decoder objected to.
        reason: String,
    },
}

/// SHA-256 over the RFC 8785 canonical form of a document.
///
/// Rendered as lower-case hex wherever it crosses the wire — as an `ETag`, in a digest listing,
/// or in a drift report — so the hex form is as much part of the contract as the bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentHash([u8; HASH_LEN]);

impl ContentHash {
    /// The hash of the empty document, which is the payload a tombstone carries.
    ///
    /// Pinned as a constant rather than computed, so recording a deletion needs no fallible
    /// call. The test below asserts it against [`content_hash`], which is what makes the
    /// constant safe to trust.
    pub const EMPTY_DOCUMENT: Self = Self([
        0x44, 0x13, 0x6f, 0xa3, 0x55, 0xb3, 0x67, 0x8a, 0x11, 0x46, 0xad, 0x16, 0xf7, 0xe8, 0x64,
        0x9e, 0x94, 0xfb, 0x4f, 0xc2, 0x1f, 0xe7, 0x7e, 0x83, 0x10, 0xc0, 0x60, 0xf6, 0x1c, 0xaa,
        0xff, 0x8a,
    ]);

    /// Wraps raw digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; HASH_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw digest bytes, as stored.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; HASH_LEN] {
        &self.0
    }

    /// The lower-case hex rendering used on the wire.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl FromStr for ContentHash {
    type Err = HashParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != HASH_LEN * 2 {
            return Err(HashParseError::Length { len: value.len() });
        }
        let mut bytes = [0_u8; HASH_LEN];
        hex::decode_to_slice(value, &mut bytes).map_err(|error| HashParseError::NotHex {
            reason: error.to_string(),
        })?;
        Ok(Self(bytes))
    }
}

impl TryFrom<String> for ContentHash {
    type Error = HashParseError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ContentHash> for String {
    fn from(value: ContentHash) -> Self {
        value.to_hex()
    }
}

/// Serialises `value` into its RFC 8785 canonical form.
///
/// Object keys are ordered by their UTF-16 code units, numbers use the ECMAScript
/// representation, and strings carry the minimum escaping the standard allows.
///
/// # Errors
///
/// Returns [`CanonicalError`] if the value cannot be represented — in practice a number that is
/// not finite, which JSON has no form for.
pub fn canonicalize(value: &Value) -> Result<Vec<u8>, CanonicalError> {
    Ok(serde_jcs::to_vec(value)?)
}

/// The content hash of `value`: SHA-256 over [`canonicalize`].
///
/// This is the identity a payload is compared by. An ingest whose payload hashes to what is
/// already stored advances no `seq` and emits no change-feed entry.
///
/// # Errors
///
/// Returns [`CanonicalError`] if the value cannot be canonicalised.
pub fn content_hash(value: &Value) -> Result<ContentHash, CanonicalError> {
    let canonical = canonicalize(value)?;
    let digest = Sha256::digest(&canonical);
    let mut bytes = [0_u8; HASH_LEN];
    bytes.copy_from_slice(&digest);
    Ok(ContentHash::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn key_order_does_not_change_the_canonical_form() {
        let a = json!({ "sku": "SW1", "name": "Chair", "price": 49.9 });
        let b = json!({ "price": 49.9, "name": "Chair", "sku": "SW1" });
        assert_eq!(canonicalize(&a).expect("a"), canonicalize(&b).expect("b"));
        assert_eq!(content_hash(&a).expect("a"), content_hash(&b).expect("b"));
    }

    #[test]
    fn keys_sort_by_utf16_code_unit_not_by_byte() {
        // RFC 8785 section 3.2.3: sorting is defined over UTF-16 code units, which puts a
        // supplementary-plane key (surrogate pair, 0xD83D...) before U+FB33, even though its
        // UTF-8 bytes are larger.
        let value = json!({ "\u{fb33}": 1, "\u{1f600}": 2 });
        let canonical = String::from_utf8(canonicalize(&value).expect("canonical")).expect("utf8");
        let emoji_at = canonical.find('\u{1f600}').expect("emoji present");
        let hebrew_at = canonical.find('\u{fb33}').expect("hebrew present");
        assert!(emoji_at < hebrew_at, "got {canonical}");
    }

    #[test]
    fn numbers_use_the_ecmascript_form() {
        let canonical = canonicalize(&json!({ "n": 1e30 })).expect("canonical");
        assert_eq!(
            String::from_utf8(canonical).expect("utf8"),
            r#"{"n":1e+30}"#
        );
    }

    #[test]
    fn nested_objects_are_canonicalised_too() {
        let a = json!({ "outer": { "b": 1, "a": 2 } });
        let b = json!({ "outer": { "a": 2, "b": 1 } });
        assert_eq!(content_hash(&a).expect("a"), content_hash(&b).expect("b"));
    }

    #[test]
    fn array_order_is_significant() {
        let a = json!({ "images": ["a", "b"] });
        let b = json!({ "images": ["b", "a"] });
        assert_ne!(content_hash(&a).expect("a"), content_hash(&b).expect("b"));
    }

    #[test]
    fn hash_of_the_empty_document_is_the_sha256_of_two_braces() {
        // Pins the encoding as much as the digest: if canonicalisation ever emitted whitespace,
        // this vector would move.
        let hash = content_hash(&json!({})).expect("hash");
        assert_eq!(
            hash.to_hex(),
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
    }

    #[test]
    fn the_pinned_empty_document_constant_matches_the_computed_hash() {
        assert_eq!(
            content_hash(&json!({})).expect("hash"),
            ContentHash::EMPTY_DOCUMENT
        );
    }

    #[test]
    fn hex_round_trips() {
        let hash = content_hash(&json!({ "a": 1 })).expect("hash");
        let parsed: ContentHash = hash.to_hex().parse().expect("parse");
        assert_eq!(hash, parsed);
    }

    #[test]
    fn hex_parsing_rejects_the_obvious_mistakes() {
        assert_eq!(
            "abc".parse::<ContentHash>(),
            Err(HashParseError::Length { len: 3 })
        );
        let not_hex = "z".repeat(64);
        assert!(matches!(
            not_hex.parse::<ContentHash>(),
            Err(HashParseError::NotHex { .. })
        ));
    }

    #[test]
    fn serde_uses_the_hex_form() {
        let hash = content_hash(&json!({ "a": 1 })).expect("hash");
        let encoded = serde_json::to_string(&hash).expect("encode");
        assert_eq!(encoded, format!("\"{}\"", hash.to_hex()));
        let decoded: ContentHash = serde_json::from_str(&encoded).expect("decode");
        assert_eq!(hash, decoded);
    }
}
