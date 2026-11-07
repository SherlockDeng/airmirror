use thiserror::Error;

#[derive(Debug, Error)]
pub enum AirMirrorError {
    #[error("network error: {0}")]
    Network(#[from] std::io::Error),

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("operation is not supported: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, AirMirrorError>;
