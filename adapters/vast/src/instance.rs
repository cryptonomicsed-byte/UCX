use std::collections::HashMap;
use std::sync::Mutex;

use chrono::Utc;
use serde_json::{json, Value};
use ucx_protocol::{
    Allocation, BillingCurrency, BillingRecord, ComputeProvider, ComputeReceipt,
    CpuArch, CpuCapability, ExternalProviderAdapter, GpuCapability, GpuVendor,
    Job, JobId, JobStatus, LineItem, Offer, ProviderCapability, ProviderTier,
    ResourceUsage, RuntimeKind, TrustLevel, UcxError, VerificationProof,
    WorkloadType,
};
use uuid::Uuid;

const BASE: &str = "https://console.vast.ai/api/v0";

pub struct VastAdapter {
    api_key:    String,
    capability: ProviderCapability,
    jobs:       Mutex<HashMap<JobId, VastJobRecord>>,
}

struct VastJobRecord {
    instance_id:      u64,
    offer_id:         u64,
    price_per_hr:     f64,
    started_at:       chrono::DateTime<Utc>,
    status:           JobStatus,
}

impl VastAdapter {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key:    api_key.into(),
            capability: build_capability(),
            jobs:       Mutex::new(HashMap::new()),
        }
    }

    pub fn from_env() -> Option<Self> {
        let key = std::env::var("VAST_KEY").ok().filter(|s| !s.is_empty())?;
        Some(Self::new(key))
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    /// Find the cheapest offer matching job requirements.
    fn find_offer(&self, job: &Job) -> Result<(u64, f64), UcxError> {
        let client = reqwest::blocking::Client::new();

        let mut params = vec![
            ("verified", "true"),
            ("rentable", "true"),
            ("order", "score-"),
        ];

        // Build query — Vast.ai uses query params for filtering
        let gpu_count = job.requirements.gpu_count.unwrap_or(0);
        let gpu_count_str;
        let vram_str;
        let max_price_str;

        if gpu_count > 0 {
            gpu_count_str = gpu_count.to_string();
            params.push(("num_gpus", &gpu_count_str));
        }
        if let Some(vram) = job.requirements.vram_gb {
            vram_str = format!("{:.0}", vram);
            params.push(("gpu_ram", &vram_str));
        }
        if let Some(max_cents) = job.constraints.max_price_cents {
            // max_price_cents is per-job total; Vast uses $/hr — interpret as $/hr cap
            let dollars_hr = max_cents as f64 / 100.0;
            max_price_str = format!("{:.2}", dollars_hr);
            params.push(("dph_total", &max_price_str));
        }

        let resp = client
            .get(format!("{BASE}/bundles/"))
            .header("Authorization", self.auth_header())
            .query(&params)
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "vast".into(), reason: e.to_string() })?;

        let val: Value = resp.json().unwrap_or(Value::Null);
        let offers = val["offers"].as_array()
            .ok_or_else(|| UcxError::Adapter {
                adapter: "vast".into(),
                reason: "no offers in response".into(),
            })?;

        let offer = offers.first()
            .ok_or_else(|| UcxError::InsufficientCapacity { reason: "no vast.ai offers match job requirements".into() })?;

        let id    = offer["id"].as_u64().unwrap_or(0);
        let price = offer["dph_total"].as_f64().unwrap_or(0.5);
        Ok((id, price))
    }
}

impl ComputeProvider for VastAdapter {
    fn id(&self)         -> &str               { "vast-ai" }
    fn capability(&self) -> &ProviderCapability { &self.capability }

    fn can_accept(&self, job: &Job) -> bool {
        job.constraints.allow_external
            && matches!(job.workload, WorkloadType::Training | WorkloadType::Inference | WorkloadType::Rendering)
    }

    fn submit(&self, job: Job) -> Result<Allocation, UcxError> {
        let (offer_id, price_hr) = self.find_offer(&job)?;

        let image = job.runtime_spec["image"]
            .as_str()
            .unwrap_or("pytorch/pytorch:latest");
        let onstart = job.runtime_spec["onstart"]
            .as_str()
            .unwrap_or("sleep infinity");

        let body = json!({
            "client_id":  "ucx",
            "image":      image,
            "onstart":    onstart,
            "runtype":    "jupyter_dirpath",
            "env": {},
        });

        let client = reqwest::blocking::Client::new();
        let url = format!("{BASE}/asks/{offer_id}/");
        let resp = client
            .put(&url)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "vast".into(), reason: e.to_string() })?;

        let status_code = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);

        if !status_code.is_success() {
            return Err(UcxError::Adapter {
                adapter: "vast".into(),
                reason: format!("PUT /asks/{offer_id}/ {status_code}: {val}"),
            });
        }

        let instance_id = val["new_contract"]
            .as_u64()
            .unwrap_or(0);

        let price_cents = (price_hr * 100.0) as u64;

        self.jobs.lock().unwrap().insert(job.id, VastJobRecord {
            instance_id,
            offer_id,
            price_per_hr: price_hr,
            started_at:   Utc::now(),
            status:       JobStatus::Pending,
        });

        Ok(Allocation {
            job_id:       job.id,
            provider_id:  self.id().to_string(),
            offer: Offer {
                job_id:       job.id,
                provider_id:  self.id().to_string(),
                price_cents,
                eta_secs:     120,
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
        let instance_id = rec.instance_id;
        drop(record);

        let client = reqwest::blocking::Client::new();
        let url = format!("{BASE}/instances/{instance_id}/");
        let resp = client
            .get(&url)
            .header("Authorization", self.auth_header())
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "vast".into(), reason: e.to_string() })?;

        let val: Value = resp.json().unwrap_or(Value::Null);
        let actual_status = val["instances"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(|i| i["actual_status"].as_str())
            .unwrap_or("unknown");

        let ucx_status = match actual_status {
            "running"   => JobStatus::Running,
            "stopped"   | "exited"    => JobStatus::Completed,
            "failed"    | "error"     => JobStatus::Failed,
            "cancelled"               => JobStatus::Cancelled,
            _                         => JobStatus::Pending,
        };

        let mut record = self.jobs.lock().unwrap();
        if let Some(rec) = record.get_mut(&job_id) {
            rec.status = ucx_status.clone();
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
        let instance_id  = rec.instance_id;
        let price_per_hr = rec.price_per_hr;
        let started_at   = rec.started_at;
        drop(record);

        let elapsed_hrs = (Utc::now() - started_at).num_seconds() as f64 / 3600.0;
        let amount_cents = (price_per_hr * elapsed_hrs * 100.0) as u64;

        Ok(ComputeReceipt {
            job_id,
            provider_id:  self.id().to_string(),
            completed_at: Utc::now(),
            resources: ResourceUsage {
                gpu_seconds:    elapsed_hrs * 3600.0,
                cpu_seconds:    0.0,
                ram_gb_seconds: 0.0,
                storage_gb:     0.0,
                egress_gb:      0.0,
            },
            billing: BillingRecord {
                amount_cents,
                currency:   BillingCurrency::Usd,
                line_items: vec![LineItem {
                    label: format!("vast instance {instance_id}"),
                    cents: amount_cents,
                }],
            },
            verification: VerificationProof {
                artifact_hash:       Some(instance_id.to_string()),
                runtime_attestation: Some("vast.ai".into()),
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
        let instance_id = rec.instance_id;
        drop(record);

        let client = reqwest::blocking::Client::new();
        let url = format!("{BASE}/instances/{instance_id}/");
        client
            .delete(&url)
            .header("Authorization", self.auth_header())
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "vast".into(), reason: e.to_string() })?;

        Ok(())
    }
}

impl ExternalProviderAdapter for VastAdapter {
    fn network_name(&self) -> &str { "vast.ai" }

    fn is_available(&self) -> bool {
        let client = reqwest::blocking::Client::new();
        client
            .get(format!("{BASE}/users/current/"))
            .header("Authorization", self.auth_header())
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    fn translate_job(&self, job: &Job) -> Result<Value, UcxError> {
        let (offer_id, price) = self.find_offer(job)?;
        Ok(json!({
            "offer_id":   offer_id,
            "price_hr":   price,
            "image":      job.runtime_spec["image"].as_str().unwrap_or("pytorch/pytorch:latest"),
            "workload":   format!("{:?}", job.workload),
        }))
    }

    fn translate_receipt(&self, raw: Value) -> Result<ComputeReceipt, UcxError> {
        let instance_id  = raw["instance_id"].as_u64().unwrap_or(0);
        let amount_cents = raw["amount_cents"].as_u64().unwrap_or(0);
        let gpu_secs     = raw["gpu_seconds"].as_f64().unwrap_or(0.0);

        Ok(ComputeReceipt {
            job_id:       Uuid::new_v4(),
            provider_id:  self.id().to_string(),
            completed_at: Utc::now(),
            resources: ResourceUsage {
                gpu_seconds:    gpu_secs,
                cpu_seconds:    0.0,
                ram_gb_seconds: 0.0,
                storage_gb:     0.0,
                egress_gb:      0.0,
            },
            billing: BillingRecord {
                amount_cents,
                currency:   BillingCurrency::Usd,
                line_items: vec![LineItem {
                    label: format!("vast instance {instance_id}"),
                    cents: amount_cents,
                }],
            },
            verification: VerificationProof {
                artifact_hash:       Some(instance_id.to_string()),
                runtime_attestation: Some("vast.ai".into()),
                execution_hash:      None,
            },
            zangbeto_anchor: None,
            gix1_canonical_id: None,
        })
    }
}

fn build_capability() -> ProviderCapability {
    ProviderCapability {
        provider_id:    "vast-ai".into(),
        owner_agent_id: None,
        tier:           ProviderTier::Community,
        trust:          TrustLevel::Standard,
        gpu: Some(GpuCapability {
            vendor:  GpuVendor::Nvidia,
            model:   "RTX 3090/4090/A100 (offer-matched)".into(),
            vram_gb: 24.0,
            fp16:    true,
            bf16:    true,
            cuda:    true,
            rocm:    false,
            count:   1,
        }),
        cpu: CpuCapability { cores: 8, arch: CpuArch::X86_64, frequency_mhz: None },
        ram_gb:               32.0,
        disk_gb:              200.0,
        runtimes:             vec![RuntimeKind::Oci],
        price_gpu_hour_cents: Some(30),  // Vast.ai starts ~$0.20-0.30/hr
        price_cpu_hour_cents: 1,
        policy_deny:          vec![],
        regions:              vec!["global".into()],
    }
}
