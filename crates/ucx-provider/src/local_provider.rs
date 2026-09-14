use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use ucx_protocol::{
    Allocation, BillingCurrency, BillingRecord, ComputeProvider, ComputeReceipt,
    Job, JobId, JobStatus, LineItem, ProviderCapability, ResourceUsage, UcxError,
    VerificationProof,
};

use crate::discovery::discover_capability;

/// A native provider backed by the local machine.
///
/// The actual job runner is pluggable — the default stub marks jobs completed
/// immediately (useful for testing the broker wire protocol).  A real
/// implementation would spawn a container via the OCI runtime.
pub struct LocalProvider {
    capability: ProviderCapability,
    jobs:       Mutex<HashMap<JobId, JobRecord>>,
    runner:     Arc<dyn JobRunner>,
}

impl LocalProvider {
    pub fn new(provider_id: impl Into<String>) -> Self {
        let id = provider_id.into();
        Self {
            capability: discover_capability(&id),
            jobs:       Mutex::new(HashMap::new()),
            runner:     Arc::new(StubRunner),
        }
    }

    pub fn with_runner(mut self, runner: Arc<dyn JobRunner>) -> Self {
        self.runner = runner;
        self
    }
}

/// Pluggable job execution backend.
pub trait JobRunner: Send + Sync {
    fn run(&self, job: &Job) -> Result<RunResult, UcxError>;
}

pub struct RunResult {
    pub artifact_hash:  Option<String>,
    pub execution_hash: Option<String>,
    pub gpu_seconds:    f64,
    pub cpu_seconds:    f64,
}

/// Stub runner: instantly marks the job as succeeded with zero resource usage.
struct StubRunner;
impl JobRunner for StubRunner {
    fn run(&self, _job: &Job) -> Result<RunResult, UcxError> {
        Ok(RunResult {
            artifact_hash:  None,
            execution_hash: None,
            gpu_seconds:    0.0,
            cpu_seconds:    0.0,
        })
    }
}

struct JobRecord {
    job:     Job,
    status:  JobStatus,
    receipt: Option<ComputeReceipt>,
}

impl ComputeProvider for LocalProvider {
    fn id(&self)         -> &str             { &self.capability.provider_id }
    fn capability(&self) -> &ProviderCapability { &self.capability }

    fn can_accept(&self, job: &Job) -> bool {
        let req = &job.requirements;

        // GPU check
        if let Some(vram_needed) = req.vram_gb {
            match &self.capability.gpu {
                None => return false,
                Some(gpu) => {
                    if gpu.vram_gb < vram_needed { return false; }
                    if req.cuda && !gpu.cuda     { return false; }
                }
            }
        }

        // RAM check
        if let Some(ram_needed) = req.ram_gb {
            if self.capability.ram_gb < ram_needed { return false; }
        }

        // CPU check
        if let Some(cores_needed) = req.cpu_cores {
            if self.capability.cpu.cores < cores_needed { return false; }
        }

        // Policy check — deny if any deny keyword matches workload type
        let workload_label = format!("{:?}", job.workload).to_lowercase();
        for denied in &self.capability.policy_deny {
            if workload_label.contains(denied.as_str()) { return false; }
        }

        true
    }

    fn submit(&self, job: Job) -> Result<Allocation, UcxError> {
        let job_id = job.id;

        // Run synchronously in this stub — async runtime is ucx-broker's concern.
        let result = self.runner.run(&job)?;

        let receipt = build_receipt(job_id, self.id(), result);
        let allocation = Allocation {
            job_id,
            provider_id: self.id().to_string(),
            offer:       ucx_protocol::Offer {
                job_id,
                provider_id:  self.id().to_string(),
                price_cents:  0,
                eta_secs:     0,
                expires_at:   Utc::now(),
                is_external:  false,
            },
            allocated_at: Utc::now(),
            status:       JobStatus::Completed,
        };

        self.jobs.lock().unwrap().insert(job_id, JobRecord {
            job,
            status: JobStatus::Completed,
            receipt: Some(receipt),
        });

        Ok(allocation)
    }

    fn status(&self, job_id: JobId) -> Result<JobStatus, UcxError> {
        self.jobs.lock().unwrap()
            .get(&job_id)
            .map(|r| r.status.clone())
            .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })
    }

    fn receipt(&self, job_id: JobId) -> Result<ComputeReceipt, UcxError> {
        self.jobs.lock().unwrap()
            .get(&job_id)
            .and_then(|r| r.receipt.clone())
            .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })
    }

    fn cancel(&self, job_id: JobId) -> Result<(), UcxError> {
        if let Some(record) = self.jobs.lock().unwrap().get_mut(&job_id) {
            if record.status == JobStatus::Running || record.status == JobStatus::Pending {
                record.status = JobStatus::Cancelled;
                return Ok(());
            }
        }
        Err(UcxError::JobNotFound { job_id: job_id.to_string() })
    }
}

fn build_receipt(job_id: JobId, provider_id: &str, result: RunResult) -> ComputeReceipt {
    ComputeReceipt {
        job_id,
        provider_id: provider_id.to_string(),
        completed_at: Utc::now(),
        resources: ResourceUsage {
            gpu_seconds:    result.gpu_seconds,
            cpu_seconds:    result.cpu_seconds,
            ram_gb_seconds: 0.0,
            storage_gb:     0.0,
            egress_gb:      0.0,
        },
        billing: BillingRecord {
            amount_cents: 0,
            currency:     BillingCurrency::Usd,
            line_items:   vec![LineItem { label: "compute".into(), cents: 0 }],
        },
        verification: VerificationProof {
            artifact_hash:       result.artifact_hash,
            runtime_attestation: None,
            execution_hash:      result.execution_hash,
        },
        zangbeto_anchor: None,
    }
}
