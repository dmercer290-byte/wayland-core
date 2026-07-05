//! The engine's public error type.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("provider error: {0}")]
    Provider(String),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("provider returned unexpected payload: {0}")]
    BadResponse(String),

    #[error("tool '{name}' failed: {message}")]
    Tool { name: String, message: String },

    #[error("unknown tool '{0}'")]
    UnknownTool(String),

    #[error("agent exceeded the maximum of {0} turns")]
    MaxTurns(usize),

    #[error("configuration error: {0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, EngineError>;
