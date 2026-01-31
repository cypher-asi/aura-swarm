//! Error types for the storage layer.

use thiserror::Error;

/// A result type using `StoreError`.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Errors that can occur during storage operations.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The requested record was not found.
    #[error("record not found")]
    NotFound,

    /// A database error occurred.
    #[error("database error: {0}")]
    Database(String),

    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    Serialization(String),
}
