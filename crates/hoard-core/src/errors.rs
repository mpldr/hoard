use thiserror::Error;

#[derive(Debug, Error)]
pub enum HoardError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("unauthorized")]
    Unauthorized,

    #[error("forbidden")]
    Forbidden,

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("storage quota exceeded")]
    QuotaExceeded,

    #[error("payload too large")]
    PayloadTooLarge,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

pub type Result<T, E = HoardError> = std::result::Result<T, E>;
