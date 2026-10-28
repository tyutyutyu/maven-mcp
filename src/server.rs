use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{
        router::tool::ToolRouter,
        wrapper::{Json, Parameters},
    },
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedMutexGuard, Semaphore};

use crate::blocking::{HeavyWork, HeavyWorkError, HeavyWorkLimits};
use crate::build_info;
use crate::config::Config;
use crate::index::{
    ArtifactApiDiff, ArtifactHealth, ArtifactOutsideRepository, ClassDescription, ClassList,
    ClassLocation, ClassMemberMatch, ClassReference, ClassReferenceKind, DeclarationSourceLookup,
    EntryMatch, IndexStats, JarContentSearch, JarEntryContent, JarSummary, MavenIndex,
    PomDescriptorLookup, ProviderDescriptorKind, ProviderFact, ReactorLayout, ReferenceDirection,
    SourceResult, SourceSearch, TypeHierarchyMatch, UnindexedClasspathEntry, UnindexedReason,
    validate_exact_coordinate,
};
use crate::project::{
    ClasspathKind, CoverageGapResult, CoverageSummaryResult, DependencyResolutionResult,
    DependencyResolutionStatus, DependencyScope, DependencyTreeResult, EffectivePomResult,
    FocusedTestInvocation, FocusedTestResult, LastTestFailures, LifecyclePhase, MavenBuildOutcome,
    MavenBuildResult, MavenClasspathResult, MavenInvocation, MavenProject, MavenRunResult,
    MavenRunner, canonical_project_root,
};
use crate::project_cache::{
    BuildOutcome, BuildOwner, CacheLimits, IndexLookup, ProjectCache, ProjectFingerprint,
    Publication, ReserveError, RunnerSnapshot,
};
use crate::runtime_stats::RuntimeStatusPublisher;

/// Number of times one request retries after the project changed while its index was built.
const MAX_INDEX_BUILD_ATTEMPTS: usize = 4;
/// Runner entries retained per allowed project index; runners carry the last focused-test
/// state, so they outlive their indexes.
const RUNNERS_PER_INDEX: usize = 4;
#[derive(Debug, Serialize, JsonSchema)]
pub struct Results<T> {
    pub results: Vec<T>,
}

impl<T> From<Vec<T>> for Results<T> {
    fn from(results: Vec<T>) -> Self {
        Self { results }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProjectPathRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
}

/// Outcome of a completed refresh. Failures are MCP errors, never this type.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RefreshResult {
    #[schemars(description = "Always refreshed: the model was re-read and the index rebuilt")]
    status: RefreshStatus,
    #[schemars(
        description = "The cached model and index were dropped and running builds abandoned"
    )]
    invalidated: bool,
    #[schemars(description = "The POMs, wrapper and Maven configuration were read again")]
    model_reloaded: bool,
    #[schemars(
        description = "A new index was built from a Maven classpath resolved after the refresh"
    )]
    index_rebuilt: bool,
    #[schemars(description = "Statistics of the rebuilt index")]
    index: IndexStats,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefreshStatus {
    Refreshed,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Case-insensitive substring to search for")]
    query: String,
    #[schemars(description = "Maximum result count; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EntrySearchRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Case-insensitive substring matched against JAR entry paths")]
    query: String,
    #[schemars(
        description = "Optional exact coordinate, repository-relative path, or JAR filename"
    )]
    jar: Option<String>,
    #[schemars(description = "Maximum result count; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClassListRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Exact coordinate, repository-relative path, or JAR filename")]
    jar: String,
    #[schemars(description = "Zero-based result offset")]
    offset: Option<usize>,
    #[schemars(description = "Maximum class count per matching JAR; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SourceRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Fully-qualified class name; inner classes may use $")]
    class_name: String,
    #[schemars(
        description = "Optional exact coordinate, repository-relative path, or JAR filename"
    )]
    jar: Option<String>,
    #[schemars(description = "Optional Maven artifact version")]
    version: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JarEntryRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Exact coordinate, repository-relative path, or JAR filename")]
    jar: String,
    #[schemars(description = "Exact case-sensitive path of the entry inside the selected JAR")]
    entry: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PomDescriptorRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Exact Maven coordinate in groupId:artifactId:version form")]
    coordinate: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArtifactHealthRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Exact Maven coordinate in groupId:artifactId:version form")]
    coordinate: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClassMemberSearchRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(
        description = "Case-insensitive substring matched against member or annotation names"
    )]
    query: String,
    #[schemars(
        description = "Optional exact coordinate, repository-relative path, or JAR filename"
    )]
    jar: Option<String>,
    #[schemars(description = "Maximum result count; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArtifactApiDiffRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Exact Maven groupId")]
    group_id: String,
    #[schemars(description = "Exact Maven artifactId")]
    artifact_id: String,
    #[schemars(description = "Baseline version present in the local Maven repository")]
    previous_version: String,
    #[schemars(description = "Comparison version present in the local Maven repository")]
    current_version: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct JarContentSearchRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Case-insensitive substring matched inside supported text resources")]
    query: String,
    #[schemars(
        description = "Optional exact coordinate, repository-relative path, or JAR filename"
    )]
    jar: Option<String>,
    #[schemars(description = "Maximum result count; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeHierarchyRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Fully-qualified interface or base class name")]
    type_name: String,
    #[schemars(description = "Include indirect implementations and subclasses")]
    #[serde(default)]
    transitive: bool,
    #[schemars(
        description = "Optional exact coordinate, repository-relative path, or JAR filename"
    )]
    jar: Option<String>,
    #[schemars(description = "Maximum result count; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SourceSearchRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Substring or regular expression matched in Java/Kotlin sources")]
    query: String,
    #[schemars(description = "Interpret query as a Rust regular expression")]
    #[serde(default)]
    regex: bool,
    #[schemars(
        description = "Optional exact coordinate, repository-relative path, or JAR filename"
    )]
    jar: Option<String>,
    #[schemars(description = "Context lines before and after each match; capped at 10")]
    #[serde(default)]
    context_lines: usize,
    #[schemars(description = "Maximum result count; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeclarationSourceRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Fully-qualified class name; inner classes may use $")]
    class_name: String,
    #[schemars(
        description = "Optional exact field or method name; omit for the class declaration"
    )]
    member_name: Option<String>,
    #[schemars(description = "Optional exact JVM field or method descriptor")]
    descriptor: Option<String>,
    #[schemars(
        description = "Optional exact coordinate, repository-relative path, or JAR filename"
    )]
    jar: Option<String>,
    #[schemars(description = "Optional Maven artifact version")]
    version: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClassReferenceRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Fully-qualified class name used as source or target owner")]
    class_name: String,
    #[schemars(description = "Inbound or outbound reference direction")]
    direction: ReferenceDirection,
    #[schemars(description = "Optional class, field, method, or interface_method filter")]
    kind: Option<ClassReferenceKind>,
    #[schemars(description = "Optional exact referenced member name")]
    member_name: Option<String>,
    #[schemars(description = "Optional exact JVM member descriptor")]
    descriptor: Option<String>,
    #[schemars(
        description = "Optional exact coordinate, repository-relative path, or JAR filename"
    )]
    jar: Option<String>,
    #[schemars(description = "Maximum result count; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProviderSearchRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Optional case-insensitive service or extension-point filter")]
    service: Option<String>,
    #[schemars(description = "Optional case-insensitive provider implementation filter")]
    provider: Option<String>,
    #[schemars(description = "Optional descriptor family")]
    descriptor_kind: Option<ProviderDescriptorKind>,
    #[schemars(
        description = "Optional exact coordinate, repository-relative path, or JAR filename"
    )]
    jar: Option<String>,
    #[schemars(description = "Maximum result count; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MavenArgumentsRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(
        description = "Exact ordered Maven argument list, including any lifecycle phases, plugin goals and options. Each item is one argument; no shell parsing or automatic flags. Use --offline explicitly if needed."
    )]
    arguments: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MavenLifecycleRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Allowed lifecycle phase: compile, test_compile, or verify")]
    phase: LifecyclePhase,
    #[schemars(description = "Optional exact selector from inspect_maven_project.modules")]
    module: Option<String>,
    #[schemars(description = "Build required reactor dependencies; valid only with module")]
    #[serde(default)]
    also_make: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FocusedTestRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Fully-qualified test class name")]
    test_class: String,
    #[schemars(description = "Optional exact test method name")]
    test_method: Option<String>,
    #[schemars(description = "Optional exact selector from inspect_maven_project.modules")]
    module: Option<String>,
    #[schemars(description = "Build required reactor dependencies; valid only with module")]
    #[serde(default)]
    also_make: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProjectModuleRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Optional exact selector from inspect_maven_project.modules")]
    module: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DependencyTreeRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Optional exact selector from inspect_maven_project.modules")]
    module: Option<String>,
    #[schemars(description = "Optional Maven dependency scope")]
    scope: Option<DependencyScope>,
    #[schemars(
        description = "Optional Maven coordinate pattern using alphanumeric, . _ - : and *"
    )]
    coordinate_filter: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DependencyResolutionRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Optional exact selector from inspect_maven_project.modules")]
    module: Option<String>,
    #[schemars(description = "Optional Maven dependency scope")]
    scope: Option<DependencyScope>,
    #[schemars(
        description = "Optional Maven coordinate pattern using alphanumeric, . _ - : and *"
    )]
    coordinate_filter: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MavenClasspathRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Optional exact selector from inspect_maven_project.modules")]
    module: Option<String>,
    #[schemars(description = "Build or test classpath")]
    kind: ClasspathKind,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CoverageGapRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Maximum result count; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClassVisibility {
    Public,
    All,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClassDescriptionRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Fully-qualified class name; inner classes may use $")]
    class_name: String,
    #[schemars(
        description = "Optional exact coordinate, repository-relative path, or JAR filename"
    )]
    jar: Option<String>,
    #[schemars(description = "Optional Maven artifact version")]
    version: Option<String>,
    #[schemars(description = "Member visibility: public (default) or all")]
    visibility: Option<ClassVisibility>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct VersionsRequest {
    #[schemars(description = "Absolute path to the trusted Maven project root containing pom.xml")]
    project_path: String,
    #[schemars(description = "Maven artifactId")]
    artifact_id: String,
    #[schemars(description = "Optional groupId used to disambiguate artifacts")]
    group_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MavenMcpServer {
    config: Config,
    execution_permit: Arc<Semaphore>,
    heavy_work: HeavyWork,
    projects: Arc<Mutex<ProjectCache>>,
    runtime_status: Arc<RuntimeStatusPublisher>,
    tool_router: ToolRouter<Self>,
}

fn heavy_work_error(error: HeavyWorkError) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}

fn artifact_lookup_error(error: anyhow::Error) -> ErrorData {
    if error.is::<ArtifactOutsideRepository>() {
        ErrorData::invalid_params(error.to_string(), None)
    } else {
        ErrorData::internal_error(error.to_string(), None)
    }
}

impl MavenMcpServer {
    pub fn new(config: Config) -> Self {
        let runtime_status = RuntimeStatusPublisher::register(&config)
            .expect("runtime status registration must be writable");
        let projects = ProjectCache::new(CacheLimits {
            max_indexes: config.max_project_indexes,
            max_runners: config.max_project_indexes.saturating_mul(RUNNERS_PER_INDEX),
            max_bytes: u64::try_from(config.max_project_index_bytes).unwrap_or(u64::MAX),
        });
        let heavy_work = HeavyWork::new(HeavyWorkLimits {
            running: config.max_heavy_jobs,
            queued: config.max_queued_heavy_jobs,
        });
        Self {
            config,
            execution_permit: Arc::new(Semaphore::new(1)),
            heavy_work,
            projects: Arc::new(Mutex::new(projects)),
            runtime_status: Arc::new(runtime_status),
            tool_router: Self::tool_router(),
        }
    }

    /// Cancels all heavy jobs and rejects new ones; called when the server stops.
    pub fn shutdown(&self) {
        self.heavy_work.shutdown();
    }

    /// Runs blocking index, archive or file work outside the async runtime within the bounded
    /// heavy-job pool.
    async fn blocking<T, F>(&self, work: F) -> Result<T, ErrorData>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.heavy_work.run(work).await.map_err(heavy_work_error)
    }

    /// Resolves the project's index and evaluates `work` on it as a heavy job.
    async fn query<T, F>(&self, project_path: String, work: F) -> Result<T, ErrorData>
    where
        F: FnOnce(&MavenIndex) -> T + Send + 'static,
        T: Send + 'static,
    {
        let index = self.index_for(&project_path).await?;
        self.blocking(move || work(&index)).await
    }

    fn validate_project_path(&self, project_path: &str) -> Result<PathBuf, ErrorData> {
        canonical_project_root(&PathBuf::from(project_path))
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))
    }

    fn lock_projects(&self) -> Result<MutexGuard<'_, ProjectCache>, ErrorData> {
        self.projects
            .lock()
            .map_err(|_| ErrorData::internal_error("project cache is poisoned", None))
    }

    /// Canonicalizes `project_path` and requires it inside a host-trusted directory tree.
    fn trusted_project_root(&self, project_path: &str) -> Result<PathBuf, ErrorData> {
        let root = self.validate_project_path(project_path)?;
        if !self
            .config
            .execution
            .trusted_project_directories
            .iter()
            .any(|directory| root.starts_with(directory))
        {
            return Err(ErrorData::invalid_params(
                "project_path is not inside a directory configured by MAVEN_TRUSTED_PROJECT_DIRECTORIES",
                None,
            ));
        }
        Ok(root)
    }

    fn runner_for(&self, project_path: &str) -> Result<Arc<MavenRunner>, ErrorData> {
        self.project_runner(project_path)
            .map(|(_, snapshot)| snapshot.runner)
    }

    /// Returns the project's runner, re-reading the reactor model when a POM, the wrapper or
    /// the Maven configuration changed or Maven ran in the project since it was discovered.
    fn project_runner(&self, project_path: &str) -> Result<(PathBuf, RunnerSnapshot), ErrorData> {
        let root = self.trusted_project_root(project_path)?;
        let cached = self.lock_projects()?.runner(&root);
        if let Some(snapshot) = cached.as_ref()
            && !snapshot.revalidate
            && snapshot.fingerprint.is_current()
        {
            return Ok((root, snapshot.clone()));
        }
        let discovered = match &cached {
            Some(snapshot) => snapshot
                .runner
                .rediscover(&self.config.execution, Arc::clone(&self.execution_permit)),
            None => MavenRunner::discover_canonical_with_permit(
                root.clone(),
                &self.config.execution,
                Arc::clone(&self.execution_permit),
            ),
        };
        let runner = match discovered {
            Ok(runner) => Arc::new(runner),
            Err(error) => {
                self.lock_projects()?.discard_project(&root);
                self.publish_runtime_status();
                return Err(ErrorData::invalid_params(error.to_string(), None));
            }
        };
        let fingerprint = ProjectFingerprint::capture(runner.fingerprint_paths());
        let snapshot = self
            .lock_projects()?
            .install_runner(&root, runner, fingerprint);
        self.publish_runtime_status();
        Ok((root, snapshot))
    }

    async fn index_for(&self, project_path: &str) -> Result<Arc<MavenIndex>, ErrorData> {
        for _ in 0..MAX_INDEX_BUILD_ATTEMPTS {
            let (root, snapshot) = self.project_runner(project_path)?;
            let lookup = self
                .lock_projects()?
                .lookup_index(&root, &snapshot.fingerprint);
            match lookup {
                IndexLookup::Cached { entry, generation } => {
                    // Validation stats every stamped artifact and class-directory file, so it
                    // is heavy work too.
                    let checked = Arc::clone(&entry);
                    if self
                        .blocking(move || checked.artifacts_are_current())
                        .await?
                    {
                        self.lock_projects()?.record_hit(&root);
                        self.publish_runtime_status();
                        return Ok(Arc::clone(&entry.index));
                    }
                    self.lock_projects()?
                        .invalidate_generation(&root, generation);
                }
                IndexLookup::Build {
                    cell,
                    owner,
                    generation,
                } => {
                    let outcome = cell
                        .get_or_init(|| self.build_index(&root, &snapshot, generation, owner))
                        .await
                        .clone();
                    self.publish_runtime_status();
                    match outcome {
                        BuildOutcome::Built(index) => return Ok(index),
                        BuildOutcome::Failed(error) => return Err(error),
                        BuildOutcome::Stale => {}
                    }
                }
            }
        }
        Err(ErrorData::internal_error(
            "the project changed repeatedly while its index was being built; retry the request",
            None,
        ))
    }

    /// Runs the single build of one project generation; failures are shared with every waiter
    /// and leave the project ready for a retry.
    async fn build_index(
        &self,
        root: &Path,
        snapshot: &RunnerSnapshot,
        generation: u64,
        owner: BuildOwner,
    ) -> BuildOutcome {
        // Blocking work of a cancelled build of this generation may still run: wait for it.
        let ownership = owner.lock_owned().await;
        let outcome = self
            .try_build_index(root, snapshot, generation, ownership)
            .await;
        if matches!(outcome, BuildOutcome::Failed(_))
            && let Ok(mut projects) = self.lock_projects()
        {
            projects.finish_failed(root, generation);
        }
        outcome
    }

    /// `ownership` moves into every blocking job of the build, so it is released only when the
    /// build's blocking work has stopped, even after the request was cancelled.
    async fn try_build_index(
        &self,
        root: &Path,
        snapshot: &RunnerSnapshot,
        generation: u64,
        ownership: OwnedMutexGuard<()>,
    ) -> BuildOutcome {
        let classpath = snapshot
            .runner
            .build_classpath_paths(None, ClasspathKind::Test)
            .await;
        if classpath.build.outcome != MavenBuildOutcome::Success {
            return BuildOutcome::Failed(ErrorData::invalid_params(
                format!(
                    "cannot build Maven classpath for project index: {}",
                    maven_failure_detail(&classpath.build)
                ),
                None,
            ));
        }
        if classpath.paths_incomplete {
            return BuildOutcome::Failed(ErrorData::invalid_params(
                "Maven classpath output was truncated or malformed; the project index was not built because it would be incomplete",
                None,
            ));
        }
        let repository_root = repository_root_for(&classpath.paths, root, &self.config);
        if !has_indexable_jars(&classpath.paths, root, repository_root.is_some()) {
            return BuildOutcome::Failed(ErrorData::invalid_params(
                "Maven classpath did not contain indexable local repository JARs",
                None,
            ));
        }
        // Class directories are stamped file by file, so editing a nested class invalidates.
        let stamped_paths = classpath.paths.clone();
        let (artifacts, ownership) = match self
            .blocking(move || {
                let artifacts =
                    ProjectFingerprint::capture_trees(stamped_paths.iter().flat_map(|path| {
                        [path.clone()]
                            .into_iter()
                            .chain(crate::index::source_jar_path(path))
                    }));
                (artifacts, ownership)
            })
            .await
        {
            Ok(captured) => captured,
            Err(error) => return BuildOutcome::Failed(error),
        };
        let reservation = {
            let Ok(mut projects) = self.lock_projects() else {
                return BuildOutcome::Failed(ErrorData::internal_error(
                    "project cache is poisoned",
                    None,
                ));
            };
            // Until the index exists, charge one byte per classpath JAR byte: local measurements
            // found finished indexes at 0.7-0.9 bytes per JAR byte.
            match projects.reserve(root, generation, artifacts.file_bytes()) {
                Ok(reservation) => reservation,
                Err(ReserveError::Stale) => return BuildOutcome::Stale,
                Err(ReserveError::OverBudget { needed, limit }) => {
                    return BuildOutcome::Failed(ErrorData::invalid_params(
                        format!(
                            "building this project index needs an estimated {needed} bytes, which exceeds MAX_PROJECT_INDEX_BYTES ({limit})"
                        ),
                        None,
                    ));
                }
            }
        };
        let modules = snapshot.runner.module_directories();
        let project_root = root.to_owned();
        let fingerprint = Arc::clone(&snapshot.fingerprint);
        let (max_results, max_source_bytes) =
            (self.config.max_results, self.config.max_source_bytes);
        // The reservation moves into the job: an abandoned build keeps counting against the
        // memory budget until its blocking thread has really stopped.
        let built = self
            .blocking(move || {
                let scoped = MavenIndex::build_scoped(
                    repository_root.as_deref(),
                    &ReactorLayout {
                        root: &project_root,
                        modules: &modules,
                    },
                    &classpath.paths,
                    max_results,
                    max_source_bytes,
                );
                // Files that changed while Maven and the indexer ran describe a different project.
                let changed = !fingerprint.is_current() || !artifacts.is_current();
                (scoped, changed, artifacts, reservation, ownership)
            })
            .await;
        let (scoped, changed, artifacts, reservation, _ownership) = match built {
            Ok(built) => built,
            Err(error) => return BuildOutcome::Failed(error),
        };
        let scoped = match scoped {
            Ok(scoped) => scoped,
            Err(error) => {
                return BuildOutcome::Failed(ErrorData::internal_error(error.to_string(), None));
            }
        };
        if !scoped.unindexed.is_empty() {
            return BuildOutcome::Failed(ErrorData::invalid_params(
                unindexed_classpath_message(&scoped.unindexed),
                None,
            ));
        }
        let index = Arc::new(scoped.index);
        let Ok(mut projects) = self.lock_projects() else {
            return BuildOutcome::Failed(ErrorData::internal_error(
                "project cache is poisoned",
                None,
            ));
        };
        if changed {
            projects.invalidate_generation(root, generation);
            return BuildOutcome::Stale;
        }
        match projects.publish(
            root,
            generation,
            &index,
            artifacts,
            Arc::clone(&snapshot.fingerprint),
            reservation,
        ) {
            Publication::Stale => BuildOutcome::Stale,
            Publication::Retained | Publication::Uncached => BuildOutcome::Built(index),
        }
    }

    /// Maven may have changed dependencies, sources, outputs or POMs: drop the project's
    /// index, abandon its running build and re-read the model on the next request.
    fn invalidate_project(&self, project_path: &str) -> Result<(), ErrorData> {
        let root = self.validate_project_path(project_path)?;
        self.lock_projects()?.invalidate_after_maven(&root);
        self.publish_runtime_status();
        Ok(())
    }

    /// Invalidates the trusted project's cached model and index, then rebuilds both. The
    /// rebuild starts Maven (the classpath goal) through the shared execution permit like any
    /// cold request. Only an index that was built after the invalidation counts as refreshed;
    /// every failure says the cache is already invalidated, so it is never reported as success.
    async fn refresh_project(&self, project_path: &str) -> Result<IndexStats, ErrorData> {
        let root = self.trusted_project_root(project_path)?;
        self.lock_projects()?.invalidate_after_maven(&root);
        self.publish_runtime_status();
        let index = self.index_for(project_path).await.map_err(|error| {
            ErrorData::new(
                error.code,
                format!(
                    "the project cache was invalidated but the refresh did not complete: {}",
                    error.message
                ),
                error.data,
            )
        })?;
        Ok(index.stats())
    }

    fn artifact_repository_for(&self, project_path: &str) -> Result<PathBuf, ErrorData> {
        self.runner_for(project_path)?;
        let repository = match &self.config.execution.execution_repository {
            Some(repository) => repository.clone(),
            None => PathBuf::from(env::var_os("HOME").ok_or_else(|| {
                ErrorData::invalid_params("HOME is required to locate the Maven repository", None)
            })?)
            .join(".m2/repository"),
        };
        if repository.exists() {
            repository.canonicalize().map_err(|error| {
                ErrorData::invalid_params(
                    format!("cannot access local Maven repository: {error}"),
                    None,
                )
            })
        } else if self.config.execution.execution_repository.is_some() {
            Err(ErrorData::invalid_params(
                "MAVEN_EXECUTION_REPO_PATH must name an existing directory",
                None,
            ))
        } else {
            Ok(repository)
        }
    }

    fn publish_runtime_status(&self) {
        let Ok(projects) = self.projects.lock() else {
            return;
        };
        let _ = self
            .runtime_status
            .update_cache(projects.runtime_cache(), projects.runtime_projects());
    }
}

const MAX_MAVEN_FAILURE_DETAIL_CHARS: usize = 2_048;

fn maven_failure_detail(build: &MavenBuildResult) -> String {
    let error_lines = [&build.run.stderr, &build.run.stdout]
        .into_iter()
        .flat_map(|output| output.lines())
        .map(str::trim)
        .filter(|line| line.contains("[ERROR]") && *line != "[ERROR]")
        .collect::<Vec<_>>();
    let fallback_lines = [&build.run.stderr, &build.run.stdout]
        .into_iter()
        .flat_map(|output| output.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let detail = if error_lines.is_empty() {
        fallback_lines.collect::<Vec<_>>().join("\n")
    } else {
        error_lines.join("\n")
    };
    if detail.is_empty() {
        return build.run.exit_code.map_or_else(
            || {
                format!(
                    "Maven failed with outcome {:?} without diagnostic output",
                    build.outcome
                )
            },
            |code| format!("Maven exited with status {code} without diagnostic output"),
        );
    }
    let mut bounded = detail
        .chars()
        .take(MAX_MAVEN_FAILURE_DETAIL_CHARS + 1)
        .collect::<String>();
    if bounded.chars().count() > MAX_MAVEN_FAILURE_DETAIL_CHARS {
        bounded = bounded
            .chars()
            .take(MAX_MAVEN_FAILURE_DETAIL_CHARS - 1)
            .collect();
        bounded.push('…');
    }
    bounded
}

fn repository_root_for(paths: &[PathBuf], project_root: &Path, config: &Config) -> Option<PathBuf> {
    if let Some(repository) = config.execution.execution_repository.as_ref()
        && paths.iter().any(|path| path.starts_with(repository))
    {
        return Some(repository.clone());
    }
    paths.iter().find_map(|path| {
        // Directory names above the project root say nothing about the repository layout.
        let (mut root, relative) = match path.strip_prefix(project_root) {
            Ok(relative) => (project_root.to_owned(), relative),
            Err(_) => (PathBuf::new(), path.as_path()),
        };
        for component in relative.components() {
            root.push(component.as_os_str());
            if component.as_os_str() == "repository" {
                return Some(root);
            }
        }
        None
    })
}

/// A classpath is indexable when it names repository artifacts below a recognized repository
/// root, or reactor outputs of the project. Entries that are neither fail the build later.
fn has_indexable_jars(paths: &[PathBuf], project_root: &Path, has_repository: bool) -> bool {
    has_repository || paths.iter().any(|path| path.starts_with(project_root))
}

const MAX_REPORTED_UNINDEXED_ENTRIES: usize = 3;

/// Explains why a classpath could not be indexed completely: counts per reason and the file
/// names, not the full local paths, of the first entries.
fn unindexed_classpath_message(unindexed: &[UnindexedClasspathEntry]) -> String {
    let mut counts = BTreeMap::<UnindexedReason, usize>::new();
    for entry in unindexed {
        *counts.entry(entry.reason).or_default() += 1;
    }
    let reasons = counts
        .iter()
        .map(|(reason, count)| format!("{count} {}", reason.description()))
        .collect::<Vec<_>>()
        .join(", ");
    let names = unindexed
        .iter()
        .take(MAX_REPORTED_UNINDEXED_ENTRIES)
        .map(|entry| {
            entry.path.file_name().map_or_else(
                || "?".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let more = if unindexed.len() > MAX_REPORTED_UNINDEXED_ENTRIES {
        ", …"
    } else {
        ""
    };
    format!(
        "{} selected classpath entries cannot be indexed ({reasons}: {names}{more}); the project index was not built because it would be incomplete",
        unindexed.len()
    )
}

#[tool_router(router = tool_router)]
impl MavenMcpServer {
    #[tool(description = "Return startup index statistics")]
    async fn index_stats(
        &self,
        Parameters(request): Parameters<ProjectPathRequest>,
    ) -> Result<Json<IndexStats>, ErrorData> {
        Ok(Json(
            self.query(request.project_path, |index| index.stats())
                .await?,
        ))
    }

    #[tool(
        description = "Find classes by partial or fully-qualified class name and return every containing JAR"
    )]
    async fn search_classes(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<Json<Results<ClassLocation>>, ErrorData> {
        Ok(Json(
            self.query(request.project_path.clone(), move |index| {
                index.search_classes(&request.query, request.limit)
            })
            .await?
            .into(),
        ))
    }

    #[tool(
        description = "Find Maven JARs by coordinate, artifact name, version, filename, or repository path"
    )]
    async fn search_jars(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<Json<Results<JarSummary>>, ErrorData> {
        Ok(Json(
            self.query(request.project_path.clone(), move |index| {
                index.search_jars(&request.query, request.limit)
            })
            .await?
            .into(),
        ))
    }

    #[tool(description = "Search file and class entry paths inside all JARs or one selected JAR")]
    async fn search_jar_entries(
        &self,
        Parameters(request): Parameters<EntrySearchRequest>,
    ) -> Result<Json<Results<EntryMatch>>, ErrorData> {
        Ok(Json(
            self.query(request.project_path.clone(), move |index| {
                index.search_entries(&request.query, request.jar.as_deref(), request.limit)
            })
            .await?
            .into(),
        ))
    }

    #[tool(description = "List the classes in a selected JAR with pagination")]
    async fn list_jar_classes(
        &self,
        Parameters(request): Parameters<ClassListRequest>,
    ) -> Result<Json<Results<ClassList>>, ErrorData> {
        Ok(Json(
            self.query(request.project_path.clone(), move |index| {
                index.list_classes(&request.jar, request.offset.unwrap_or(0), request.limit)
            })
            .await?
            .into(),
        ))
    }

    #[tool(description = "Return Java or Kotlin source for a class from the matching -sources.jar")]
    async fn get_class_source(
        &self,
        Parameters(request): Parameters<SourceRequest>,
    ) -> Result<Json<Results<SourceResult>>, ErrorData> {
        self.query(request.project_path.clone(), move |index| {
            index.class_source(
                &request.class_name,
                request.jar.as_deref(),
                request.version.as_deref(),
            )
        })
        .await?
        .map(Results::from)
        .map(Json)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Read one exact entry from a selected JAR without extracting it; text and binary content are size-limited"
    )]
    async fn get_jar_entry(
        &self,
        Parameters(request): Parameters<JarEntryRequest>,
    ) -> Result<Json<Results<JarEntryContent>>, ErrorData> {
        if request.jar.trim().is_empty() || request.entry.trim().is_empty() {
            return Err(ErrorData::invalid_params(
                "jar and entry must not be empty".to_owned(),
                None,
            ));
        }
        self.query(request.project_path.clone(), move |index| {
            index.jar_entry(&request.jar, &request.entry)
        })
        .await?
        .map(Results::from)
        .map(Json)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Read a local artifact POM as a structured descriptor with declared dependencies, dependency management, BOM imports, and properties"
    )]
    async fn get_artifact_pom(
        &self,
        Parameters(request): Parameters<PomDescriptorRequest>,
    ) -> Result<Json<PomDescriptorLookup>, ErrorData> {
        validate_exact_coordinate(&request.coordinate)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        let repository = self.artifact_repository_for(&request.project_path)?;
        self.blocking(move || MavenIndex::pom_descriptor_at(&repository, &request.coordinate))
            .await?
            .map(Json)
            .map_err(artifact_lookup_error)
    }

    #[tool(
        description = "Describe classfile API metadata without requiring a sources JAR, including hierarchy, members, generic signatures, and annotations"
    )]
    async fn describe_class(
        &self,
        Parameters(request): Parameters<ClassDescriptionRequest>,
    ) -> Result<Json<Results<ClassDescription>>, ErrorData> {
        self.query(request.project_path.clone(), move |index| {
            index.describe_class(
                &request.class_name,
                request.jar.as_deref(),
                request.version.as_deref(),
                !matches!(request.visibility, Some(ClassVisibility::All)),
            )
        })
        .await?
        .map(Results::from)
        .map(Json)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Diagnose the read-only local state of one Maven artifact without exposing absolute paths or credentials"
    )]
    async fn diagnose_artifact(
        &self,
        Parameters(request): Parameters<ArtifactHealthRequest>,
    ) -> Result<Json<ArtifactHealth>, ErrorData> {
        validate_exact_coordinate(&request.coordinate)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        let repository = self.artifact_repository_for(&request.project_path)?;
        self.blocking(move || MavenIndex::artifact_health_at(&repository, &request.coordinate))
            .await?
            .map(Json)
            .map_err(artifact_lookup_error)
    }

    #[tool(
        description = "Search method names, field names, and annotation types across indexed classfiles"
    )]
    async fn search_class_members(
        &self,
        Parameters(request): Parameters<ClassMemberSearchRequest>,
    ) -> Result<Json<Results<ClassMemberMatch>>, ErrorData> {
        Ok(Json(
            self.query(request.project_path.clone(), move |index| {
                index.search_class_members(&request.query, request.jar.as_deref(), request.limit)
            })
            .await?
            .into(),
        ))
    }

    #[tool(
        description = "Compare the public and protected class API of two exact local versions of one Maven artifact read from the local repository, independent of the project classpath"
    )]
    async fn compare_artifact_api(
        &self,
        Parameters(request): Parameters<ArtifactApiDiffRequest>,
    ) -> Result<Json<ArtifactApiDiff>, ErrorData> {
        for coordinate in [
            format!(
                "{}:{}:{}",
                request.group_id, request.artifact_id, request.previous_version
            ),
            format!(
                "{}:{}:{}",
                request.group_id, request.artifact_id, request.current_version
            ),
        ] {
            validate_exact_coordinate(&coordinate)
                .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        }
        let repository = self.artifact_repository_for(&request.project_path)?;
        let max_source_bytes = self.config.max_source_bytes;
        self.blocking(move || {
            MavenIndex::compare_artifact_api_at(
                &repository,
                max_source_bytes,
                &request.group_id,
                &request.artifact_id,
                &request.previous_version,
                &request.current_version,
            )
        })
        .await?
        .map(Json)
        .map_err(|error| ErrorData::invalid_params(error.to_string(), None))
    }

    #[tool(
        description = "Search supported UTF-8 text resources inside indexed JARs with entry, total-byte, context, and result limits"
    )]
    async fn search_jar_content(
        &self,
        Parameters(request): Parameters<JarContentSearchRequest>,
    ) -> Result<Json<JarContentSearch>, ErrorData> {
        self.query(request.project_path.clone(), move |index| {
            index.search_jar_content(&request.query, request.jar.as_deref(), request.limit)
        })
        .await?
        .map(Json)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Find direct or transitive implementations and subclasses of a class or interface across indexed artifacts"
    )]
    async fn search_type_hierarchy(
        &self,
        Parameters(request): Parameters<TypeHierarchyRequest>,
    ) -> Result<Json<Results<TypeHierarchyMatch>>, ErrorData> {
        Ok(Json(
            self.query(request.project_path.clone(), move |index| {
                index.search_type_hierarchy(
                    &request.type_name,
                    request.transitive,
                    request.jar.as_deref(),
                    request.limit,
                )
            })
            .await?
            .into(),
        ))
    }

    #[tool(
        description = "Search Java and Kotlin sources in local sources artifacts with bounded line context"
    )]
    async fn search_source(
        &self,
        Parameters(request): Parameters<SourceSearchRequest>,
    ) -> Result<Json<SourceSearch>, ErrorData> {
        if request.query.trim().is_empty() {
            return Err(ErrorData::invalid_params(
                "query must not be empty".to_owned(),
                None,
            ));
        }
        if request.regex {
            regex::Regex::new(&request.query)
                .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        }
        self.query(request.project_path.clone(), move |index| {
            index.search_source(
                &request.query,
                request.regex,
                request.jar.as_deref(),
                request.context_lines,
                request.limit,
            )
        })
        .await?
        .map(Json)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Return the bounded source slice for one Java or Kotlin class, field, or method declaration; a declaration whose exact source cannot be proven is listed in unresolved"
    )]
    async fn get_declaration_source(
        &self,
        Parameters(request): Parameters<DeclarationSourceRequest>,
    ) -> Result<Json<DeclarationSourceLookup>, ErrorData> {
        if request.descriptor.is_some() && request.member_name.is_none() {
            return Err(ErrorData::invalid_params(
                "descriptor requires member_name".to_owned(),
                None,
            ));
        }
        self.query(request.project_path.clone(), move |index| {
            index.get_declaration_source(
                &request.class_name,
                request.member_name.as_deref(),
                request.descriptor.as_deref(),
                request.jar.as_deref(),
                request.version.as_deref(),
            )
        })
        .await?
        .map(Json)
        .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Search inbound or outbound classfile references to classes, fields, and methods"
    )]
    async fn search_class_references(
        &self,
        Parameters(request): Parameters<ClassReferenceRequest>,
    ) -> Result<Json<Results<ClassReference>>, ErrorData> {
        Ok(Json(
            self.query(request.project_path.clone(), move |index| {
                index.search_class_references(
                    &request.class_name,
                    request.direction,
                    request.kind,
                    request.member_name.as_deref(),
                    request.descriptor.as_deref(),
                    request.jar.as_deref(),
                    request.limit,
                )
            })
            .await?
            .into(),
        ))
    }

    #[tool(
        description = "Find structured Java ServiceLoader, JPMS, and supported Spring provider declarations"
    )]
    async fn search_providers(
        &self,
        Parameters(request): Parameters<ProviderSearchRequest>,
    ) -> Result<Json<Results<ProviderFact>>, ErrorData> {
        Ok(Json(
            self.query(request.project_path.clone(), move |index| {
                index.search_providers(
                    request.service.as_deref(),
                    request.provider.as_deref(),
                    request.descriptor_kind,
                    request.jar.as_deref(),
                    request.limit,
                )
            })
            .await?
            .into(),
        ))
    }

    #[tool(
        description = "Return the validated root POM, packaging, Maven Wrapper, and reactor module model for the opt-in project"
    )]
    fn inspect_maven_project(
        &self,
        Parameters(request): Parameters<ProjectPathRequest>,
    ) -> Result<Json<MavenProject>, ErrorData> {
        Ok(Json(
            self.runner_for(&request.project_path)?.project().clone(),
        ))
    }

    #[tool(
        description = "Explicitly refresh one trusted project: drop its cached model and index, abandon any running build, re-read the POMs, wrapper and Maven configuration, and rebuild the index. Starts Maven for the classpath goal through the shared execution permit and the same offline/network policy as a cold request; the refresh itself never deletes or rewrites the local Maven repository, and other projects' caches are untouched. Returns status refreshed only after the rebuild succeeded. A project_path that fails validation is an error that changes nothing; a failure after the invalidation is an error and the project stays invalidated, so the next request retries."
    )]
    async fn refresh_maven_project(
        &self,
        Parameters(request): Parameters<ProjectPathRequest>,
    ) -> Result<Json<RefreshResult>, ErrorData> {
        let index = self.refresh_project(&request.project_path).await?;
        Ok(Json(RefreshResult {
            status: RefreshStatus::Refreshed,
            invalidated: true,
            model_reloaded: true,
            index_rebuilt: true,
            index,
        }))
    }

    #[tool(
        description = "Run any Maven goals and arguments in a host-trusted project without shell interpretation or injected flags. Maven options control network, settings, profiles, repository and alternate POM paths; execution is not a sandbox. Returns bounded, redacted stdout/stderr and execution status."
    )]
    async fn run_maven(
        &self,
        Parameters(request): Parameters<MavenArgumentsRequest>,
    ) -> Result<Json<MavenRunResult>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        let result = runner
            .run_arguments(&request.arguments)
            .await
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        // Arbitrary goals may change dependencies, sources or classpath.
        self.invalidate_project(&request.project_path)?;
        Ok(Json(result))
    }

    #[tool(
        description = "Run an allowlisted Maven compile, test-compile, or verify phase in the configured project with optional validated reactor selection; use run_maven for arbitrary goals and arguments"
    )]
    async fn run_maven_lifecycle(
        &self,
        Parameters(request): Parameters<MavenLifecycleRequest>,
    ) -> Result<Json<MavenBuildResult>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        let run = runner
            .run(&MavenInvocation {
                phase: request.phase,
                module: request.module,
                also_make: request.also_make,
            })
            .await;
        // Packaging and verification change the reactor outputs on the classpath.
        self.invalidate_project(&request.project_path)?;
        Ok(Json(MavenBuildResult::from_run(run)))
    }

    #[tool(
        description = "List Java and Kotlin test classes under the validated Maven reactor root"
    )]
    async fn list_maven_test_classes(
        &self,
        Parameters(request): Parameters<ProjectPathRequest>,
    ) -> Result<Json<Results<String>>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        self.blocking(move || runner.test_classes())
            .await?
            .map(Results::from)
            .map(Json)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Run one validated Surefire test class or method and return structured counts and bounded failure details"
    )]
    async fn run_maven_test(
        &self,
        Parameters(request): Parameters<FocusedTestRequest>,
    ) -> Result<Json<FocusedTestResult>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        Ok(Json(
            runner
                .run_focused_test(&FocusedTestInvocation {
                    test_class: request.test_class,
                    test_method: request.test_method,
                    module: request.module,
                    also_make: request.also_make,
                })
                .await,
        ))
    }

    #[tool(description = "Return failures from the last focused Maven test run in this process")]
    async fn get_last_maven_test_failures(
        &self,
        Parameters(request): Parameters<ProjectPathRequest>,
    ) -> Result<Json<LastTestFailures>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        Ok(Json(runner.last_test_failures().await))
    }

    #[tool(
        description = "Resolve an effective Maven POM into structured project, parent, property, dependency, and plugin metadata"
    )]
    async fn get_effective_pom(
        &self,
        Parameters(request): Parameters<ProjectModuleRequest>,
    ) -> Result<Json<EffectivePomResult>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        Ok(Json(runner.effective_pom(request.module.as_deref()).await))
    }

    #[tool(
        description = "Return a normalized Maven dependency tree with optional validated module, scope, and coordinate filters"
    )]
    async fn get_dependency_tree(
        &self,
        Parameters(request): Parameters<DependencyTreeRequest>,
    ) -> Result<Json<DependencyTreeResult>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        Ok(Json(
            runner
                .dependency_tree(
                    request.module.as_deref(),
                    request.scope,
                    request.coordinate_filter.as_deref(),
                )
                .await,
        ))
    }

    #[tool(
        description = "Explain selected and omitted Maven dependency versions with module-aware paths and mediation reasons"
    )]
    async fn explain_dependency_resolution(
        &self,
        Parameters(request): Parameters<DependencyResolutionRequest>,
    ) -> Result<Json<DependencyResolutionResult>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        let result = runner
            .explain_dependency_resolution(
                request.module.as_deref(),
                request.scope,
                request.coordinate_filter.as_deref(),
            )
            .await;
        if matches!(&result.status, DependencyResolutionStatus::Invalid) {
            return Err(ErrorData::invalid_params(
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "invalid dependency resolution request".to_owned()),
                None,
            ));
        }
        Ok(Json(result))
    }

    #[tool(
        description = "Return a build or test classpath normalized to Maven coordinates without local absolute paths"
    )]
    async fn get_maven_classpath(
        &self,
        Parameters(request): Parameters<MavenClasspathRequest>,
    ) -> Result<Json<MavenClasspathResult>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        Ok(Json(
            runner
                .build_classpath(request.module.as_deref(), request.kind)
                .await,
        ))
    }

    #[tool(
        description = "Read existing JaCoCo XML reports without running Maven and return per-module counters with missing and stale state"
    )]
    async fn get_jacoco_coverage(
        &self,
        Parameters(request): Parameters<ProjectPathRequest>,
    ) -> Result<Json<CoverageSummaryResult>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        Ok(Json(self.blocking(move || runner.jacoco_coverage()).await?))
    }

    #[tool(
        description = "Rank low-coverage classes from existing JaCoCo XML reports without running Maven"
    )]
    async fn get_jacoco_coverage_gaps(
        &self,
        Parameters(request): Parameters<CoverageGapRequest>,
    ) -> Result<Json<CoverageGapResult>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        Ok(Json(
            self.blocking(move || runner.jacoco_coverage_gaps(request.limit))
                .await?,
        ))
    }

    #[tool(description = "List all locally available versions of a Maven artifact")]
    async fn list_artifact_versions(
        &self,
        Parameters(request): Parameters<VersionsRequest>,
    ) -> Result<Json<BTreeMap<String, Vec<String>>>, ErrorData> {
        Ok(Json(
            self.query(request.project_path.clone(), move |index| {
                index.artifact_versions(&request.artifact_id, request.group_id.as_deref())
            })
            .await?,
        ))
    }
}

const SERVER_NAME: &str = "maven-mcp";
const SERVER_INSTRUCTIONS: &str = "Search and inspect classes, JAR resources, POM metadata, local artifact health, sources, and versioned APIs from a read-only local Maven repository.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MavenMcpServer {
    /// `serverInfo.version` carries the embedded build commit, so a host can identify the
    /// exact installed binary it started.
    fn get_info(&self) -> rmcp::model::ServerConfig {
        rmcp::model::ServerConfig::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new(
            SERVER_NAME,
            build_info::server_version(),
        ))
        .with_instructions(SERVER_INSTRUCTIONS.to_owned())
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, ErrorData> {
        let cancellation = context.ct.clone();
        let call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                Err(ErrorData::internal_error("tool request cancelled", None))
            }
            result = self.tool_router.call(call) => result,
        }
    }
}
