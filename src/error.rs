use thiserror::Error;

#[derive(Error, Debug)]
pub enum FederationError {
    #[error("config error: {0}")]
    Config(String),

    #[error("leaf server '{alias}' is unreachable: {source}")]
    LeafUnreachable {
        alias: String,
        source: reqwest::Error,
    },

    #[error("leaf server '{alias}' returned error: {message}")]
    LeafError { alias: String, message: String },

    #[error("unknown leaf server: {0}")]
    UnknownLeaf(String),

    #[error("tool not found: {0}")]
    ToolNotFound(String),

    #[error("invalid tool name format: {0}")]
    InvalidToolName(String),

    #[error("leaf server '{0}' is unhealthy")]
    LeafUnhealthy(String),
}
