use thiserror::Error;

/// Core errors for S4Drive operations
#[derive(Error, Debug)]
pub enum CoreError {
    #[error("S3 error: {0}")]
    S3(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Network error: {0}")]
    Network(String),

    #[error("File system error: {0}")]
    FileSystem(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Authentication failed: {0}")]
    Auth(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<anyhow::Error> for CoreError {
    fn from(e: anyhow::Error) -> Self {
        CoreError::Internal(e.to_string())
    }
}

/// Result type alias for S4Drive operations
pub type CoreResult<T> = Result<T, CoreError>;
