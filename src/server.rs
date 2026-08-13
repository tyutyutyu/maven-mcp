use std::{collections::BTreeMap, sync::Arc};

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
    FocusedTestInvocation, FocusedTestResult, LastTestFailures, LifecyclePhase, MavenBuildResult,
    MavenClasspathResult, MavenInvocation, MavenProject, MavenRunner,
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
pub struct SearchRequest {
    #[schemars(description = "Case-insensitive substring to search for")]
    query: String,
    #[schemars(description = "Maximum result count; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EntrySearchRequest {
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
    #[schemars(description = "Exact coordinate, repository-relative path, or JAR filename")]
    jar: String,
    #[schemars(description = "Zero-based result offset")]
    offset: Option<usize>,
    #[schemars(description = "Maximum class count per matching JAR; capped by MAX_RESULTS")]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SourceRequest {
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
    #[schemars(description = "Exact coordinate, repository-relative path, or JAR filename")]
    jar: String,
    #[schemars(description = "Exact case-sensitive path of the entry inside the selected JAR")]
    entry: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PomDescriptorRequest {
    #[schemars(description = "Exact Maven coordinate in groupId:artifactId:version form")]
    coordinate: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ArtifactHealthRequest {
    #[schemars(description = "Exact Maven coordinate in groupId:artifactId:version form")]
    coordinate: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClassMemberSearchRequest {
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
    #[schemars(description = "Optional exact selector from inspect_maven_project.modules")]
    module: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DependencyTreeRequest {
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
    #[schemars(description = "Optional exact selector from inspect_maven_project.modules")]
    module: Option<String>,
    #[schemars(description = "Build or test classpath")]
    kind: ClasspathKind,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CoverageGapRequest {
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
    #[schemars(description = "Maven artifactId")]
    artifact_id: String,
    #[schemars(description = "Optional groupId used to disambiguate artifacts")]
    group_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MavenMcpServer {
    index: Arc<MavenIndex>,
    runner: Option<Arc<MavenRunner>>,
    tool_router: ToolRouter<Self>,
}

impl MavenMcpServer {
    pub fn new(index: Arc<MavenIndex>) -> Self {
        Self::with_runner(index, None)
    }

    pub fn with_runner(index: Arc<MavenIndex>, runner: Option<Arc<MavenRunner>>) -> Self {
        let mut tool_router = Self::tool_router();
        if runner.is_none() {
            tool_router.disable_route("inspect_maven_project");
            tool_router.disable_route("run_maven_lifecycle");
            tool_router.disable_route("list_maven_test_classes");
            tool_router.disable_route("run_maven_test");
            tool_router.disable_route("get_last_maven_test_failures");
            tool_router.disable_route("get_effective_pom");
            tool_router.disable_route("get_dependency_tree");
            tool_router.disable_route("explain_dependency_resolution");
            tool_router.disable_route("get_maven_classpath");
            tool_router.disable_route("get_jacoco_coverage");
            tool_router.disable_route("get_jacoco_coverage_gaps");
        }
        Self {
            index,
            runner,
            tool_router,
        }
    }
}

#[tool_router(router = tool_router)]
impl MavenMcpServer {
    #[tool(description = "Return startup index statistics")]
    fn index_stats(&self) -> Json<IndexStats> {
        Json(self.index.stats())
    }

    #[tool(
        description = "Find classes by partial or fully-qualified class name and return every containing JAR"
    )]
    fn search_classes(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Json<Results<ClassLocation>> {
        Json(
            self.index
                .search_classes(&request.query, request.limit)
                .into(),
        )
    }

    #[tool(
        description = "Find Maven JARs by coordinate, artifact name, version, filename, or repository path"
    )]
    fn search_jars(
        &self,
        Parameters(request): Parameters<SearchRequest>,
    ) -> Json<Results<JarSummary>> {
        Json(self.index.search_jars(&request.query, request.limit).into())
    }

    #[tool(description = "Search file and class entry paths inside all JARs or one selected JAR")]
    fn search_jar_entries(
        &self,
        Parameters(request): Parameters<EntrySearchRequest>,
    ) -> Json<Results<EntryMatch>> {
        Json(
            self.index
                .search_entries(&request.query, request.jar.as_deref(), request.limit)
                .into(),
        )
    }

    #[tool(description = "List the classes in a selected JAR with pagination")]
    fn list_jar_classes(
        &self,
        Parameters(request): Parameters<ClassListRequest>,
    ) -> Json<Results<ClassList>> {
        Json(
            self.index
                .list_classes(&request.jar, request.offset.unwrap_or(0), request.limit)
                .into(),
        )
    }

    #[tool(description = "Return Java or Kotlin source for a class from the matching -sources.jar")]
    fn get_class_source(
        &self,
        Parameters(request): Parameters<SourceRequest>,
    ) -> Result<Json<Results<SourceResult>>, ErrorData> {
        self.index
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
    fn get_jar_entry(
        &self,
        Parameters(request): Parameters<JarEntryRequest>,
    ) -> Result<Json<Results<JarEntryContent>>, ErrorData> {
        if request.jar.trim().is_empty() || request.entry.trim().is_empty() {
            return Err(ErrorData::invalid_params(
                "jar and entry must not be empty".to_owned(),
                None,
            ));
        }
        self.index
            .jar_entry(&request.jar, &request.entry)
            .map(Results::from)
            .map(Json)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Read a local artifact POM as a structured descriptor with declared dependencies, dependency management, BOM imports, and properties"
    )]
    fn get_artifact_pom(
        &self,
        Parameters(request): Parameters<PomDescriptorRequest>,
    ) -> Result<Json<PomDescriptorLookup>, ErrorData> {
        validate_exact_coordinate(&request.coordinate)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.index
            .pom_descriptor(&request.coordinate)
            .map(Json)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Describe classfile API metadata without requiring a sources JAR, including hierarchy, members, generic signatures, and annotations"
    )]
    fn describe_class(
        &self,
        Parameters(request): Parameters<ClassDescriptionRequest>,
    ) -> Result<Json<Results<ClassDescription>>, ErrorData> {
        self.index
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
    fn diagnose_artifact(
        &self,
        Parameters(request): Parameters<ArtifactHealthRequest>,
    ) -> Result<Json<ArtifactHealth>, ErrorData> {
        validate_exact_coordinate(&request.coordinate)
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        self.index
            .artifact_health(&request.coordinate)
            .map(Json)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Search method names, field names, and annotation types across indexed classfiles"
    )]
    fn search_class_members(
        &self,
        Parameters(request): Parameters<ClassMemberSearchRequest>,
    ) -> Json<Results<ClassMemberMatch>> {
        Json(
            self.index
                .search_class_members(&request.query, request.jar.as_deref(), request.limit)
                .into(),
        )
    }

    #[tool(
        description = "Compare the public and protected class API of two locally available versions of one Maven artifact"
    )]
    fn compare_artifact_api(
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
        self.index
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
    fn search_jar_content(
        &self,
        Parameters(request): Parameters<JarContentSearchRequest>,
    ) -> Result<Json<JarContentSearch>, ErrorData> {
        self.index
            .search_jar_content(&request.query, request.jar.as_deref(), request.limit)
            .map(Json)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))
    }

    #[tool(
        description = "Find direct or transitive implementations and subclasses of a class or interface across indexed artifacts"
    )]
    fn search_type_hierarchy(
        &self,
        Parameters(request): Parameters<TypeHierarchyRequest>,
    ) -> Json<Results<TypeHierarchyMatch>> {
        Json(
            self.index
                .search_type_hierarchy(
                    &request.type_name,
                    request.transitive,
                    request.jar.as_deref(),
                    request.limit,
                )
                .into(),
        )
    }

    #[tool(
        description = "Search Java and Kotlin sources in local sources artifacts with bounded line context"
    )]
    fn search_source(
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
        self.index
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
    fn get_declaration_source(
        &self,
        Parameters(request): Parameters<DeclarationSourceRequest>,
    ) -> Result<Json<DeclarationSourceLookup>, ErrorData> {
        if request.descriptor.is_some() && request.member_name.is_none() {
            return Err(ErrorData::invalid_params(
                "descriptor requires member_name".to_owned(),
                None,
            ));
        }
        self.index
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
    fn search_class_references(
        &self,
        Parameters(request): Parameters<ClassReferenceRequest>,
    ) -> Json<Results<ClassReference>> {
        Json(
            self.index
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
        )
    }

    #[tool(
        description = "Find structured Java ServiceLoader, JPMS, and supported Spring provider declarations"
    )]
    fn search_providers(
        &self,
        Parameters(request): Parameters<ProviderSearchRequest>,
    ) -> Json<Results<ProviderFact>> {
        Json(
            self.index
                .search_providers(
                    request.service.as_deref(),
                    request.provider.as_deref(),
                    request.descriptor_kind,
                    request.jar.as_deref(),
                    request.limit,
                )
                .into(),
        )
    }

    #[tool(
        description = "Return the validated root POM, packaging, Maven Wrapper, and reactor module model for the opt-in project"
    )]
    fn inspect_maven_project(&self) -> Result<Json<MavenProject>, ErrorData> {
        self.runner
            .as_ref()
            .map(|runner| Json(runner.project().clone()))
            .ok_or_else(|| ErrorData::invalid_params("project execution is disabled", None))
    }

    #[tool(
        description = "Run an allowlisted Maven compile, test-compile, or verify phase in the configured project with optional validated reactor selection"
    )]
    async fn run_maven_lifecycle(
        &self,
        Parameters(request): Parameters<MavenLifecycleRequest>,
    ) -> Result<Json<MavenBuildResult>, ErrorData> {
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| ErrorData::invalid_params("project execution is disabled", None))?;
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
    fn list_maven_test_classes(&self) -> Result<Json<Results<String>>, ErrorData> {
        self.runner
            .as_ref()
            .ok_or_else(|| ErrorData::invalid_params("project execution is disabled", None))?
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
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| ErrorData::invalid_params("project execution is disabled", None))?;
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
    async fn get_last_maven_test_failures(&self) -> Result<Json<LastTestFailures>, ErrorData> {
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| ErrorData::invalid_params("project execution is disabled", None))?;
        Ok(Json(runner.last_test_failures().await))
    }

    #[tool(
        description = "Resolve an effective Maven POM into structured project, parent, property, dependency, and plugin metadata"
    )]
    async fn get_effective_pom(
        &self,
        Parameters(request): Parameters<ProjectModuleRequest>,
    ) -> Result<Json<EffectivePomResult>, ErrorData> {
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| ErrorData::invalid_params("project execution is disabled", None))?;
        Ok(Json(runner.effective_pom(request.module.as_deref()).await))
    }

    #[tool(
        description = "Return a normalized Maven dependency tree with optional validated module, scope, and coordinate filters"
    )]
    async fn get_dependency_tree(
        &self,
        Parameters(request): Parameters<DependencyTreeRequest>,
    ) -> Result<Json<DependencyTreeResult>, ErrorData> {
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| ErrorData::invalid_params("project execution is disabled", None))?;
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
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| ErrorData::invalid_params("project execution is disabled", None))?;
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
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| ErrorData::invalid_params("project execution is disabled", None))?;
        Ok(Json(
            runner
                .build_classpath(request.module.as_deref(), request.kind)
                .await,
        ))
    }

    #[tool(
        description = "Read existing JaCoCo XML reports without running Maven and return per-module counters with missing and stale state"
    )]
    fn get_jacoco_coverage(&self) -> Result<Json<CoverageSummaryResult>, ErrorData> {
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| ErrorData::invalid_params("project execution is disabled", None))?;
        Ok(Json(runner.jacoco_coverage()))
    }

    #[tool(
        description = "Rank low-coverage classes from existing JaCoCo XML reports without running Maven"
    )]
    fn get_jacoco_coverage_gaps(
        &self,
        Parameters(request): Parameters<CoverageGapRequest>,
    ) -> Result<Json<CoverageGapResult>, ErrorData> {
        let runner = self
            .runner
            .as_ref()
            .ok_or_else(|| ErrorData::invalid_params("project execution is disabled", None))?;
        Ok(Json(runner.jacoco_coverage_gaps(request.limit)))
    }

    #[tool(description = "List all locally available versions of a Maven artifact")]
    fn list_artifact_versions(
        &self,
        Parameters(request): Parameters<VersionsRequest>,
    ) -> Json<BTreeMap<String, Vec<String>>> {
        Json(
            self.index
                .artifact_versions(&request.artifact_id, request.group_id.as_deref()),
        )
    }
}

#[tool_handler(
    router = self.tool_router,
    name = "maven-mcp",
    version = "0.1.0",
    instructions = "Search and inspect classes, JAR resources, POM metadata, local artifact health, sources, and versioned APIs from a read-only local Maven repository."
)]
impl ServerHandler for MavenMcpServer {}
