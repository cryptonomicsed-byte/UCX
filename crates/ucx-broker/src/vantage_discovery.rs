use ucx_protocol::ProviderCapability;

/// Fetches active ProviderCapability records from Vantage's UCX rendezvous.
///
/// Called by the Broker before each submission to refresh its view of native
/// providers.  Cached for `cache_secs` to avoid hammering Vantage on every job.
pub struct VantageDiscovery {
    base_url:   String,
    api_key:    String,
    cache_secs: u64,
    cached_at:  std::sync::Mutex<Option<(std::time::Instant, Vec<ProviderCapability>)>>,
}

impl VantageDiscovery {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            base_url:   base_url.into().trim_end_matches('/').to_string(),
            api_key:    api_key.into(),
            cache_secs: 30,
            cached_at:  std::sync::Mutex::new(None),
        }
    }

    pub fn from_env() -> Option<Self> {
        let url = std::env::var("VANTAGE_URL").ok().filter(|s| !s.is_empty())?;
        let key = std::env::var("VANTAGE_KEY").unwrap_or_default();
        Some(Self::new(url, key))
    }

    /// Returns the cached capability list if fresh, otherwise fetches from Vantage.
    pub async fn providers(&self) -> Vec<ProviderCapability> {
        // Check cache.
        {
            let guard = self.cached_at.lock().unwrap();
            if let Some((at, ref caps)) = *guard {
                if at.elapsed().as_secs() < self.cache_secs {
                    return caps.clone();
                }
            }
        }

        // Fetch.
        let fresh = self.fetch().await.unwrap_or_default();
        *self.cached_at.lock().unwrap() = Some((std::time::Instant::now(), fresh.clone()));
        fresh
    }

    async fn fetch(&self) -> Result<Vec<ProviderCapability>, String> {
        let url = format!("{}/api/ucx/providers", self.base_url);
        let resp = reqwest::Client::new()
            .get(&url)
            .header("X-Agent-Key", &self.api_key)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| format!("VantageDiscovery: {e}"))?;

        if !resp.status().is_success() {
            return Err(format!("VantageDiscovery: {}", resp.status()));
        }

        let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let raw = val["providers"].as_array().cloned().unwrap_or_default();

        let caps: Vec<ProviderCapability> = raw
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect();

        tracing::debug!(count = caps.len(), "VantageDiscovery: refreshed provider list");
        Ok(caps)
    }
}
