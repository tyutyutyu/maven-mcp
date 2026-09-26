use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    env,
    path::PathBuf,
    sync::{Arc, Mutex},
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
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::index::{
    ArtifactApiDiff, ArtifactHealth, ClassDescription, ClassList, ClassLocation, ClassMemberMatch,
    ClassReference, ClassReferenceKind, DeclarationSourceLookup, EntryMatch, IndexStats,
    JarContentSearch, JarEntryContent, JarSummary, MavenIndex, PomDescriptorLookup,
    ProviderDescriptorKind, ProviderFact, ReferenceDirection, SourceResult, SourceSearch,
    TypeHierarchyMatch, validate_exact_coordinate,
};
use crate::project::{
    ClasspathKind, CoverageGapResult, CoverageSummaryResult, DependencyResolutionResult,
    DependencyResolutionStatus, DependencyScope, DependencyTreeResult, EffectivePomResult,
    FocusedTestInvocation, FocusedTestResult, LastTestFailures, LifecyclePhase, MavenBuildOutcome,
    MavenBuildResult, MavenClasspathResult, MavenInvocation, MavenProject, MavenRunner,
    canonical_project_root,
};
use crate::runtime_stats::{
    ProjectIndexStatus, RuntimeCacheStatus, RuntimeStatusPublisher, unix_ms,
};

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
    #[schemars(description = "Locally available baseline version")]
    previous_version: String,
    #[schemars(description = "Locally available comparison version")]
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
    runners: Arc<Mutex<HashMap<PathBuf, Arc<MavenRunner>>>>,
    execution_permit: Arc<Semaphore>,
    indexes: Arc<Mutex<ProjectIndexCache>>,
    runtime_status: Arc<RuntimeStatusPublisher>,
    tool_router: ToolRouter<Self>,
}

impl MavenMcpServer {
    pub fn new(config: Config) -> Self {
        let runtime_status = RuntimeStatusPublisher::register(&config)
            .expect("runtime status registration must be writable");
        Self {
            config,
            runners: Arc::new(Mutex::new(HashMap::new())),
            execution_permit: Arc::new(Semaphore::new(1)),
            indexes: Arc::new(Mutex::new(ProjectIndexCache::default())),
            runtime_status: Arc::new(runtime_status),
            tool_router: Self::tool_router(),
        }
    }

    fn validate_project_path(&self, project_path: &str) -> Result<PathBuf, ErrorData> {
        canonical_project_root(&PathBuf::from(project_path))
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))
    }

    fn runner_for(&self, project_path: &str) -> Result<Arc<MavenRunner>, ErrorData> {
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
        let mut runners = self
            .runners
            .lock()
            .map_err(|_| ErrorData::internal_error("project runner cache is poisoned", None))?;
        if let Some(runner) = runners.get(&root) {
            return Ok(Arc::clone(runner));
        }
        let runner = Arc::new(
            MavenRunner::discover_canonical_with_permit(
                root.clone(),
                &self.config.execution,
                Arc::clone(&self.execution_permit),
            )
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?,
        );
        runners.insert(root, Arc::clone(&runner));
        Ok(runner)
    }

    async fn index_for(&self, project_path: &str) -> Result<Arc<MavenIndex>, ErrorData> {
        let root = self.validate_project_path(project_path)?;
        if let Some(index) = self.cached_index(&root)? {
            return Ok(index);
        }

        let runner = self.runner_for(project_path)?;
        let classpath = runner
            .build_classpath_paths(None, ClasspathKind::Test)
            .await;
        if classpath.build.outcome != MavenBuildOutcome::Success {
            return Err(ErrorData::invalid_params(
                format!(
                    "cannot build Maven classpath for project index: {}",
                    maven_failure_detail(&classpath.build)
                ),
                None,
            ));
        }
        let Some(repository_root) = repository_root_for(&classpath.paths, &self.config) else {
            return Err(ErrorData::invalid_params(
                "Maven classpath did not contain indexable local repository JARs",
                None,
            ));
        };
        let index = Arc::new(
            MavenIndex::build_scoped(
                &repository_root,
                &classpath.paths,
                self.config.max_results,
                self.config.max_source_bytes,
            )
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?,
        );
        self.store_index(root, Arc::clone(&index))?;
        Ok(index)
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

    fn cached_index(&self, root: &PathBuf) -> Result<Option<Arc<MavenIndex>>, ErrorData> {
        let result = self
            .indexes
            .lock()
            .map_err(|_| ErrorData::internal_error("project index cache is poisoned", None))
            .map(|mut cache| cache.get(root));
        self.publish_runtime_status();
        result
    }

    fn store_index(&self, root: PathBuf, index: Arc<MavenIndex>) -> Result<(), ErrorData> {
        self.indexes
            .lock()
            .map_err(|_| ErrorData::internal_error("project index cache is poisoned", None))?
            .insert(root, index, self.config.max_project_indexes);
        self.publish_runtime_status();
        Ok(())
    }

    fn publish_runtime_status(&self) {
        let Ok(cache) = self.indexes.lock() else {
            return;
        };
        let _ = self
            .runtime_status
            .update_cache(cache.runtime_cache(), cache.runtime_projects());
    }
}

#[derive(Debug, Default)]
struct ProjectIndexCache {
    indexes: HashMap<PathBuf, Arc<MavenIndex>>,
    order: VecDeque<PathBuf>,
    hits: u64,
    misses: u64,
    evictions: u64,
    last_used: HashMap<PathBuf, u128>,
}

impl ProjectIndexCache {
    fn get(&mut self, root: &PathBuf) -> Option<Arc<MavenIndex>> {
        let Some(index) = self.indexes.get(root).cloned() else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;
        self.touch(root);
        Some(index)
    }

    fn insert(&mut self, root: PathBuf, index: Arc<MavenIndex>, limit: usize) {
        self.indexes.insert(root.clone(), index);
        self.touch(&root);
        while self.indexes.len() > limit {
            let Some(evicted) = self.order.pop_front() else {
                break;
            };
            if evicted != root && self.indexes.remove(&evicted).is_some() {
                self.evictions += 1;
                self.last_used.remove(&evicted);
            }
        }
    }

    fn touch(&mut self, root: &PathBuf) {
        self.order.retain(|candidate| candidate != root);
        self.order.push_back(root.clone());
        self.last_used.insert(root.clone(), unix_ms());
    }

    fn runtime_cache(&self) -> RuntimeCacheStatus {
        RuntimeCacheStatus {
            entries: self.indexes.len(),
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
        }
    }

    fn runtime_projects(&self) -> Vec<ProjectIndexStatus> {
        let mut projects = self
            .indexes
            .iter()
            .map(|(root, index)| ProjectIndexStatus {
                project_path: root.display().to_string(),
                last_used_unix_ms: self.last_used.get(root).copied().unwrap_or_default(),
                index: index.stats(),
            })
            .collect::<Vec<_>>();
        projects.sort_by(|left, right| left.project_path.cmp(&right.project_path));
        projects
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

fn repository_root_for(paths: &[PathBuf], config: &Config) -> Option<PathBuf> {
    if let Some(repository) = config.execution.execution_repository.as_ref()
        && paths.iter().any(|path| path.starts_with(repository))
    {
        return Some(repository.clone());
    }
    paths.iter().find_map(|path| {
        let mut root = PathBuf::new();
        for component in path.components() {
            root.push(component.as_os_str());
            if component.as_os_str() == "repository" {
                return Some(root);
            }
        }
        None
    })
}

#[tool_router(router = tool_router)]
impl MavenMcpServer {
    #[tool(description = "Return startup index statistics")]
    async fn index_stats(
        &self,
        Parameters(request): Parameters<ProjectPathRequest>,
    ) -> Result<Json<IndexStats>, ErrorData> {
        Ok(Json(self.index_for(&request.project_path).await?.stats()))
    }

    #[tool(
        description = "Find classes by partial or fully-qualified class name and return every containing JAR"
    )]
    async fn search_classes(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Result<Json<Results<ClassLocation>>, ErrorData> {
        Ok(Json(
            self.index_for(&request.project_path)
                .await?
                .search_classes(&request.query, request.limit)
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
            self.index_for(&request.project_path)
                .await?
                .search_jars(&request.query, request.limit)
                .into(),
        ))
    }

    #[tool(description = "Search file and class entry paths inside all JARs or one selected JAR")]
    async fn search_jar_entries(
        &self,
        Parameters(request): Parameters<EntrySearchRequest>,
    ) -> Result<Json<Results<EntryMatch>>, ErrorData> {
        Ok(Json(
            self.index_for(&request.project_path)
                .await?
                .search_entries(&request.query, request.jar.as_deref(), request.limit)
                .into(),
        ))
    }

    #[tool(description = "List the classes in a selected JAR with pagination")]
    async fn list_jar_classes(
        &self,
        Parameters(request): Parameters<ClassListRequest>,
    ) -> Result<Json<Results<ClassList>>, ErrorData> {
        Ok(Json(
            self.index_for(&request.project_path)
                .await?
                .list_classes(&request.jar, request.offset.unwrap_or(0), request.limit)
                .into(),
        ))
    }

    #[tool(description = "Return Java or Kotlin source for a class from the matching -sources.jar")]
    async fn get_class_source(
        &self,
        Parameters(request): Parameters<SourceRequest>,
    ) -> Result<Json<Results<SourceResult>>, ErrorData> {
        self.index_for(&request.project_path)
            .await?
            .class_source(
                &request.class_name,
                request.jar.as_deref(),
                request.version.as_deref(),
            )
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
        self.index_for(&request.project_path)
            .await?
            .jar_entry(&request.jar, &request.entry)
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
        MavenIndex::pom_descriptor_at(&repository, &request.coordinate)
            .map(Json)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Describe classfile API metadata without requiring a sources JAR, including hierarchy, members, generic signatures, and annotations"
    )]
    async fn describe_class(
        &self,
        Parameters(request): Parameters<ClassDescriptionRequest>,
    ) -> Result<Json<Results<ClassDescription>>, ErrorData> {
        self.index_for(&request.project_path)
            .await?
            .describe_class(
                &request.class_name,
                request.jar.as_deref(),
                request.version.as_deref(),
                !matches!(request.visibility, Some(ClassVisibility::All)),
            )
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
        MavenIndex::artifact_health_at(&repository, &request.coordinate)
            .map(Json)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Search method names, field names, and annotation types across indexed classfiles"
    )]
    async fn search_class_members(
        &self,
        Parameters(request): Parameters<ClassMemberSearchRequest>,
    ) -> Result<Json<Results<ClassMemberMatch>>, ErrorData> {
        Ok(Json(
            self.index_for(&request.project_path)
                .await?
                .search_class_members(&request.query, request.jar.as_deref(), request.limit)
                .into(),
        ))
    }

    #[tool(
        description = "Compare the public and protected class API of two locally available versions of one Maven artifact"
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
        self.index_for(&request.project_path)
            .await?
            .compare_artifact_api(
                &request.group_id,
                &request.artifact_id,
                &request.previous_version,
                &request.current_version,
            )
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
        self.index_for(&request.project_path)
            .await?
            .search_jar_content(&request.query, request.jar.as_deref(), request.limit)
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
            self.index_for(&request.project_path)
                .await?
                .search_type_hierarchy(
                    &request.type_name,
                    request.transitive,
                    request.jar.as_deref(),
                    request.limit,
                )
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
        self.index_for(&request.project_path)
            .await?
            .search_source(
                &request.query,
                request.regex,
                request.jar.as_deref(),
                request.context_lines,
                request.limit,
            )
            .map(Json)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Return the bounded source slice for one Java or Kotlin class, field, or method declaration"
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
        self.index_for(&request.project_path)
            .await?
            .get_declaration_source(
                &request.class_name,
                request.member_name.as_deref(),
                request.descriptor.as_deref(),
                request.jar.as_deref(),
                request.version.as_deref(),
            )
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
            self.index_for(&request.project_path)
                .await?
                .search_class_references(
                    &request.class_name,
                    request.direction,
                    request.kind,
                    request.member_name.as_deref(),
                    request.descriptor.as_deref(),
                    request.jar.as_deref(),
                    request.limit,
                )
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
            self.index_for(&request.project_path)
                .await?
                .search_providers(
                    request.service.as_deref(),
                    request.provider.as_deref(),
                    request.descriptor_kind,
                    request.jar.as_deref(),
                    request.limit,
                )
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
        description = "Run an allowlisted Maven compile, test-compile, or verify phase in the configured project with optional validated reactor selection"
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
        Ok(Json(MavenBuildResult::from_run(run)))
    }

    #[tool(
        description = "List Java and Kotlin test classes under the validated Maven reactor root"
    )]
    fn list_maven_test_classes(
        &self,
        Parameters(request): Parameters<ProjectPathRequest>,
    ) -> Result<Json<Results<String>>, ErrorData> {
        self.runner_for(&request.project_path)?
            .test_classes()
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
    fn get_jacoco_coverage(
        &self,
        Parameters(request): Parameters<ProjectPathRequest>,
    ) -> Result<Json<CoverageSummaryResult>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        Ok(Json(runner.jacoco_coverage()))
    }

    #[tool(
        description = "Rank low-coverage classes from existing JaCoCo XML reports without running Maven"
    )]
    fn get_jacoco_coverage_gaps(
        &self,
        Parameters(request): Parameters<CoverageGapRequest>,
    ) -> Result<Json<CoverageGapResult>, ErrorData> {
        let runner = self.runner_for(&request.project_path)?;
        Ok(Json(runner.jacoco_coverage_gaps(request.limit)))
    }

    #[tool(description = "List all locally available versions of a Maven artifact")]
    async fn list_artifact_versions(
        &self,
        Parameters(request): Parameters<VersionsRequest>,
    ) -> Result<Json<BTreeMap<String, Vec<String>>>, ErrorData> {
        Ok(Json(
            self.index_for(&request.project_path)
                .await?
                .artifact_versions(&request.artifact_id, request.group_id.as_deref()),
        ))
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "maven-mcp",
    version = "0.1.0",
    instructions = "Search and inspect classes, JAR resources, POM metadata, local artifact health, sources, and versioned APIs from a read-only local Maven repository."
)]
impl ServerHandler for MavenMcpServer {}
