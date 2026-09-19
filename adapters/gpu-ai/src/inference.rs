// GPU.ai inference adapter — routes Inference workloads to /v1/chat/completions.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::Utc;
use serde_json::{json, Value};
use ucx_protocol::{
    Allocation, BillingCurrency, BillingRecord, ComputeProvider, ComputeReceipt,
    ExternalProviderAdapter, GpuCapability, GpuVendor, CpuArch, CpuCapability,
    Job, JobId, JobStatus, LineItem, Offer, ProviderCapability, ProviderTier,
    ResourceUsage, TrustLevel, UcxError, VerificationProof,
    WorkloadType,
};
use uuid::Uuid;

const BASE: &str = "https://api.gpu.ai/v1";

pub struct GpuAiInferenceAdapter {
    api_key:    String,
    capability: ProviderCapability,
    results:    Mutex<HashMap<JobId, Value>>,
}

impl GpuAiInferenceAdapter {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key:    api_key.into(),
            capability: build_inference_capability(),
            results:    Mutex::new(HashMap::new()),
        }
    }

    pub fn from_env() -> Option<Self> {
        let key = std::env::var("GPUAI_KEY").ok().filter(|s| !s.is_empty())?;
        Some(Self::new(key))
    }

    fn auth_header(&self) -> String { format!("Bearer {}", self.api_key) }
}

impl ComputeProvider for GpuAiInferenceAdapter {
    fn id(&self)         -> &str              { "gpu-ai-inference" }
    fn capability(&self) -> &ProviderCapability { &self.capability }

    fn can_accept(&self, job: &Job) -> bool {
        job.workload == WorkloadType::Inference && job.constraints.allow_external
    }

    fn submit(&self, job: Job) -> Result<Allocation, UcxError> {
        let body = self.translate_job(&job)?;

        let client = reqwest::blocking::Client::new();
        let resp = client
            .post(format!("{BASE}/chat/completions"))
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "gpu.ai".into(), reason: e.to_string() })?;

        let status = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(UcxError::Adapter {
                adapter: "gpu.ai".into(),
                reason: format!("POST /chat/completions {status}: {val}"),
            });
        }

        self.results.lock().unwrap().insert(job.id, val);

        Ok(Allocation {
            job_id:       job.id,
            provider_id:  self.id().to_string(),
            offer: Offer {
                job_id:      job.id,
                provider_id: self.id().to_string(),
                price_cents: 0,
                eta_secs:    0,
                expires_at:  Utc::now(),
                is_external: true,
            },
            allocated_at: Utc::now(),
            status:       JobStatus::Completed,
        })
    }

    fn status(&self, job_id: JobId) -> Result<JobStatus, UcxError> {
        if self.results.lock().unwrap().contains_key(&job_id) {
            Ok(JobStatus::Completed)
        } else {
            Err(UcxError::JobNotFound { job_id: job_id.to_string() })
        }
    }

    fn receipt(&self, job_id: JobId) -> Result<ComputeReceipt, UcxError> {
        let results = self.results.lock().unwrap();
        let raw = results.get(&job_id)
            .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?
            .clone();
        drop(results);
        self.translate_receipt(raw)
    }

    fn cancel(&self, _job_id: JobId) -> Result<(), UcxError> {
        // Inference is synchronous — nothing to cancel.
        Ok(())
    }
}

impl ExternalProviderAdapter for GpuAiInferenceAdapter {
    fn network_name(&self) -> &str { "gpu.ai" }

    fn is_available(&self) -> bool {
        reqwest::blocking::Client::new()
            .get(format!("{BASE}/models"))
            .header("Authorization", self.auth_header())
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    fn translate_job(&self, job: &Job) -> Result<Value, UcxError> {
        let spec = &job.runtime_spec;
        let model = spec["model"].as_str().unwrap_or("qwen3.5-9b");
        let messages = spec.get("messages").cloned().unwrap_or(json!([]));
        Ok(json!({ "model": model, "messages": messages }))
    }

    fn translate_receipt(&self, raw: Value) -> Result<ComputeReceipt, UcxError> {
        let usage = &raw["usage"];
        let total_tokens = usage["total_tokens"].as_u64().unwrap_or(0);
        // Rough cost: $0.001 per 1K tokens for qwen3.5-9b tier
        let cents = (total_tokens as f64 / 1000.0 * 0.1).ceil() as u64;

        Ok(ComputeReceipt {
            job_id:       Uuid::new_v4(),
            provider_id:  self.id().to_string(),
            completed_at: Utc::now(),
            resources: ResourceUsage {
                gpu_seconds:    0.0,
                cpu_seconds:    total_tokens as f64 * 0.001,
                ram_gb_seconds: 0.0,
                storage_gb:     0.0,
                egress_gb:      0.0,
            },
            billing: BillingRecord {
                amount_cents: cents,
                currency:     BillingCurrency::Usd,
                line_items:   vec![LineItem { label: format!("{total_tokens} tokens"), cents }],
            },
            verification: VerificationProof {
                artifact_hash:       raw["id"].as_str().map(str::to_string),
                runtime_attestation: Some("gpu.ai".into()),
                execution_hash:      None,
            },
            zangbeto_anchor: None,
            gix1_canonical_id: None,
        })
    }
}

fn build_inference_capability() -> ProviderCapability {
    ProviderCapability {
        provider_id:    "gpu-ai-inference".into(),
        owner_agent_id: None,
        tier:           ProviderTier::Professional,
        trust:          TrustLevel::Standard,
        gpu: Some(GpuCapability {
            vendor:  GpuVendor::Nvidia,
            model:   "cloud-assigned".into(),
            vram_gb: 80.0,
            fp16: true, bf16: true, cuda: true, rocm: false,
            count: 1,
        }),
        cpu: CpuCapability { cores: 8, arch: CpuArch::X86_64, frequency_mhz: None },
        ram_gb:               64.0,
        disk_gb:              0.0,
        runtimes:             vec![],
        price_gpu_hour_cents: None,
        price_cpu_hour_cents: 0,
        policy_deny:          vec![],
        regions:              vec!["US".into()],
    }
}
