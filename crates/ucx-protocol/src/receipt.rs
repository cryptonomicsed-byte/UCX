use chrono::{DateTime, Utc};
use gix_types::{Gix1, GixKind, GixNamespace, RoutingHints};
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

    /// GIX1 canonical_id (hex) — `Gix1(Receipt, OsovmExecution, job_id)`.
    /// Stamped via `stamp_gix1()` after receipt creation.
    #[serde(default)]
    pub gix1_canonical_id: Option<String>,
}

impl ComputeReceipt {
    /// Stamp a GIX1 Receipt envelope onto this receipt (idempotent).
    pub fn stamp_gix1(&mut self) {
        if self.gix1_canonical_id.is_some() { return; }
        let ts = self.completed_at.timestamp_millis() as u64;
        let env = Gix1::new(
            GixKind::Receipt,
            GixNamespace::OsovmExecution,
            self.job_id.to_string().as_bytes(),
            None,
            ts,
            RoutingHints::default(),
        );
        self.gix1_canonical_id = Some(hex::encode(env.canonical_id));
    }

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

#[cfg(test)]
mod gix_tests {
    use super::*;
    use crate::job::JobId;
    use chrono::Utc;

    fn make_receipt() -> ComputeReceipt {
        ComputeReceipt {
            job_id:       JobId::new_v4(),
            provider_id:  "test-provider".into(),
            completed_at: Utc::now(),
            resources: ResourceUsage { gpu_seconds: 1.0, cpu_seconds: 1.0, ram_gb_seconds: 0.0, storage_gb: 0.0, egress_gb: 0.0 },
            billing: BillingRecord { amount_cents: 0, currency: BillingCurrency::Usd, line_items: vec![] },
            verification: VerificationProof { artifact_hash: None, runtime_attestation: None, execution_hash: None },
            zangbeto_anchor:   None,
            gix1_canonical_id: None,
        }
    }

    #[test]
    fn stamp_gix1_sets_canonical_id() {
        let mut r = make_receipt();
        r.stamp_gix1();
        assert!(r.gix1_canonical_id.is_some());
        assert_eq!(r.gix1_canonical_id.unwrap().len(), 64);
    }

    #[test]
    fn stamp_gix1_is_idempotent() {
        let mut r = make_receipt();
        r.stamp_gix1();
        let first = r.gix1_canonical_id.clone();
        r.stamp_gix1();
        assert_eq!(r.gix1_canonical_id, first);
    }
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
