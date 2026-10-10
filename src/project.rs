use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    ffi::OsString,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, bail};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    sync::{RwLock, Semaphore},
};
use walkdir::WalkDir;

use crate::config::{JavaEnvironment, MavenExecutionConfig, ProjectExecutionConfig};

const JENV_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct MavenModule {
    pub selector: String,
    pub artifact_id: String,
    pub packaging: String,
    pub modules: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct MavenProject {
    pub artifact_id: String,
    pub packaging: String,
    pub wrapper: bool,
    pub modules: Vec<MavenModule>,
}

#[derive(Debug, Clone)]
enum MavenExecutable {
    Wrapper(PathBuf),
    System(PathBuf),
}

impl MavenExecutable {
    fn path(&self) -> &Path {
        match self {
            Self::Wrapper(path) | Self::System(path) => path,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    Compile,
    TestCompile,
    Verify,
}

impl LifecyclePhase {
    fn argument(self) -> &'static str {
        match self {
            Self::Compile => "compile",
            Self::TestCompile => "test-compile",
            Self::Verify => "verify",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct MavenInvocation {
    pub phase: LifecyclePhase,
    pub module: Option<String>,
    pub also_make: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MavenRunStatus {
    Success,
    BuildFailure,
    Timeout,
    RunnerError,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct MavenRunResult {
    pub status: MavenRunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub redaction_count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MavenBuildOutcome {
    Success,
    CompilationError,
    TestFailure,
    BuildFailure,
    Timeout,
    RunnerError,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct CompilerDiagnostic {
    pub message: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReactorModuleResult {
    pub module: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct MavenBuildResult {
    pub outcome: MavenBuildOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Server-policy guidance when Maven could not complete the build")]
    pub policy_notice: Option<String>,
    pub compiler_diagnostics: Vec<CompilerDiagnostic>,
    pub reactor_summary: Vec<ReactorModuleResult>,
    pub run: MavenRunResult,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct FocusedTestInvocation {
    pub test_class: String,
    pub test_method: Option<String>,
    pub module: Option<String>,
    pub also_make: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Default)]
pub struct TestSummary {
    pub passed: usize,
    pub failed: usize,
    pub errors: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TestFailureKind {
    Failure,
    Error,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct TestFailureDetail {
    pub class_name: String,
    pub test_name: String,
    pub kind: TestFailureKind,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TestReportStatus {
    Available,
    Missing,
    Invalid,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct FocusedTestResult {
    pub report_status: TestReportStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<TestSummary>,
    pub failures: Vec<TestFailureDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_error: Option<String>,
    pub build: MavenBuildResult,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct LastTestFailures {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<TestSummary>,
    pub failures: Vec<TestFailureDetail>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyScope {
    Compile,
    Provided,
    Runtime,
    Test,
    System,
}

impl DependencyScope {
    fn argument(self) -> &'static str {
        match self {
            Self::Compile => "compile",
            Self::Provided => "provided",
            Self::Runtime => "runtime",
            Self::Test => "test",
            Self::System => "system",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClasspathKind {
    Build,
    Test,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct EffectiveCoordinate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    pub artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
pub struct EffectiveDependency {
    pub group_id: String,
    pub artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    pub exclusions: Vec<EffectiveExclusion>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
pub struct EffectiveExclusion {
    pub group_id: String,
    pub artifact_id: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
pub struct EffectivePlugin {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    pub artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct EffectiveProject {
    pub coordinate: EffectiveCoordinate,
    pub packaging: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<EffectiveCoordinate>,
    pub properties: BTreeMap<String, String>,
    pub dependencies: Vec<EffectiveDependency>,
    pub dependency_management: Vec<EffectiveDependency>,
    pub plugins: Vec<EffectivePlugin>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStatus {
    Available,
    Missing,
    Invalid,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct EffectivePomResult {
    pub status: DiagnosticStatus,
    pub projects: Vec<EffectiveProject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub build: MavenBuildResult,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DependencyNode {
    pub depth: usize,
    pub coordinate: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DependencyTreeResult {
    pub dependencies: Vec<DependencyNode>,
    pub incomplete: bool,
    pub build: MavenBuildResult,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DependencyResolutionStatus {
    Available,
    Empty,
    Incomplete,
    ResolutionFailed,
    Invalid,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum DependencyPathStatus {
    Selected,
    ConflictOmitted,
    DuplicateOmitted,
    Excluded,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum DependencySelectionReason {
    DirectDeclaration,
    NearestDefinition,
    DependencyManagement,
    OnlyCandidate,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DependencyPathNode {
    pub coordinate: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DependencyResolutionPath {
    pub module: String,
    pub nodes: Vec<DependencyPathNode>,
    pub requested_version: String,
    pub status: DependencyPathStatus,
    pub annotations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DependencyResolutionExplanation {
    pub artifact: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_version: Option<String>,
    pub requested_versions: Vec<String>,
    pub selection_reasons: Vec<DependencySelectionReason>,
    pub paths: Vec<DependencyResolutionPath>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DependencyResolutionResult {
    pub status: DependencyResolutionStatus,
    pub explanations: Vec<DependencyResolutionExplanation>,
    pub incomplete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub build: MavenBuildResult,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct MavenClasspathResult {
    pub artifacts: Vec<String>,
    pub incomplete: bool,
    pub build: MavenBuildResult,
}

#[derive(Debug, Clone)]
pub struct MavenClasspathPaths {
    pub artifacts: Vec<String>,
    pub paths: Vec<PathBuf>,
    pub incomplete: bool,
    pub build: MavenBuildResult,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct CoverageCounter {
    pub missed: u64,
    pub covered: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema, PartialEq)]
pub struct CoverageMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<CoverageCounter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branches: Option<CoverageCounter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<CoverageCounter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub methods: Option<CoverageCounter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classes: Option<CoverageCounter>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct ModuleCoverage {
    pub module: String,
    pub metrics: CoverageMetrics,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStatus {
    Available,
    Missing,
    Stale,
    Invalid,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct CoverageSummaryResult {
    pub status: CoverageStatus,
    pub project_metrics: CoverageMetrics,
    pub reports: Vec<ModuleCoverage>,
    pub missing_modules: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct CoverageGap {
    pub module: String,
    pub package_name: String,
    pub class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<CoverageCounter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<CoverageCounter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instruction: Option<CoverageCounter>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct CoverageGapResult {
    pub status: CoverageStatus,
    pub gaps: Vec<CoverageGap>,
    pub missing_modules: Vec<String>,
    pub incomplete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl MavenBuildResult {
    pub fn from_run(run: MavenRunResult) -> Self {
        let combined = format!("{}\n{}", run.stdout, run.stderr);
        let lowercase = combined.to_lowercase();
        let policy_notice = offline_resolution_policy_notice(&run.status, &lowercase);
        let outcome = match run.status {
            MavenRunStatus::Success => MavenBuildOutcome::Success,
            MavenRunStatus::Timeout => MavenBuildOutcome::Timeout,
            MavenRunStatus::RunnerError => MavenBuildOutcome::RunnerError,
            MavenRunStatus::BuildFailure
                if lowercase.contains("compilation error")
                    || lowercase.contains("compilation failure") =>
            {
                MavenBuildOutcome::CompilationError
            }
            MavenRunStatus::BuildFailure
                if lowercase.contains("there are test failures")
                    || lowercase.contains("failures: 1")
                    || lowercase.contains("errors: 1") =>
            {
                MavenBuildOutcome::TestFailure
            }
            MavenRunStatus::BuildFailure => MavenBuildOutcome::BuildFailure,
        };
        let compiler_diagnostics = combined
            .lines()
            .filter(|line| {
                line.contains("[ERROR]")
                    && (line.contains(".java:[")
                        || line.contains(".kt:[")
                        || line.to_lowercase().contains("compilation"))
            })
            .map(|line| CompilerDiagnostic {
                message: line.trim().to_owned(),
            })
            .collect();
        let reactor_summary = parse_reactor_summary(&combined);
        Self {
            outcome,
            policy_notice,
            compiler_diagnostics,
            reactor_summary,
            run,
        }
    }
}

fn offline_resolution_policy_notice(
    status: &MavenRunStatus,
    lowercase_output: &str,
) -> Option<String> {
    const NOTICE: &str = "Maven could not resolve a remote artifact because maven-mcp is running Maven offline. This can be an intentional security policy or missing MCP server configuration and is not necessarily a project error. If remote resolution is intended for this trusted project, set MAVEN_EXECUTION_NETWORK=true in the MCP server environment and restart the server.";

    matches!(status, MavenRunStatus::BuildFailure)
        .then_some(lowercase_output)
        .filter(|output| output.contains("cannot access") && output.contains("in offline mode"))
        .map(|_| NOTICE.to_owned())
}

#[derive(Debug, Clone)]
pub struct MavenRunner {
    root: PathBuf,
    executable: MavenExecutable,
    project: MavenProject,
    timeout: Duration,
    max_output_bytes: usize,
    max_results: usize,
    network_enabled: bool,
    java_environment: JavaEnvironment,
    execution_repository: Option<PathBuf>,
    permit: Arc<Semaphore>,
    last_test_result: Arc<RwLock<Option<FocusedTestResult>>>,
}

impl MavenRunner {
    pub fn discover(config: &ProjectExecutionConfig) -> Result<Self> {
        let root = canonical_project_root(&config.project_root)?;
        Self::discover_canonical(
            root,
            &MavenExecutionConfig {
                trusted_project_directories: Vec::new(),
                maven_executable: config.maven_executable.clone(),
                execution_repository: config.execution_repository.clone(),
                timeout: config.timeout,
                max_output_bytes: config.max_output_bytes,
                max_results: config.max_results,
                network_enabled: config.network_enabled,
                java_environment: config.java_environment.clone(),
            },
        )
    }

    pub fn discover_root(root: PathBuf, config: &MavenExecutionConfig) -> Result<Self> {
        let root = canonical_project_root(&root)?;
        Self::discover_canonical(root, config)
    }

    pub fn discover_canonical(root: PathBuf, config: &MavenExecutionConfig) -> Result<Self> {
        Self::discover_canonical_with_permit(root, config, Arc::new(Semaphore::new(1)))
    }

    pub fn discover_canonical_with_permit(
        root: PathBuf,
        config: &MavenExecutionConfig,
        permit: Arc<Semaphore>,
    ) -> Result<Self> {
        if !root.is_dir() || !root.join("pom.xml").is_file() {
            bail!("project_path must contain a root pom.xml");
        }
        let executable = select_executable(&root, config.maven_executable.as_deref())?;
        let project = discover_project(&root, matches!(executable, MavenExecutable::Wrapper(_)))?;
        let execution_repository = config
            .execution_repository
            .as_ref()
            .map(|path| canonicalize_directory(path, "MAVEN_EXECUTION_REPO_PATH"))
            .transpose()?;
        Ok(Self {
            root,
            executable,
            project,
            timeout: config.timeout,
            max_output_bytes: config.max_output_bytes,
            max_results: config.max_results,
            network_enabled: config.network_enabled,
            java_environment: config.java_environment.clone(),
            execution_repository,
            permit,
            last_test_result: Arc::new(RwLock::new(None)),
        })
    }

    pub fn project(&self) -> &MavenProject {
        &self.project
    }

    pub async fn run(&self, invocation: &MavenInvocation) -> MavenRunResult {
        let started = Instant::now();
        let arguments = match self.arguments(invocation) {
            Ok(arguments) => arguments,
            Err(error) => return self.runner_error(started, error.to_string()),
        };
        self.execute(arguments).await
    }

    pub async fn run_arguments(&self, arguments: &[String]) -> Result<MavenRunResult> {
        if arguments.iter().any(|argument| argument.contains('\0')) {
            bail!("Maven arguments must not contain NUL bytes");
        }
        Ok(self
            .execute(arguments.iter().map(OsString::from).collect())
            .await)
    }

    pub async fn run_focused_test(&self, invocation: &FocusedTestInvocation) -> FocusedTestResult {
        let started = Instant::now();
        let arguments = match self.focused_test_arguments(invocation) {
            Ok(arguments) => arguments,
            Err(error) => {
                return FocusedTestResult {
                    report_status: TestReportStatus::Missing,
                    summary: None,
                    failures: Vec::new(),
                    report_error: None,
                    build: MavenBuildResult::from_run(
                        self.runner_error(started, error.to_string()),
                    ),
                };
            }
        };
        let before = self.report_snapshot();
        let run = self.execute(arguments).await;
        let build = MavenBuildResult::from_run(run);
        let mut result = match self.updated_reports(&before) {
            Ok(reports) if reports.is_empty() => FocusedTestResult {
                report_status: TestReportStatus::Missing,
                summary: None,
                failures: Vec::new(),
                report_error: None,
                build,
            },
            Ok(reports) => match parse_test_reports(&reports) {
                Ok((summary, failures)) => FocusedTestResult {
                    report_status: TestReportStatus::Available,
                    summary: Some(summary),
                    failures,
                    report_error: None,
                    build,
                },
                Err(error) => FocusedTestResult {
                    report_status: TestReportStatus::Invalid,
                    summary: None,
                    failures: Vec::new(),
                    report_error: Some(error.to_string()),
                    build,
                },
            },
            Err(error) => FocusedTestResult {
                report_status: TestReportStatus::Invalid,
                summary: None,
                failures: Vec::new(),
                report_error: Some(error.to_string()),
                build,
            },
        };
        if result
            .summary
            .as_ref()
            .is_some_and(|summary| summary.failed + summary.errors > 0)
        {
            result.build.outcome = MavenBuildOutcome::TestFailure;
        }
        *self.last_test_result.write().await = Some(result.clone());
        result
    }

    pub async fn last_test_failures(&self) -> LastTestFailures {
        let result = self.last_test_result.read().await;
        result.as_ref().map_or(
            LastTestFailures {
                available: false,
                summary: None,
                failures: Vec::new(),
            },
            |result| LastTestFailures {
                available: true,
                summary: result.summary.clone(),
                failures: result.failures.clone(),
            },
        )
    }

    pub fn test_classes(&self) -> Result<Vec<String>> {
        let mut classes = BTreeSet::new();
        for entry in WalkDir::new(&self.root).follow_links(false) {
            let entry = entry?;
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !matches!(
                path.extension().and_then(|value| value.to_str()),
                Some("java" | "kt")
            ) {
                continue;
            }
            let Some(relative) = test_source_relative(path) else {
                continue;
            };
            let canonical = path.canonicalize()?;
            if !canonical.starts_with(&self.root) {
                bail!("test source escaped project_path");
            }
            let mut class_name = relative
                .with_extension("")
                .to_string_lossy()
                .replace(['/', '\\'], ".");
            class_name = class_name.trim_matches('.').to_owned();
            if !class_name.is_empty() {
                classes.insert(class_name);
            }
        }
        Ok(classes.into_iter().collect())
    }

    pub async fn effective_pom(&self, module: Option<&str>) -> EffectivePomResult {
        let started = Instant::now();
        let output_directory = self.root.join("target/maven-mcp");
        if let Err(error) = std::fs::create_dir_all(&output_directory) {
            return EffectivePomResult {
                status: DiagnosticStatus::Invalid,
                projects: Vec::new(),
                error: Some("cannot prepare effective POM output directory".to_owned()),
                build: MavenBuildResult::from_run(self.runner_error(started, error.to_string())),
            };
        }
        let output_directory = match output_directory.canonicalize() {
            Ok(path) if path.starts_with(&self.root) => path,
            _ => {
                return EffectivePomResult {
                    status: DiagnosticStatus::Invalid,
                    projects: Vec::new(),
                    error: Some("effective POM output escaped project root".to_owned()),
                    build: MavenBuildResult::from_run(self.runner_error(
                        started,
                        "effective POM output escaped project root".to_owned(),
                    )),
                };
            }
        };
        let output = output_directory.join("effective-pom.xml");
        let previous_modified = output
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok();
        let mut arguments = match self.base_arguments(module, false) {
            Ok(arguments) => arguments,
            Err(error) => {
                return EffectivePomResult {
                    status: DiagnosticStatus::Invalid,
                    projects: Vec::new(),
                    error: Some(error.to_string()),
                    build: MavenBuildResult::from_run(
                        self.runner_error(started, error.to_string()),
                    ),
                };
            }
        };
        arguments.push(OsString::from("help:effective-pom"));
        arguments.push(OsString::from(format!("-Doutput={}", output.display())));
        let build = MavenBuildResult::from_run(self.execute(arguments).await);
        let modified = output
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok();
        if modified.is_none()
            || previous_modified.is_some_and(|previous| modified == Some(previous))
        {
            return EffectivePomResult {
                status: DiagnosticStatus::Missing,
                projects: Vec::new(),
                error: None,
                build,
            };
        }
        match parse_effective_pom(&output) {
            Ok(projects) => EffectivePomResult {
                status: DiagnosticStatus::Available,
                projects,
                error: None,
                build,
            },
            Err(error) => EffectivePomResult {
                status: DiagnosticStatus::Invalid,
                projects: Vec::new(),
                error: Some(error.to_string()),
                build,
            },
        }
    }

    pub async fn dependency_tree(
        &self,
        module: Option<&str>,
        scope: Option<DependencyScope>,
        coordinate_filter: Option<&str>,
    ) -> DependencyTreeResult {
        let started = Instant::now();
        if let Some(filter) = coordinate_filter
            && let Err(error) = validate_coordinate_filter(filter)
        {
            return DependencyTreeResult {
                dependencies: Vec::new(),
                incomplete: false,
                build: MavenBuildResult::from_run(self.runner_error(started, error.to_string())),
            };
        }
        let mut arguments = match self.base_arguments(module, false) {
            Ok(arguments) => arguments,
            Err(error) => {
                return DependencyTreeResult {
                    dependencies: Vec::new(),
                    incomplete: false,
                    build: MavenBuildResult::from_run(
                        self.runner_error(started, error.to_string()),
                    ),
                };
            }
        };
        arguments.push(OsString::from("dependency:tree"));
        arguments.push(OsString::from("-DoutputType=text"));
        if let Some(scope) = scope {
            arguments.push(OsString::from(format!("-Dscope={}", scope.argument())));
        }
        if let Some(filter) = coordinate_filter {
            arguments.push(OsString::from(format!("-Dincludes={filter}")));
        }
        let executed = self.execute_with_raw_stdout(arguments).await;
        let run = executed.result;
        let incomplete = run.stdout_truncated;
        let dependencies = parse_dependency_tree(&executed.raw_stdout);
        DependencyTreeResult {
            dependencies,
            incomplete,
            build: MavenBuildResult::from_run(run),
        }
    }

    pub async fn explain_dependency_resolution(
        &self,
        module: Option<&str>,
        scope: Option<DependencyScope>,
        coordinate_filter: Option<&str>,
    ) -> DependencyResolutionResult {
        let started = Instant::now();
        if let Some(filter) = coordinate_filter
            && let Err(error) = validate_coordinate_filter(filter)
        {
            return invalid_dependency_resolution(
                MavenBuildResult::from_run(self.runner_error(started, error.to_string())),
                error.to_string(),
            );
        }
        let mut arguments = match self.base_arguments(module, false) {
            Ok(arguments) => arguments,
            Err(error) => {
                return invalid_dependency_resolution(
                    MavenBuildResult::from_run(self.runner_error(started, error.to_string())),
                    error.to_string(),
                );
            }
        };
        arguments.push(OsString::from("dependency:tree"));
        arguments.push(OsString::from("-DoutputType=text"));
        arguments.push(OsString::from("-Dverbose"));
        if let Some(scope) = scope {
            arguments.push(OsString::from(format!("-Dscope={}", scope.argument())));
        }

        let executed = self.execute_with_raw_stdout(arguments).await;
        let run = executed.result;
        let output_truncated = run.stdout_truncated;
        let build = MavenBuildResult::from_run(run);
        if build.outcome != MavenBuildOutcome::Success {
            return DependencyResolutionResult {
                status: DependencyResolutionStatus::ResolutionFailed,
                explanations: Vec::new(),
                incomplete: output_truncated,
                error: None,
                build,
            };
        }
        let parsed = parse_dependency_resolution(&executed.raw_stdout, &self.project);
        let mut explanations = parsed.explanations;
        if let Some(filter) = coordinate_filter {
            explanations.retain(|explanation| coordinate_matches(filter, explanation));
        }
        let result_limit_reached = explanations.len() > self.max_results;
        explanations.truncate(self.max_results);
        let incomplete = output_truncated || result_limit_reached;
        let status = if parsed.malformed {
            DependencyResolutionStatus::Invalid
        } else if incomplete {
            DependencyResolutionStatus::Incomplete
        } else if explanations.is_empty() {
            DependencyResolutionStatus::Empty
        } else {
            DependencyResolutionStatus::Available
        };
        DependencyResolutionResult {
            status,
            explanations,
            incomplete: incomplete || parsed.malformed,
            error: parsed
                .malformed
                .then(|| "Maven dependency tree contained malformed dependency entries".to_owned()),
            build,
        }
    }

    pub async fn build_classpath(
        &self,
        module: Option<&str>,
        kind: ClasspathKind,
    ) -> MavenClasspathResult {
        let result = self.build_classpath_paths(module, kind).await;
        MavenClasspathResult {
            artifacts: result.artifacts,
            incomplete: result.incomplete,
            build: result.build,
        }
    }

    pub async fn build_classpath_paths(
        &self,
        module: Option<&str>,
        kind: ClasspathKind,
    ) -> MavenClasspathPaths {
        let started = Instant::now();
        let mut arguments = match self.base_arguments(module, false) {
            Ok(arguments) => arguments,
            Err(error) => {
                return MavenClasspathPaths {
                    artifacts: Vec::new(),
                    paths: Vec::new(),
                    incomplete: false,
                    build: MavenBuildResult::from_run(
                        self.runner_error(started, error.to_string()),
                    ),
                };
            }
        };
        arguments.push(OsString::from("dependency:build-classpath"));
        arguments.push(OsString::from(match kind {
            ClasspathKind::Build => "-Dmdep.includeScope=compile",
            ClasspathKind::Test => "-Dmdep.includeScope=test",
        }));
        let executed = self.execute_with_raw_stdout(arguments).await;
        let run = executed.result;
        let (artifacts, normalization_incomplete) = parse_classpath(&run.stdout);
        let (paths, path_incomplete) = parse_classpath_paths(&executed.raw_stdout);
        MavenClasspathPaths {
            artifacts,
            paths,
            incomplete: run.stdout_truncated || normalization_incomplete || path_incomplete,
            build: MavenBuildResult::from_run(run),
        }
    }

    pub fn jacoco_coverage(&self) -> CoverageSummaryResult {
        match self.read_jacoco_reports() {
            Ok(reports) => coverage_summary(reports),
            Err(error) => CoverageSummaryResult {
                status: CoverageStatus::Invalid,
                project_metrics: CoverageMetrics::default(),
                reports: Vec::new(),
                missing_modules: Vec::new(),
                error: Some(error.to_string()),
            },
        }
    }

    pub fn jacoco_coverage_gaps(&self, limit: Option<usize>) -> CoverageGapResult {
        let limit = limit.unwrap_or(self.max_results).min(self.max_results);
        match self.read_jacoco_reports() {
            Ok(reports) => coverage_gaps(reports, limit),
            Err(error) => CoverageGapResult {
                status: CoverageStatus::Invalid,
                gaps: Vec::new(),
                missing_modules: Vec::new(),
                incomplete: false,
                error: Some(error.to_string()),
            },
        }
    }

    fn read_jacoco_reports(&self) -> Result<ParsedCoverageReports> {
        let expected = self.coverage_module_roots();
        let paths = discover_jacoco_reports(&self.root)?;
        let mut reports = Vec::new();
        let mut found = BTreeSet::new();
        let mut xml_budget = crate::config::XmlBudget::default();
        for path in paths {
            let (module, module_root) = expected
                .iter()
                .filter(|(_, root)| path.starts_with(root))
                .max_by_key(|(_, root)| root.components().count())
                .map(|(module, root)| (module.clone(), root.clone()))
                .unwrap_or_else(|| (".".to_owned(), self.root.clone()));
            let xml = xml_budget.read(&path).map_err(|error| {
                if error.is::<quick_xml::Error>() {
                    anyhow::anyhow!("invalid JaCoCo XML report: {error:#}")
                } else {
                    anyhow::anyhow!("cannot read JaCoCo report: {error:#}")
                }
            })?;
            let parsed: RawJacocoReport = quick_xml::de::from_str(&xml)
                .with_context(|| format!("invalid JaCoCo XML report for module {module}"))?;
            let stale = report_is_stale(&path, &module_root)?;
            found.insert(module.clone());
            reports.push(ParsedCoverageReport {
                module,
                metrics: coverage_metrics(&parsed.counters),
                packages: parsed.packages,
                stale,
            });
        }
        reports.sort_by(|left, right| left.module.cmp(&right.module));
        let missing_modules = expected
            .into_keys()
            .filter(|module| !found.contains(module))
            .collect();
        Ok(ParsedCoverageReports {
            reports,
            missing_modules,
        })
    }

    fn coverage_module_roots(&self) -> BTreeMap<String, PathBuf> {
        let mut modules = BTreeMap::new();
        if self.project.packaging != "pom" {
            modules.insert(".".to_owned(), self.root.clone());
        }
        for module in &self.project.modules {
            if module.packaging != "pom" {
                modules.insert(module.selector.clone(), self.root.join(&module.selector));
            }
        }
        modules
    }

    async fn execute(&self, arguments: Vec<OsString>) -> MavenRunResult {
        self.execute_with_raw_stdout(arguments).await.result
    }

    async fn execute_with_raw_stdout(&self, arguments: Vec<OsString>) -> ExecutedMaven {
        let started = Instant::now();
        let _permit = match self.permit.acquire().await {
            Ok(permit) => permit,
            Err(error) => {
                return ExecutedMaven {
                    result: self.runner_error(started, error.to_string()),
                    raw_stdout: String::new(),
                };
            }
        };
        let executable = match self.validated_executable() {
            Ok(executable) => executable,
            Err(error) => {
                return ExecutedMaven {
                    result: self.runner_error(started, error.to_string()),
                    raw_stdout: String::new(),
                };
            }
        };

        let mut command = Command::new(executable);
        command
            .args(arguments)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Err(error) = self.configure_java_environment(&mut command).await {
            return ExecutedMaven {
                result: self.runner_error(started, error.to_string()),
                raw_stdout: String::new(),
            };
        }
        command.as_std_mut().process_group(0);
        let mut child = match spawn_maven(&mut command).await {
            Ok(child) => child,
            Err(error) => {
                return ExecutedMaven {
                    result: self.runner_error(started, format!("cannot start Maven: {error}")),
                    raw_stdout: String::new(),
                };
            }
        };
        let process_id = child.id();
        let mut process_group = ProcessGroupGuard::new(process_id);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let max_output = self.max_output_bytes;
        let stdout_task = tokio::spawn(async move { read_bounded(stdout, max_output).await });
        let stderr_task = tokio::spawn(async move { read_bounded(stderr, max_output).await });

        let (status, exit_code, timed_out) = match tokio::time::timeout(self.timeout, child.wait())
            .await
        {
            Ok(Ok(status)) if status.success() => (MavenRunStatus::Success, status.code(), false),
            Ok(Ok(status)) => (MavenRunStatus::BuildFailure, status.code(), false),
            Ok(Err(error)) => {
                let _ = terminate_process_group(process_id);
                let _ = child.wait().await;
                return ExecutedMaven {
                    result: self.runner_error(started, format!("cannot wait for Maven: {error}")),
                    raw_stdout: String::new(),
                };
            }
            Err(_) => {
                let _ = terminate_process_group(process_id);
                let _ = child.wait().await;
                (MavenRunStatus::Timeout, None, true)
            }
        };
        process_group.disarm();
        let stdout = stdout_task
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let stderr = stderr_task
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        let failed = !matches!(&status, MavenRunStatus::Success);
        let stdout_truncated = stdout.truncated;
        let stderr_truncated = stderr.truncated;
        let raw_stdout = String::from_utf8_lossy(&stdout.bytes).into_owned();
        let stdout = stdout.into_bytes(failed);
        let stderr = stderr.into_bytes(failed);
        let (stdout_text, stdout_redactions) = self.redact(&stdout);
        let (stderr_text, stderr_redactions) = self.redact(&stderr);
        ExecutedMaven {
            result: MavenRunResult {
                status,
                exit_code,
                duration_ms: elapsed_ms(started),
                timed_out,
                stdout: stdout_text,
                stderr: stderr_text,
                stdout_truncated,
                stderr_truncated,
                redaction_count: stdout_redactions + stderr_redactions,
            },
            raw_stdout,
        }
    }

    fn arguments(&self, invocation: &MavenInvocation) -> Result<Vec<OsString>> {
        let mut arguments =
            self.base_arguments(invocation.module.as_deref(), invocation.also_make)?;
        arguments.push(OsString::from(invocation.phase.argument()));
        Ok(arguments)
    }

    fn focused_test_arguments(&self, invocation: &FocusedTestInvocation) -> Result<Vec<OsString>> {
        validate_java_selector(&invocation.test_class, "test_class")?;
        if let Some(method) = invocation.test_method.as_deref() {
            validate_java_selector(method, "test_method")?;
        }
        let mut arguments =
            self.base_arguments(invocation.module.as_deref(), invocation.also_make)?;
        arguments.push(OsString::from("test"));
        let selector = invocation.test_method.as_ref().map_or_else(
            || invocation.test_class.clone(),
            |method| format!("{}#{method}", invocation.test_class),
        );
        arguments.push(OsString::from(format!("-Dtest={selector}")));
        arguments.push(OsString::from("-Dsurefire.failIfNoSpecifiedTests=false"));
        Ok(arguments)
    }

    fn base_arguments(&self, module: Option<&str>, also_make: bool) -> Result<Vec<OsString>> {
        let mut arguments = vec![
            OsString::from("--batch-mode"),
            OsString::from("--no-transfer-progress"),
        ];
        if !self.network_enabled {
            arguments.push(OsString::from("--offline"));
        }
        if let Some(repository) = &self.execution_repository {
            arguments.push(OsString::from(format!(
                "-Dmaven.repo.local={}",
                repository.display()
            )));
        }
        if let Some(module) = module {
            if !self
                .project
                .modules
                .iter()
                .any(|candidate| candidate.selector == module)
            {
                bail!("unknown reactor module: {module}");
            }
            arguments.push(OsString::from("--projects"));
            arguments.push(OsString::from(module));
            if also_make {
                arguments.push(OsString::from("--also-make"));
            }
        } else if also_make {
            bail!("also_make requires a selected module");
        }
        Ok(arguments)
    }

    fn report_snapshot(&self) -> BTreeMap<PathBuf, SystemTime> {
        discover_report_files(&self.root)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|path| {
                path.metadata()
                    .ok()?
                    .modified()
                    .ok()
                    .map(|time| (path, time))
            })
            .collect()
    }

    fn updated_reports(&self, before: &BTreeMap<PathBuf, SystemTime>) -> Result<Vec<PathBuf>> {
        Ok(discover_report_files(&self.root)?
            .into_iter()
            .filter(|path| {
                let modified = path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok();
                before
                    .get(path)
                    .is_none_or(|previous| modified.is_some_and(|current| current > *previous))
            })
            .collect())
    }

    fn validated_executable(&self) -> Result<&Path> {
        let executable = self.executable.path().canonicalize()?;
        if matches!(self.executable, MavenExecutable::Wrapper(_))
            && !executable.starts_with(&self.root)
        {
            bail!("Maven wrapper escaped the configured project root");
        }
        if !is_executable(&executable)? {
            bail!("selected Maven executable is not executable");
        }
        Ok(self.executable.path())
    }

    async fn configure_java_environment(&self, command: &mut Command) -> Result<()> {
        let JavaEnvironment::Jenv { root } = &self.java_environment else {
            return Ok(());
        };
        let java_home = resolve_jenv_java_home(root, &self.root).await?;
        let java_bin = java_home.join("bin");
        let inherited_path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(java_bin).chain(std::env::split_paths(&inherited_path)),
        )
        .context("cannot construct PATH for jenv-selected Java")?;
        command.env("JAVA_HOME", java_home).env("PATH", path);
        Ok(())
    }

    fn redact(&self, bytes: &[u8]) -> (String, usize) {
        let mut output = String::from_utf8_lossy(bytes).into_owned();
        let mut count = 0;
        if let Some(repository) = &self.execution_repository {
            count += replace_all(
                &mut output,
                &repository.to_string_lossy(),
                "<MAVEN_REPOSITORY>",
            );
        }
        count += replace_all(&mut output, &self.root.to_string_lossy(), "<PROJECT_ROOT>");
        if let Some(home) = std::env::var_os("HOME") {
            count += replace_all(&mut output, &home.to_string_lossy(), "<HOME>");
        }
        let (output, sensitive_count) = redact_sensitive_values(output);
        (output, count + sensitive_count)
    }

    fn runner_error(&self, started: Instant, message: String) -> MavenRunResult {
        let (stderr, redaction_count) = self.redact(message.as_bytes());
        MavenRunResult {
            status: MavenRunStatus::RunnerError,
            exit_code: None,
            duration_ms: elapsed_ms(started),
            timed_out: false,
            stdout: String::new(),
            stderr,
            stdout_truncated: false,
            stderr_truncated: false,
            redaction_count,
        }
    }
}

async fn resolve_jenv_java_home(jenv_root: &Path, project_root: &Path) -> Result<PathBuf> {
    let executable = jenv_root.join("bin/jenv");
    let executable = executable
        .canonicalize()
        .context("configured jenv executable is missing or unreadable")?;
    if !is_executable(&executable)? {
        bail!("configured jenv executable is not executable");
    }

    let mut command = Command::new(executable);
    command
        .arg("prefix")
        .current_dir(project_root)
        .env("JENV_ROOT", jenv_root)
        .env_remove("JENV_DIR")
        .env_remove("JENV_VERSION")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(JENV_RESOLUTION_TIMEOUT, command.output())
        .await
        .context("jenv Java resolution timed out")?
        .context("cannot run configured jenv executable")?;
    if !output.status.success() {
        bail!(
            "jenv could not resolve Java for project_path; verify .java-version and installed jenv versions"
        );
    }
    let prefix = std::str::from_utf8(&output.stdout)
        .context("jenv returned a non-UTF-8 Java home")?
        .trim();
    if prefix.is_empty() || prefix.lines().count() != 1 {
        bail!("jenv returned an invalid Java home");
    }
    let java_home = PathBuf::from(prefix);
    if !java_home.is_absolute() {
        bail!("jenv returned a non-absolute Java home");
    }
    let java_home = java_home
        .canonicalize()
        .context("jenv-selected Java home is missing or unreadable")?;
    let java = java_home.join("bin/java");
    if !java_home.is_dir() || !java.is_file() || !is_executable(&java)? {
        bail!("jenv-selected Java home does not contain executable bin/java");
    }
    Ok(java_home)
}

struct ExecutedMaven {
    result: MavenRunResult,
    raw_stdout: String,
}

pub fn canonical_project_root(project_path: &Path) -> Result<PathBuf> {
    if !project_path.is_absolute() {
        bail!("project_path must be an absolute path");
    }
    let root = project_path
        .canonicalize()
        .context("project_path must be a readable directory")?;
    if !root.is_dir() {
        bail!("project_path must be a directory");
    }
    if !root.join("pom.xml").is_file() {
        bail!("project_path must contain a pom.xml");
    }
    Ok(root)
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawProject {
    #[serde(default)]
    artifact_id: String,
    packaging: Option<String>,
    #[serde(default)]
    modules: RawModules,
}

#[derive(Debug, Deserialize, Default)]
struct RawModules {
    #[serde(rename = "module", default)]
    modules: Vec<String>,
}

fn discover_project(root: &Path, wrapper: bool) -> Result<MavenProject> {
    let mut discovered = BTreeMap::new();
    discover_module(
        root,
        root,
        ".",
        &mut discovered,
        &mut BTreeSet::new(),
        &mut crate::config::XmlBudget::default(),
        0,
    )?;
    let root_module = discovered
        .remove(".")
        .context("root project was not discovered")?;
    Ok(MavenProject {
        artifact_id: root_module.artifact_id,
        packaging: root_module.packaging,
        wrapper,
        modules: discovered.into_values().collect(),
    })
}

fn discover_module(
    root: &Path,
    directory: &Path,
    selector: &str,
    discovered: &mut BTreeMap<String, MavenModule>,
    visited: &mut BTreeSet<PathBuf>,
    xml_budget: &mut crate::config::XmlBudget,
    depth: usize,
) -> Result<()> {
    if depth > crate::config::MAX_XML_DEPTH {
        bail!("module discovery exceeds MAX_XML_DEPTH");
    }
    let directory = directory.canonicalize()?;
    if !directory.starts_with(root) {
        bail!("module path escapes project_path");
    }
    if !visited.insert(directory.clone()) {
        return Ok(());
    }
    let xml = xml_budget
        .read(&directory.join("pom.xml"))
        .map_err(|error| anyhow::anyhow!("module {selector} has no readable pom.xml: {error:#}"))?;
    let raw: RawProject = quick_xml::de::from_str(&xml)
        .with_context(|| format!("cannot parse module POM for {selector}"))?;
    let child_selectors = raw
        .modules
        .modules
        .iter()
        .map(|module| normalize_module_selector(selector, module))
        .collect::<Result<Vec<_>>>()?;
    discovered.insert(
        selector.to_owned(),
        MavenModule {
            selector: selector.to_owned(),
            artifact_id: raw.artifact_id,
            packaging: raw.packaging.unwrap_or_else(|| "jar".to_owned()),
            modules: child_selectors.clone(),
        },
    );
    for (module, child_selector) in raw.modules.modules.iter().zip(child_selectors) {
        let child = directory
            .join(module)
            .canonicalize()
            .with_context(|| format!("module {child_selector} is not a readable directory"))?;
        if !child.starts_with(root) {
            bail!("module path escapes project_path");
        }
        discover_module(
            root,
            &child,
            &child_selector,
            discovered,
            visited,
            xml_budget,
            depth + 1,
        )?;
    }
    Ok(())
}

fn normalize_module_selector(parent: &str, module: &str) -> Result<String> {
    let path = Path::new(module);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        bail!("module selector must remain inside project_path");
    }
    let normalized = if parent == "." {
        module.to_owned()
    } else {
        format!("{parent}/{module}")
    };
    Ok(normalized.replace('\\', "/"))
}

fn select_executable(root: &Path, configured: Option<&Path>) -> Result<MavenExecutable> {
    let wrapper = root.join("mvnw");
    if wrapper.is_file()
        && root.join(".mvn/wrapper/maven-wrapper.properties").is_file()
        && is_executable(&wrapper)?
    {
        return Ok(MavenExecutable::Wrapper(wrapper));
    }
    let configured = configured.context(
        "MAVEN_EXECUTABLE must be an absolute executable path when no Maven Wrapper is available",
    )?;
    if !configured.is_absolute() {
        bail!("MAVEN_EXECUTABLE must be absolute");
    }
    let configured = configured
        .canonicalize()
        .context("MAVEN_EXECUTABLE must exist")?;
    if !is_executable(&configured)? {
        bail!("MAVEN_EXECUTABLE is not executable");
    }
    Ok(MavenExecutable::System(configured))
}

fn is_executable(path: &Path) -> Result<bool> {
    let metadata = path.metadata()?;
    Ok(metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn canonicalize_directory(path: &Path, name: &str) -> Result<PathBuf> {
    let path = path.canonicalize()?;
    if !path.is_dir() {
        bail!("{name} must be a directory");
    }
    Ok(path)
}

#[derive(Default)]
struct BoundedOutput {
    bytes: Vec<u8>,
    tail: VecDeque<u8>,
    truncated: bool,
}

impl BoundedOutput {
    fn into_bytes(self, prefer_tail: bool) -> Vec<u8> {
        if prefer_tail && self.truncated {
            self.tail.into_iter().collect()
        } else {
            self.bytes
        }
    }
}

async fn read_bounded<R>(reader: Option<R>, limit: usize) -> Result<BoundedOutput>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let Some(mut reader) = reader else {
        return Ok(BoundedOutput::default());
    };
    let mut output = BoundedOutput {
        bytes: Vec::with_capacity(limit),
        tail: VecDeque::with_capacity(limit),
        truncated: false,
    };
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if read >= limit {
            output.tail.clear();
            output
                .tail
                .extend(buffer[read - limit..read].iter().copied());
        } else {
            let excess = (output.tail.len() + read).saturating_sub(limit);
            output.tail.drain(..excess);
            output.tail.extend(buffer[..read].iter().copied());
        }
        let remaining = limit.saturating_sub(output.bytes.len());
        output
            .bytes
            .extend_from_slice(&buffer[..read.min(remaining)]);
        output.truncated |= read > remaining;
    }
    Ok(output)
}

async fn spawn_maven(command: &mut Command) -> std::io::Result<Child> {
    const MAX_ATTEMPTS: usize = 3;

    for attempt in 1..=MAX_ATTEMPTS {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error)
                if error.raw_os_error() == Some(nix::libc::ETXTBSY) && attempt < MAX_ATTEMPTS =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("spawn retry loop must return")
}

fn terminate_process_group(process_id: Option<u32>) -> Result<()> {
    if let Some(process_id) = process_id {
        let process_id = i32::try_from(process_id).context("process id is too large")?;
        killpg(Pid::from_raw(process_id), Signal::SIGKILL)?;
    }
    Ok(())
}

struct ProcessGroupGuard {
    process_id: Option<u32>,
}

impl ProcessGroupGuard {
    fn new(process_id: Option<u32>) -> Self {
        Self { process_id }
    }

    fn disarm(&mut self) {
        self.process_id = None;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        let _ = terminate_process_group(self.process_id);
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn replace_all(output: &mut String, value: &str, replacement: &str) -> usize {
    if value.is_empty() {
        return 0;
    }
    let count = output.matches(value).count();
    if count > 0 {
        *output = output.replace(value, replacement);
    }
    count
}

fn redact_sensitive_values(mut output: String) -> (String, usize) {
    let mut count = 0;
    for key in ["password=", "token=", "secret=", "authorization:"] {
        let mut offset = 0;
        loop {
            let lowercase = output.to_lowercase();
            let Some(relative_start) = lowercase[offset..].find(key) else {
                break;
            };
            let start = offset + relative_start;
            let value_start = start + key.len();
            let value_end = output[value_start..]
                .find(char::is_whitespace)
                .map(|offset| value_start + offset)
                .unwrap_or(output.len());
            output.replace_range(value_start..value_end, "<redacted>");
            count += 1;
            offset = value_start + "<redacted>".len();
        }
    }
    let mut offset = 0;
    loop {
        let remaining = &output[offset..];
        let Some(scheme) = remaining.find("://") else {
            break;
        };
        let credentials_start = offset + scheme + 3;
        let after_scheme = &output[credentials_start..];
        let authority_end = after_scheme
            .find(['/', ' ', '\n', '\r'])
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        let Some(at) = authority.rfind('@') else {
            offset = credentials_start + authority_end;
            continue;
        };
        output.replace_range(credentials_start..credentials_start + at, "<credentials>");
        count += 1;
        offset = credentials_start + "<credentials>".len() + 1;
    }
    (output, count)
}

fn parse_reactor_summary(output: &str) -> Vec<ReactorModuleResult> {
    let mut in_summary = false;
    let mut results = Vec::new();
    for line in output.lines() {
        if line.contains("Reactor Summary") {
            in_summary = true;
            continue;
        }
        if !in_summary {
            continue;
        }
        let line = line.trim().trim_start_matches("[INFO]").trim();
        if line.starts_with("BUILD ") {
            break;
        }
        let Some((status_index, status)) = ["SUCCESS", "FAILURE", "SKIPPED"]
            .iter()
            .find_map(|status| line.find(status).map(|index| (index, *status)))
        else {
            continue;
        };
        let module = line[..status_index].trim().trim_end_matches('.').trim();
        if !module.is_empty() {
            results.push(ReactorModuleResult {
                module: module.to_owned(),
                status: status.to_lowercase(),
            });
        }
    }
    results
}

fn validate_java_selector(value: &str, name: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 512
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '$' | '.')
        })
    {
        bail!("{name} must be a Java class or method name without shell syntax");
    }
    Ok(())
}

fn test_source_relative(path: &Path) -> Option<PathBuf> {
    let components = path.components().collect::<Vec<_>>();
    let start = components.windows(3).position(|window| {
        window[0].as_os_str() == "src"
            && window[1].as_os_str() == "test"
            && matches!(window[2].as_os_str().to_str(), Some("java" | "kotlin"))
    })? + 3;
    Some(components[start..].iter().collect())
}

fn discover_report_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut reports = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("");
        if !file_name.starts_with("TEST-")
            || !file_name.ends_with(".xml")
            || path
                .parent()
                .and_then(Path::file_name)
                .and_then(|value| value.to_str())
                != Some("surefire-reports")
        {
            continue;
        }
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(root) {
            bail!("Surefire report escaped project_path");
        }
        if reports.len() >= crate::config::MAX_XML_FILES {
            bail!("XML report discovery exceeds MAX_XML_FILES limit");
        }
        reports.push(canonical);
    }
    reports.sort();
    Ok(reports)
}

#[derive(Debug, Deserialize, Default)]
struct RawTestSuite {
    #[serde(rename = "@tests", default)]
    tests: usize,
    #[serde(rename = "@failures", default)]
    failures: usize,
    #[serde(rename = "@errors", default)]
    errors: usize,
    #[serde(rename = "@skipped", default)]
    skipped: usize,
    #[serde(rename = "testcase", default)]
    test_cases: Vec<RawTestCase>,
}

#[derive(Debug, Deserialize)]
struct RawTestCase {
    #[serde(rename = "@classname", default)]
    class_name: String,
    #[serde(rename = "@name", default)]
    name: String,
    failure: Option<RawTestProblem>,
    error: Option<RawTestProblem>,
}

#[derive(Debug, Deserialize, Default)]
struct RawTestProblem {
    #[serde(rename = "@message")]
    message: Option<String>,
    #[serde(rename = "$text")]
    text: Option<String>,
}

fn parse_test_reports(paths: &[PathBuf]) -> Result<(TestSummary, Vec<TestFailureDetail>)> {
    let mut summary = TestSummary::default();
    let mut failures = Vec::new();
    let mut xml_budget = crate::config::XmlBudget::default();
    for path in paths {
        let xml = xml_budget.read(path)?;
        let suite: RawTestSuite = quick_xml::de::from_str(&xml).with_context(|| {
            format!(
                "invalid Surefire XML report {}",
                path.file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("<unknown>")
            )
        })?;
        summary.failed += suite.failures;
        summary.errors += suite.errors;
        summary.skipped += suite.skipped;
        summary.passed += suite
            .tests
            .saturating_sub(suite.failures + suite.errors + suite.skipped);
        for test_case in suite.test_cases {
            let RawTestCase {
                class_name,
                name,
                failure,
                error,
            } = test_case;
            if let Some(problem) = failure {
                failures.push(test_failure_detail(
                    &class_name,
                    &name,
                    TestFailureKind::Failure,
                    problem,
                ));
            } else if let Some(problem) = error {
                failures.push(test_failure_detail(
                    &class_name,
                    &name,
                    TestFailureKind::Error,
                    problem,
                ));
            }
        }
    }
    failures.sort_by(|left, right| {
        (&left.class_name, &left.test_name).cmp(&(&right.class_name, &right.test_name))
    });
    Ok((summary, failures))
}

fn test_failure_detail(
    class_name: &str,
    test_name: &str,
    kind: TestFailureKind,
    problem: RawTestProblem,
) -> TestFailureDetail {
    let message = problem
        .message
        .or(problem.text)
        .unwrap_or_else(|| "test failed without a message".to_owned())
        .chars()
        .take(2_000)
        .collect();
    TestFailureDetail {
        class_name: class_name.to_owned(),
        test_name: test_name.to_owned(),
        kind,
        message,
    }
}

fn validate_coordinate_filter(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 512
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-' | ':' | '*')
        })
    {
        bail!("coordinate_filter contains unsupported characters");
    }
    Ok(())
}

#[derive(Debug, Deserialize, Default)]
struct RawEffectiveProjects {
    #[serde(rename = "project", default)]
    projects: Vec<RawEffectiveProject>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawEffectiveProject {
    group_id: Option<String>,
    #[serde(default)]
    artifact_id: String,
    version: Option<String>,
    packaging: Option<String>,
    parent: Option<RawEffectiveParent>,
    #[serde(default)]
    properties: BTreeMap<String, String>,
    #[serde(default)]
    dependencies: RawEffectiveDependencies,
    #[serde(default)]
    dependency_management: RawEffectiveDependencyManagement,
    #[serde(default)]
    build: RawEffectiveBuild,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEffectiveParent {
    group_id: Option<String>,
    artifact_id: String,
    version: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawEffectiveDependencies {
    #[serde(rename = "dependency", default)]
    dependencies: Vec<RawEffectiveDependency>,
}

#[derive(Debug, Deserialize, Default)]
struct RawEffectiveDependencyManagement {
    #[serde(default)]
    dependencies: RawEffectiveDependencies,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEffectiveDependency {
    group_id: String,
    artifact_id: String,
    version: Option<String>,
    scope: Option<String>,
    #[serde(rename = "type")]
    dependency_type: Option<String>,
    classifier: Option<String>,
    #[serde(default)]
    exclusions: RawEffectiveExclusions,
}

#[derive(Debug, Deserialize, Default)]
struct RawEffectiveExclusions {
    #[serde(rename = "exclusion", default)]
    exclusions: Vec<RawEffectiveExclusion>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEffectiveExclusion {
    group_id: String,
    artifact_id: String,
}

#[derive(Debug, Deserialize, Default)]
struct RawEffectiveBuild {
    #[serde(default)]
    plugins: RawEffectivePlugins,
}

#[derive(Debug, Deserialize, Default)]
struct RawEffectivePlugins {
    #[serde(rename = "plugin", default)]
    plugins: Vec<RawEffectivePlugin>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEffectivePlugin {
    group_id: Option<String>,
    artifact_id: String,
    version: Option<String>,
}

fn parse_effective_pom(path: &Path) -> Result<Vec<EffectiveProject>> {
    let xml = crate::config::read_bounded_xml(path)?;
    let mut raw_projects = if xml.trim_start().starts_with("<projects") {
        quick_xml::de::from_str::<RawEffectiveProjects>(&xml)?.projects
    } else {
        vec![quick_xml::de::from_str::<RawEffectiveProject>(&xml)?]
    };
    let mut projects = raw_projects
        .drain(..)
        .map(|raw| {
            let map_dependency = |dependency: RawEffectiveDependency| {
                let mut exclusions = dependency
                    .exclusions
                    .exclusions
                    .into_iter()
                    .map(|exclusion| EffectiveExclusion {
                        group_id: exclusion.group_id,
                        artifact_id: exclusion.artifact_id,
                    })
                    .collect::<Vec<_>>();
                exclusions.sort();
                EffectiveDependency {
                    group_id: dependency.group_id,
                    artifact_id: dependency.artifact_id,
                    version: dependency.version,
                    scope: dependency.scope,
                    r#type: dependency.dependency_type,
                    classifier: dependency.classifier,
                    exclusions,
                }
            };
            let mut dependencies = raw
                .dependencies
                .dependencies
                .into_iter()
                .map(&map_dependency)
                .collect::<Vec<_>>();
            dependencies.sort();
            let mut dependency_management = raw
                .dependency_management
                .dependencies
                .dependencies
                .into_iter()
                .map(map_dependency)
                .collect::<Vec<_>>();
            dependency_management.sort();
            let mut plugins = raw
                .build
                .plugins
                .plugins
                .into_iter()
                .map(|plugin| EffectivePlugin {
                    group_id: plugin.group_id,
                    artifact_id: plugin.artifact_id,
                    version: plugin.version,
                })
                .collect::<Vec<_>>();
            plugins.sort();
            EffectiveProject {
                coordinate: EffectiveCoordinate {
                    group_id: raw.group_id,
                    artifact_id: raw.artifact_id,
                    version: raw.version,
                },
                packaging: raw.packaging.unwrap_or_else(|| "jar".to_owned()),
                parent: raw.parent.map(|parent| EffectiveCoordinate {
                    group_id: parent.group_id,
                    artifact_id: parent.artifact_id,
                    version: parent.version,
                }),
                properties: raw.properties,
                dependencies,
                dependency_management,
                plugins,
            }
        })
        .collect::<Vec<_>>();
    projects.sort_by(|left, right| {
        (
            &left.coordinate.group_id,
            &left.coordinate.artifact_id,
            &left.coordinate.version,
        )
            .cmp(&(
                &right.coordinate.group_id,
                &right.coordinate.artifact_id,
                &right.coordinate.version,
            ))
    });
    Ok(projects)
}

#[derive(Debug)]
struct ParsedDependencyResolution {
    explanations: Vec<DependencyResolutionExplanation>,
    malformed: bool,
}

#[derive(Debug)]
struct ParsedDependencyCoordinate {
    identity: String,
    coordinate: String,
    version: String,
}

#[derive(Debug)]
struct ParsedDependencyLine {
    depth: usize,
    coordinate: ParsedDependencyCoordinate,
    status: DependencyPathStatus,
    annotations: Vec<String>,
    requested_version: String,
}

#[derive(Default)]
struct DependencyExplanationBuilder {
    selected_version: Option<String>,
    requested_versions: BTreeSet<String>,
    selection_reasons: BTreeSet<DependencySelectionReason>,
    paths: Vec<DependencyResolutionPath>,
}

fn invalid_dependency_resolution(
    build: MavenBuildResult,
    error: String,
) -> DependencyResolutionResult {
    DependencyResolutionResult {
        status: DependencyResolutionStatus::Invalid,
        explanations: Vec::new(),
        incomplete: false,
        error: Some(error),
        build,
    }
}

fn parse_dependency_resolution(output: &str, project: &MavenProject) -> ParsedDependencyResolution {
    let mut builders = BTreeMap::<String, DependencyExplanationBuilder>::new();
    let mut path_stack = Vec::<String>::new();
    let mut module = ".".to_owned();
    let mut malformed = false;

    for raw_line in output.lines() {
        let line = maven_log_message(raw_line);
        if let Some(artifact_id) = plugin_module_artifact(line) {
            module = module_selector(project, artifact_id);
            path_stack.clear();
            continue;
        }
        let marker = line.find("+- ").or_else(|| line.find("\\- "));
        let Some(marker_index) = marker else {
            if let Some(root) = parse_root_coordinate(line) {
                module = module_selector(project, root.0);
                path_stack.clear();
                path_stack.push(root.1);
            }
            continue;
        };
        let Some(parsed) = parse_dependency_line(line, marker_index) else {
            malformed = true;
            continue;
        };
        let parent_count = parsed.depth.min(path_stack.len());
        path_stack.truncate(parent_count);
        let mut nodes = path_stack
            .iter()
            .cloned()
            .map(|coordinate| DependencyPathNode { coordinate })
            .collect::<Vec<_>>();
        nodes.push(DependencyPathNode {
            coordinate: parsed.coordinate.coordinate.clone(),
        });
        path_stack.push(parsed.coordinate.coordinate.clone());

        let builder = builders
            .entry(parsed.coordinate.identity.clone())
            .or_default();
        builder
            .requested_versions
            .insert(parsed.requested_version.clone());
        if parsed.status == DependencyPathStatus::Selected {
            builder.selected_version = Some(parsed.coordinate.version.clone());
            if parsed.depth == 1 {
                builder
                    .selection_reasons
                    .insert(DependencySelectionReason::DirectDeclaration);
            }
        }
        if parsed
            .annotations
            .iter()
            .any(|annotation| annotation.to_ascii_lowercase().contains("managed"))
        {
            builder
                .selection_reasons
                .insert(DependencySelectionReason::DependencyManagement);
        }
        builder.paths.push(DependencyResolutionPath {
            module: module.clone(),
            nodes,
            requested_version: parsed.requested_version,
            status: parsed.status,
            annotations: parsed.annotations,
        });
    }

    let explanations = builders
        .into_iter()
        .map(|(artifact, mut builder)| {
            add_mediation_reason(&mut builder);
            DependencyResolutionExplanation {
                artifact,
                selected_version: builder.selected_version,
                requested_versions: builder.requested_versions.into_iter().collect(),
                selection_reasons: builder.selection_reasons.into_iter().collect(),
                paths: builder.paths,
            }
        })
        .collect();
    ParsedDependencyResolution {
        explanations,
        malformed,
    }
}

fn add_mediation_reason(builder: &mut DependencyExplanationBuilder) {
    let has_conflict = builder
        .paths
        .iter()
        .any(|path| path.status == DependencyPathStatus::ConflictOmitted);
    if has_conflict {
        if !builder
            .selection_reasons
            .contains(&DependencySelectionReason::DirectDeclaration)
        {
            builder
                .selection_reasons
                .insert(DependencySelectionReason::NearestDefinition);
        }
    } else if builder.requested_versions.len() == 1 {
        builder
            .selection_reasons
            .insert(DependencySelectionReason::OnlyCandidate);
    }
}

fn maven_log_message(line: &str) -> &str {
    let line = line.trim_start().trim_end();
    for level in ["[INFO]", "[WARNING]", "[WARN]", "[ERROR]"] {
        if let Some(message) = line.strip_prefix(level) {
            return message.strip_prefix(' ').unwrap_or(message).trim_end();
        }
    }
    line
}

fn plugin_module_artifact(line: &str) -> Option<&str> {
    let (_, remainder) = line.split_once(" @ ")?;
    let artifact = remainder.trim_end_matches('-').trim();
    (!artifact.is_empty()).then_some(artifact)
}

fn module_selector(project: &MavenProject, artifact_id: &str) -> String {
    project
        .modules
        .iter()
        .find(|module| module.artifact_id == artifact_id)
        .map_or_else(|| ".".to_owned(), |module| module.selector.clone())
}

fn parse_root_coordinate(line: &str) -> Option<(&str, String)> {
    if line.contains(char::is_whitespace) || line.starts_with('(') {
        return None;
    }
    let parts = line.split(':').collect::<Vec<_>>();
    if !(4..=5).contains(&parts.len()) || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    Some((parts[1], normalize_coordinate(&parts)?))
}

fn parse_dependency_line(line: &str, marker_index: usize) -> Option<ParsedDependencyLine> {
    let depth = marker_index / 3 + 1;
    let value = line.get(marker_index + 3..)?.trim();
    let (coordinate_text, annotations) = split_dependency_annotations(value);
    let parts = coordinate_text
        .trim_matches(|character| character == '(' || character == ')')
        .split(':')
        .collect::<Vec<_>>();
    let coordinate = parse_dependency_coordinate(&parts)?;
    let annotation_text = annotations.join("; ").to_ascii_lowercase();
    let status = if annotation_text.contains("excluded") || annotation_text.contains("exclusion") {
        DependencyPathStatus::Excluded
    } else if annotation_text.contains("omitted for conflict") {
        DependencyPathStatus::ConflictOmitted
    } else if annotation_text.contains("omitted for duplicate") {
        DependencyPathStatus::DuplicateOmitted
    } else {
        DependencyPathStatus::Selected
    };
    let requested_version =
        managed_from_version(&annotations).unwrap_or_else(|| coordinate.version.clone());
    Some(ParsedDependencyLine {
        depth,
        coordinate,
        status,
        annotations,
        requested_version,
    })
}

fn split_dependency_annotations(value: &str) -> (&str, Vec<String>) {
    let trimmed = value.trim();
    if let Some(inner) = trimmed
        .strip_prefix('(')
        .and_then(|text| text.strip_suffix(')'))
        && let Some((coordinate, annotation)) = inner.split_once(" - ")
    {
        return (coordinate.trim(), vec![annotation.trim().to_owned()]);
    }
    if let Some((coordinate, annotation)) = trimmed.rsplit_once(" (") {
        return (
            coordinate.trim(),
            vec![annotation.trim_end_matches(')').trim().to_owned()],
        );
    }
    (trimmed, Vec::new())
}

fn parse_dependency_coordinate(parts: &[&str]) -> Option<ParsedDependencyCoordinate> {
    if !matches!(parts.len(), 5 | 6) || parts.iter().any(|part| part.is_empty()) {
        return None;
    }
    let scope_index = parts.iter().position(|part| is_dependency_scope(part))?;
    let version_index = match (parts.len(), scope_index) {
        (5, 4) => 3,
        (5, 3) => 4,
        (6, 5) => 4,
        (6, 4) => 5,
        _ => return None,
    };
    let classifier = (parts.len() == 6).then_some(parts[3]);
    let identity = classifier.map_or_else(
        || format!("{}:{}:{}", parts[0], parts[1], parts[2]),
        |classifier| format!("{}:{}:{}:{classifier}", parts[0], parts[1], parts[2]),
    );
    let coordinate = classifier.map_or_else(
        || format!("{}:{}:{}", parts[0], parts[1], parts[version_index]),
        |classifier| {
            format!(
                "{}:{}:{}:{classifier}",
                parts[0], parts[1], parts[version_index]
            )
        },
    );
    Some(ParsedDependencyCoordinate {
        identity,
        coordinate,
        version: parts[version_index].to_owned(),
    })
}

fn normalize_coordinate(parts: &[&str]) -> Option<String> {
    match parts {
        [group, artifact, _kind, version] => Some(format!("{group}:{artifact}:{version}")),
        [group, artifact, _kind, classifier, version] => {
            Some(format!("{group}:{artifact}:{version}:{classifier}"))
        }
        _ => None,
    }
}

fn is_dependency_scope(value: &str) -> bool {
    matches!(
        value,
        "compile" | "provided" | "runtime" | "test" | "system" | "import"
    )
}

fn managed_from_version(annotations: &[String]) -> Option<String> {
    annotations.iter().find_map(|annotation| {
        let lowercase = annotation.to_ascii_lowercase();
        let start = lowercase.find("version managed from ")? + "version managed from ".len();
        annotation[start..]
            .split([';', ',', ' '])
            .next()
            .filter(|version| !version.is_empty())
            .map(str::to_owned)
    })
}

fn coordinate_matches(pattern: &str, explanation: &DependencyResolutionExplanation) -> bool {
    wildcard_matches(pattern, &explanation.artifact)
        || explanation.paths.iter().any(|path| {
            path.nodes
                .last()
                .is_some_and(|node| wildcard_matches(pattern, &node.coordinate))
        })
}

fn wildcard_matches(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let (mut pattern_index, mut value_index) = (0, 0);
    let (mut star_index, mut star_value_index) = (None, 0);
    while value_index < value.len() {
        if pattern.get(pattern_index) == Some(&value[value_index]) {
            pattern_index += 1;
            value_index += 1;
        } else if pattern.get(pattern_index) == Some(&b'*') {
            star_index = Some(pattern_index);
            pattern_index += 1;
            star_value_index = value_index;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }
    pattern[pattern_index..]
        .iter()
        .all(|character| *character == b'*')
}

fn parse_dependency_tree(output: &str) -> Vec<DependencyNode> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches("[INFO]").trim();
            let marker = line.find("+- ").or_else(|| line.find("\\- "));
            let (depth, coordinate) =
                marker.map_or((0, line), |index| (index / 3 + 1, line[index + 3..].trim()));
            let coordinate = coordinate.split_whitespace().next()?.trim_end_matches(',');
            let parts = coordinate.split(':').collect::<Vec<_>>();
            let (normalized, scope) = match parts.as_slice() {
                [group, artifact, _kind, scope, version] => (
                    format!("{group}:{artifact}:{version}"),
                    Some((*scope).to_owned()),
                ),
                [group, artifact, _kind, classifier, scope, version] => (
                    format!("{group}:{artifact}:{version}:{classifier}"),
                    Some((*scope).to_owned()),
                ),
                _ => return None,
            };
            Some(DependencyNode {
                depth,
                coordinate: normalized,
                scope,
            })
        })
        .collect()
}

fn parse_classpath(output: &str) -> (Vec<String>, bool) {
    let (classpath_lines, mut incomplete) = classpath_lines(output);
    let mut artifacts = classpath_lines
        .into_iter()
        .flat_map(|classpath| classpath.split(':'))
        .filter(|item| !item.is_empty())
        .filter_map(|item| match coordinate_from_repository_path(item) {
            Some(coordinate) => Some(coordinate),
            None => {
                incomplete = true;
                None
            }
        })
        .collect::<Vec<_>>();
    artifacts.sort();
    artifacts.dedup();
    (artifacts, incomplete)
}

fn parse_classpath_paths(output: &str) -> (Vec<PathBuf>, bool) {
    let (classpath_lines, mut incomplete) = classpath_lines(output);
    let mut paths = classpath_lines
        .into_iter()
        .flat_map(|classpath| classpath.split(':'))
        .filter(|item| !item.is_empty())
        .filter_map(|item| {
            let path = PathBuf::from(item);
            if path.is_absolute() {
                Some(path)
            } else {
                incomplete = true;
                None
            }
        })
        .collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    (paths, incomplete)
}

fn classpath_lines(output: &str) -> (Vec<&str>, bool) {
    let mut lines = output.lines();
    let mut classpaths = Vec::new();
    let mut found_marker = false;
    let mut incomplete = false;
    while let Some(line) = lines.next() {
        if !line.contains("Dependencies classpath:") {
            continue;
        }
        found_marker = true;
        match lines.next() {
            Some(classpath) => classpaths.push(classpath.trim()),
            None => incomplete = true,
        }
    }
    (classpaths, incomplete || !found_marker)
}

fn coordinate_from_repository_path(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let relative = normalized.strip_prefix("<MAVEN_REPOSITORY>/").or_else(|| {
        normalized
            .rsplit_once("/repository/")
            .map(|(_, value)| value)
    })?;
    let parts = relative.split('/').collect::<Vec<_>>();
    if parts.len() < 4 {
        return None;
    }
    let file_name = parts.last()?.strip_suffix(".jar")?;
    let version = parts[parts.len() - 2];
    let artifact = parts[parts.len() - 3];
    let group = parts[..parts.len() - 3].join(".");
    let prefix = format!("{artifact}-{version}");
    let suffix = file_name.strip_prefix(&prefix)?;
    let classifier = suffix.strip_prefix('-').filter(|value| !value.is_empty());
    Some(classifier.map_or_else(
        || format!("{group}:{artifact}:{version}"),
        |classifier| format!("{group}:{artifact}:{version}:{classifier}"),
    ))
}

#[derive(Debug, Deserialize, Default)]
struct RawJacocoReport {
    #[serde(rename = "package", default)]
    packages: Vec<RawJacocoPackage>,
    #[serde(rename = "counter", default)]
    counters: Vec<RawJacocoCounter>,
}

#[derive(Debug, Clone, Deserialize)]
struct RawJacocoCounter {
    #[serde(rename = "@type")]
    kind: String,
    #[serde(rename = "@missed")]
    missed: u64,
    #[serde(rename = "@covered")]
    covered: u64,
}

#[derive(Debug, Deserialize)]
struct RawJacocoPackage {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "class", default)]
    classes: Vec<RawJacocoClass>,
}

#[derive(Debug, Deserialize)]
struct RawJacocoClass {
    #[serde(rename = "@name", default)]
    name: String,
    #[serde(rename = "counter", default)]
    counters: Vec<RawJacocoCounter>,
}

struct ParsedCoverageReport {
    module: String,
    metrics: CoverageMetrics,
    packages: Vec<RawJacocoPackage>,
    stale: bool,
}

struct ParsedCoverageReports {
    reports: Vec<ParsedCoverageReport>,
    missing_modules: Vec<String>,
}

fn discover_jacoco_reports(root: &Path) -> Result<Vec<PathBuf>> {
    let mut reports = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if !entry.file_type().is_file() || entry.file_name() != "jacoco.xml" {
            continue;
        }
        let path = entry.path();
        if !path
            .components()
            .any(|component| component.as_os_str() == "target")
        {
            continue;
        }
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(root) {
            bail!("JaCoCo report escaped project_path");
        }
        if reports.len() >= crate::config::MAX_XML_FILES {
            bail!("XML report discovery exceeds MAX_XML_FILES limit");
        }
        reports.push(canonical);
    }
    reports.sort();
    Ok(reports)
}

fn report_is_stale(report: &Path, module_root: &Path) -> Result<bool> {
    let report_modified = report.metadata()?.modified()?;
    for directory in ["target/classes", "target/test-classes"] {
        let directory = module_root.join(directory);
        if !directory.is_dir() {
            continue;
        }
        for entry in WalkDir::new(directory).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_file() && entry.metadata()?.modified()? > report_modified {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn coverage_counter(counter: &RawJacocoCounter) -> CoverageCounter {
    let total = counter.missed + counter.covered;
    CoverageCounter {
        missed: counter.missed,
        covered: counter.covered,
        percent: (total > 0).then(|| counter.covered as f64 * 100.0 / total as f64),
    }
}

fn coverage_metrics(counters: &[RawJacocoCounter]) -> CoverageMetrics {
    let find = |kind: &str| {
        counters
            .iter()
            .find(|counter| counter.kind == kind)
            .map(coverage_counter)
    };
    CoverageMetrics {
        instructions: find("INSTRUCTION"),
        branches: find("BRANCH"),
        lines: find("LINE"),
        methods: find("METHOD"),
        classes: find("CLASS"),
    }
}

fn coverage_status(reports: &[ParsedCoverageReport]) -> CoverageStatus {
    if reports.is_empty() {
        CoverageStatus::Missing
    } else if reports.iter().any(|report| report.stale) {
        CoverageStatus::Stale
    } else {
        CoverageStatus::Available
    }
}

fn coverage_summary(parsed: ParsedCoverageReports) -> CoverageSummaryResult {
    let status = coverage_status(&parsed.reports);
    let project_metrics =
        aggregate_coverage_metrics(parsed.reports.iter().map(|report| &report.metrics));
    CoverageSummaryResult {
        status,
        project_metrics,
        reports: parsed
            .reports
            .into_iter()
            .map(|report| ModuleCoverage {
                module: report.module,
                metrics: report.metrics,
                stale: report.stale,
            })
            .collect(),
        missing_modules: parsed.missing_modules,
        error: None,
    }
}

fn aggregate_coverage_metrics<'a>(
    metrics: impl Iterator<Item = &'a CoverageMetrics>,
) -> CoverageMetrics {
    let metrics = metrics.collect::<Vec<_>>();
    let sum = |select: fn(&CoverageMetrics) -> &Option<CoverageCounter>| {
        let counters = metrics
            .iter()
            .filter_map(|metrics| select(metrics).as_ref())
            .collect::<Vec<_>>();
        (!counters.is_empty()).then(|| {
            let missed = counters.iter().map(|counter| counter.missed).sum();
            let covered = counters.iter().map(|counter| counter.covered).sum();
            let total = missed + covered;
            CoverageCounter {
                missed,
                covered,
                percent: (total > 0).then(|| covered as f64 * 100.0 / total as f64),
            }
        })
    };
    CoverageMetrics {
        instructions: sum(|metrics| &metrics.instructions),
        branches: sum(|metrics| &metrics.branches),
        lines: sum(|metrics| &metrics.lines),
        methods: sum(|metrics| &metrics.methods),
        classes: sum(|metrics| &metrics.classes),
    }
}

fn coverage_gaps(parsed: ParsedCoverageReports, limit: usize) -> CoverageGapResult {
    let status = coverage_status(&parsed.reports);
    let mut ranked = parsed
        .reports
        .iter()
        .flat_map(|report| {
            report.packages.iter().flat_map(move |package| {
                package.classes.iter().map(move |class| {
                    let metrics = coverage_metrics(&class.counters);
                    let score = metrics
                        .lines
                        .as_ref()
                        .or(metrics.instructions.as_ref())
                        .map_or(u64::MAX, coverage_basis_points);
                    let missed = metrics
                        .lines
                        .as_ref()
                        .or(metrics.instructions.as_ref())
                        .map_or(0, |counter| counter.missed);
                    (
                        score,
                        std::cmp::Reverse(missed),
                        CoverageGap {
                            module: report.module.clone(),
                            package_name: package.name.replace('/', "."),
                            class_name: class.name.replace('/', "."),
                            line: metrics.lines,
                            branch: metrics.branches,
                            instruction: metrics.instructions,
                        },
                    )
                })
            })
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        (&left.0, &left.1, &left.2.module, &left.2.class_name).cmp(&(
            &right.0,
            &right.1,
            &right.2.module,
            &right.2.class_name,
        ))
    });
    let incomplete = ranked.len() > limit;
    CoverageGapResult {
        status,
        gaps: ranked
            .into_iter()
            .take(limit)
            .map(|(_, _, gap)| gap)
            .collect(),
        missing_modules: parsed.missing_modules,
        incomplete,
        error: None,
    }
}

fn coverage_basis_points(counter: &CoverageCounter) -> u64 {
    let total = counter.missed + counter.covered;
    if total == 0 {
        u64::MAX
    } else {
        counter.covered.saturating_mul(10_000) / total
    }
}

#[cfg(test)]
#[path = "../tests/unit/project.rs"]
mod tests;
