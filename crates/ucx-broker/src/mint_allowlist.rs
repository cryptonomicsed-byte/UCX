use serde_json::json;

fn osovm_base() -> String {
    std::env::var("OSOVM_URL").unwrap_or_else(|_| "http://127.0.0.1:7780".to_string())
}

/// Fetch a Zàngbétò anchor for the given job from the Zàngbétò witness service.
///
/// Reads `ZANGBETO_URL` from the environment.  If the variable is absent or the
/// service is unreachable the function logs a warning and returns `None`
/// (fail-open: the receipt is still finalized, just without a sovereign anchor).
pub async fn get_zangbeto_anchor(job_id: &str) -> Option<String> {
    let base = match std::env::var("ZANGBETO_URL") {
        Ok(u) => u,
        Err(_) => {
            tracing::warn!(
                job_id,
                "ZANGBETO_URL not set — ComputeReceipt will have no zangbeto_anchor; \
                 mint trust level will be reduced"
            );
            return None;
        }
    };

    let client = reqwest::Client::new();
    let url = format!("{}/api/anchor", base);
    let body = json!({ "job_id": job_id, "receipt_type": "compute" });

    match client
        .post(&url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<serde_json::Value>().await {
                Ok(v) => {
                    let anchor = v["anchor"]
                        .as_str()
                        .or_else(|| v["zangbeto_anchor"].as_str())
                        .map(|s| s.to_owned());
                    if anchor.is_none() {
                        tracing::warn!(
                            job_id,
                            "Zàngbétò returned success but response contained no anchor field"
                        );
                    }
                    anchor
                }
                Err(e) => {
                    tracing::warn!(job_id, error = %e, "Zàngbétò anchor response parse failed");
                    None
                }
            }
        }
        Ok(resp) => {
            tracing::warn!(
                job_id,
                status = resp.status().as_u16(),
                "Zàngbétò anchor request returned non-2xx; continuing without anchor"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                job_id,
                error = %e,
                "Zàngbétò unreachable — ComputeReceipt will have no zangbeto_anchor; \
                 mint trust level will be reduced"
            );
            None
        }
    }
}

/// Check if a provider is eligible for Synapse minting via OSOVM.
/// A provider is eligible if:
///   1. The job had verified GPU work (gpu_seconds > 0)
///   2. OSOVM's allowlist includes this provider_id (or allowlist is open)
///   3. The zangbeto_anchor exists (settlement completed, higher trust)
///      — absent anchor is allowed but emits a warning and reduces trust.
pub async fn check_mint_eligible(
    provider_id: &str,
    gpu_seconds: f64,
    zangbeto_anchor: Option<&str>,
) -> bool {
    if gpu_seconds <= 0.0 { return false; }
    if zangbeto_anchor.is_none() {
        tracing::warn!(
            provider_id,
            "no zangbeto_anchor on ComputeReceipt — allowing mint at reduced trust; \
             ensure ZANGBETO_URL is configured for full sovereign settlement"
        );
        // Fall through: OSOVM will apply a lower-trust multiplier rather than blocking.
    }

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
