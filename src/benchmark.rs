use std::{
    collections::BTreeSet,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use rmcp::{
    RoleClient, ServiceExt, model::CallToolRequestParams, service::RunningService,
    transport::TokioChildProcess,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    time::Instant,
};

use crate::project::{ProcessGroupGuard, terminate_process_group};

pub const SPEC_SCHEMA_VERSION: u32 = 1;
pub const REPORT_SCHEMA_VERSION: &str = "1.0";

/// Name of the environment variable exposed to shell commands, and of the
/// placeholder substituted in `shell.cwd` and MCP arguments, for the project
/// bound with `--project-path`.
pub const PROJECT_PATH_VARIABLE: &str = "PROJECT_PATH";
const PROJECT_PATH_PLACEHOLDER: &str = "${PROJECT_PATH}";
const TRUSTED_DIRECTORIES_VARIABLE: &str = "MAVEN_TRUSTED_PROJECT_DIRECTORIES";
const INDEX_CACHE_VARIABLE: &str = "MAVEN_MCP_INDEX_CACHE";

/// Most differing values listed per side in a failed identity check.
const IDENTITY_SAMPLE_LIMIT: usize = 10;

/// Time allowed for the shell process group to exit and its pipes to close
/// after the run has ended or timed out.
const SHELL_CLEANUP_GRACE: Duration = Duration::from_secs(1);

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
    /// How both sides' results are normalized and compared before measuring.
    #[serde(default)]
    pub identity: Option<ResultIdentity>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultIdentity {
    /// Slash-separated path into the MCP structured result; `*` iterates the
    /// elements of an array or the values of an object.
    pub mcp_values: String,
    #[serde(default)]
    pub normalize: IdentityNormalization,
}

/// Every value is split into lines, trimmed, and stripped of empty lines; the
/// sides are then compared as sorted sets.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityNormalization {
    #[default]
    Lines,
    /// Keeps only the last path component of each line, so absolute shell
    /// paths compare equal to repository-relative MCP paths.
    FileNames,
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
    /// Parses and validates a specification. With a bound project, every
    /// `${PROJECT_PATH}` in `shell.cwd` and MCP arguments is replaced by its
    /// canonical path and each MCP case must target exactly that project.
    pub fn from_slice(bytes: &[u8], project_path: Option<&Path>) -> Result<Self> {
        let mut spec: Self =
            serde_json::from_slice(bytes).context("invalid benchmark specification")?;
        spec.bind_project_path(project_path)?;
        spec.validate()?;
        Ok(spec)
    }

    fn bind_project_path(&mut self, project_path: Option<&Path>) -> Result<()> {
        let project = project_path
            .map(|path| {
                path.to_str()
                    .context("project path must be valid UTF-8")
                    .map(str::to_owned)
            })
            .transpose()?;
        for case in &mut self.cases {
            let mut uses_project = case.shell.command.contains(PROJECT_PATH_VARIABLE);
            if let Some(cwd) = &case.shell.cwd
                && let Some(text) = cwd.to_str()
                && text.contains(PROJECT_PATH_PLACEHOLDER)
            {
                uses_project = true;
                if let Some(project) = &project {
                    case.shell.cwd = Some(PathBuf::from(
                        text.replace(PROJECT_PATH_PLACEHOLDER, project),
                    ));
                }
            }
            for value in case.mcp.arguments.values_mut() {
                uses_project |= substitute_project_path(value, project.as_deref());
            }
            let Some(project) = &project else {
                if uses_project {
                    bail!(
                        "case {} uses {PROJECT_PATH_PLACEHOLDER}; pass --project-path",
                        case.id
                    );
                }
                continue;
            };
            if case
                .mcp
                .arguments
                .get("project_path")
                .and_then(Value::as_str)
                != Some(project)
            {
                bail!(
                    "MCP arguments of case {} must set project_path to {PROJECT_PATH_PLACEHOLDER} when --project-path is used",
                    case.id
                );
            }
        }
        Ok(())
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

/// Replaces the placeholder in every string of `value`; returns whether any
/// string contained it.
fn substitute_project_path(value: &mut Value, project: Option<&str>) -> bool {
    match value {
        Value::String(text) if text.contains(PROJECT_PATH_PLACEHOLDER) => {
            if let Some(project) = project {
                *text = text.replace(PROJECT_PATH_PLACEHOLDER, project);
            }
            true
        }
        Value::Array(items) => items.iter_mut().fold(false, |found, item| {
            substitute_project_path(item, project) | found
        }),
        Value::Object(map) => map.values_mut().fold(false, |found, item| {
            substitute_project_path(item, project) | found
        }),
        _ => false,
    }
}

/// Canonicalizes the benchmark project: an absolute path to a directory that
/// contains `pom.xml`.
pub fn canonical_project_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("project path must be absolute: {}", path.display());
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve project path {}", path.display()))?;
    if !canonical.join("pom.xml").is_file() {
        bail!(
            "project path must be a directory containing pom.xml: {}",
            canonical.display()
        );
    }
    if canonical.to_str().is_none() {
        bail!("project path must be valid UTF-8");
    }
    Ok(canonical)
}

#[derive(Debug, Clone)]
pub struct BenchmarkOptions {
    pub mcp_command: PathBuf,
    /// Canonical project (see [`canonical_project_path`]). It is exposed to
    /// shell commands as `PROJECT_PATH` and is the only trusted directory of
    /// the MCP child process.
    pub project_path: Option<PathBuf>,
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
    pub measurement_modes: MeasurementModes,
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

/// States which preparation work is inside the measured durations.
#[derive(Debug, Serialize)]
pub struct MeasurementModes {
    pub first_call: &'static str,
    pub reuse: &'static str,
    pub unmeasured: &'static str,
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
    /// Cold first execution (iteration 0); also the source of the identity check.
    pub first_call: RunMeasurement,
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
    pub identity: IdentityCheck,
    pub shell_to_mcp_duration_ratio: Option<f64>,
    pub first_call_shell_to_mcp_duration_ratio: Option<f64>,
    pub estimated_token_reduction_percent: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityStatus {
    /// The case declares no identity check.
    NotChecked,
    Matched,
    /// Both sides succeeded with different normalized result sets.
    Mismatched,
    /// A side failed or its result cannot be compared.
    Unavailable,
}

#[derive(Debug, Serialize)]
pub struct IdentityCheck {
    pub status: IdentityStatus,
    pub shell_values: usize,
    pub mcp_values: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub only_in_shell: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub only_in_mcp: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl IdentityCheck {
    fn with_status(status: IdentityStatus) -> Self {
        Self {
            status,
            shell_values: 0,
            mcp_values: 0,
            only_in_shell: Vec::new(),
            only_in_mcp: Vec::new(),
            detail: None,
        }
    }

    fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            detail: Some(detail.into()),
            ..Self::with_status(IdentityStatus::Unavailable)
        }
    }

    /// Speed and token ratios are only meaningful for the same results.
    fn permits_ratios(&self) -> bool {
        matches!(
            self.status,
            IdentityStatus::NotChecked | IdentityStatus::Matched
        )
    }
}

type McpClient = RunningService<RoleClient, ()>;

pub async fn run_benchmark(
    spec: BenchmarkSpec,
    options: &BenchmarkOptions,
) -> Result<BenchmarkReport> {
    options.validate()?;
    let mut mcp_command = Command::new(&options.mcp_command);
    if let Some(project_path) = &options.project_path {
        mcp_command.env(TRUSTED_DIRECTORIES_VARIABLE, project_path);
    }
    // Every invocation measures a cold first call unless the caller opts into the on-disk
    // index cache explicitly.
    if std::env::var_os(INDEX_CACHE_VARIABLE).is_none() {
        mcp_command.env(INDEX_CACHE_VARIABLE, "false");
    }
    let client = match TokioChildProcess::new(mcp_command) {
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
        let shell_request = payload_size(case.shell.command.as_bytes());
        let mcp_request_value = json!({
            "name": case.mcp.tool,
            "arguments": case.mcp.arguments,
        });
        let mcp_request = payload_size(&serde_json::to_vec(&mcp_request_value)?);

        let (shell_first, shell_stdout) = run_shell_captured(&case.shell, options, 0).await;
        let (mcp_first, mcp_result) = match &client {
            Ok(client) => run_mcp_captured(client, &case.mcp, options, 0).await,
            Err(error) => (unavailable_mcp_run(0, mcp_request, error), None),
        };
        let identity = check_identity(
            case.identity.as_ref(),
            &shell_first,
            &shell_stdout,
            &mcp_first,
            mcp_result.as_ref(),
        );

        for _ in 0..options.warmup {
            let _ = run_shell(&case.shell, options, 0).await;
            if let Ok(client) = &client {
                let _ = run_mcp(client, &case.mcp, options, 0).await;
            }
        }

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
        let comparison = compare(
            &shell_summary,
            &mcp_summary,
            (&shell_first, &mcp_first),
            identity,
        );
        cases.push(CaseReport {
            id: case.id,
            description: case.description,
            shell: SideReport {
                request: shell_request,
                first_call: shell_first,
                runs: shell_runs,
                summary: shell_summary,
            },
            mcp: SideReport {
                request: mcp_request,
                first_call: mcp_first,
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
        measurement_modes: MeasurementModes {
            first_call: "Iteration 0 of each side and case, run once before warmup. For MCP, the first call for a project in the fresh server process includes Maven classpath resolution and project index construction; later cases on the same project find the index cached. For shell, it is a normal run. This run also supplies the result for the identity check.",
            reuse: "Iterations 1..N after the warmup runs. MCP calls reuse the cached project index; every shell run repeats all of its own work, including any Maven classpath resolution written in the command.",
            unmeasured: "MCP process start and initialization, warmup runs, and the cost of building this binary.",
        },
        cases,
    })
}

async fn run_shell(
    invocation: &ShellInvocation,
    options: &BenchmarkOptions,
    iteration: u32,
) -> RunMeasurement {
    run_shell_captured(invocation, options, iteration).await.0
}

/// Runs the shell command and also returns its bounded stdout.
async fn run_shell_captured(
    invocation: &ShellInvocation,
    options: &BenchmarkOptions,
    iteration: u32,
) -> (RunMeasurement, Vec<u8>) {
    let request = payload_size(invocation.command.as_bytes());
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(&invocation.command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);
    if let Some(project_path) = &options.project_path {
        command.env(PROJECT_PATH_VARIABLE, project_path);
    }
    if let Some(cwd) = &invocation.cwd {
        command.current_dir(cwd);
    }
    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return (
                failed_run(
                    iteration,
                    started.elapsed().as_nanos(),
                    request,
                    format!("shell execution failed: {error}"),
                ),
                Vec::new(),
            );
        }
    };
    let mut process_group = ProcessGroupGuard::new(child.id());
    let execution = supervise_shell(&mut child, options, started).await;
    process_group.disarm();
    let response = payload_size(&execution.output);
    let measurement = match execution.outcome {
        ShellOutcome::Exited(status) => RunMeasurement {
            iteration,
            duration_nanoseconds: execution.duration,
            success: status.success(),
            request,
            response,
            exit_code: status.code(),
            error: (!status.success())
                .then(|| format!("shell command exited with status {status}")),
            output_truncated: execution.truncated,
        },
        ShellOutcome::TimedOut => RunMeasurement {
            iteration,
            duration_nanoseconds: execution.duration,
            success: false,
            request,
            response,
            exit_code: None,
            error: Some(format!(
                "shell command timed out after {} ms",
                options.timeout.as_millis()
            )),
            output_truncated: execution.truncated,
        },
        ShellOutcome::Failed(error) => RunMeasurement {
            iteration,
            duration_nanoseconds: execution.duration,
            success: false,
            request,
            response,
            exit_code: None,
            error: Some(error),
            output_truncated: execution.truncated,
        },
    };
    let mut stdout = execution.output;
    stdout.truncate(execution.stdout_len);
    (measurement, stdout)
}

enum ShellOutcome {
    Exited(ExitStatus),
    TimedOut,
    Failed(String),
}

struct ShellExecution {
    outcome: ShellOutcome,
    duration: u128,
    /// Bounded stdout followed by stderr; never larger than the output budget.
    output: Vec<u8>,
    /// Length of the stdout prefix of `output`.
    stdout_len: usize,
    truncated: bool,
}

/// Runs the shell process group to completion within the run deadline.
///
/// The process group is always killed once the direct child has exited or the
/// deadline has passed, so descendants cannot keep the output pipes open or
/// outlive the run. Output beyond the shared budget is read and discarded.
async fn supervise_shell(
    child: &mut Child,
    options: &BenchmarkOptions,
    started: Instant,
) -> ShellExecution {
    let process_id = child.id();
    let budget = AtomicUsize::new(options.max_output_bytes);
    let truncated = AtomicBool::new(false);
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut readers = Box::pin(async {
        tokio::join!(
            drain_bounded(stdout, &budget, &truncated, &mut stdout_bytes),
            drain_bounded(stderr, &budget, &truncated, &mut stderr_bytes)
        )
    });

    let deadline = started + options.timeout;
    let mut read_result = None;
    let waited = tokio::time::timeout_at(deadline, async {
        let mut exit = Box::pin(child.wait());
        loop {
            tokio::select! {
                status = &mut exit => return status,
                result = &mut readers, if read_result.is_none() => read_result = Some(result),
            }
        }
    })
    .await;
    let duration = started.elapsed().as_nanos();
    let timed_out = waited.is_err();
    let _ = terminate_process_group(process_id);

    let cleanup_deadline = Instant::now() + SHELL_CLEANUP_GRACE;
    let waited = match waited {
        Ok(status) => status,
        Err(_) => tokio::time::timeout_at(cleanup_deadline, child.wait())
            .await
            .unwrap_or_else(|_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "shell did not exit after process-group termination",
                ))
            }),
    };
    if read_result.is_none() {
        read_result = tokio::time::timeout_at(cleanup_deadline, &mut readers)
            .await
            .ok();
    }
    drop(readers);

    let outcome = if timed_out {
        ShellOutcome::TimedOut
    } else {
        match (waited, read_result) {
            (Err(error), _) => ShellOutcome::Failed(format!("cannot wait for shell: {error}")),
            (Ok(_), None) => ShellOutcome::Failed(
                "shell output pipes stayed open after process-group termination".to_owned(),
            ),
            (Ok(_), Some((Err(error), _) | (_, Err(error)))) => {
                ShellOutcome::Failed(format!("cannot read shell output: {error}"))
            }
            (Ok(status), Some((Ok(()), Ok(())))) => ShellOutcome::Exited(status),
        }
    };
    let stdout_len = stdout_bytes.len();
    let mut output = stdout_bytes;
    output.append(&mut stderr_bytes);
    ShellExecution {
        outcome,
        duration,
        output,
        stdout_len,
        truncated: truncated.load(Ordering::Relaxed),
    }
}

/// Reads a stream to EOF, keeping bytes only while the budget shared with the
/// other stream has room and discarding the rest.
async fn drain_bounded<R>(
    reader: Option<R>,
    budget: &AtomicUsize,
    truncated: &AtomicBool,
    stored: &mut Vec<u8>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return Ok(());
    };
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        let reserved = budget
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                Some(remaining - remaining.min(read))
            })
            .map_or(0, |remaining| remaining.min(read));
        stored.extend_from_slice(&buffer[..reserved]);
        if reserved < read {
            truncated.store(true, Ordering::Relaxed);
        }
    }
}

async fn run_mcp(
    client: &McpClient,
    invocation: &McpInvocation,
    options: &BenchmarkOptions,
    iteration: u32,
) -> RunMeasurement {
    run_mcp_captured(client, invocation, options, iteration)
        .await
        .0
}

/// Calls the MCP tool and also returns the full, untruncated result as JSON.
async fn run_mcp_captured(
    client: &McpClient,
    invocation: &McpInvocation,
    options: &BenchmarkOptions,
    iteration: u32,
) -> (RunMeasurement, Option<Value>) {
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
            let measurement = RunMeasurement {
                iteration,
                duration_nanoseconds: duration,
                success,
                request: request_size,
                response: payload_size(&response),
                exit_code: None,
                error: (!success).then(|| "MCP tool returned an error".to_owned()),
                output_truncated: original_len > response.len(),
            };
            (measurement, serde_json::to_value(&result).ok())
        }
        Ok(Err(error)) => (
            failed_run(
                iteration,
                duration,
                request_size,
                format!("MCP tool call failed: {error}"),
            ),
            None,
        ),
        Err(_) => (
            failed_run(
                iteration,
                duration,
                request_size,
                format!(
                    "MCP tool call timed out after {} ms",
                    options.timeout.as_millis()
                ),
            ),
            None,
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

fn compare(
    shell: &MeasurementSummary,
    mcp: &MeasurementSummary,
    first_calls: (&RunMeasurement, &RunMeasurement),
    identity: IdentityCheck,
) -> Comparison {
    if !identity.permits_ratios() {
        return Comparison {
            identity,
            shell_to_mcp_duration_ratio: None,
            first_call_shell_to_mcp_duration_ratio: None,
            estimated_token_reduction_percent: None,
        };
    }
    let duration_ratio = shell
        .mean_duration_nanoseconds
        .zip(mcp.mean_duration_nanoseconds)
        .and_then(|(shell, mcp)| (mcp > 0).then(|| shell as f64 / mcp as f64));
    let (shell_first, mcp_first) = first_calls;
    let first_call_ratio = (shell_first.success && mcp_first.success)
        .then_some((
            shell_first.duration_nanoseconds,
            mcp_first.duration_nanoseconds,
        ))
        .and_then(|(shell, mcp)| (mcp > 0).then(|| shell as f64 / mcp as f64));
    let token_reduction = shell
        .mean_total_estimated_tokens
        .zip(mcp.mean_total_estimated_tokens)
        .and_then(|(shell, mcp)| {
            (shell > 0).then(|| ((shell as f64 - mcp as f64) / shell as f64) * 100.0)
        });
    Comparison {
        identity,
        shell_to_mcp_duration_ratio: duration_ratio,
        first_call_shell_to_mcp_duration_ratio: first_call_ratio,
        estimated_token_reduction_percent: token_reduction,
    }
}

/// Compares the normalized results of the first shell and MCP executions of a
/// case, so a speed or token ratio is never reported for different results.
fn check_identity(
    identity: Option<&ResultIdentity>,
    shell_run: &RunMeasurement,
    shell_stdout: &[u8],
    mcp_run: &RunMeasurement,
    mcp_result: Option<&Value>,
) -> IdentityCheck {
    let Some(identity) = identity else {
        return IdentityCheck::with_status(IdentityStatus::NotChecked);
    };
    if !shell_run.success {
        return IdentityCheck::unavailable("the shell side failed, so results cannot be compared");
    }
    if shell_run.output_truncated {
        return IdentityCheck::unavailable(
            "shell output exceeded --max-output-bytes; results cannot be compared",
        );
    }
    let Some(mcp_result) = mcp_result.filter(|_| mcp_run.success) else {
        return IdentityCheck::unavailable("the MCP side failed, so results cannot be compared");
    };
    let mcp_values = match mcp_result_values(mcp_result, &identity.mcp_values) {
        Ok(values) => values,
        Err(error) => return IdentityCheck::unavailable(format!("{error:#}")),
    };
    let shell = normalize_values(
        [String::from_utf8_lossy(shell_stdout).into_owned()],
        identity.normalize,
    );
    let mcp = normalize_values(mcp_values, identity.normalize);
    if shell.is_empty() && mcp.is_empty() {
        return IdentityCheck::unavailable("both sides returned an empty result set");
    }
    let sample = |values: &BTreeSet<String>, other: &BTreeSet<String>| {
        values
            .difference(other)
            .take(IDENTITY_SAMPLE_LIMIT)
            .cloned()
            .collect::<Vec<_>>()
    };
    let only_in_shell = sample(&shell, &mcp);
    let only_in_mcp = sample(&mcp, &shell);
    let matched = shell == mcp;
    IdentityCheck {
        status: if matched {
            IdentityStatus::Matched
        } else {
            IdentityStatus::Mismatched
        },
        shell_values: shell.len(),
        mcp_values: mcp.len(),
        only_in_shell,
        only_in_mcp,
        detail: (!matched).then(|| "shell and MCP returned different result sets".to_owned()),
    }
}

/// Selects the values at `pointer` from the MCP result, preferring the
/// structured content and falling back to JSON text content.
fn mcp_result_values(result: &Value, pointer: &str) -> Result<Vec<String>> {
    let structured = match result
        .get("structuredContent")
        .filter(|value| !value.is_null())
    {
        Some(value) => value.clone(),
        None => result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|content| {
                content
                    .iter()
                    .find_map(|item| item.get("text").and_then(Value::as_str))
            })
            .and_then(|text| serde_json::from_str(text).ok())
            .context("MCP result has no structured content to compare")?,
    };
    let mut values = Vec::new();
    collect_values(
        &structured,
        pointer.split('/').filter(|part| !part.is_empty()),
        &mut values,
    )
    .with_context(|| format!("cannot select {pointer} from the MCP result"))?;
    Ok(values)
}

fn collect_values<'a>(
    value: &Value,
    mut segments: impl Iterator<Item = &'a str> + Clone,
    values: &mut Vec<String>,
) -> Result<()> {
    let Some(segment) = segments.next() else {
        values.push(match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        });
        return Ok(());
    };
    if segment == "*" {
        let children: Vec<&Value> = match value {
            Value::Array(items) => items.iter().collect(),
            Value::Object(map) => map.values().collect(),
            _ => bail!("`*` needs an array or object"),
        };
        for child in children {
            collect_values(child, segments.clone(), values)?;
        }
        return Ok(());
    }
    let child = value
        .get(segment)
        .with_context(|| format!("missing field `{segment}`"))?;
    collect_values(child, segments, values)
}

fn normalize_values(
    values: impl IntoIterator<Item = String>,
    normalization: IdentityNormalization,
) -> BTreeSet<String> {
    values
        .into_iter()
        .flat_map(|value| {
            value
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| match normalization {
                    IdentityNormalization::Lines => line.to_owned(),
                    IdentityNormalization::FileNames => {
                        line.rsplit('/').next().unwrap_or(line).to_owned()
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
#[path = "../tests/unit/benchmark.rs"]
mod tests;
