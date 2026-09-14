// Vast.ai external provider adapter for UCX.
//
// Translates UCX Job/Receipt to/from the Vast.ai REST API v0.
// Searches available offers matching job requirements, creates an instance,
// and polls for running state.
//
// Auth: VAST_KEY env var (Vast.ai API key from console.vast.ai)
// Base: https://console.vast.ai/api/v0

mod instance;

pub use instance::VastAdapter;
