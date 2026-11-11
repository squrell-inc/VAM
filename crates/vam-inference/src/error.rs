use thiserror::Error;

#[derive(Debug, Error)]
pub enum VamInferenceError {
    #[error("Model not found in catalog: {model_name}")]
    ModelNotFound { model_name: String },

    #[error("Provider '{provider}' error: {details}")]
    ProviderError { provider: String, details: String },

    #[error("Financial/token budget exceeded: limit {limit:?}, current {current:?}")]
    BudgetExceeded { limit: String, current: String },

    #[error("Invalid request: {reason}")]
    InvalidRequest { reason: String },

    #[error("Connection closed prematurely")]
    ConnectionClosed,

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Internal system error: {0}")]
    Internal(String),

    #[error("Secret not found: {key}")]
    SecretNotFound { key: String },

    #[error("Rate limited by provider '{provider}'")]
    RateLimited { provider: String },
}
