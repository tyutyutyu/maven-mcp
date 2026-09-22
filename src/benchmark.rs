use std::{collections::BTreeSet, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use rmcp::{
    RoleClient, ServiceExt, model::CallToolRequestParams, service::RunningService,
    transport::TokioChildProcess,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::{process::Command, time::Instant};

pub const SPEC_SCHEMA_VERSION: u32 = 1;
pub const REPORT_SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkSpec {
    pub schema_version: u32,
    pub name: String,
    pub cases: Vec<BenchmarkCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkCase {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    pub shell: ShellInvocation,
    pub mcp: McpInvocation,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellInvocation {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpInvocation {
    pub tool: String,
    #[serde(default = "empty_arguments")]
    pub arguments: Map<String, Value>,
}

fn empty_arguments() -> Map<String, Value> {
    Map::new()
}

impl BenchmarkSpec {
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        let spec: Self =
            serde_json::from_slice(bytes).context("invalid benchmark specification")?;
        spec.validate()?;
        Ok(spec)
    }

    fn validate(&self) -> Result<()> {
        if self.schema_version != SPEC_SCHEMA_VERSION {
            bail!(
                "unsupported specification schema_version {}; expected {SPEC_SCHEMA_VERSION}",
                self.schema_version
            );
        }
        if self.name.trim().is_empty() {
            bail!("benchmark name must not be empty");
        }
        if self.cases.is_empty() {
            bail!("benchmark must contain at least one case");
        }
        let mut ids = BTreeSet::new();
        for case in &self.cases {
            if case.id.trim().is_empty() {
                bail!("benchmark case id must not be empty");
            }
            if !ids.insert(&case.id) {
                bail!("duplicate benchmark case id: {}", case.id);
            }
            if case.shell.command.trim().is_empty() {
                bail!("shell command must not be empty for case {}", case.id);
            }
            if case.mcp.tool.trim().is_empty() {
                bail!("MCP tool must not be empty for case {}", case.id);
            }
            if let Some(cwd) = &case.shell.cwd
                && !cwd.is_dir()
            {
                bail!(
                    "shell cwd is not a directory for case {}: {}",
                    case.id,
                    cwd.display()
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct BenchmarkOptions {
    pub mcp_command: PathBuf,
    pub iterations: u32,
    pub warmup: u32,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl BenchmarkOptions {
    pub fn validate(&self) -> Result<()> {
        if self.iterations == 0 {
            bail!("iterations must be greater than zero");
        }
        if self.timeout.is_zero() {
            bail!("timeout must be greater than zero");
        }
        if self.max_output_bytes == 0 {
            bail!("max output bytes must be greater than zero");
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct BenchmarkReport {
    pub schema_version: &'static str,
    pub benchmark: BenchmarkMetadata,
    pub token_measurement: TokenMeasurement,
    pub cases: Vec<CaseReport>,
}

#[derive(Debug, Serialize)]
pub struct BenchmarkMetadata {
    pub name: String,
    pub iterations: u32,
    pub warmup_iterations: u32,
    pub timeout_milliseconds: u128,
    pub max_output_bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct TokenMeasurement {
    pub kind: &'static str,
    pub algorithm: &'static str,
    pub characters_per_token: u32,
    pub limitation: &'static str,
}

#[derive(Debug, Serialize)]
pub struct CaseReport {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub shell: SideReport,
    pub mcp: SideReport,
    pub comparison: Comparison,
}

#[derive(Debug, Serialize)]
pub struct SideReport {
    pub request: PayloadSize,
    pub runs: Vec<RunMeasurement>,
    pub summary: MeasurementSummary,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct PayloadSize {
    pub bytes: usize,
    pub estimated_tokens: u64,
}

#[derive(Debug, Serialize)]
pub struct RunMeasurement {
    pub iteration: u32,
    pub duration_nanoseconds: u128,
    pub success: bool,
    pub request: PayloadSize,
    pub response: PayloadSize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub output_truncated: bool,
}

#[derive(Debug, Serialize)]
pub struct MeasurementSummary {
    pub runs: usize,
    pub successful_runs: usize,
    pub mean_duration_nanoseconds: Option<u128>,
    pub median_duration_nanoseconds: Option<u128>,
    pub mean_total_estimated_tokens: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Comparison {
    pub shell_to_mcp_duration_ratio: Option<f64>,
    pub estimated_token_reduction_percent: Option<f64>,
}

type McpClient = RunningService<RoleClient, ()>;

pub async fn run_benchmark(
    spec: BenchmarkSpec,
    options: &BenchmarkOptions,
) -> Result<BenchmarkReport> {
    options.validate()?;
    let client = match TokioChildProcess::new(Command::new(&options.mcp_command)) {
        Ok(transport) => match tokio::time::timeout(options.timeout, ().serve(transport)).await {
            Ok(Ok(client)) => Ok(client),
            Ok(Err(error)) => Err(format!("MCP client initialization failed: {error}")),
            Err(_) => Err(format!(
                "MCP client initialization timed out after {} ms",
                options.timeout.as_millis()
            )),
        },
        Err(error) => Err(format!(
            "cannot start MCP command {}: {error}",
            options.mcp_command.display()
        )),
    };

    let mut cases = Vec::with_capacity(spec.cases.len());
    for case in spec.cases {
        for _ in 0..options.warmup {
            let _ = run_shell(&case.shell, options, 0).await;
            if let Ok(client) = &client {
                let _ = run_mcp(client, &case.mcp, options, 0).await;
            }
        }

        let shell_request = payload_size(case.shell.command.as_bytes());
        let mcp_request_value = json!({
            "name": case.mcp.tool,
            "arguments": case.mcp.arguments,
        });
        let mcp_request = payload_size(&serde_json::to_vec(&mcp_request_value)?);
        let mut shell_runs = Vec::with_capacity(options.iterations as usize);
        let mut mcp_runs = Vec::with_capacity(options.iterations as usize);
        for iteration in 1..=options.iterations {
            shell_runs.push(run_shell(&case.shell, options, iteration).await);
            mcp_runs.push(match &client {
                Ok(client) => run_mcp(client, &case.mcp, options, iteration).await,
                Err(error) => unavailable_mcp_run(iteration, mcp_request, error),
            });
        }
        let shell_summary = summarize(&shell_runs, shell_request);
        let mcp_summary = summarize(&mcp_runs, mcp_request);
        let comparison = compare(&shell_summary, &mcp_summary);
        cases.push(CaseReport {
            id: case.id,
            description: case.description,
            shell: SideReport {
                request: shell_request,
                runs: shell_runs,
                summary: shell_summary,
            },
            mcp: SideReport {
                request: mcp_request,
                runs: mcp_runs,
                summary: mcp_summary,
            },
            comparison,
        });
    }

    if let Ok(mut client) = client {
        let _ = client.close().await;
    }
    Ok(BenchmarkReport {
        schema_version: REPORT_SCHEMA_VERSION,
        benchmark: BenchmarkMetadata {
            name: spec.name,
            iterations: options.iterations,
            warmup_iterations: options.warmup,
            timeout_milliseconds: options.timeout.as_millis(),
            max_output_bytes: options.max_output_bytes,
        },
        token_measurement: TokenMeasurement {
            kind: "estimate",
            algorithm: "ceil(unicode_scalar_values / 4)",
            characters_per_token: 4,
            limitation: "Model-independent estimate; it is not a provider tokenizer or billed-token count.",
        },
        cases,
    })
}

async fn run_shell(
    invocation: &ShellInvocation,
    options: &BenchmarkOptions,
    iteration: u32,
) -> RunMeasurement {
    let request = payload_size(invocation.command.as_bytes());
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(&invocation.command)
        .kill_on_drop(true);
    if let Some(cwd) = &invocation.cwd {
        command.current_dir(cwd);
    }
    let started = Instant::now();
    let result = tokio::time::timeout(options.timeout, command.output()).await;
    let duration = started.elapsed().as_nanos();
    match result {
        Ok(Ok(output)) => {
            let mut response = output.stdout;
            response.extend_from_slice(&output.stderr);
            let original_len = response.len();
            response.truncate(options.max_output_bytes);
            RunMeasurement {
                iteration,
                duration_nanoseconds: duration,
                success: output.status.success(),
                request,
                response: payload_size(&response),
                exit_code: output.status.code(),
                error: (!output.status.success())
                    .then(|| format!("shell command exited with status {}", output.status)),
                output_truncated: original_len > response.len(),
            }
        }
        Ok(Err(error)) => failed_run(
            iteration,
            duration,
            request,
            format!("shell execution failed: {error}"),
        ),
        Err(_) => failed_run(
            iteration,
            duration,
            request,
            format!(
                "shell command timed out after {} ms",
                options.timeout.as_millis()
            ),
        ),
    }
}

async fn run_mcp(
    client: &McpClient,
    invocation: &McpInvocation,
    options: &BenchmarkOptions,
    iteration: u32,
) -> RunMeasurement {
    let request_payload = serde_json::to_vec(&json!({
        "name": invocation.tool,
        "arguments": invocation.arguments,
    }))
    .unwrap_or_default();
    let request_size = payload_size(&request_payload);
    let request = CallToolRequestParams::new(invocation.tool.clone())
        .with_arguments(invocation.arguments.clone());
    let started = Instant::now();
    let result = tokio::time::timeout(options.timeout, client.call_tool(request)).await;
    let duration = started.elapsed().as_nanos();
    match result {
        Ok(Ok(result)) => {
            let success = result.is_error != Some(true);
            let mut response = serde_json::to_vec(&result).unwrap_or_default();
            let original_len = response.len();
            response.truncate(options.max_output_bytes);
            RunMeasurement {
                iteration,
                duration_nanoseconds: duration,
                success,
                request: request_size,
                response: payload_size(&response),
                exit_code: None,
                error: (!success).then(|| "MCP tool returned an error".to_owned()),
                output_truncated: original_len > response.len(),
            }
        }
        Ok(Err(error)) => failed_run(
            iteration,
            duration,
            request_size,
            format!("MCP tool call failed: {error}"),
        ),
        Err(_) => failed_run(
            iteration,
            duration,
            request_size,
            format!(
                "MCP tool call timed out after {} ms",
                options.timeout.as_millis()
            ),
        ),
    }
}

fn unavailable_mcp_run(iteration: u32, request: PayloadSize, error: &str) -> RunMeasurement {
    failed_run(iteration, 0, request, error.to_owned())
}

fn failed_run(
    iteration: u32,
    duration: u128,
    request: PayloadSize,
    error: String,
) -> RunMeasurement {
    RunMeasurement {
        iteration,
        duration_nanoseconds: duration,
        success: false,
        request,
        response: payload_size(&[]),
        exit_code: None,
        error: Some(error),
        output_truncated: false,
    }
}

fn payload_size(bytes: &[u8]) -> PayloadSize {
    let characters = String::from_utf8_lossy(bytes).chars().count() as u64;
    PayloadSize {
        bytes: bytes.len(),
        estimated_tokens: characters.div_ceil(4),
    }
}

fn summarize(runs: &[RunMeasurement], request: PayloadSize) -> MeasurementSummary {
    let successful = runs.iter().filter(|run| run.success).collect::<Vec<_>>();
    let mut durations = successful
        .iter()
        .map(|run| run.duration_nanoseconds)
        .collect::<Vec<_>>();
    durations.sort_unstable();
    MeasurementSummary {
        runs: runs.len(),
        successful_runs: successful.len(),
        mean_duration_nanoseconds: mean_u128(&durations),
        median_duration_nanoseconds: median(&durations),
        mean_total_estimated_tokens: if successful.is_empty() {
            None
        } else {
            Some(
                successful
                    .iter()
                    .map(|run| request.estimated_tokens + run.response.estimated_tokens)
                    .sum::<u64>()
                    / successful.len() as u64,
            )
        },
    }
}

fn mean_u128(values: &[u128]) -> Option<u128> {
    (!values.is_empty()).then(|| values.iter().sum::<u128>() / values.len() as u128)
}

fn median(values: &[u128]) -> Option<u128> {
    if values.is_empty() {
        None
    } else if values.len() % 2 == 1 {
        Some(values[values.len() / 2])
    } else {
        Some((values[values.len() / 2 - 1] + values[values.len() / 2]) / 2)
    }
}

fn compare(shell: &MeasurementSummary, mcp: &MeasurementSummary) -> Comparison {
    let duration_ratio = shell
        .mean_duration_nanoseconds
        .zip(mcp.mean_duration_nanoseconds)
        .and_then(|(shell, mcp)| (mcp > 0).then(|| shell as f64 / mcp as f64));
    let token_reduction = shell
        .mean_total_estimated_tokens
        .zip(mcp.mean_total_estimated_tokens)
        .and_then(|(shell, mcp)| {
            (shell > 0).then(|| ((shell as f64 - mcp as f64) / shell as f64) * 100.0)
        });
    Comparison {
        shell_to_mcp_duration_ratio: duration_ratio,
        estimated_token_reduction_percent: token_reduction,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_uses_unicode_scalar_values() {
        assert_eq!(payload_size("árvíz".as_bytes()).bytes, 7);
        assert_eq!(payload_size("árvíz".as_bytes()).estimated_tokens, 2);
    }

    #[test]
    fn rejects_duplicate_case_ids() {
        let error = BenchmarkSpec::from_slice(
            br#"{"schema_version":1,"name":"x","cases":[{"id":"same","shell":{"command":"true"},"mcp":{"tool":"index_stats"}},{"id":"same","shell":{"command":"true"},"mcp":{"tool":"index_stats"}}]}"#,
        )
        .unwrap_err();
        assert!(error.to_string().contains("duplicate benchmark case id"));
    }

    #[test]
    fn summary_uses_only_successful_runs() {
        let request = PayloadSize {
            bytes: 4,
            estimated_tokens: 1,
        };
        let runs = vec![
            RunMeasurement {
                iteration: 1,
                duration_nanoseconds: 10,
                success: true,
                request,
                response: request,
                exit_code: Some(0),
                error: None,
                output_truncated: false,
            },
            failed_run(2, 1_000, request, "failed".to_owned()),
        ];
        let summary = summarize(&runs, request);
        assert_eq!(summary.successful_runs, 1);
        assert_eq!(summary.mean_duration_nanoseconds, Some(10));
        assert_eq!(summary.mean_total_estimated_tokens, Some(2));
    }
}
