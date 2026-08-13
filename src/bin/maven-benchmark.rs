use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::Parser;
use maven_mcp::benchmark::{BenchmarkOptions, BenchmarkSpec, run_benchmark};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Benchmark paired agent shell commands and Maven MCP tool calls"
)]
struct Cli {
    /// JSON benchmark specification containing paired shell and MCP cases.
    #[arg(long)]
    spec: PathBuf,
    /// Streamable HTTP endpoint of a running Maven MCP server.
    #[arg(long, default_value = "http://127.0.0.1:8080/mcp")]
    mcp_url: String,
    /// JSON report destination.
    #[arg(long)]
    output: PathBuf,
    /// Measured executions per side and case.
    #[arg(long, default_value_t = 5)]
    iterations: u32,
    /// Unmeasured executions per side and case before measurement.
    #[arg(long, default_value_t = 1)]
    warmup: u32,
    /// Per-operation timeout.
    #[arg(long, default_value_t = 300)]
    timeout_seconds: u64,
    /// Maximum response bytes included in token and byte metrics.
    #[arg(long, default_value_t = 1_048_576)]
    max_output_bytes: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let input = std::fs::read(&cli.spec)
        .with_context(|| format!("cannot read benchmark spec {}", cli.spec.display()))?;
    let spec = BenchmarkSpec::from_slice(&input)?;
    let report = run_benchmark(
        spec,
        &BenchmarkOptions {
            mcp_url: cli.mcp_url,
            iterations: cli.iterations,
            warmup: cli.warmup,
            timeout: Duration::from_secs(cli.timeout_seconds),
            max_output_bytes: cli.max_output_bytes,
        },
    )
    .await?;
    let json = serde_json::to_vec_pretty(&report)?;
    write_report(&cli.output, &json)?;
    Ok(())
}

fn write_report(path: &std::path::Path, json: &[u8]) -> Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("output path must have a UTF-8 file name")?;
    let temporary = path.with_file_name(format!(".{file_name}.tmp"));
    std::fs::write(&temporary, json)
        .with_context(|| format!("cannot write temporary report {}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .with_context(|| format!("cannot move report into {}", path.display()))?;
    Ok(())
}
