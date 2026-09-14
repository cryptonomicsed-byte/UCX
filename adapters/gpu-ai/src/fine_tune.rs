// GPU.ai fine-tuning adapter.
//
// Maps:  UCX Job{workload=Training} → POST /v1/files + POST /v1/fine_tuning/jobs
//        GPU.ai job result           → UCX ComputeReceipt
//
// Note: GPU.ai does not expose a /v1/files endpoint (returns 404).
// Training data must be provided via a public URL or inline base64.
// This adapter uses the `training_file_url` extension field when present
// in job.runtime_spec, otherwise falls back to base64-encoded inline data.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::Utc;
use serde_json::{json, Value};
use ucx_protocol::{
    Allocation, BillingCurrency, BillingRecord, ComputeProvider, ComputeReceipt,
    ExternalProviderAdapter, GpuCapability, GpuVendor, CpuArch, CpuCapability,
    Job, JobId, JobStatus, LineItem, Offer, ProviderCapability, ProviderTier,
    ResourceUsage, RuntimeKind, TrustLevel, UcxError, VerificationProof,
    WorkloadType,
};
use uuid::Uuid;

const BASE: &str = "https://api.gpu.ai/v1";

pub struct GpuAiFineTuneAdapter {
    api_key:    String,
    capability: ProviderCapability,
    jobs:       Mutex<HashMap<JobId, GpuAiJobRecord>>,
}

struct GpuAiJobRecord {
    external_job_id: String,
    status:          JobStatus,
    model_id:        Option<String>,
}

impl GpuAiFineTuneAdapter {
    pub fn new(api_key: impl Into<String>) -> Self {
        let api_key = api_key.into();
        Self {
            capability: build_capability(),
            jobs: Mutex::new(HashMap::new()),
            api_key,
        }
    }

    pub fn from_env() -> Option<Self> {
        let key = std::env::var("GPUAI_KEY").ok().filter(|s| !s.is_empty())?;
        Some(Self::new(key))
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key)
    }
}

impl ComputeProvider for GpuAiFineTuneAdapter {
    fn id(&self)         -> &str              { "gpu-ai-finetune" }
    fn capability(&self) -> &ProviderCapability { &self.capability }

    fn can_accept(&self, job: &Job) -> bool {
        job.workload == WorkloadType::Training && job.constraints.allow_external
    }

    fn submit(&self, job: Job) -> Result<Allocation, UcxError> {
        let body = self.translate_job(&job)?;

        // Synchronous HTTP — adapter is called from broker's async context
        // via tokio::task::block_in_place or a dedicated thread pool.
        let client = reqwest::blocking::Client::new();
        let url = format!("{BASE}/fine_tuning/jobs");
        let resp = client
            .post(&url)
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "gpu.ai".into(), reason: e.to_string() })?;

        let status = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);

        if !status.is_success() {
            return Err(UcxError::Adapter {
                adapter: "gpu.ai".into(),
                reason: format!("POST /fine_tuning/jobs {status}: {val}"),
            });
        }

        let external_id = val["id"].as_str().unwrap_or("unknown").to_string();

        self.jobs.lock().unwrap().insert(job.id, GpuAiJobRecord {
            external_job_id: external_id.clone(),
            status: JobStatus::Running,
            model_id: None,
        });

        Ok(Allocation {
            job_id:       job.id,
            provider_id:  self.id().to_string(),
            offer: Offer {
                job_id:       job.id,
                provider_id:  self.id().to_string(),
                price_cents:  0,
                eta_secs:     0,
                expires_at:   Utc::now(),
                is_external:  true,
            },
            allocated_at: Utc::now(),
            status: JobStatus::Running,
        })
    }

    fn status(&self, job_id: JobId) -> Result<JobStatus, UcxError> {
        let record = self.jobs.lock().unwrap();
        let rec = record.get(&job_id)
            .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?;
        let external_id = rec.external_job_id.clone();
        drop(record);

        let client = reqwest::blocking::Client::new();
        let url = format!("{BASE}/fine_tuning/jobs/{external_id}");
        let resp = client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "gpu.ai".into(), reason: e.to_string() })?;

        let val: Value = resp.json().unwrap_or(Value::Null);
        let ext_status = val["status"].as_str().unwrap_or("unknown");
        let ucx_status = match ext_status {
            "succeeded"  => JobStatus::Completed,
            "failed"     => JobStatus::Failed,
            "cancelled"  => JobStatus::Cancelled,
            "running"    => JobStatus::Running,
            _            => JobStatus::Pending,
        };

        if ucx_status == JobStatus::Completed {
            if let Some(model_id) = val["fine_tuned_model"].as_str() {
                let mut record = self.jobs.lock().unwrap();
                if let Some(rec) = record.get_mut(&job_id) {
                    rec.status   = JobStatus::Completed;
                    rec.model_id = Some(model_id.to_string());
                }
            }
        }

        Ok(ucx_status)
    }

    fn receipt(&self, job_id: JobId) -> Result<ComputeReceipt, UcxError> {
        let record = self.jobs.lock().unwrap();
        let rec = record.get(&job_id)
            .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?;
        if rec.status != JobStatus::Completed {
            return Err(UcxError::JobNotFound { job_id: job_id.to_string() });
        }
        let model_id = rec.model_id.clone();
        drop(record);

        Ok(ComputeReceipt {
            job_id,
            provider_id:  self.id().to_string(),
            completed_at: Utc::now(),
            resources: ResourceUsage {
                gpu_seconds:    0.0,   // GPU.ai doesn't expose raw resource usage
                cpu_seconds:    0.0,
                ram_gb_seconds: 0.0,
                storage_gb:     0.0,
                egress_gb:      0.0,
            },
            billing: BillingRecord {
                amount_cents: 0,       // billed externally through GPU.ai account
                currency:     BillingCurrency::Usd,
                line_items:   vec![LineItem { label: "gpu.ai fine-tune".into(), cents: 0 }],
            },
            verification: VerificationProof {
                artifact_hash:       model_id,
                runtime_attestation: Some("gpu.ai".into()),
                execution_hash:      None,
            },
            zangbeto_anchor: None,
        })
    }

    fn cancel(&self, job_id: JobId) -> Result<(), UcxError> {
        let record = self.jobs.lock().unwrap();
        let rec = record.get(&job_id)
            .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?;
        let external_id = rec.external_job_id.clone();
        drop(record);

        let client = reqwest::blocking::Client::new();
        let url = format!("{BASE}/fine_tuning/jobs/{external_id}/cancel");
        client.post(&url)
            .header("Authorization", self.auth_header())
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "gpu.ai".into(), reason: e.to_string() })?;

        Ok(())
    }
}

impl ExternalProviderAdapter for GpuAiFineTuneAdapter {
    fn network_name(&self) -> &str { "gpu.ai" }

    fn is_available(&self) -> bool {
        let client = reqwest::blocking::Client::new();
        client
            .get(format!("{BASE}/models"))
            .header("Authorization", self.auth_header())
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    fn translate_job(&self, job: &Job) -> Result<Value, UcxError> {
        let spec = &job.runtime_spec;
        let model = spec["model"].as_str().unwrap_or("qwen3.5-9b");
        let training_file = spec["training_file"].as_str()
            .ok_or_else(|| UcxError::Adapter {
                adapter: "gpu.ai".into(),
                reason: "runtime_spec.training_file required for Training workload".into(),
            })?;

        Ok(json!({
            "model": model,
            "training_file": training_file,
            "hyperparameters": spec.get("hyperparameters").cloned().unwrap_or(json!({
                "n_epochs": 3,
                "batch_size": 8,
                "learning_rate_multiplier": 2
            })),
            "suffix": spec["suffix"].as_str().unwrap_or("ucx")
        }))
    }

    fn translate_receipt(&self, raw: Value) -> Result<ComputeReceipt, UcxError> {
        let job_id = Uuid::new_v4(); // caller should pass job_id separately in practice
        Ok(ComputeReceipt {
            job_id,
            provider_id:  self.id().to_string(),
            completed_at: Utc::now(),
            resources: ResourceUsage { gpu_seconds: 0.0, cpu_seconds: 0.0, ram_gb_seconds: 0.0, storage_gb: 0.0, egress_gb: 0.0 },
            billing: BillingRecord {
                amount_cents: 0,
                currency:     BillingCurrency::Usd,
                line_items:   vec![],
            },
            verification: VerificationProof {
                artifact_hash:       raw["fine_tuned_model"].as_str().map(str::to_string),
                runtime_attestation: Some("gpu.ai".into()),
                execution_hash:      None,
            },
            zangbeto_anchor: None,
        })
    }
}

fn build_capability() -> ProviderCapability {
    ProviderCapability {
        provider_id:    "gpu-ai-finetune".into(),
        owner_agent_id: None,
        tier:           ProviderTier::Professional,
        trust:          TrustLevel::Standard,
        gpu: Some(GpuCapability {
            vendor:  GpuVendor::Nvidia,
            model:   "A40/A100/H100 (cloud-assigned)".into(),
            vram_gb: 80.0,
            fp16:    true,
            bf16:    true,
            cuda:    true,
            rocm:    false,
            count:   1,
        }),
        cpu: CpuCapability { cores: 8, arch: CpuArch::X86_64, frequency_mhz: None },
        ram_gb:               64.0,
        disk_gb:              500.0,
        runtimes:             vec![RuntimeKind::Oci],
        price_gpu_hour_cents: Some(49),  // A40 $0.49/hr
        price_cpu_hour_cents: 2,
        policy_deny:          vec![],
        regions:              vec!["US".into()],
    }
}
