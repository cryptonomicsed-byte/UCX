use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::job::JobId;

/// The atomic unit of the compute economy.
///
/// One receipt per completed job.  UCX defines the transport representation;
/// OSOVM / Zàngbétò is authoritative for sovereign execution proofs.
/// Integrate by embedding `zangbeto_anchor` when the receipt flows through OSOVM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeReceipt {
    pub job_id:       JobId,
    pub provider_id:  String,
    pub completed_at: DateTime<Utc>,

    pub resources:    ResourceUsage,
    pub billing:      BillingRecord,
    pub verification: VerificationProof,

    /// Optional anchor into OSOVM / Zàngbétò receipt chain.
    /// Set by the ucx-osovm integration crate when settlement flows through OSOVM.
    pub zangbeto_anchor: Option<String>,
}

impl ComputeReceipt {
    /// Canonical receipt hash — stable identifier for anchoring / deduplication.
    pub fn hash(&self) -> String {
        let canonical = serde_json::json!({
            "job_id":      self.job_id,
            "provider_id": self.provider_id,
            "completed_at": self.completed_at.to_rfc3339(),
            "resources":   self.resources,
            "billing":     self.billing,
        });
        let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
        let digest = Sha256::digest(&bytes);
        hex::encode(digest)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub gpu_seconds:     f64,
    pub cpu_seconds:     f64,
    pub ram_gb_seconds:  f64,
    pub storage_gb:      f64,
    pub egress_gb:       f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BillingRecord {
    /// Total charged in USD cents.
    pub amount_cents: u64,
    /// Currency — reserved for future ASE/on-chain settlement.
    pub currency:     BillingCurrency,
    pub line_items:   Vec<LineItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineItem {
    pub label:       String,
    pub cents:       u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BillingCurrency {
    Usd,
    /// Future: Àṣẹ token settlement via OSOVM.
    Ase,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationProof {
    /// SHA-256 of the output artifact(s).
    pub artifact_hash:      Option<String>,
    /// Runtime attestation — opaque, provider-specific.
    pub runtime_attestation: Option<String>,
    /// Execution hash — deterministic fingerprint of the computation.
    pub execution_hash:     Option<String>,
}
