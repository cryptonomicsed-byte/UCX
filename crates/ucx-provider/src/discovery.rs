use ucx_protocol::capability::{
    CpuArch, CpuCapability, GpuCapability, GpuVendor, ProviderCapability,
    ProviderTier, RuntimeKind, TrustLevel,
};

/// Probe the local machine and return a best-effort ProviderCapability.
/// Read-only; never mutates system state.
///
/// On Linux: reads /proc/cpuinfo, /proc/meminfo, nvidia-smi if available.
/// On all platforms: falls back to safe defaults if files aren't accessible.
pub fn discover_capability(provider_id: &str) -> ProviderCapability {
    let cpu   = discover_cpu();
    let gpu   = discover_gpu();
    let ram   = discover_ram_gb();
    let disk  = discover_disk_gb();
    let runtimes = discover_runtimes();

    ProviderCapability {
        provider_id:           provider_id.to_string(),
        owner_agent_id:        None,
        tier:                  ProviderTier::Community,
        trust:                 TrustLevel::Standard,
        gpu,
        cpu,
        ram_gb:                ram,
        disk_gb:               disk,
        runtimes,
        price_gpu_hour_cents:  None,   // owner sets manually
        price_cpu_hour_cents:  2,      // $0.02/hr default
        policy_deny:           vec![],
        regions:               vec![],
    }
}

fn discover_cpu() -> CpuCapability {
    let arch = detect_arch();

    #[cfg(target_os = "linux")]
    {
        if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") {
            let cores = info.lines()
                .filter(|l| l.starts_with("processor"))
                .count() as u32;
            let freq = info.lines()
                .find(|l| l.starts_with("cpu MHz"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|s| s.trim().parse::<f64>().ok())
                .map(|f| f as u32);
            return CpuCapability { cores: cores.max(1), arch, frequency_mhz: freq };
        }
    }

    CpuCapability { cores: num_cpus_fallback(), arch, frequency_mhz: None }
}

fn discover_ram_gb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(info) = std::fs::read_to_string("/proc/meminfo") {
            if let Some(line) = info.lines().find(|l| l.starts_with("MemTotal:")) {
                if let Some(kb_str) = line.split_whitespace().nth(1) {
                    if let Ok(kb) = kb_str.parse::<f64>() {
                        return kb / (1024.0 * 1024.0);
                    }
                }
            }
        }
    }
    4.0 // conservative default
}

fn discover_disk_gb() -> f64 {
    100.0 // placeholder — proper impl uses statvfs
}

fn discover_gpu() -> Option<GpuCapability> {
    // Only attempt nvidia-smi discovery; AMD/Intel discovery is a future pass.
    if let Ok(output) = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name,memory.total", "--format=csv,noheader,nounits"])
        .output()
    {
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines().take(1) {
                let parts: Vec<&str> = line.splitn(2, ',').collect();
                if parts.len() == 2 {
                    let model   = parts[0].trim().to_string();
                    let vram_mb = parts[1].trim().parse::<f64>().unwrap_or(0.0);
                    return Some(GpuCapability {
                        vendor:  GpuVendor::Nvidia,
                        model,
                        vram_gb: vram_mb / 1024.0,
                        fp16:    true,
                        bf16:    true,
                        cuda:    true,
                        rocm:    false,
                        count:   count_nvidia_gpus(),
                    });
                }
            }
        }
    }
    None
}

fn count_nvidia_gpus() -> u8 {
    std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=name", "--format=csv,noheader"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count() as u8)
        .unwrap_or(1)
}

fn discover_runtimes() -> Vec<RuntimeKind> {
    let mut runtimes = vec![];
    for cmd in &["docker", "podman"] {
        if std::process::Command::new(cmd)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            runtimes.push(RuntimeKind::Oci);
            break;
        }
    }
    runtimes
}

fn detect_arch() -> CpuArch {
    if cfg!(target_arch = "x86_64")  { return CpuArch::X86_64; }
    if cfg!(target_arch = "aarch64") { return CpuArch::Arm64; }
    if cfg!(target_arch = "riscv64") { return CpuArch::Riscv64; }
    CpuArch::Other(std::env::consts::ARCH.to_string())
}

fn num_cpus_fallback() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}
