use std::sync::{Arc, RwLock};

use ucx_protocol::{
    ComputeProvider, Job, Allocation, ComputeReceipt, JobId, JobStatus,
    ProviderCapability, ProviderTier, TrustLevel, UcxError, WorkloadRequirements,
};

/// The UCX matching engine.
///
/// Allocation strategy:
///   1. Score all registered native providers → pick best fit.
///   2. If no native provider qualifies AND job.constraints.allow_external,
///      forward to the external overflow pool.
///   3. If still unmatched → InsufficientCapacity.
pub struct Broker {
    native:   RwLock<Vec<Arc<dyn ComputeProvider>>>,
    external: RwLock<Vec<Arc<dyn ComputeProvider>>>,
}

impl Broker {
    pub fn new() -> Self {
        Self {
            native:   RwLock::new(vec![]),
            external: RwLock::new(vec![]),
        }
    }

    pub fn register_native(&self, provider: Arc<dyn ComputeProvider>) {
        self.native.write().unwrap().push(provider);
    }

    pub fn register_external(&self, provider: Arc<dyn ComputeProvider>) {
        self.external.write().unwrap().push(provider);
    }

    /// Submit a job: native-first, then external overflow.
    pub fn submit(&self, job: Job) -> Result<Allocation, UcxError> {
        // 1. Try native providers.
        {
            let providers = self.native.read().unwrap();
            if let Some(allocation) = Self::try_allocate(&providers, &job) {
                return allocation;
            }
        }

        // 2. Overflow to external if policy permits.
        if job.constraints.allow_external {
            let providers = self.external.read().unwrap();
            if let Some(allocation) = Self::try_allocate(&providers, &job) {
                return allocation;
            }
        }

        Err(UcxError::InsufficientCapacity {
            reason: "no provider matched job requirements".into(),
        })
    }

    fn try_allocate(
        providers: &[Arc<dyn ComputeProvider>],
        job: &Job,
    ) -> Option<Result<Allocation, UcxError>> {
        // Scored best-fit: accumulate all willing providers, pick highest score.
        let mut best: Option<(u32, &Arc<dyn ComputeProvider>)> = None;
        for provider in providers {
            if provider.can_accept(job) {
                let score = score_provider(provider.capability(), &job.requirements);
                if best.map_or(true, |(s, _)| score > s) {
                    best = Some((score, provider));
                }
            }
        }
        best.map(|(_, p)| p.submit(job.clone()))
    }

    pub fn status(&self, job_id: JobId, provider_id: &str) -> Result<JobStatus, UcxError> {
        self.find_provider(provider_id)?.status(job_id)
    }

    pub fn receipt(&self, job_id: JobId, provider_id: &str) -> Result<ComputeReceipt, UcxError> {
        let mut r = self.find_provider(provider_id)?.receipt(job_id)?;
        r.stamp_gix1();
        Ok(r)
    }

    pub fn cancel(&self, job_id: JobId, provider_id: &str) -> Result<(), UcxError> {
        self.find_provider(provider_id)?.cancel(job_id)
    }

    /// After a job completes, notify OSOVM event bridge for GPU_CONTRIBUTION → TOC_MINT.
    /// Fail-open: returns false if OSOVM is unreachable but never blocks job completion.
    pub async fn notify_osovm_on_complete(
        &self,
        receipt: &ComputeReceipt,
        agent_id: &str,
    ) -> bool {
        crate::mint_allowlist::notify_gpu_contribution(
            &receipt.provider_id,
            agent_id,
            &receipt.job_id.to_string(),
            receipt.resources.gpu_seconds,
            receipt.zangbeto_anchor.as_deref(),
        ).await
    }

    fn find_provider(&self, provider_id: &str) -> Result<Arc<dyn ComputeProvider>, UcxError> {
        for p in self.native.read().unwrap().iter() {
            if p.id() == provider_id { return Ok(p.clone()); }
        }
        for p in self.external.read().unwrap().iter() {
            if p.id() == provider_id { return Ok(p.clone()); }
        }
        Err(UcxError::ProviderNotFound { provider_id: provider_id.into() })
    }
}

impl Default for Broker {
    fn default() -> Self { Self::new() }
}

// ── VantageDiscovery dynamic registration ─────────────────────────────────────

/// Proxy provider built from a Vantage-discovered ProviderCapability.
/// Forwards all requests to the real provider via the UCX broker HTTP API.
struct DiscoveredProvider {
    cap:      ProviderCapability,
    base_url: String,
}

impl ComputeProvider for DiscoveredProvider {
    fn id(&self)         -> &str               { &self.cap.provider_id }
    fn capability(&self) -> &ProviderCapability { &self.cap }

    fn can_accept(&self, job: &Job) -> bool {
        job.constraints.allow_external
            && self.cap.gpu.as_ref().map_or(false, |g| {
                job.requirements.vram_gb.map_or(true, |need| g.vram_gb >= need)
            })
    }

    fn submit(&self, job: Job) -> Result<Allocation, UcxError> {
        let url = format!("{}/api/jobs", self.base_url);
        let body = serde_json::json!({ "job": job, "provider_id": self.cap.provider_id });
        let resp = reqwest::blocking::Client::new()
            .post(&url)
            .json(&body)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .map_err(|e| UcxError::Adapter { adapter: self.cap.provider_id.clone(), reason: e.to_string() })?;
        let val: serde_json::Value = resp.json().unwrap_or(serde_json::Value::Null);
        serde_json::from_value(val["allocation"].clone())
            .map_err(|e| UcxError::Adapter { adapter: self.cap.provider_id.clone(), reason: e.to_string() })
    }

    fn status(&self, _job_id: JobId)  -> Result<JobStatus,      UcxError> { Ok(JobStatus::Pending) }
    fn receipt(&self, _job_id: JobId) -> Result<ComputeReceipt, UcxError> {
        Err(UcxError::JobNotFound { job_id: "n/a".into() })
    }
    fn cancel(&self, _job_id: JobId)  -> Result<(),              UcxError> { Ok(()) }
}

impl Broker {
    /// Pull the latest provider list from Vantage and (re-)register them as
    /// external providers.  Existing discovered providers with the same id are
    /// replaced; manually registered native providers are untouched.
    pub async fn refresh_from_vantage(&self, discovery: &crate::VantageDiscovery) {
        let caps = discovery.providers().await;
        let base_url = std::env::var("UCX_BROKER_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7790".to_string());

        let new_providers: Vec<Arc<dyn ComputeProvider>> = caps
            .into_iter()
            .map(|cap| {
                let p: Arc<dyn ComputeProvider> = Arc::new(DiscoveredProvider {
                    cap,
                    base_url: base_url.clone(),
                });
                p
            })
            .collect();

        // Replace external list with fresh discovered set.
        *self.external.write().unwrap() = new_providers;
        tracing::debug!("broker: refreshed {} discovered providers from Vantage", self.external.read().unwrap().len());
    }
}

// ── scoring ───────────────────────────────────────────────────────────────────

/// Score a provider against job requirements. Higher = better match.
/// Max score: 100 pts.
///
/// Distribution:
///   40 pts  VRAM fit      (tight match preferred over massive over-provision)
///   20 pts  GPU count
///   15 pts  Tier
///   15 pts  Price
///   10 pts  Trust
fn score_provider(cap: &ProviderCapability, req: &WorkloadRequirements) -> u32 {
    let mut score: u32 = 0;

    // ── VRAM (40 pts) ─────────────────────────────────────────────────────────
    if let Some(need_vram) = req.vram_gb {
        if let Some(gpu) = &cap.gpu {
            if gpu.vram_gb >= need_vram {
                // Perfect fit = 40; 2× over-provision = 30; 4× = 20; 8× = 10
                let ratio = gpu.vram_gb / need_vram;
                let pts = if ratio < 1.5 {
                    40
                } else if ratio < 2.5 {
                    30
                } else if ratio < 4.0 {
                    20
                } else {
                    10
                };
                score += pts;
            }
            // 0 pts if vram_gb < need_vram (can_accept should have rejected this)
        }
    } else {
        score += 20; // no VRAM requirement → half credit
    }

    // ── GPU count (20 pts) ────────────────────────────────────────────────────
    if let Some(need_gpus) = req.gpu_count {
        if let Some(gpu) = &cap.gpu {
            if gpu.count >= need_gpus {
                // Exact match = 20; extra GPUs waste budget → discount
                let extra = gpu.count.saturating_sub(need_gpus);
                score += 20u32.saturating_sub(extra as u32 * 3);
            }
        }
    } else {
        score += 10;
    }

    // ── Tier (15 pts) ─────────────────────────────────────────────────────────
    score += match (&cap.tier, &req.min_tier) {
        (ProviderTier::Professional, _) => 15,
        (ProviderTier::Community, Some(ProviderTier::Community) | None) => 10,
        (ProviderTier::Community, _) => 10,
        (ProviderTier::Personal, None) => 8,
        (ProviderTier::Personal, _) => 5,
    };

    // ── Price (15 pts) ────────────────────────────────────────────────────────
    let price_pts = if let Some(gpu_price) = cap.price_gpu_hour_cents {
        // $0-$0.30/hr = 15 pts, $0.30-$0.60 = 12, $0.60-$1.50 = 8, $1.50+ = 3
        if gpu_price <= 30 { 15 }
        else if gpu_price <= 60 { 12 }
        else if gpu_price <= 150 { 8 }
        else { 3 }
    } else {
        10  // CPU-only provider
    };
    score += price_pts;

    // ── Trust (10 pts) ────────────────────────────────────────────────────────
    score += match &cap.trust {
        TrustLevel::Tee          => 10,
        TrustLevel::Confidential => 7,
        TrustLevel::Standard     => 4,
    };

    score
}
