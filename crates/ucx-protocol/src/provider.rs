use crate::{capability::ProviderCapability, error::UcxError, job::{Allocation, Job, JobId, JobStatus}, receipt::ComputeReceipt};

pub type ProviderId = String;

/// The single interface every compute source implements — native or external.
///
/// Implementations must be `Send + Sync` so the broker can hold them in an
/// `Arc<dyn ComputeProvider>`.  Async is intentionally NOT here; async is an
/// implementation detail of ucx-broker, not the protocol.
pub trait ComputeProvider: Send + Sync {
    fn id(&self)           -> &str;
    fn capability(&self)   -> &ProviderCapability;

    /// Returns true if this provider CAN accept the job (capability check only,
    /// no side effects).
    fn can_accept(&self, job: &Job) -> bool;

    /// Submit a job; returns an Allocation if accepted.
    fn submit(&self, job: Job) -> Result<Allocation, UcxError>;

    /// Poll current status of a running job.
    fn status(&self, job_id: JobId) -> Result<JobStatus, UcxError>;

    /// Retrieve the completed receipt.  Only valid when status == Completed.
    fn receipt(&self, job_id: JobId) -> Result<ComputeReceipt, UcxError>;

    /// Cancel a running or pending job.  Best-effort; may not always succeed.
    fn cancel(&self, job_id: JobId) -> Result<(), UcxError>;
}

/// Extension trait for external networks (Akash, Vast, GPU.ai, etc.).
/// Separates native providers from adapters at the type level.
pub trait ExternalProviderAdapter: ComputeProvider {
    /// The external network name, e.g. "gpu.ai", "akash", "vast".
    fn network_name(&self) -> &str;

    /// True if the external network is currently reachable / healthy.
    fn is_available(&self) -> bool;

    /// Translate a UCX Job into the external network's native job format.
    /// Returns an opaque bytes/JSON value.
    fn translate_job(&self, job: &Job) -> Result<serde_json::Value, UcxError>;

    /// Translate the external network's native receipt/result into a UCX ComputeReceipt.
    fn translate_receipt(&self, raw: serde_json::Value) -> Result<ComputeReceipt, UcxError>;
}
