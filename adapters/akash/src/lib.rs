// Akash Network external provider adapter for UCX.
//
// Translates UCX Job/Receipt to/from the Akash REST API.
// Deployments are described as SDL manifests passed via runtime_spec.sdl.
// If no SDL is provided, a generic Docker workload manifest is generated.
//
// Auth: AKASH_KEY env var (Akash wallet mnemonic for signing, or REST API key
//       for hosted provider endpoints like Spheron/Cloudmos).
// Base: AKASH_API_URL env var (default: https://api.akashnet.net)

mod deployment;

pub use deployment::AkashAdapter;
