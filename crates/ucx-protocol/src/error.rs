use thiserror::Error;

#[derive(Debug, Error)]
pub enum UcxError {
    #[error("insufficient capacity: {reason}")]
    InsufficientCapacity { reason: String },

    #[error("job not found: {job_id}")]
    JobNotFound { job_id: String },

    #[error("provider not found: {provider_id}")]
    ProviderNotFound { provider_id: String },

    #[error("provider rejected job: {reason}")]
    ProviderRejected { reason: String },

    #[error("job execution failed: {reason}")]
    ExecutionFailed { reason: String },

    #[error("policy violation: {policy} — {reason}")]
    PolicyViolation { policy: String, reason: String },

    #[error("billing error: {reason}")]
    Billing { reason: String },

    #[error("receipt verification failed: {reason}")]
    ReceiptVerification { reason: String },

    #[error("adapter error ({adapter}): {reason}")]
    Adapter { adapter: String, reason: String },

    #[error("serialization error: {0}")]
    Serialization(String),
}
