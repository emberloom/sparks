use thiserror::Error;

#[derive(Debug, Error)]
pub enum SparksError {
    #[error("LLM error: {0}")]
    Llm(String),

    #[error("Docker error: {0}")]
    Docker(String),

    #[error("Database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("Tool error: {0}")]
    Tool(String),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Operation cancelled by user")]
    Cancelled,

    #[error("Step limit exceeded ({0} steps)")]
    StepLimitExceeded(usize),

    #[error("Timeout after {0}s")]
    Timeout(u64),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl From<bollard::errors::Error> for SparksError {
    fn from(e: bollard::errors::Error) -> Self {
        SparksError::Docker(e.to_string())
    }
}

impl From<reqwest::Error> for SparksError {
    fn from(e: reqwest::Error) -> Self {
        SparksError::Llm(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SparksError>;
