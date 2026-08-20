//! Ingest credentials.
//!
//! A token is a random secret the sender presents; what the service keeps is HMAC-SHA256 of it,
//! keyed by an application secret. Three properties follow, and all three are the point:
//!
//! - A database dump does not yield working credentials, because the secret is not in the database.
//! - Authentication is a primary-key lookup on a MAC, so application code never compares a secret
//!   byte by byte and there is no comparison to get wrong.
//! - A token cannot be recovered for display. It is shown once, when it is minted.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// Prefix every token carries, so a leaked one is recognisable to a secret scanner.
pub const TOKEN_PREFIX: &str = "hyd_";

/// Bytes of entropy in a token.
pub const TOKEN_BYTES: usize = 32;

/// Length of a token hash in bytes.
pub const HASH_LEN: usize = 32;

/// Why a token could not be produced.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// The operating system would not provide randomness.
    #[error("no randomness available to mint a token")]
    Randomness(#[from] getrandom::Error),

    /// The application secret was not usable as an HMAC key.
    #[error("the application secret cannot be used as an HMAC key")]
    Secret,
}

/// A token in plaintext.
///
/// Exists only between minting and being shown to the operator, or between arriving in a request
/// and being hashed. It deliberately has no `Display`, and its `Debug` says nothing: a token in a
/// log is a credential in a log.
#[derive(Clone)]
pub struct Token(String);

impl Token {
    /// Mints a token from operating-system entropy.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Randomness`] if the OS random source is unavailable.
    pub fn generate() -> Result<Self, TokenError> {
        let mut bytes = [0_u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(format!("{TOKEN_PREFIX}{}", hex::encode(bytes))))
    }

    /// Wraps a token that arrived from a sender.
    #[must_use]
    pub fn from_presented(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The plaintext, for showing to the operator exactly once.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The stored form: HMAC-SHA256 of the token, keyed by the application secret.
    ///
    /// # Errors
    ///
    /// Returns [`TokenError::Secret`] if the secret cannot be used as an HMAC key. HMAC accepts any
    /// key length, so this does not happen in practice - but the authentication path should fail
    /// with an error rather than on an assumption.
    pub fn hash(&self, secret: &[u8]) -> Result<TokenHash, TokenError> {
        let mut mac =
            <Hmac<Sha256> as KeyInit>::new_from_slice(secret).map_err(|_| TokenError::Secret)?;
        mac.update(self.0.as_bytes());
        let digest = mac.finalize().into_bytes();
        let mut hash = [0_u8; HASH_LEN];
        hash.copy_from_slice(&digest);
        Ok(TokenHash(hash))
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(redacted)")
    }
}

/// The stored form of a token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenHash([u8; HASH_LEN]);

impl TokenHash {
    /// The raw MAC, as stored.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; HASH_LEN] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_token_is_prefixed_and_long_enough() {
        let token = Token::generate().expect("randomness");
        assert!(token.expose().starts_with(TOKEN_PREFIX));
        assert_eq!(token.expose().len(), TOKEN_PREFIX.len() + TOKEN_BYTES * 2);
    }

    #[test]
    fn two_tokens_differ() {
        let first = Token::generate().expect("randomness");
        let second = Token::generate().expect("randomness");
        assert_ne!(first.expose(), second.expose());
    }

    fn hash_of(token: &str, secret: &[u8]) -> TokenHash {
        Token::from_presented(token)
            .hash(secret)
            .expect("usable secret")
    }

    #[test]
    fn hashing_is_deterministic_under_one_secret() {
        assert_eq!(hash_of("hyd_abc", b"secret"), hash_of("hyd_abc", b"secret"));
    }

    #[test]
    fn a_different_secret_yields_a_different_hash() {
        // This is what makes a stolen database dump useless on its own.
        assert_ne!(
            hash_of("hyd_abc", b"secret"),
            hash_of("hyd_abc", b"other secret")
        );
    }

    #[test]
    fn a_different_token_yields_a_different_hash() {
        assert_ne!(hash_of("hyd_abc", b"secret"), hash_of("hyd_abd", b"secret"));
    }

    #[test]
    fn debug_does_not_leak_the_token() {
        let token = Token::from_presented("hyd_supersecret");
        assert_eq!(format!("{token:?}"), "Token(redacted)");
        assert!(!format!("{token:?}").contains("supersecret"));
    }
}
