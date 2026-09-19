use std::collections::HashMap;
use std::sync::Mutex;

use chrono::Utc;
use serde_json::{json, Value};
use ucx_protocol::{
    Allocation, BillingCurrency, BillingRecord, ComputeProvider, ComputeReceipt,
    CpuArch, CpuCapability, ExternalProviderAdapter,
    Job, JobId, JobStatus, LineItem, Offer, ProviderCapability, ProviderTier,
    ResourceUsage, RuntimeKind, TrustLevel, UcxError, VerificationProof,
};
use uuid::Uuid;

const DEFAULT_BASE: &str = "https://api.akashnet.net";

pub struct AkashAdapter {
    api_key:    String,
    base_url:   String,
    capability: ProviderCapability,
    jobs:       Mutex<HashMap<JobId, AkashJobRecord>>,
}

struct AkashJobRecord {
    dseq:   String,   // Akash deployment sequence number
    status: JobStatus,
    cost_uakt: u64,   // micro-AKT spent
}

impl AkashAdapter {
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key:    api_key.into(),
            base_url:   base_url.into().trim_end_matches('/').to_string(),
            capability: build_capability(),
            jobs:       Mutex::new(HashMap::new()),
        }
    }

    pub fn from_env() -> Option<Self> {
        let key = std::env::var("AKASH_KEY").ok().filter(|s| !s.is_empty())?;
        let base = std::env::var("AKASH_API_URL")
            .unwrap_or_else(|_| DEFAULT_BASE.to_string());
        Some(Self::new(key, base))
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    fn sdl_for_job(job: &Job) -> Value {
        // Prefer caller-supplied SDL; otherwise synthesise a minimal generic SDL.
        if let Some(sdl) = job.runtime_spec.get("sdl") {
            return sdl.clone();
        }
        let image = job.runtime_spec["image"]
            .as_str()
            .unwrap_or("ubuntu:22.04");
        let cpu_units = job.requirements.cpu_cores.unwrap_or(1);
        let ram_mb    = (job.requirements.ram_gb.unwrap_or(1.0) * 1024.0) as u64;
        let storage_gb = 10u64;

        json!({
            "version": "2.0",
            "services": {
                "ucx-workload": {
                    "image": image,
                    "expose": []
                }
            },
            "profiles": {
                "compute": {
                    "ucx-workload": {
                        "resources": {
                            "cpu": { "units": cpu_units },
                            "memory": { "size": format!("{ram_mb}Mi") },
                            "storage": { "size": format!("{storage_gb}Gi") }
                        }
                    }
                },
                "placement": {
                    "akash": {
                        "pricing": {
                            "ucx-workload": { "denom": "uakt", "amount": 100 }
                        }
                    }
                }
            },
            "deployment": {
                "ucx-workload": {
                    "akash": {
                        "profile": "ucx-workload",
                        "count": 1
                    }
                }
            }
        })
    }
}

impl ComputeProvider for AkashAdapter {
    fn id(&self)         -> &str               { "akash-network" }
    fn capability(&self) -> &ProviderCapability { &self.capability }

    fn can_accept(&self, job: &Job) -> bool {
        job.constraints.allow_external
    }

    fn submit(&self, job: Job) -> Result<Allocation, UcxError> {
        let sdl = Self::sdl_for_job(&job);
        let body = json!({ "sdl": sdl, "ucx_job_id": job.id });

        let client = reqwest::blocking::Client::new();
        let url = format!("{}/deployments", self.base_url);
        let resp = client
            .post(&url)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "akash".into(), reason: e.to_string() })?;

        let status = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);

        if !status.is_success() {
            return Err(UcxError::Adapter {
                adapter: "akash".into(),
                reason: format!("POST /deployments {status}: {val}"),
            });
        }

        let dseq = val["deployment_id"]
            .as_str()
            .or_else(|| val["dseq"].as_str())
            .unwrap_or("unknown")
            .to_string();

        self.jobs.lock().unwrap().insert(job.id, AkashJobRecord {
            dseq:      dseq.clone(),
            status:    JobStatus::Pending,
            cost_uakt: 0,
        });

        Ok(Allocation {
            job_id:       job.id,
            provider_id:  self.id().to_string(),
            offer: Offer {
                job_id:       job.id,
                provider_id:  self.id().to_string(),
                price_cents:  0,
                eta_secs:     60,
                expires_at:   Utc::now(),
                is_external:  true,
            },
            allocated_at: Utc::now(),
            status: JobStatus::Pending,
        })
    }

    fn status(&self, job_id: JobId) -> Result<JobStatus, UcxError> {
        let record = self.jobs.lock().unwrap();
        let rec = record.get(&job_id)
            .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?;
        let dseq = rec.dseq.clone();
        drop(record);

        let client = reqwest::blocking::Client::new();
        let url = format!("{}/deployments/{dseq}", self.base_url);
        let resp = client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "akash".into(), reason: e.to_string() })?;

        let val: Value = resp.json().unwrap_or(Value::Null);
        let ext_status = val["state"].as_str().unwrap_or("unknown");

        let ucx_status = match ext_status {
            "active"    | "running"    => JobStatus::Running,
            "closed"    | "completed"  => JobStatus::Completed,
            "failed"                   => JobStatus::Failed,
            "cancelled" | "withdrawn"  => JobStatus::Cancelled,
            _                          => JobStatus::Pending,
        };

        let cost = val["escrow_account"]["balance"]["amount"]
            .as_str()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let mut record = self.jobs.lock().unwrap();
        if let Some(rec) = record.get_mut(&job_id) {
            rec.status    = ucx_status.clone();
            rec.cost_uakt = cost;
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
        let cost_uakt = rec.cost_uakt;
        let dseq      = rec.dseq.clone();
        drop(record);

        // 1 AKT = 1_000_000 uAKT; ~$0.20/AKT → 1 uAKT ≈ $0.000_000_20
        // Convert to cents: cost_uakt * 0.00002 (rough; Akash price fluctuates)
        let amount_cents = (cost_uakt as f64 * 0.00002) as u64;

        Ok(ComputeReceipt {
            job_id,
            provider_id:  self.id().to_string(),
            completed_at: Utc::now(),
            resources: ResourceUsage {
                gpu_seconds:    0.0,
                cpu_seconds:    0.0,
                ram_gb_seconds: 0.0,
                storage_gb:     0.0,
                egress_gb:      0.0,
            },
            billing: BillingRecord {
                amount_cents,
                currency:   BillingCurrency::Usd,
                line_items: vec![LineItem {
                    label: format!("akash deployment {dseq}"),
                    cents: amount_cents,
                }],
            },
            verification: VerificationProof {
                artifact_hash:       Some(dseq),
                runtime_attestation: Some("akash".into()),
                execution_hash:      None,
            },
            zangbeto_anchor: None,
            gix1_canonical_id: None,
        })
    }

    fn cancel(&self, job_id: JobId) -> Result<(), UcxError> {
        let record = self.jobs.lock().unwrap();
        let rec = record.get(&job_id)
            .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?;
        let dseq = rec.dseq.clone();
        drop(record);

        let client = reqwest::blocking::Client::new();
        let url = format!("{}/deployments/{dseq}", self.base_url);
        client
            .delete(&url)
            .header("Authorization", self.auth_header())
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "akash".into(), reason: e.to_string() })?;

        Ok(())
    }
}

impl ExternalProviderAdapter for AkashAdapter {
    fn network_name(&self) -> &str { "akash" }

    fn is_available(&self) -> bool {
        let client = reqwest::blocking::Client::new();
        client
            .get(format!("{}/node/status", self.base_url))
            .header("Authorization", self.auth_header())
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    fn translate_job(&self, job: &Job) -> Result<Value, UcxError> {
        Ok(json!({
            "sdl":         Self::sdl_for_job(job),
            "ucx_job_id":  job.id,
            "workload":    format!("{:?}", job.workload),
        }))
    }

    fn translate_receipt(&self, raw: Value) -> Result<ComputeReceipt, UcxError> {
        let cost_uakt = raw["cost_uakt"].as_u64().unwrap_or(0);
        let dseq      = raw["dseq"].as_str().unwrap_or("unknown").to_string();
        let cents     = (cost_uakt as f64 * 0.00002) as u64;

        Ok(ComputeReceipt {
            job_id:       Uuid::new_v4(),
            provider_id:  self.id().to_string(),
            completed_at: Utc::now(),
            resources:    ResourceUsage { gpu_seconds: 0.0, cpu_seconds: 0.0, ram_gb_seconds: 0.0, storage_gb: 0.0, egress_gb: 0.0 },
            billing: BillingRecord {
                amount_cents: cents,
                currency:     BillingCurrency::Usd,
                line_items:   vec![LineItem { label: format!("akash {dseq}"), cents }],
            },
            verification: VerificationProof {
                artifact_hash:       Some(dseq),
                runtime_attestation: Some("akash".into()),
                execution_hash:      None,
            },
            zangbeto_anchor: None,
            gix1_canonical_id: None,
        })
    }
}

fn build_capability() -> ProviderCapability {
    ProviderCapability {
        provider_id:    "akash-network".into(),
        owner_agent_id: None,
        tier:           ProviderTier::Community,
        trust:          TrustLevel::Standard,
        gpu: None,   // Akash has GPU providers but we discover dynamically
        cpu: CpuCapability { cores: 4, arch: CpuArch::X86_64, frequency_mhz: None },
        ram_gb:               8.0,
        disk_gb:              100.0,
        runtimes:             vec![RuntimeKind::Oci],
        price_gpu_hour_cents: None,
        price_cpu_hour_cents: 1,   // Akash is typically cheaper than cloud
        policy_deny:          vec![],
        regions:              vec!["global".into()],
    }
}
