// GPU.ai external provider adapter for UCX.
//
// Translates UCX Job/Receipt to/from the GPU.ai OpenAI-compatible API.
// Fine-tuning endpoint: POST /v1/fine_tuning/jobs
// Inference endpoint:   POST /v1/chat/completions
//
// Auth: Bearer token via GPUAI_KEY env var (or constructor injection).
// Marketplace/funding auth: UCX_GPUAI_MASTER_KEY (master key, never exposed to agents).

mod fine_tune;
mod inference;
pub mod marketplace;

pub use fine_tune::GpuAiFineTuneAdapter;
pub use inference::GpuAiInferenceAdapter;
pub use marketplace::{
    AccountBalance, DepositAddress, EarningsReport, GpuAiMarketplaceAdapter, GpuSpec, MachineId,
};
