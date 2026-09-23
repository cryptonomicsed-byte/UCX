// UCX CapabilityToken mediator (Phase 9.1)
//
// Agents access GPU resources via a short-lived CapabilityToken rather than
// holding the master API key.  The master key (UCX_GPUAI_MASTER_KEY) never
// leaves the ucx-broker process.
//
// Token wire format:
//   cap:v1:<agent_id>:<tool_name>:<expires_at_unix_secs>
//
// HMAC-SHA256(UCX_GPUAI_MASTER_KEY, wire_format) is hex-encoded and appended:
//   <wire_format>.<hmac_hex>

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use hmac::{Hmac, Mac};
use hex;

type HmacSha256 = Hmac<Sha256>;

/// A short-lived bearer token issued to an agent for one tool+tier.
/// TTL: 1 hour (3600 seconds).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityToken {
    pub agent_id:   String,
    pub tool_name:  String,
    pub tier:       String,
    pub expires_at: DateTime<Utc>,
    /// HMAC-SHA256 signed token string (opaque to caller).
    pub token:      String,
}

impl CapabilityToken {
    /// TTL for newly minted tokens.
    pub const TTL_SECS: i64 = 3600;

    /// Mint a new CapabilityToken.
    ///
    /// Returns `None` when `UCX_GPUAI_MASTER_KEY` is not set.
    pub fn mint(agent_id: &str, tool_name: &str, tier: &str) -> Option<Self> {
        let master_key = std::env::var("UCX_GPUAI_MASTER_KEY").ok()
            .filter(|s| !s.is_empty())?;

        let expires_at = Utc::now() + chrono::Duration::seconds(Self::TTL_SECS);
        let expires_ts  = expires_at.timestamp();

        let payload = format!("cap:v1:{}:{}:{}", agent_id, tool_name, expires_ts);
        let sig     = hmac_sign(&master_key, &payload);
        let token   = format!("{}.{}", payload, sig);

        Some(Self {
            agent_id:   agent_id.to_string(),
            tool_name:  tool_name.to_string(),
            tier:       tier.to_string(),
            expires_at,
            token,
        })
    }

    /// Verify a raw token string.
    ///
    /// Returns `Some(CapabilityToken)` when:
    ///   1. `UCX_GPUAI_MASTER_KEY` is set.
    ///   2. HMAC signature is valid.
    ///   3. Token has not expired.
    ///
    /// Returns `None` (fail-open not allowed here — invalid = rejected) otherwise.
    pub fn verify(raw: &str) -> Option<Self> {
        let master_key = std::env::var("UCX_GPUAI_MASTER_KEY").ok()
            .filter(|s| !s.is_empty())?;

        // Split on last '.' to get payload / signature
        let dot_pos = raw.rfind('.')?;
        let payload = &raw[..dot_pos];
        let sig_hex = &raw[dot_pos + 1..];

        // Constant-time HMAC compare
        let expected = hmac_sign(&master_key, payload);
        if !constant_eq(sig_hex.as_bytes(), expected.as_bytes()) {
            return None;
        }

        // Parse payload: cap:v1:<agent_id>:<tool_name>:<expires_ts>
        let mut parts = payload.splitn(6, ':');
        let _cap   = parts.next()?;  // "cap"
        let _v1    = parts.next()?;  // "v1"
        let agent_id  = parts.next()?.to_string();
        let tool_name = parts.next()?.to_string();
        let expires_ts: i64 = parts.next()?.parse().ok()?;

        let expires_at = chrono::DateTime::from_timestamp(expires_ts, 0)
            .map(|dt| dt.with_timezone(&Utc))?;

        if Utc::now() > expires_at {
            tracing::debug!(agent_id, "capability token expired");
            return None;
        }

        Some(Self {
            agent_id,
            tool_name,
            tier: String::new(),   // tier not encoded in token — caller stores separately
            expires_at,
            token: raw.to_string(),
        })
    }

    /// Whether the master key gate is enabled (i.e. UCX_GPUAI_MASTER_KEY is set).
    pub fn gate_enabled() -> bool {
        std::env::var("UCX_GPUAI_MASTER_KEY")
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn hmac_sign(key: &str, message: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(key.as_bytes())
        .expect("HMAC can accept any key length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Constant-time byte comparison to resist timing attacks.
fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ── request / response wire types for the HTTP layer ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct MintTokenRequest {
    pub agent_id:  String,
    pub tool_name: String,
    pub tier:      Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MintTokenResponse {
    pub token:      String,
    pub expires_at: String,   // RFC 3339
}
