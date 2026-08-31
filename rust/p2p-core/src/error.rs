//! Error type shared by every layer of the node.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization: {0}")]
    Codec(String),

    #[error("crypto: {0}")]
    Crypto(String),

    /// The peer presented a certificate we will not accept. This is the one
    /// error that must always result in the connection being dropped.
    #[error("rejected peer: {0}")]
    Rejected(String),

    #[error("not enrolled in an organisation yet")]
    NotEnrolled,

    #[error("schema: {0}")]
    Schema(String),

    #[error("network: {0}")]
    Network(String),

    #[error("node is not running")]
    NotRunning,

    #[error("{0}")]
    Other(String),
}

impl From<postcard::Error> for Error {
    fn from(e: postcard::Error) -> Self {
        Error::Codec(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Codec(e.to_string())
    }
}

impl From<libp2p::identity::DecodingError> for Error {
    fn from(e: libp2p::identity::DecodingError) -> Self {
        Error::Crypto(e.to_string())
    }
}

impl From<libp2p::identity::SigningError> for Error {
    fn from(e: libp2p::identity::SigningError) -> Self {
        Error::Crypto(e.to_string())
    }
}

impl From<libp2p::multiaddr::Error> for Error {
    fn from(e: libp2p::multiaddr::Error) -> Self {
        Error::Network(e.to_string())
    }
}
