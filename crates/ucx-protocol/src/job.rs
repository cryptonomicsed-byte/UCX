use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type JobId = Uuid;

/// Universal Workload Descriptor — what a consumer submits to the broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id:           JobId,
    pub submitted_at: DateTime<Utc>,
    pub submitter_id: String,        // agent or user identity

    pub workload:     WorkloadType,
    pub requirements: WorkloadRequirements,
    pub constraints:  ComputeConstraints,

    /// Opaque payload for the provider runtime (image ref, env vars, cmd, etc.)
    pub runtime_spec: serde_json::Value,
}

impl Job {
    pub fn new(
        submitter_id: impl Into<String>,
        workload: WorkloadType,
        requirements: WorkloadRequirements,
        constraints: ComputeConstraints,
        runtime_spec: serde_json::Value,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            submitted_at: Utc::now(),
            submitter_id: submitter_id.into(),
            workload,
            requirements,
            constraints,
            runtime_spec,
        }
    }
}

/// Broad workload classification drives provider selection heuristics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum WorkloadType {
    Inference,
    Training,
    Rendering,
    Simulation,
    CiCd,
    Generic,
}

/// What the job needs from the hardware.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadRequirements {
    pub vram_gb:    Option<f64>,
    pub ram_gb:     Option<f64>,
    pub cpu_cores:  Option<u32>,
    pub gpu_count:  Option<u8>,
    pub fp16:       bool,
    pub bf16:       bool,
    pub cuda:       bool,
    pub min_tier:   Option<crate::capability::ProviderTier>,
}

/// Consumer-side policy constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeConstraints {
    /// Maximum total price in USD cents. None = no limit.
    pub max_price_cents:    Option<u64>,
    /// Maximum time-to-start in seconds. None = no limit.
    pub max_queue_secs:     Option<u64>,
    /// Required trust level. Defaults to Standard.
    pub privacy:            crate::capability::TrustLevel,
    /// ISO 3166-1 alpha-2 region allowlist. Empty = any.
    pub regions:            Vec<String>,
    /// Whether the broker may overflow to external providers.
    pub allow_external:     bool,
}

impl Default for ComputeConstraints {
    fn default() -> Self {
        Self {
            max_price_cents: None,
            max_queue_secs:  None,
            privacy:         crate::capability::TrustLevel::Standard,
            regions:         vec![],
            allow_external:  true,
        }
    }
}

/// A provider's response to a Job — a priced bid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offer {
    pub job_id:       JobId,
    pub provider_id:  String,
    pub price_cents:  u64,       // total estimated cost
    pub eta_secs:     u64,       // estimated time to start
    pub expires_at:   DateTime<Utc>,
    pub is_external:  bool,      // true if routed to an external network
}

/// Confirmed match: a Job has been allocated to a Provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Allocation {
    pub job_id:       JobId,
    pub provider_id:  String,
    pub offer:        Offer,
    pub allocated_at: DateTime<Utc>,
    pub status:       JobStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum JobStatus {
    /// Submitted, awaiting broker match.
    Pending,
    /// Matched to a provider, not yet started.
    Allocated,
    /// Provider confirmed start.
    Running,
    /// Completed successfully — receipt available.
    Completed,
    /// Provider reported failure.
    Failed,
    /// Cancelled by submitter or timed out.
    Cancelled,
}
