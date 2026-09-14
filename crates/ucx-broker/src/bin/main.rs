//! ucx-broker — HTTP server for the Universal Compute Exchange matching engine.
//!
//! Config (env vars):
//!   UCX_PORT         — listen port (default 7790)
//!   VANTAGE_URL      — Vantage rendezvous base URL (optional, enables native provider discovery)
//!   VANTAGE_KEY      — Vantage API key (used for /api/ucx/providers query)
//!   GPUAI_KEY        — GPU.ai API key (enables gpu-ai external adapter)
//!
//! Routes:
//!   POST /api/jobs              — submit a compute job
//!   GET  /api/jobs/:id          — poll job status
//!   GET  /api/jobs/:id/receipt  — retrieve completed receipt
//!   DELETE /api/jobs/:id        — cancel a job
//!   GET  /health                — liveness probe

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use ucx_broker::{Broker, VantageDiscovery};
use ucx_protocol::{
    ComputeConstraints, Job, TrustLevel, WorkloadRequirements, WorkloadType,
};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    broker:    Arc<Broker>,
    discovery: Option<Arc<VantageDiscovery>>,
}

// ── job submission body ───────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct SubmitBody {
    submitter_id: String,
    workload:     Option<String>,
    requirements: Option<Value>,
    constraints:  Option<Value>,
    runtime_spec: Option<Value>,
}

fn parse_workload(s: &str) -> WorkloadType {
    match s {
        "Inference"  => WorkloadType::Inference,
        "Training"   => WorkloadType::Training,
        "Rendering"  => WorkloadType::Rendering,
        "Simulation" => WorkloadType::Simulation,
        "CiCd"       => WorkloadType::CiCd,
        _            => WorkloadType::Generic,
    }
}

fn parse_requirements(v: Option<&Value>) -> WorkloadRequirements {
    let v = v.and_then(|x| x.as_object()).cloned().unwrap_or_default();
    WorkloadRequirements {
        vram_gb:   v.get("vram_gb").and_then(|x| x.as_f64()),
        ram_gb:    v.get("ram_gb").and_then(|x| x.as_f64()),
        cpu_cores: v.get("cpu_cores").and_then(|x| x.as_u64()).map(|n| n as u32),
        gpu_count: v.get("gpu_count").and_then(|x| x.as_u64()).map(|n| n as u8),
        fp16:      v.get("fp16").and_then(|x| x.as_bool()).unwrap_or(false),
        bf16:      v.get("bf16").and_then(|x| x.as_bool()).unwrap_or(false),
        cuda:      v.get("cuda").and_then(|x| x.as_bool()).unwrap_or(false),
        min_tier:  None,
    }
}

fn parse_constraints(v: Option<&Value>) -> ComputeConstraints {
    let v = v.and_then(|x| x.as_object()).cloned().unwrap_or_default();
    ComputeConstraints {
        max_price_cents:  v.get("max_price_cents").and_then(|x| x.as_u64()),
        max_queue_secs:   v.get("max_queue_secs").and_then(|x| x.as_u64()),
        privacy:          TrustLevel::Standard,
        regions:          vec![],
        allow_external:   v.get("allow_external").and_then(|x| x.as_bool()).unwrap_or(true),
    }
}

// ── handlers ─────────────────────────────────────────────────────────────────

async fn submit_job(
    State(state): State<AppState>,
    Json(body): Json<SubmitBody>,
) -> impl IntoResponse {
    // Gap #74: on each submission, refresh discovered providers from Vantage
    // and (re-)register them as external providers in the broker.
    if let Some(ref disc) = state.discovery {
        state.broker.refresh_from_vantage(disc).await;
    }

    let job = Job::new(
        body.submitter_id,
        parse_workload(body.workload.as_deref().unwrap_or("Generic")),
        parse_requirements(body.requirements.as_ref()),
        parse_constraints(body.constraints.as_ref()),
        body.runtime_spec.unwrap_or(json!({})),
    );
    let job_id = job.id;

    match state.broker.submit(job) {
        Ok(allocation) => {
            tracing::info!(job_id = %job_id, provider = %allocation.provider_id, "job allocated");
            (StatusCode::OK, Json(json!({
                "job_id":      allocation.job_id,
                "provider_id": allocation.provider_id,
                "status":      format!("{:?}", allocation.status),
                "allocated_at": allocation.allocated_at.to_rfc3339(),
            })))
        }
        Err(e) => {
            tracing::warn!(job_id = %job_id, error = %e, "job allocation failed");
            (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": e.to_string() })))
        }
    }
}

async fn get_job_status(
    State(state): State<AppState>,
    Path((job_id_str, provider_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let job_id = match Uuid::parse_str(&job_id_str) {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid job_id" }))),
    };
    match state.broker.status(job_id, &provider_id) {
        Ok(status) => (StatusCode::OK, Json(json!({ "status": format!("{status:?}") }))),
        Err(e)     => (StatusCode::NOT_FOUND, Json(json!({ "error": e.to_string() }))),
    }
}

async fn get_receipt(
    State(state): State<AppState>,
    Path((job_id_str, provider_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let job_id = match Uuid::parse_str(&job_id_str) {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid job_id" }))),
    };
    match state.broker.receipt(job_id, &provider_id) {
        Ok(receipt) => {
            let hash = receipt.hash();

            // Gap #22: wire UCX receipt → OSOVM settlement data flow.
            // Notify OSOVM of GPU contribution when the receipt is fetched.
            // Fail-open: never blocks receipt delivery.
            if receipt.resources.gpu_seconds > 0.0 {
                let broker = state.broker.clone();
                let agent_id = std::env::var("UCX_AGENT_ID")
                    .unwrap_or_else(|_| receipt.provider_id.clone());
                let r = receipt.clone();
                tokio::spawn(async move {
                    let notified = broker.notify_osovm_on_complete(&r, &agent_id).await;
                    tracing::debug!(
                        job_id = %r.job_id,
                        gpu_seconds = r.resources.gpu_seconds,
                        notified,
                        "OSOVM GPU_CONTRIBUTION notification"
                    );
                });
            }

            (StatusCode::OK, Json(json!({
                "job_id":       receipt.job_id,
                "provider_id":  receipt.provider_id,
                "completed_at": receipt.completed_at.to_rfc3339(),
                "resources":    receipt.resources,
                "billing":      receipt.billing,
                "verification": receipt.verification,
                "receipt_hash": hash,
            })))
        }
        Err(e) => (StatusCode::NOT_FOUND, Json(json!({ "error": e.to_string() }))),
    }
}

async fn cancel_job(
    State(state): State<AppState>,
    Path((job_id_str, provider_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let job_id = match Uuid::parse_str(&job_id_str) {
        Ok(id) => id,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid job_id" }))),
    };
    match state.broker.cancel(job_id, &provider_id) {
        Ok(())  => (StatusCode::OK,        Json(json!({ "cancelled": true }))),
        Err(e)  => (StatusCode::NOT_FOUND, Json(json!({ "error": e.to_string() }))),
    }
}

async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "ucx-broker" }))
}

// ── startup ───────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let port: u16 = std::env::var("UCX_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(7790);

    let broker = Arc::new(Broker::new());

    // Register local machine as a native provider.
    {
        use ucx_provider::LocalProvider;
        let local = Arc::new(LocalProvider::new("local"));
        broker.register_native(local);
        tracing::info!("registered local provider");
    }

    // Register GPU.ai external adapters if GPUAI_KEY is set.
    #[cfg(feature = "gpu-ai")]
    {
        use ucx_adapter_gpu_ai::{GpuAiFineTuneAdapter, GpuAiInferenceAdapter};
        if let Some(ft) = GpuAiFineTuneAdapter::from_env() {
            broker.register_external(Arc::new(ft));
            tracing::info!("registered gpu.ai fine-tune adapter");
        }
        if let Some(inf) = GpuAiInferenceAdapter::from_env() {
            broker.register_external(Arc::new(inf));
            tracing::info!("registered gpu.ai inference adapter");
        }
    }

    // Register Akash Network adapter if AKASH_KEY is set.
    #[cfg(feature = "akash")]
    {
        use ucx_adapter_akash::AkashAdapter;
        if let Some(akash) = AkashAdapter::from_env() {
            broker.register_external(Arc::new(akash));
            tracing::info!("registered akash network adapter");
        }
    }

    // Register Vast.ai adapter if VAST_KEY is set.
    #[cfg(feature = "vast")]
    {
        use ucx_adapter_vast::VastAdapter;
        if let Some(vast) = VastAdapter::from_env() {
            broker.register_external(Arc::new(vast));
            tracing::info!("registered vast.ai adapter");
        }
    }

    // Register RunPod adapters if RUNPOD_API_KEY is set.
    #[cfg(feature = "runpod")]
    {
        use ucx_adapter_runpod::{RunPodServerlessAdapter, RunPodPodAdapter};
        if let Some(serverless) = RunPodServerlessAdapter::from_env() {
            broker.register_external(Arc::new(serverless));
            tracing::info!("registered runpod serverless adapter");
        }
        if let Some(pod) = RunPodPodAdapter::from_env() {
            broker.register_external(Arc::new(pod));
            tracing::info!("registered runpod pod adapter");
        }
    }

    let discovery = VantageDiscovery::from_env().map(Arc::new);
    if discovery.is_some() {
        tracing::info!("Vantage provider discovery enabled");
    }

    let state = AppState { broker, discovery };

    let app = Router::new()
        .route("/api/jobs",                               post(submit_job))
        .route("/api/jobs/{job_id}/{provider_id}",          get(get_job_status).delete(cancel_job))
        .route("/api/jobs/{job_id}/{provider_id}/receipt",  get(get_receipt))
        .route("/health",                                  get(health))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("ucx-broker listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
