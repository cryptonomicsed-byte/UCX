// GPU.ai external provider adapter for UCX.
//
// Translates UCX Job/Receipt to/from the GPU.ai OpenAI-compatible API.
// Fine-tuning endpoint: POST /v1/fine_tuning/jobs
// Inference endpoint:   POST /v1/chat/completions
//
// Auth: Bearer token via GPUAI_KEY env var (or constructor injection).

mod fine_tune;
mod inference;

pub use fine_tune::GpuAiFineTuneAdapter;
pub use inference::GpuAiInferenceAdapter;
