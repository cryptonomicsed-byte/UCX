use serde::{Deserialize, Serialize};

/// What a provider advertises to the broker.
/// Uses capability flags rather than raw hardware model names so different
/// hardware can participate uniformly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCapability {
    pub provider_id:     String,
    /// Omo-Koda2 agent_id that owns/controls this provider. None = anonymous/unowned.
    pub owner_agent_id:  Option<String>,
    pub tier:            ProviderTier,
    pub trust:           TrustLevel,

    pub gpu:     Option<GpuCapability>,
    pub cpu:     CpuCapability,
    pub ram_gb:  f64,
    pub disk_gb: f64,

    /// OCI-compatible container runtimes available.
    pub runtimes: Vec<RuntimeKind>,

    /// Max price per GPU-hour in USD cents.
    pub price_gpu_hour_cents: Option<u64>,
    /// Max price per CPU-hour in USD cents.
    pub price_cpu_hour_cents: u64,

    /// Workload policy — types this provider will NOT accept.
    pub policy_deny: Vec<String>,

    /// ISO 3166-1 alpha-2 region tags, e.g. ["US", "DE"]. Empty = any.
    pub regions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuCapability {
    pub vendor:   GpuVendor,
    pub model:    String,      // human label, not matched on
    pub vram_gb:  f64,
    pub fp16:     bool,
    pub bf16:     bool,
    pub cuda:     bool,
    pub rocm:     bool,
    pub count:    u8,          // number of identical GPUs
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CpuCapability {
    pub cores:        u32,
    pub arch:         CpuArch,
    pub frequency_mhz: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Apple,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CpuArch {
    X86_64,
    Arm64,
    Riscv64,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum RuntimeKind {
    /// Any OCI-compliant runtime (Docker, Podman, containerd).
    Oci,
    /// Bare process (no container isolation — Tier 0 / trusted only).
    Bare,
}

/// Three-tier marketplace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProviderTier {
    /// Personal devices — private/trusted workloads only.
    Personal,
    /// Homelabs, gaming PCs, small servers — public marketplace.
    Community,
    /// Data centers, enterprise clusters — high SLA.
    Professional,
}

/// Trust/privacy capability level.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum TrustLevel {
    /// Standard container isolation.
    Standard,
    /// Confidential VM + hardware attestation available.
    Confidential,
    /// Full TEE (Trusted Execution Environment) with quote.
    Tee,
}
