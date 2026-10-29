//! Opt-in multi-project smoke suite for an installed candidate or stable binary on real
//! Maven projects. Run it through `scripts/dogfood-smoke.sh`, which validates that the
//! private project manifest and the result directory are outside the repository. It is
//! ignored by the deterministic gate.
//!
//! Every project gets a freshly started server (a cold, cache-free process unless the
//! manifest sets `MAVEN_MCP_INDEX_CACHE`) trusted for
//! exactly that project. The suite checks initialize, tools/list, project inspection,
//! index build and reuse, class source, dependency tree, classpath, effective POM, Maven
//! and JDK versions through `run_maven`, and an optional focused test and lifecycle phase,
//! and records timings and server RSS. The JSON record is private evidence and never
//! belongs in the repository.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use rmcp::{
    RoleClient, ServiceExt,
    model::CallToolRequestParams,
    service::RunningService,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use serde::Deserialize;
use serde_json::{Value, json};

const MANIFEST_SCHEMA_VERSION: u32 = 1;
const RECORD_SCHEMA_VERSION: u32 = 1;
const DEFAULT_REUSE_REPETITIONS: usize = 5;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
const REQUIRED_TOOLS: [&str; 10] = [
    "inspect_maven_project",
    "index_stats",
    "search_classes",
    "get_class_source",
    "get_dependency_tree",
    "get_maven_classpath",
    "get_effective_pom",
    "run_maven",
    "list_maven_test_classes",
    "run_maven_test",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema_version: u32,
    #[serde(default)]
    server_arguments: Vec<String>,
    #[serde(default)]
    server_environment: std::collections::BTreeMap<String, String>,
    reuse_repetitions: Option<usize>,
    projects: Vec<ProjectCase>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectCase {
    id: String,
    layout: Layout,
    path: PathBuf,
    /// Substring that must find at least one class on the project's test classpath.
    class_query: String,
    /// Optional fully qualified class whose `-sources.jar` source must be returned.
    source_class: Option<String>,
    /// Optional focused test (opt-in project execution).
    focused_test: Option<FocusedTest>,
    /// Optional allowlisted lifecycle phase (opt-in project execution).
    lifecycle: Option<Lifecycle>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Layout {
    SingleModule,
    MultiModule,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FocusedTest {
    test_class: String,
    test_method: Option<String>,
    module: Option<String>,
    #[serde(default)]
    also_make: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lifecycle {
    phase: String,
    module: Option<String>,
    #[serde(default)]
    also_make: bool,
}

struct SmokeEnvironment {
    binary: PathBuf,
    manifest: Manifest,
    output: PathBuf,
}

impl SmokeEnvironment {
    fn from_env() -> Result<Self> {
        let path = |name: &str| -> Result<PathBuf> {
            let value = std::env::var_os(name)
                .map(PathBuf::from)
                .with_context(|| format!("{name} is not set; run scripts/dogfood-smoke.sh"))?;
            ensure!(value.is_absolute(), "{name} must be absolute");
            Ok(value)
        };
        let binary = path("MAVEN_MCP_DOGFOOD_BINARY")?;
        let manifest_path = path("MAVEN_MCP_DOGFOOD_MANIFEST")?;
        let output = path("MAVEN_MCP_DOGFOOD_OUTPUT")?;
        std::fs::create_dir_all(&output)?;
        for private in [&manifest_path, &output] {
            ensure!(
                !is_inside_repository(private)?,
                "{} must be outside the public repository",
                private.display()
            );
        }
        let manifest: Manifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)
            .context("invalid dogfood manifest")?;
        ensure!(
            manifest.schema_version == MANIFEST_SCHEMA_VERSION,
            "unsupported manifest schema_version {}",
            manifest.schema_version
        );
        ensure!(
            !manifest.projects.is_empty(),
            "the manifest lists no project"
        );
        for project in &manifest.projects {
            ensure!(
                project.path.is_absolute() && project.path.join("pom.xml").is_file(),
                "project {} must be an absolute Maven project root",
                project.id
            );
        }
        Ok(Self {
            binary,
            manifest,
            output,
        })
    }
}

fn is_inside_repository(path: &Path) -> Result<bool> {
    let repository = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR"))?;
    Ok(std::fs::canonicalize(path)?.starts_with(repository))
}

/// One checked step of a project run; failures are recorded, not thrown, so the record
/// keeps every result of a partially failing project.
#[derive(Default)]
struct Steps {
    entries: Vec<Value>,
    failed: usize,
}

impl Steps {
    fn last_duration_ms(&self) -> Value {
        self.entries
            .last()
            .map_or(Value::Null, |step| step["duration_ms"].clone())
    }

    async fn run<F>(&mut self, name: &str, step: F) -> Option<Value>
    where
        F: std::future::Future<Output = Result<Value>>,
    {
        let started = Instant::now();
        let result = step.await;
        let duration_ms = elapsed_ms(started);
        match result {
            Ok(detail) => {
                self.entries.push(json!({
                    "name": name, "ok": true, "duration_ms": duration_ms, "detail": detail,
                }));
                Some(detail)
            }
            Err(error) => {
                self.failed += 1;
                self.entries.push(json!({
                    "name": name, "ok": false, "duration_ms": duration_ms,
                    "error": format!("{error:#}"),
                }));
                None
            }
        }
    }
}

fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

/// One started server bound to one manifest project.
struct Session<'a> {
    client: RunningService<RoleClient, ()>,
    pid: u32,
    project: &'a ProjectCase,
    project_path: String,
}

impl Session<'_> {
    /// Calls `tool` with the session's `project_path` added to `arguments`.
    async fn call(&self, tool: &str, arguments: Value) -> Result<Value> {
        let mut arguments = arguments
            .as_object()
            .context("tool arguments must be an object")?
            .clone();
        arguments.insert("project_path".into(), json!(self.project_path));
        let result = self
            .client
            .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments))
            .await?;
        if result.is_error != Some(false) {
            bail!("{tool} returned an MCP error: {:?}", result.content);
        }
        result
            .structured_content
            .with_context(|| format!("{tool} returned no structured content"))
    }
}

fn array_len(value: &Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}

fn ensure_build_succeeded(tool: &str, result: &Value) -> Result<()> {
    let outcome = &result["build"]["outcome"];
    ensure!(outcome == "success", "{tool} build outcome {outcome}");
    Ok(())
}

/// VmRSS and VmHWM of the server process, in bytes.
fn server_memory(pid: u32) -> Value {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    let kilobytes = |key: &str| {
        status.lines().find_map(|line| {
            line.strip_prefix(key)?
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
                .map(|value| value * 1024)
        })
    };
    json!({ "rss_bytes": kilobytes("VmRSS:"), "peak_rss_bytes": kilobytes("VmHWM:") })
}

fn first_line_starting_with<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with(prefix))
}

async fn start_session<'a>(
    environment: &SmokeEnvironment,
    project: &'a ProjectCase,
    runtime_dir: &Path,
) -> Result<Session<'a>> {
    let manifest = &environment.manifest;
    let command = tokio::process::Command::new(&environment.binary).configure(|command| {
        // Cold by default; a manifest may enable the on-disk index cache explicitly.
        command
            .args(&manifest.server_arguments)
            .env("MAVEN_MCP_INDEX_CACHE", "false")
            .envs(&manifest.server_environment)
            .env("MAVEN_TRUSTED_PROJECT_DIRECTORIES", &project.path)
            .env("MAVEN_MCP_RUNTIME_DIR", runtime_dir)
            .env("RUST_LOG", "maven_mcp=warn");
    });
    let transport = TokioChildProcess::new(command)?;
    let pid = transport.id().context("missing STDIO server process id")?;
    let client = tokio::time::timeout(STARTUP_TIMEOUT, ().serve(transport))
        .await
        .context("STDIO MCP server startup timed out")?
        .context("STDIO MCP client initialization failed")?;
    Ok(Session {
        client,
        pid,
        project,
        project_path: project.path.display().to_string(),
    })
}

/// initialize (identity), tools/list and the declared project layout.
async fn check_protocol(
    steps: &mut Steps,
    session: &Session<'_>,
    server_version: Option<&str>,
    candidate_id: Option<&str>,
) {
    steps
        .run("initialize", async {
            let version = server_version.context("no serverInfo")?;
            if let Some(expected) = candidate_id {
                ensure!(
                    version == expected,
                    "serverInfo.version {version} != {expected}"
                );
            }
            Ok(json!({ "server_version": version }))
        })
        .await;
    steps
        .run("tools_list", async {
            let tools = session.client.list_all_tools().await?;
            let missing = REQUIRED_TOOLS
                .iter()
                .filter(|required| !tools.iter().any(|tool| tool.name == **required))
                .collect::<Vec<_>>();
            ensure!(missing.is_empty(), "missing tools {missing:?}");
            Ok(json!({ "tool_count": tools.len() }))
        })
        .await;
    steps
        .run("inspect_maven_project", async {
            let result = session.call("inspect_maven_project", json!({})).await?;
            let modules = array_len(&result["modules"]);
            match session.project.layout {
                Layout::SingleModule => ensure!(modules <= 1, "expected one module: {modules}"),
                Layout::MultiModule => ensure!(modules > 1, "expected modules: {modules}"),
            }
            Ok(json!({
                "modules": modules,
                "packaging": result["packaging"],
                "wrapper": result["wrapper"],
            }))
        })
        .await;
}

/// Cold first query, cached reuse, index statistics and class source.
async fn check_index(
    steps: &mut Steps,
    session: &Session<'_>,
    repetitions: usize,
    metrics: &mut serde_json::Map<String, Value>,
) {
    let search = json!({ "query": session.project.class_query, "limit": 5 });
    let cold = steps
        .run("cold_first_query", async {
            let result = session.call("search_classes", search.clone()).await?;
            let count = array_len(&result["results"]);
            ensure!(count > 0, "search_classes found no class for the query");
            Ok(json!({ "results": count }))
        })
        .await;
    if cold.is_some() {
        metrics.insert("cold_first_query_ms".into(), steps.last_duration_ms());
    }
    metrics.insert("memory_after_cold_query".into(), server_memory(session.pid));
    steps
        .run("reuse_queries", async {
            ensure!(repetitions > 0, "reuse_repetitions must be positive");
            let mut durations = Vec::with_capacity(repetitions);
            for _ in 0..repetitions {
                let started = Instant::now();
                session.call("search_classes", search.clone()).await?;
                durations.push(elapsed_ms(started));
            }
            let mean = durations.iter().sum::<f64>() / durations.len() as f64;
            let min = durations.iter().copied().fold(f64::INFINITY, f64::min);
            let max = durations.iter().copied().fold(0.0, f64::max);
            Ok(json!({
                "repetitions": durations.len(),
                "mean_ms": mean,
                "min_ms": min,
                "max_ms": max,
            }))
        })
        .await;
    steps
        .run("index_stats", session.call("index_stats", json!({})))
        .await;
    if let Some(source_class) = &session.project.source_class {
        steps
            .run("class_source", async {
                let result = session
                    .call("get_class_source", json!({ "class_name": source_class }))
                    .await?;
                let first = &result["results"][0];
                let source = first["source"].as_str().unwrap_or_default();
                ensure!(!source.is_empty(), "no source returned");
                Ok(json!({ "truncated": first["truncated"] }))
            })
            .await;
    }
}

/// Dependency tree, test classpath, effective POM, and the Maven and JDK versions.
async fn check_maven_reports(steps: &mut Steps, session: &Session<'_>) -> Option<Value> {
    steps
        .run("dependency_tree", async {
            let result = session.call("get_dependency_tree", json!({})).await?;
            ensure_build_succeeded("get_dependency_tree", &result)?;
            Ok(json!({
                "dependencies": array_len(&result["dependencies"]),
                "incomplete": result["incomplete"],
            }))
        })
        .await;
    steps
        .run("test_classpath", async {
            let result = session
                .call("get_maven_classpath", json!({ "kind": "test" }))
                .await?;
            ensure_build_succeeded("get_maven_classpath", &result)?;
            Ok(json!({
                "artifacts": array_len(&result["artifacts"]),
                "incomplete": result["incomplete"],
            }))
        })
        .await;
    steps
        .run("effective_pom", async {
            let result = session.call("get_effective_pom", json!({})).await?;
            let status = &result["status"];
            ensure!(status == "available", "effective POM status {status}");
            Ok(json!({ "projects": array_len(&result["projects"]) }))
        })
        .await;
    steps
        .run("maven_and_jdk_versions", async {
            let result = session
                .call("run_maven", json!({ "arguments": ["--version"] }))
                .await?;
            let status = &result["status"];
            ensure!(status == "success", "mvn --version status {status}");
            let stdout = result["stdout"].as_str().unwrap_or_default();
            Ok(json!({
                "maven": first_line_starting_with(stdout, "Apache Maven"),
                "java": first_line_starting_with(stdout, "Java version:"),
            }))
        })
        .await
}

/// Test discovery and the opt-in focused test and lifecycle phase.
async fn check_execution(steps: &mut Steps, session: &Session<'_>) {
    steps
        .run("list_test_classes", async {
            let result = session.call("list_maven_test_classes", json!({})).await?;
            Ok(json!({ "test_classes": array_len(&result["results"]) }))
        })
        .await;
    if let Some(test) = &session.project.focused_test {
        let arguments = json!({
            "test_class": test.test_class,
            "test_method": test.test_method,
            "module": test.module,
            "also_make": test.also_make,
        });
        steps
            .run("focused_test", async {
                let result = session.call("run_maven_test", arguments).await?;
                ensure_build_succeeded("run_maven_test", &result)?;
                let summary = &result["summary"];
                let passed = summary["passed"].as_u64().unwrap_or_default();
                ensure!(passed > 0, "no passing test in {summary}");
                Ok(json!({
                    "summary": summary,
                    "duration_ms": result["build"]["run"]["duration_ms"],
                }))
            })
            .await;
    }
    if let Some(lifecycle) = &session.project.lifecycle {
        let arguments = json!({
            "phase": lifecycle.phase,
            "module": lifecycle.module,
            "also_make": lifecycle.also_make,
        });
        steps
            .run("lifecycle", async {
                let result = session.call("run_maven_lifecycle", arguments).await?;
                let outcome = &result["outcome"];
                ensure!(outcome == "success", "lifecycle outcome {outcome}");
                Ok(json!({
                    "reactor_modules": array_len(&result["reactor_summary"]),
                    "duration_ms": result["run"]["duration_ms"],
                }))
            })
            .await;
    }
}

async fn run_project(
    environment: &SmokeEnvironment,
    project: &ProjectCase,
    candidate_id: Option<&str>,
) -> Result<(Value, usize)> {
    let runtime_dir = tempfile::tempdir()?;
    let repetitions = environment
        .manifest
        .reuse_repetitions
        .unwrap_or(DEFAULT_REUSE_REPETITIONS);
    let mut steps = Steps::default();
    let mut metrics = serde_json::Map::new();

    let started = Instant::now();
    let session = start_session(environment, project, runtime_dir.path()).await?;
    metrics.insert("start_and_initialize_ms".into(), json!(elapsed_ms(started)));
    metrics.insert("memory_after_initialize".into(), server_memory(session.pid));
    let server_version = session
        .client
        .peer_info()
        .and_then(|info| info.server_info.clone())
        .map(|info| info.version);

    check_protocol(
        &mut steps,
        &session,
        server_version.as_deref(),
        candidate_id,
    )
    .await;
    check_index(&mut steps, &session, repetitions, &mut metrics).await;
    let toolchain = check_maven_reports(&mut steps, &session).await;
    check_execution(&mut steps, &session).await;

    metrics.insert("memory_at_end".into(), server_memory(session.pid));
    session.client.cancel().await?;

    let failed = steps.failed;
    let record = json!({
        "id": project.id,
        "layout": project.layout_label(),
        "server_version": server_version,
        "toolchain": toolchain,
        "metrics": metrics,
        "steps": steps.entries,
        "failed_steps": failed,
    });
    Ok((record, failed))
}

impl ProjectCase {
    fn layout_label(&self) -> &'static str {
        match self.layout {
            Layout::SingleModule => "single_module",
            Layout::MultiModule => "multi_module",
        }
    }
}

/// The `<version>+<short commit>` id the binary reports, or `None` for unidentified builds.
fn candidate_identity(binary: &Path) -> Result<(String, Option<String>)> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()?;
    ensure!(
        output.status.success(),
        "{} --version failed",
        binary.display()
    );
    let line = String::from_utf8(output.stdout)?.trim().to_owned();
    let id = line
        .strip_prefix("maven-mcp ")
        .and_then(|rest| rest.strip_suffix(')'))
        .and_then(|rest| rest.split_once(" (commit "))
        .filter(|(_, commit)| commit.len() == 40)
        .map(|(version, commit)| format!("{version}+{}", &commit[..12]));
    Ok((line, id))
}

#[tokio::test]
#[ignore = "opt-in: run scripts/dogfood-smoke.sh with a private project manifest"]
async fn installed_binary_serves_real_maven_projects() -> Result<()> {
    let environment = SmokeEnvironment::from_env()?;
    let (version_line, candidate_id) = candidate_identity(&environment.binary)?;
    let started_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();

    let mut projects = Vec::new();
    let mut failed = 0;
    for project in &environment.manifest.projects {
        eprintln!("dogfood: {} ({})", project.id, project.layout_label());
        let (record, project_failures) =
            run_project(&environment, project, candidate_id.as_deref())
                .await
                .with_context(|| format!("project {}", project.id))?;
        failed += project_failures;
        projects.push(record);
    }

    let record = json!({
        "schema_version": RECORD_SCHEMA_VERSION,
        "candidate": { "id": candidate_id, "version": version_line },
        "host": { "os": std::env::consts::OS, "arch": std::env::consts::ARCH },
        "started_unix_ms": started_unix_ms,
        "cache": "none (fresh server process per project; no persistent index)",
        "projects": projects,
    });
    let path = environment.output.join(format!(
        "dogfood-{}-{started_unix_ms}.json",
        candidate_id.as_deref().unwrap_or("unidentified")
    ));
    std::fs::write(&path, serde_json::to_vec_pretty(&record)?)?;
    eprintln!("dogfood: record written to {}", path.display());
    ensure!(
        failed == 0,
        "{failed} dogfood step(s) failed; see {}",
        path.display()
    );
    Ok(())
}
