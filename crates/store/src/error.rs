//! Failures the store can report.

use hydrant_core::hash::CanonicalError;

/// Something went wrong while reading or writing records.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database rejected the statement, or was unreachable.
    #[error("database error")]
    Database(#[from] sqlx::Error),

    /// Applying the migrations failed.
    #[error("migrations could not be applied")]
    Migration(#[from] sqlx::migrate::MigrateError),

    /// A payload could not be brought into canonical form, so it has no content hash and cannot
    /// be stored.
    #[error("payload cannot be canonicalised")]
    Canonical(#[from] CanonicalError),

    /// A stored row could not be read back into a record. This is corruption or a schema drift
    /// between the migrations and the code, never bad input.
    #[error("stored row is not a usable record: {reason}")]
    Corrupt {
        /// What did not add up.
        reason: String,
    },
}
