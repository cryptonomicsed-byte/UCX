// RunPod Pod adapter — rents a GPU pod via RunPod GraphQL API.
// Suitable for long-running Training or Simulation workloads that need
// persistent storage or a specific container image.
//
// API: POST https://api.runpod.io/graphql with mutation podFindAndDeployOnDemand

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

const GRAPHQL_URL: &str = "https://api.runpod.io/graphql";

pub struct RunPodPodAdapter {
    api_key:    String,
    gpu_type:   String,
    capability: ProviderCapability,
    pods:       Mutex<HashMap<JobId, PodRecord>>,
}

#[derive(Clone)]
struct PodRecord {
    pod_id:     String,
    status:     JobStatus,
    image:      String,
    started_at: chrono::DateTime<Utc>,
}

impl RunPodPodAdapter {
    /// `gpu_type`: RunPod GPU type ID, e.g. "NVIDIA A100 SXM4 80GB" or "NVIDIA H100 SXM5 80GB".
    pub fn new(api_key: impl Into<String>, gpu_type: impl Into<String>) -> Self {
        let gpu_type = gpu_type.into();
        Self {
            capability: build_pod_capability(&gpu_type),
            api_key: api_key.into(),
            gpu_type,
            pods: Mutex::new(HashMap::new()),
        }
    }

    pub fn from_env() -> Option<Self> {
        let key      = std::env::var("RUNPOD_API_KEY").ok().filter(|s| !s.is_empty())?;
        let gpu_type = std::env::var("RUNPOD_GPU_TYPE")
            .unwrap_or_else(|_| "NVIDIA A100 SXM4 80GB".into());
        Some(Self::new(key, gpu_type))
    }

    fn auth_header(&self) -> String { format!("Bearer {}", self.api_key) }

    fn graphql(&self, query: &str, variables: Value) -> Result<Value, UcxError> {
        let body = json!({ "query": query, "variables": variables });
        let client = reqwest::blocking::Client::new();
        let resp = client
            .post(GRAPHQL_URL)
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "runpod-pod".into(), reason: e.to_string() })?;

        let val: Value = resp.json().unwrap_or(Value::Null);
        if let Some(errors) = val.get("errors") {
            return Err(UcxError::Adapter {
                adapter: "runpod-pod".into(),
                reason: format!("GraphQL errors: {errors}"),
            });
        }
        Ok(val["data"].clone())
    }

    fn terminate_pod(&self, pod_id: &str) -> Result<(), UcxError> {
        let query = r#"
            mutation TerminatePod($input: PodTerminateInput!) {
                podTerminate(input: $input)
            }
        "#;
        self.graphql(query, json!({ "input": { "podId": pod_id } }))?;
        Ok(())
    }
}

impl ComputeProvider for RunPodPodAdapter {
    fn id(&self)         -> &str               { "runpod-pod" }
    fn capability(&self) -> &ProviderCapability { &self.capability }

    fn can_accept(&self, job: &Job) -> bool {
        matches!(job.workload, WorkloadType::Training | WorkloadType::Simulation)
            && job.constraints.allow_external
            && job.runtime_spec.get("image").is_some()
    }

    fn submit(&self, job: Job) -> Result<Allocation, UcxError> {
        let body = self.translate_job(&job)?;
        let image = job.runtime_spec["image"].as_str().unwrap_or("ubuntu:22.04").to_string();

        // Extract variables from translated body
        let variables = body["variables"].clone();
        let data = self.graphql(body["query"].as_str().unwrap_or(""), variables)?;

        let pod_id = data["podFindAndDeployOnDemand"]["id"]
            .as_str()
            .ok_or_else(|| UcxError::Adapter {
                adapter: "runpod-pod".into(),
                reason: "missing pod id in RunPod response".into(),
            })?
            .to_string();

        self.pods.lock().unwrap().insert(job.id, PodRecord {
            pod_id: pod_id.clone(),
            status: JobStatus::Pending,
            image,
            started_at: Utc::now(),
        });

        Ok(Allocation {
            job_id:       job.id,
            provider_id:  self.id().to_string(),
            offer: Offer {
                job_id:      job.id,
                provider_id: self.id().to_string(),
                price_cents: 0,
                eta_secs:    300,
                expires_at:  Utc::now(),
                is_external: true,
            },
            allocated_at: Utc::now(),
            status:       JobStatus::Pending,
        })
    }

    fn status(&self, job_id: JobId) -> Result<JobStatus, UcxError> {
        let pod_id = {
            let pods = self.pods.lock().unwrap();
            pods.get(&job_id)
                .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?
                .pod_id
                .clone()
        };

        let query = r#"
            query PodStatus($podId: String!) {
                pod(input: { podId: $podId }) {
                    id
                    desiredStatus
                    runtime { uptimeInSeconds }
                }
            }
        "#;
        let data = self.graphql(query, json!({ "podId": pod_id }))?;
        let desired = data["pod"]["desiredStatus"].as_str().unwrap_or("UNKNOWN");

        let status = match desired {
            "RUNNING"   => JobStatus::Running,
            "EXITED"    => JobStatus::Completed,
            "TERMINATED"=> JobStatus::Cancelled,
            _           => JobStatus::Pending,
        };

        if let Some(r) = self.pods.lock().unwrap().get_mut(&job_id) { r.status = status.clone(); }
        Ok(status)
    }

    fn receipt(&self, job_id: JobId) -> Result<ComputeReceipt, UcxError> {
        let rec = {
            let pods = self.pods.lock().unwrap();
            pods.get(&job_id)
                .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?
                .clone()
        };

        let elapsed_secs = (Utc::now() - rec.started_at).num_seconds().max(0) as f64;
        // ~$3.00/hr per A100 SXM4 80GB = 300 cents/hr
        let cents = (elapsed_secs * 300.0 / 3600.0).ceil() as u64;

        // Terminate pod now that we're issuing a receipt.
        let _ = self.terminate_pod(&rec.pod_id);

        Ok(ComputeReceipt {
            job_id:       Uuid::new_v4(),
            provider_id:  self.id().to_string(),
            completed_at: Utc::now(),
            resources: ResourceUsage {
                gpu_seconds:    elapsed_secs,
                cpu_seconds:    elapsed_secs * 0.25,
                ram_gb_seconds: 0.0,
                storage_gb:     0.0,
                egress_gb:      0.0,
            },
            billing: BillingRecord {
                amount_cents: cents,
                currency:     BillingCurrency::Usd,
                line_items: vec![LineItem {
                    label: format!("{elapsed_secs:.0}s pod ({}) GPU compute", self.gpu_type),
                    cents,
                }],
            },
            verification: VerificationProof {
                artifact_hash:       Some(rec.pod_id.clone()),
                runtime_attestation: Some("runpod-pod".into()),
                execution_hash:      None,
            },
            zangbeto_anchor: None,
        })
    }

    fn cancel(&self, job_id: JobId) -> Result<(), UcxError> {
        let pod_id = {
            let pods = self.pods.lock().unwrap();
            pods.get(&job_id)
                .ok_or(UcxError::JobNotFound { job_id: job_id.to_string() })?
                .pod_id
                .clone()
        };

        self.terminate_pod(&pod_id)?;
        self.pods.lock().unwrap().get_mut(&job_id).map(|r| r.status = JobStatus::Cancelled);
        Ok(())
    }
}

impl ExternalProviderAdapter for RunPodPodAdapter {
    fn network_name(&self) -> &str { "runpod" }

    fn is_available(&self) -> bool {
        let query = r#"
            query GpuTypes { gpuTypes { id displayName memoryInGb } }
        "#;
        self.graphql(query, json!({})).is_ok()
    }

    fn translate_job(&self, job: &Job) -> Result<Value, UcxError> {
        let image = job.runtime_spec["image"].as_str().unwrap_or("ubuntu:22.04");
        let gpu_count = job.requirements.gpu_count.unwrap_or(1);
        let query = r#"
            mutation Deploy($input: PodFindAndDeployOnDemandInput!) {
                podFindAndDeployOnDemand(input: $input) {
                    id
                    desiredStatus
                    imageName
                    gpuCount
                }
            }
        "#;
        Ok(json!({
            "query": query,
            "variables": {
                "input": {
                    "gpuTypeId":  self.gpu_type,
                    "imageName":  image,
                    "gpuCount":   gpu_count,
                    "containerDiskInGb": 50,
                    "volumeInGb": 0,
                    "minMemoryInGb": job.requirements.ram_gb.unwrap_or(16.0) as u32,
                    "minVcpuCount": 4,
                    "cloudType": "SECURE",
                }
            }
        }))
    }

    fn translate_receipt(&self, _raw: Value) -> Result<ComputeReceipt, UcxError> {
        // Not used — receipt() builds its own from PodRecord.
        Err(UcxError::Adapter { adapter: "runpod-pod".into(), reason: "use receipt()".into() })
    }
}

fn build_pod_capability(gpu_type: &str) -> ProviderCapability {
    let vram_gb = if gpu_type.contains("H100") || gpu_type.contains("80GB") { 80.0 }
                  else if gpu_type.contains("40GB") { 40.0 }
                  else { 24.0 };

    ProviderCapability {
        provider_id:    "runpod-pod".into(),
        owner_agent_id: None,
        tier:           ProviderTier::Professional,
        trust:          TrustLevel::Standard,
        gpu: Some(GpuCapability {
            vendor:  GpuVendor::Nvidia,
            model:   gpu_type.to_string(),
            vram_gb,
            fp16: true, bf16: true, cuda: true, rocm: false,
            count: 1,
        }),
        cpu: CpuCapability { cores: 16, arch: CpuArch::X86_64, frequency_mhz: None },
        ram_gb:               128.0,
        disk_gb:              50.0,
        runtimes:             vec![],
        price_gpu_hour_cents: Some(300),
        price_cpu_hour_cents: 5,
        policy_deny:          vec![],
        regions:              vec!["US".into(), "EU".into(), "CA".into()],
    }
}
