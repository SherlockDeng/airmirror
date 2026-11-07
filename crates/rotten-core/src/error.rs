use thiserror::Error;

pub type Result<T> = std::result::Result<T, RottenError>;

#[derive(Debug, Error)]
pub enum RottenError {
    #[error("discovery error: {0}")]
    Discovery(String),

    #[error("pairing error: {0}")]
    Pairing(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("video error: {0}")]
    Video(String),

    #[error("capture error: {0}")]
    Capture(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("device not found: {0}")]
    DeviceNotFound(String),

    #[error("credentials not found for device {device_id}")]
    CredentialsNotFound { device_id: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
