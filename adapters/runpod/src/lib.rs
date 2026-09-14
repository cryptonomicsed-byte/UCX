// RunPod external provider adapter for UCX.
//
// Supports two RunPod compute surfaces:
//   - Serverless: POST /v2/{endpoint_id}/run  (fire-and-forget, poll for result)
//   - Pod Rental: POST /graphql  (create a pod with a container image)
//
// Auth: Bearer token via RUNPOD_API_KEY env var.
// Docs: https://docs.runpod.io/serverless/references/operations

mod serverless;
mod pod;

pub use serverless::RunPodServerlessAdapter;
pub use pod::RunPodPodAdapter;
