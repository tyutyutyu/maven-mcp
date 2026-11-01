// The supported native platform is Linux: Maven process-group cleanup and the runtime stats
// liveness check rely on Linux process APIs and procfs. Other systems fail here explicitly
// instead of building a binary with untested behavior (see "Supported Platforms" in README.md).
#[cfg(not(target_os = "linux"))]
compile_error!(
    "maven-mcp supports only Linux (x86_64 and aarch64, glibc); see Supported Platforms in README.md"
);

pub mod benchmark;
pub mod blocking;
pub mod build_info;
pub mod config;
pub mod index;
pub mod index_cache;
pub mod log_analysis;
mod log_safety;
pub mod project;
pub mod project_cache;
pub mod redaction;
pub mod runtime_stats;
pub mod server;
