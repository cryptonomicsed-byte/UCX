// GPU.ai marketplace adapter — supplier (contribute GPU) + funding (crypto deposit).
//
// API key is read from UCX_GPUAI_MASTER_KEY env var (master key, never from agent request).
// Falls back to GPUAI_KEY for backward-compat during transition.
//
// NOTE: GPU.ai marketplace / supplier endpoints are modelled after their documented
// API surface.  When endpoints are not yet available on a given account tier they
// return 404/403; errors propagate as UcxError::Adapter — the caller decides
// whether to surface or swallow.

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use ucx_protocol::UcxError;

const BASE: &str = "https://api.gpu.ai/v1";

// ── Supplier types ─────────────────────────────────────────────────────────────

/// Specification for a GPU machine being listed on the marketplace.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GpuSpec {
    /// GPU model name e.g. "NVIDIA RTX 3090"
    pub gpu_model: Option<String>,
    /// VRAM in GB
    pub vram_gb: Option<f64>,
    /// Number of GPUs
    pub gpu_count: Option<u8>,
    /// CPU core count
    pub cpu_cores: Option<u32>,
    /// System RAM in GB
    pub ram_gb: Option<f64>,
    /// Disk in GB
    pub disk_gb: Option<f64>,
    /// Asking price per GPU-hour in USD cents (None = auto-price)
    pub price_gpu_hour_cents: Option<u64>,
    /// Geographic region tag e.g. "US", "EU"
    pub region: Option<String>,
}

/// Opaque marketplace machine identifier returned by list_machine.
pub type MachineId = String;

/// Earnings report from contributed compute.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EarningsReport {
    pub total_earned_cents: u64,
    pub pending_payout_cents: u64,
    pub jobs_completed: u64,
    pub gpu_hours_contributed: f64,
    /// Raw API response for forward-compat with new fields.
    pub raw: Value,
}

// ── Funding types ──────────────────────────────────────────────────────────────

/// A crypto deposit address + instructions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositAddress {
    pub currency: String,
    pub address: String,
    pub amount_usd: f64,
    /// Minimum deposit amount in the native currency, if returned by API.
    pub minimum_native: Option<f64>,
    pub raw: Value,
}

/// Account balance snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountBalance {
    pub balance_cents: u64,
    pub currency: String,
    pub raw: Value,
}

// ── Adapter ────────────────────────────────────────────────────────────────────

pub struct GpuAiMarketplaceAdapter {
    /// Master API key — sourced from env, never from agent requests.
    api_key: String,
}

impl GpuAiMarketplaceAdapter {
    /// Construct from env.
    ///
    /// Prefers `UCX_GPUAI_MASTER_KEY` (master key, never leaves broker).
    /// Falls back to `GPUAI_KEY` for backward compat.
    /// Returns `None` if neither key is set.
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("UCX_GPUAI_MASTER_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("GPUAI_KEY").ok().filter(|s| !s.is_empty()))?;
        Some(Self { api_key: key })
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.api_key)
    }

    fn client(&self) -> Client {
        Client::new()
    }

    // ── Supplier API ─────────────────────────────────────────────────────────

    /// List (contribute) own GPU on the GPU.ai marketplace.
    ///
    /// Maps to POST /v1/machines or /v1/supplier/machines depending on API version.
    pub fn list_machine_sync(&self, spec: GpuSpec) -> Result<MachineId, UcxError> {
        let body = json!({
            "gpu_model":              spec.gpu_model,
            "vram_gb":                spec.vram_gb,
            "gpu_count":              spec.gpu_count.unwrap_or(1),
            "cpu_cores":              spec.cpu_cores,
            "ram_gb":                 spec.ram_gb,
            "disk_gb":                spec.disk_gb,
            "price_gpu_hour_cents":   spec.price_gpu_hour_cents,
            "region":                 spec.region,
        });

        let resp = self.client()
            .post(format!("{BASE}/supplier/machines"))
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "gpu.ai".into(), reason: e.to_string() })?;

        let status = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);

        if !status.is_success() {
            return Err(UcxError::Adapter {
                adapter: "gpu.ai".into(),
                reason: format!("POST /supplier/machines {status}: {val}"),
            });
        }

        let machine_id = val["id"].as_str()
            .or_else(|| val["machine_id"].as_str())
            .unwrap_or("unknown")
            .to_string();

        tracing::info!(machine_id, "listed GPU machine on gpu.ai marketplace");
        Ok(machine_id)
    }

    /// Check earnings from contributed GPU compute.
    pub fn check_earnings_sync(&self) -> Result<EarningsReport, UcxError> {
        let resp = self.client()
            .get(format!("{BASE}/supplier/earnings"))
            .header("Authorization", self.auth_header())
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "gpu.ai".into(), reason: e.to_string() })?;

        let status = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);

        if !status.is_success() {
            return Err(UcxError::Adapter {
                adapter: "gpu.ai".into(),
                reason: format!("GET /supplier/earnings {status}: {val}"),
            });
        }

        let report = EarningsReport {
            total_earned_cents:      val["total_earned_cents"].as_u64().unwrap_or(0),
            pending_payout_cents:    val["pending_payout_cents"].as_u64().unwrap_or(0),
            jobs_completed:          val["jobs_completed"].as_u64().unwrap_or(0),
            gpu_hours_contributed:   val["gpu_hours_contributed"].as_f64().unwrap_or(0.0),
            raw: val,
        };

        Ok(report)
    }

    /// Remove (reclaim) a previously listed machine from the marketplace.
    pub fn reclaim_machine_sync(&self, machine_id: &MachineId) -> Result<(), UcxError> {
        let resp = self.client()
            .delete(format!("{BASE}/supplier/machines/{machine_id}"))
            .header("Authorization", self.auth_header())
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "gpu.ai".into(), reason: e.to_string() })?;

        let status = resp.status();
        if !status.is_success() {
            let val: Value = resp.json().unwrap_or(Value::Null);
            return Err(UcxError::Adapter {
                adapter: "gpu.ai".into(),
                reason: format!("DELETE /supplier/machines/{machine_id} {status}: {val}"),
            });
        }

        tracing::info!(machine_id, "reclaimed GPU machine from gpu.ai marketplace");
        Ok(())
    }

    // ── Funding API ──────────────────────────────────────────────────────────

    /// Initiate a crypto deposit.
    ///
    /// `currency` — e.g. "BTC", "ETH", "USDC"
    /// `amount_usd` — target deposit amount in USD (API converts to native)
    pub fn deposit_crypto_sync(&self, currency: &str, amount_usd: f64) -> Result<DepositAddress, UcxError> {
        let body = json!({
            "currency":   currency,
            "amount_usd": amount_usd,
        });

        let resp = self.client()
            .post(format!("{BASE}/billing/crypto/deposit"))
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "gpu.ai".into(), reason: e.to_string() })?;

        let status = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);

        if !status.is_success() {
            return Err(UcxError::Adapter {
                adapter: "gpu.ai".into(),
                reason: format!("POST /billing/crypto/deposit {status}: {val}"),
            });
        }

        let address = DepositAddress {
            currency:        currency.to_string(),
            address:         val["address"].as_str().unwrap_or("").to_string(),
            amount_usd,
            minimum_native:  val["minimum_native"].as_f64(),
            raw: val,
        };

        tracing::info!(currency, amount_usd, address = %address.address, "crypto deposit initiated");
        Ok(address)
    }

    /// Fetch current account balance.
    pub fn check_balance_sync(&self) -> Result<AccountBalance, UcxError> {
        let resp = self.client()
            .get(format!("{BASE}/billing/balance"))
            .header("Authorization", self.auth_header())
            .send()
            .map_err(|e| UcxError::Adapter { adapter: "gpu.ai".into(), reason: e.to_string() })?;

        let status = resp.status();
        let val: Value = resp.json().unwrap_or(Value::Null);

        if !status.is_success() {
            return Err(UcxError::Adapter {
                adapter: "gpu.ai".into(),
                reason: format!("GET /billing/balance {status}: {val}"),
            });
        }

        let balance = AccountBalance {
            balance_cents: val["balance_cents"].as_u64()
                .unwrap_or_else(|| {
                    // Some APIs return balance as a float in USD
                    val["balance_usd"].as_f64().map(|f| (f * 100.0) as u64).unwrap_or(0)
                }),
            currency: val["currency"].as_str().unwrap_or("USD").to_string(),
            raw: val,
        };

        Ok(balance)
    }
}
