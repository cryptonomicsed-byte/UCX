// RunPod Serverless adapter — routes Training/Simulation workloads via
// POST /v2/{endpoint_id}/run and polls /v2/{endpoint_id}/status/{run_id}.

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::Utc;
use serde_json::{json, Value};
use ucx_protocol::{
    Allocation, BillingCurrency, BillingRecord, ComputeProvider, ComputeReceipt,
    ExternalProviderAdapter, GpuCapability, GpuVendor, CpuArch, CpuCapability,
    Job, JobId, JobStatus, LineItem, Offer, ProviderCapability, ProviderTier,
    ResourceUsage, TrustLevel, UcxError, VerificationProof, WorkloadType,
};
use uuid::Uuid;

const BASE: &str = "https://api.runpod.io/v2";

pub struct RunPodServerlessAdapter {
    api_key:     String,
    endpoint_id: String,
    capability:  ProviderCapability,
    /// job_id → {run_id, status, result}
    runs:        Mutex<HashMap<JobId, RunRecord>>,
}

#[derive(Clone)]
struct RunRecord {
    run_id: String,
    status: JobStatus,
    result: Option<Value>,
}

impl RunPodServerlessAdapter {
    pub fn new(api_key: impl Into<String>, endpoint_id: impl Into<String>) -> Self {
        let endpoint_id = endpoint_id.into();
        Self {
            capability: build_serverless_capability(&endpoint_id),
            api_key: api_key.into(),
            endpoint_id,
            runs: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_env() -> Option<Self> {
        let key         = std::env::var("RUNPOD_API_KEY").ok().filter(|s| !s.is_empty())?;
        let endpoint_id = std::env::var("RUNPOD_ENDPOINT_ID").unwrap_or_else(|_| "default".into());
        Some(Self::new(key, endpoint_id))
    }

    fn auth_header(&self) -> String { format!("Bearer {}", self.api_key) }

    fn run_url(&self) -> String { format!("{BASE}/{}/run", self.endpoint_id) }

    fn status_url(&self, run_id: &str) -> String {
        format!("{BASE}/{}/status/{run_id}", self.endpoint_id)
    }

    fn poll_status(&self, run_id: &str) -> Result<(JobStatus, Option<Value>), UcxError> {
        let client = reqwest::blocking::Client::new();
        let resp = client
            .get(self.status_url(run_id))
            .header("Authorization", self.auth_header())
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "runpod".into(), reason: e.to_string() })?;

        let val: Value = resp.json().unwrap_or(Value::Null);
        let status_str = val["status"].as_str().unwrap_or("UNKNOWN");
        let job_status = match status_str {
            "COMPLETED" => JobStatus::Completed,
            "FAILED"    => JobStatus::Failed,
            "CANCELLED" => JobStatus::Cancelled,
            "IN_QUEUE" | "IN_PROGRESS" => JobStatus::Running,
            _ => JobStatus::Pending,
        };

        let output = if job_status == JobStatus::Completed {
            val.get("output").cloned()
        } else {
            None
        };

        Ok((job_status, output))
    }
}

impl ComputeProvider for RunPodServerlessAdapter {
    fn id(&self)         -> &str               { "runpod-serverless" }
    fn capability(&self) -> &ProviderCapability { &self.capability }

    fn can_accept(&self, job: &Job) -> bool {
        matches!(job.workload, WorkloadType::Training | WorkloadType::Simulation)
            && job.constraints.allow_external
    }

    fn submit(&self, job: Job) -> Result<Allocation, UcxError> {
        let body = self.translate_job(&job)?;

        let client = reqwest::blocking::Client::new();
        let resp = client
            .post(self.run_url())
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "runpod".into(), reason: e.to_string() })?;

        let http_status = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);
        if !http_status.is_success() {
            return Err(UcxError::Adapter {
                adapter: "runpod".into(),
                reason: format!("POST /run {http_status}: {val}"),
            });
        }

        let run_id = val["id"].as_str()
            .ok_or_else(|| UcxError::Adapter {
                adapter: "runpod".into(),
                reason: "missing run id in RunPod response".into(),
            })?
            .to_string();

        self.runs.lock().unwrap().insert(job.id, RunRecord {
            run_id: run_id.clone(),
            status: JobStatus::Pending,
            result: None,
        });

        Ok(Allocation {
            job_id:       job.id,
            provider_id:  self.id().to_string(),
            offer: Offer {
                job_id:      job.id,
                provider_id: self.id().to_string(),
                price_cents: 0,
                eta_secs:    120,
                expires_at:  Utc::now(),
                is_external: true,
            },
            allocated_at: Utc::now(),
            status:       JobStatus::Pending,
        })
    }

    fn status(&self, job_id: JobId) -> Result<JobStatus, UcxError> {
        let run_id = {
            let runs = self.runs.lock().unwrap();
            runs.get(&job_id)
                .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?
                .run_id
                .clone()
        };

        let (status, result) = self.poll_status(&run_id)?;

        let mut runs = self.runs.lock().unwrap();
        if let Some(rec) = runs.get_mut(&job_id) {
            rec.status = status.clone();
            if result.is_some() { rec.result = result; }
        }
        Ok(runs.get(&job_id).map(|r| r.status.clone()).unwrap_or(status))
    }

    fn receipt(&self, job_id: JobId) -> Result<ComputeReceipt, UcxError> {
        // Ensure latest status is fetched.
        self.status(job_id)?;

        let runs = self.runs.lock().unwrap();
        let rec = runs.get(&job_id)
            .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?;

        let output = rec.result.clone().unwrap_or(Value::Null);
        drop(runs);
        self.translate_receipt(output)
    }

    fn cancel(&self, job_id: JobId) -> Result<(), UcxError> {
        let run_id = {
            let runs = self.runs.lock().unwrap();
            runs.get(&job_id)
                .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?
                .run_id
                .clone()
        };

        let cancel_url = format!("{BASE}/{}/cancel/{run_id}", self.endpoint_id);
        reqwest::blocking::Client::new()
            .post(&cancel_url)
            .header("Authorization", self.auth_header())
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "runpod".into(), reason: e.to_string() })?;

        let mut runs = self.runs.lock().unwrap();
        if let Some(rec) = runs.get_mut(&job_id) {
            rec.status = JobStatus::Cancelled;
        }
        Ok(())
    }
}

impl ExternalProviderAdapter for RunPodServerlessAdapter {
    fn network_name(&self) -> &str { "runpod" }

    fn is_available(&self) -> bool {
        reqwest::blocking::Client::new()
            .get(format!("{BASE}/{}/health", self.endpoint_id))
            .header("Authorization", self.auth_header())
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    fn translate_job(&self, job: &Job) -> Result<Value, UcxError> {
        let spec = &job.runtime_spec;
        Ok(json!({
            "input": {
                "job_id":   job.id.to_string(),
                "workload": format!("{:?}", job.workload),
                "spec":     spec,
            }
        }))
    }

    fn translate_receipt(&self, raw: Value) -> Result<ComputeReceipt, UcxError> {
        // RunPod doesn't return billing in the output — estimate from execution time.
        let exec_ms = raw["executionTime"].as_u64().unwrap_or(0);
        let gpu_secs = exec_ms as f64 / 1000.0;
        // ~$0.20/hr per H100 SXM5 = 200 cents/hr = 200/3600 per second
        let cents = (gpu_secs * 200.0 / 3600.0).ceil() as u64;

        Ok(ComputeReceipt {
            job_id:       Uuid::new_v4(),
            provider_id:  self.id().to_string(),
            completed_at: Utc::now(),
            resources: ResourceUsage {
                gpu_seconds:    gpu_secs,
                cpu_seconds:    gpu_secs * 0.1,
                ram_gb_seconds: 0.0,
                storage_gb:     0.0,
                egress_gb:      0.0,
            },
            billing: BillingRecord {
                amount_cents: cents,
                currency:     BillingCurrency::Usd,
                line_items: vec![LineItem {
                    label: format!("{gpu_secs:.1}s GPU compute"),
                    cents,
                }],
            },
            verification: VerificationProof {
                artifact_hash:       raw.get("id").and_then(|v| v.as_str()).map(str::to_string),
                runtime_attestation: Some("runpod-serverless".into()),
                execution_hash:      None,
            },
            zangbeto_anchor: None,
            gix1_canonical_id: None,
        })
    }
}

fn build_serverless_capability(endpoint_id: &str) -> ProviderCapability {
    ProviderCapability {
        provider_id:    format!("runpod-serverless-{endpoint_id}"),
        owner_agent_id: None,
        tier:           ProviderTier::Professional,
        trust:          TrustLevel::Standard,
        gpu: Some(GpuCapability {
            vendor:  GpuVendor::Nvidia,
            model:   "A100/H100 (serverless pool)".into(),
            vram_gb: 80.0,
            fp16: true, bf16: true, cuda: true, rocm: false,
            count: 1,
        }),
        cpu: CpuCapability { cores: 16, arch: CpuArch::X86_64, frequency_mhz: None },
        ram_gb:               256.0,
        disk_gb:              0.0,
        runtimes:             vec![],
        price_gpu_hour_cents: Some(20),
        price_cpu_hour_cents: 2,
        policy_deny:          vec![],
        regions:              vec!["US".into(), "EU".into()],
    }
}
