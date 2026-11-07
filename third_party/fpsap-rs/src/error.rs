//! Shared error type for the vendored FairPlay SAP primitives.

/// Errors produced by the protocol layer.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// I/O or framing failure on the transport.
    #[error("transport: {0}")]
    Transport(String),
    /// A receiver responded with a non-2xx status.
    #[error("http {status}: body {body:?}")]
    HttpStatus { status: u16, body: Vec<u8> },
    /// Malformed or unexpected protocol data.
    #[error("protocol: {0}")]
    Protocol(String),
    /// Cryptographic operation failed (bad signature, auth tag, ...).
    #[error("crypto: {0}")]
    Crypto(String),
}

impl Error {
    /// Convenience: maps an `io::Error` into [`Error::Transport`].
    pub fn from_io(context: &str, e: std::io::Error) -> Error {
        Error::Transport(format!("{context}: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
