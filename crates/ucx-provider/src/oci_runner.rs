use std::process::Command;
use std::time::Instant;
use ucx_protocol::{Job, UcxError};
use crate::local_provider::{JobRunner, RunResult};

/// OCI container runner — spawns docker/podman, streams logs, captures exit code.
/// Replaces StubRunner with real container execution.
pub struct OciRunner {
    pub runtime: String,
}

impl OciRunner {
    pub fn new() -> Self {
        Self { runtime: detect_runtime() }
    }

    pub fn with_runtime(runtime: impl Into<String>) -> Self {
        Self { runtime: runtime.into() }
    }
}

impl Default for OciRunner {
    fn default() -> Self { Self::new() }
}

impl JobRunner for OciRunner {
    fn run(&self, job: &Job) -> Result<RunResult, UcxError> {
        // Extract runtime spec fields (opaque JSON payload from submitter)
        let image = job.runtime_spec.get("image")
            .and_then(|v| v.as_str())
            .unwrap_or("ubuntu:22.04");

        let memory_mb = job.requirements.ram_gb
            .map(|g| (g * 1024.0) as u64)
            .unwrap_or(512);
        let cpu_count = job.requirements.cpu_cores.unwrap_or(1);
        let gpu_count = job.requirements.gpu_count.unwrap_or(0);

        let mut cmd = Command::new(&self.runtime);
        cmd.args([
            "run", "--rm",
            "--name", &format!("ucx-job-{}", job.id),
            "--memory", &format!("{}m", memory_mb),
            "--cpus",   &cpu_count.to_string(),
        ]);

        // GPU access if requested
        if gpu_count > 0 {
            if self.runtime == "docker" {
                cmd.args(["--gpus", "all"]);
            } else {
                cmd.args(["--device", "nvidia.com/gpu=all"]);
            }
        }

        // Mount working directory
        let workdir = std::env::temp_dir().join(format!("ucx-{}", job.id));
        std::fs::create_dir_all(&workdir).ok();
        cmd.args([
            "-v", &format!("{}:/workspace", workdir.display()),
            "-w", "/workspace",
            image,
        ]);

        // Command args from runtime_spec
        if let Some(args) = job.runtime_spec.get("args").and_then(|v| v.as_array()) {
            for arg in args {
                if let Some(s) = arg.as_str() {
                    cmd.arg(s);
                }
            }
        }

        let start = Instant::now();
        let output = cmd.output().map_err(|e| UcxError::ExecutionFailed { reason: e.to_string() })?;
        let elapsed = start.elapsed().as_secs_f64();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(UcxError::ExecutionFailed {
                reason: format!("container exited {}: {}", output.status, &stderr[..stderr.len().min(200)])
            });
        }

        let artifact_hash = if output.stdout.is_empty() {
            None
        } else {
            Some(sha256_hex(&output.stdout))
        };

        let mut exec_bytes = output.stdout.clone();
        exec_bytes.extend_from_slice(&output.stderr);
        exec_bytes.extend_from_slice(output.status.code().unwrap_or(0).to_le_bytes().as_ref());
        let execution_hash = Some(sha256_hex(&exec_bytes));

        std::fs::remove_dir_all(&workdir).ok();

        Ok(RunResult {
            artifact_hash,
            execution_hash,
            gpu_seconds: if gpu_count > 0 { elapsed } else { 0.0 },
            cpu_seconds: elapsed,
        })
    }
}

fn detect_runtime() -> String {
    for rt in &["docker", "podman"] {
        if Command::new(rt).arg("--version").output().is_ok() {
            return rt.to_string();
        }
    }
    "docker".to_string()
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}
