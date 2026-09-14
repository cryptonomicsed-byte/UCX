use serde_json::json;

fn osovm_base() -> String {
    std::env::var("OSOVM_URL").unwrap_or_else(|_| "http://127.0.0.1:7780".to_string())
}

/// Check if a provider is eligible for Synapse minting via OSOVM.
/// A provider is eligible if:
///   1. The job had verified GPU work (gpu_seconds > 0)
///   2. OSOVM's allowlist includes this provider_id (or allowlist is open)
///   3. The zangbeto_anchor exists (settlement completed)
pub async fn check_mint_eligible(
    provider_id: &str,
    gpu_seconds: f64,
    zangbeto_anchor: Option<&str>,
) -> bool {
    if gpu_seconds <= 0.0 { return false; }
    if zangbeto_anchor.is_none() { return false; }

    let client = reqwest::Client::new();
    let url = format!("{}/api/toc/allowlist/check", osovm_base());
    let body = json!({
        "provider_id":      provider_id,
        "gpu_seconds":      gpu_seconds,
        "zangbeto_anchor":  zangbeto_anchor,
    });

    client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()
        .and_then(|r| {
            if r.status().is_success() {
                Some(true)
            } else if r.status().as_u16() == 403 {
                tracing::debug!("provider {} not on mint allowlist", provider_id);
                Some(false)
            } else {
                // OSOVM unreachable: fail-open → allow mint attempt
                // OSOVM will do final verification before actual mint
                None
            }
        })
        .unwrap_or(true) // fail-open: OSOVM does final gate
}

/// Notify OSOVM event bridge of a completed GPU job.
/// This triggers GPU_CONTRIBUTION (0x3f) → TOC_MINT (0x54) chain in OSOVM.
pub async fn notify_gpu_contribution(
    provider_id: &str,
    agent_id: &str,
    job_id: &str,
    gpu_seconds: f64,
    zangbeto_anchor: Option<&str>,
) -> bool {
    if !check_mint_eligible(provider_id, gpu_seconds, zangbeto_anchor).await {
        tracing::debug!("mint not eligible for provider {}", provider_id);
        return false;
    }

    let client = reqwest::Client::new();
    let url = format!("{}/api/osovm/gpu_contribution", osovm_base());
    let body = json!({
        "provider_id":      provider_id,
        "submitter_id":     agent_id,
        "job_id":           job_id,
        "gpu_seconds":      gpu_seconds,
        "zangbeto_anchor":  zangbeto_anchor,
        "timestamp": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    });

    client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}
