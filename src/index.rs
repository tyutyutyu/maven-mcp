use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use cafebabe::{
    ClassAccessFlags, FieldAccessFlags, MethodAccessFlags, ParseOptions,
    attributes::{Annotation, AttributeData, AttributeInfo},
    constant_pool::ConstantPoolItem,
};
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use zip::ZipArchive;

mod declaration_source;

use declaration_source::{SliceOutcome, SourceLanguage, SourceOutline};

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct ArtifactKey {
    group_id: String,
    artifact_id: String,
    version: String,
}

/// Where a JAR is located and which Maven artifact it represents.
#[derive(Debug, Clone)]
struct JarIdentity {
    key: ArtifactKey,
    classifier: Option<String>,
    relative_path: String,
}

#[derive(Debug, Clone)]
struct JarRecord {
    key: ArtifactKey,
    classifier: Option<String>,
    relative_path: String,
    path: PathBuf,
    classes: Vec<String>,
    entries: Vec<String>,
    /// Effective physical classfile entry per logical entry path
    /// (see [`effective_class_entries`]).
    class_entries: BTreeMap<String, String>,
    fact_strings: Vec<Arc<str>>,
    class_facts: Vec<IndexedClassFacts>,
    providers: Vec<IndexedProvider>,
}

impl JarRecord {
    fn coordinate(&self) -> String {
        match &self.classifier {
            Some(classifier) => format!(
                "{}:{}:{}:{}",
                self.key.group_id, self.key.artifact_id, self.key.version, classifier
            ),
            None => format!(
                "{}:{}:{}",
                self.key.group_id, self.key.artifact_id, self.key.version
            ),
        }
    }

    fn fact_string(&self, id: FactStringId) -> &str {
        &self.fact_strings[(id.0.get() - 1) as usize]
    }

    fn summary(&self) -> JarSummary {
        JarSummary {
            coordinate: self.coordinate(),
            group_id: self.key.group_id.clone(),
            artifact_id: self.key.artifact_id.clone(),
            version: self.key.version.clone(),
            classifier: self.classifier.clone(),
            path: self.relative_path.clone(),
            class_count: self.classes.len(),
            entry_count: self.entries.len(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct JarSummary {
    pub coordinate: String,
    pub group_id: String,
    pub artifact_id: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    pub path: String,
    pub class_count: usize,
    pub entry_count: usize,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ClassLocation {
    pub class_name: String,
    pub jar: JarSummary,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct EntryMatch {
    pub entry: String,
    pub jar: JarSummary,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ClassList {
    pub jar: JarSummary,
    pub total: usize,
    pub offset: usize,
    pub classes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct SourceResult {
    pub class_name: String,
    pub source_jar: JarSummary,
    pub entry: String,
    pub source: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JarEntryContentKind {
    Text,
    Binary,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct JarEntryContent {
    pub jar: JarSummary,
    pub entry: String,
    pub content_kind: JarEntryContentKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
    pub original_size: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
pub struct PomCoordinate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    pub artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
pub struct PomExclusion {
    pub group_id: String,
    pub artifact_id: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
pub struct PomDependency {
    pub group_id: String,
    pub artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub optional: bool,
    pub exclusions: Vec<PomExclusion>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct PomDescriptor {
    pub coordinate: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    pub artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub packaging: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<PomCoordinate>,
    pub properties: BTreeMap<String, String>,
    pub dependencies: Vec<PomDependency>,
    pub dependency_management: Vec<PomDependency>,
    pub bom_imports: Vec<PomDependency>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct PomDescriptorLookup {
    pub coordinate: String,
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<PomDescriptor>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ClassMemberDescription {
    pub name: String,
    pub descriptor: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generic_signature: Option<String>,
    pub visibility: String,
    pub modifiers: Vec<String>,
    pub annotations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ClassDescription {
    pub class_name: String,
    pub jar: JarSummary,
    pub class_file_version: u16,
    pub visibility: String,
    pub modifiers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generic_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub super_class: Option<String>,
    pub interfaces: Vec<String>,
    pub constructors: Vec<ClassMemberDescription>,
    pub methods: Vec<ClassMemberDescription>,
    pub fields: Vec<ClassMemberDescription>,
    pub annotations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
pub struct ArtifactFileStatus {
    pub file_name: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readable: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ArtifactHealth {
    pub coordinate: String,
    pub found: bool,
    pub snapshot: bool,
    pub files: Vec<ArtifactFileStatus>,
    pub checksums: Vec<String>,
    pub last_updated_markers: Vec<String>,
    pub repository_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum ClassMemberMatchKind {
    Method,
    Field,
    Annotation,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ClassMemberMatch {
    pub class_name: String,
    pub kind: ClassMemberMatchKind,
    pub name: String,
    pub signature: String,
    pub jar: JarSummary,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ClassApiChange {
    pub class_name: String,
    pub added_members: Vec<String>,
    pub removed_members: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_super_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_super_class: Option<String>,
    pub added_interfaces: Vec<String>,
    pub removed_interfaces: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ArtifactApiDiff {
    pub group_id: String,
    pub artifact_id: String,
    pub previous_version: String,
    pub current_version: String,
    pub added_classes: Vec<String>,
    pub removed_classes: Vec<String>,
    pub changed_classes: Vec<ClassApiChange>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct JarContentMatch {
    pub jar: JarSummary,
    pub entry: String,
    pub line: usize,
    pub context: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct JarContentSearch {
    pub results: Vec<JarContentMatch>,
    pub incomplete: bool,
    pub scanned_bytes: usize,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum TypeRelation {
    Extends,
    Implements,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct TypeHierarchyMatch {
    pub type_name: String,
    pub jar: JarSummary,
    pub relation: TypeRelation,
    pub depth: usize,
    pub path: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct SourceMatch {
    pub source_jar: JarSummary,
    pub entry: String,
    pub line: usize,
    pub context: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct SourceSearch {
    pub results: Vec<SourceMatch>,
    pub incomplete: bool,
    pub scanned_bytes: usize,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum DeclarationKind {
    Class,
    Field,
    Method,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
pub struct DeclarationCandidate {
    pub kind: DeclarationKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DeclarationSource {
    pub class_name: String,
    pub kind: DeclarationKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<String>,
    pub source_jar: JarSummary,
    pub entry: String,
    pub start_line: usize,
    pub end_line: usize,
    pub source: String,
    pub truncated: bool,
}

/// Why a classfile declaration has no source slice although its source file was read.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedDeclarationReason {
    /// The source has no declaration of this kind and name in the expected type.
    NotFound,
    /// Several source declarations match equally; `candidate_lines` lists them.
    Ambiguous,
    /// Same-named declarations exist, but none provably matches the descriptor;
    /// `candidate_lines` lists them.
    Unproven,
}

/// A classfile declaration whose exact source fragment cannot be proven.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct UnresolvedDeclaration {
    pub class_name: String,
    pub kind: DeclarationKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<String>,
    pub source_jar: JarSummary,
    pub entry: String,
    pub reason: UnresolvedDeclarationReason,
    /// One-based start lines of the source declarations that could not be told apart.
    pub candidate_lines: Vec<usize>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DeclarationSourceLookup {
    pub results: Vec<DeclarationSource>,
    pub ambiguous_candidates: Vec<DeclarationCandidate>,
    pub unresolved: Vec<UnresolvedDeclaration>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceDirection {
    Inbound,
    Outbound,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd,
)]
#[serde(rename_all = "snake_case")]
pub enum ClassReferenceKind {
    Class,
    Field,
    Method,
    InterfaceMethod,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ClassReference {
    pub source_class: String,
    pub source_jar: JarSummary,
    pub target_owner: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_descriptor: Option<String>,
    pub kind: ClassReferenceKind,
    pub target_artifacts: Vec<String>,
}

#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Ord, PartialOrd,
)]
#[serde(rename_all = "snake_case")]
pub enum ProviderDescriptorKind {
    ServiceLoader,
    ModuleUses,
    ModuleProvides,
    SpringFactories,
    SpringImports,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ProviderFact {
    pub service: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub descriptor_kind: ProviderDescriptorKind,
    pub entry: String,
    pub jar: JarSummary,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct IndexedClassFacts {
    source_class: FactStringId,
    parents: Vec<IndexedTypeParent>,
    references: Vec<IndexedReference>,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct IndexedTypeParent {
    parent: FactStringId,
    relation: TypeRelation,
}

/// Position of one child-to-parent fact: `jars[jar].class_facts[class].parents[parent]`.
#[derive(Debug, Clone, Copy)]
struct HierarchyEdge {
    jar: usize,
    class: usize,
    parent: usize,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct IndexedReference {
    target_owner: FactStringId,
    target_name: Option<FactStringId>,
    target_descriptor: Option<FactStringId>,
    kind: ClassReferenceKind,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct FactStringId(NonZeroU32);

#[derive(Default)]
struct FactStringPool {
    ids: HashMap<Arc<str>, FactStringId>,
    values: Vec<Arc<str>>,
}

impl FactStringPool {
    fn intern(&mut self, value: &str) -> FactStringId {
        if let Some(id) = self.ids.get(value) {
            return *id;
        }
        let raw_id = u32::try_from(self.values.len() + 1)
            .expect("one JAR cannot contain u32::MAX distinct fact strings");
        let id = FactStringId(
            NonZeroU32::new(raw_id).expect("fact string IDs start at one by construction"),
        );
        let value: Arc<str> = Arc::from(value);
        self.ids.insert(Arc::clone(&value), id);
        self.values.push(value);
        id
    }

    fn into_values(self) -> Vec<Arc<str>> {
        self.values
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct IndexedProvider {
    service: String,
    provider: Option<String>,
    descriptor_kind: ProviderDescriptorKind,
    entry: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct IndexStats {
    pub jar_count: usize,
    pub source_jar_count: usize,
    pub class_count: usize,
    pub unique_class_count: usize,
    pub artifact_count: usize,
}

#[derive(Debug)]
pub struct MavenIndex {
    root: PathBuf,
    jars: Vec<JarRecord>,
    source_jars: HashMap<ArtifactKey, usize>,
    class_locations: HashMap<String, Vec<usize>>,
    /// ASCII-lowercased parent type name to its direct children in the deterministic search
    /// order (child name, JAR path, relation); see [`build_children_by_parent`].
    children_by_parent: HashMap<String, Vec<HierarchyEdge>>,
    artifacts: BTreeMap<(String, String), BTreeSet<String>>,
    max_results: usize,
    max_source_bytes: usize,
}

impl MavenIndex {
    pub fn build(root: &Path, max_results: usize, max_source_bytes: usize) -> Result<Self> {
        let jars = collect_repository_jars(root)
            .into_iter()
            .filter_map(|path| repository_identity(root, &path).map(|identity| (path, identity)))
            .collect::<Vec<_>>();
        Self::build_from_jars(root, jars, max_results, max_source_bytes)
    }

    /// Builds an index limited to the effective classpath of one project: artifacts below
    /// `repository_root` and the outputs of the reactor modules in `reactor.modules`, both
    /// packaged JARs and class directories such as `target/classes`. A selected entry that
    /// is missing, outside both, or a reactor output without resolvable module coordinates
    /// is not indexed and is reported in [`ScopedIndex::unindexed`], never dropped silently.
    pub fn build_scoped(
        repository_root: Option<&Path>,
        reactor: &ReactorLayout<'_>,
        classpath: &[PathBuf],
        max_results: usize,
        max_source_bytes: usize,
    ) -> Result<ScopedIndex> {
        let mut jars = BTreeMap::new();
        let mut unindexed = Vec::new();
        for path in classpath {
            crate::blocking::checkpoint()?;
            let path = match path.canonicalize() {
                Ok(path) => path,
                Err(_) => {
                    unindexed.push(UnindexedClasspathEntry {
                        path: path.clone(),
                        reason: UnindexedReason::Missing,
                    });
                    continue;
                }
            };
            let identity = match scoped_identity(repository_root, reactor, &path) {
                Ok(identity) => identity,
                Err(reason) => {
                    unindexed.push(UnindexedClasspathEntry { path, reason });
                    continue;
                }
            };
            if let Some(source_path) = source_jar_path(&path)
                && source_path.is_file()
                && let Ok(source_identity) = scoped_identity(repository_root, reactor, &source_path)
            {
                jars.insert(source_path, source_identity);
            }
            jars.insert(path, identity);
        }
        let root = repository_root.unwrap_or(reactor.root);
        let index = Self::build_from_jars(
            root,
            jars.into_iter().collect(),
            max_results,
            max_source_bytes,
        )?;
        Ok(ScopedIndex { index, unindexed })
    }

    fn build_from_jars(
        root: &Path,
        jars: Vec<(PathBuf, JarIdentity)>,
        max_results: usize,
        max_source_bytes: usize,
    ) -> Result<Self> {
        let mut index = Self {
            root: root.to_owned(),
            jars: Vec::new(),
            source_jars: HashMap::new(),
            class_locations: HashMap::new(),
            children_by_parent: HashMap::new(),
            artifacts: BTreeMap::new(),
            max_results,
            max_source_bytes,
        };

        // Preflight sequentially, before ZIP metadata or class facts are allocated.
        let mut budget = ArchiveBudget::configured();
        let mut jars = jars;
        jars.sort_by(|left, right| left.0.cmp(&right.0));
        let mut records = Vec::new();
        for (path, identity) in &jars {
            crate::blocking::checkpoint()?;
            let preflight = if path.is_dir() {
                preflight_directory(path, &mut budget)
            } else {
                preflight_archive(path, &mut budget)
            };
            match preflight {
                Ok(()) => {}
                Err(error) if error.is::<ArchiveLimit>() => return Err(error),
                Err(error) => {
                    tracing::warn!(%error, "skipping malformed jar");
                    continue;
                }
            }
            if let Some(record) = load_jar_record(path, identity.clone(), max_source_bytes) {
                records.push(record);
            }
        }
        records.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));

        // A cancelled job may have skipped jars; its partial records must never become an index.
        crate::blocking::checkpoint()?;
        for record in records {
            let key = record.key.clone();
            let jar_index = index.jars.len();
            let is_source = record.classifier.as_deref() == Some("sources");

            index
                .artifacts
                .entry((key.group_id.clone(), key.artifact_id.clone()))
                .or_default()
                .insert(key.version.clone());
            if is_source {
                index.source_jars.insert(key, jar_index);
            } else {
                for class_name in &record.classes {
                    index
                        .class_locations
                        .entry(class_name.to_lowercase())
                        .or_default()
                        .push(jar_index);
                }
            }
            index.jars.push(record);
        }
        index.children_by_parent = build_children_by_parent(&index.jars);

        Ok(index)
    }

    pub fn stats(&self) -> IndexStats {
        IndexStats {
            jar_count: self.jars.len() - self.source_jars.len(),
            source_jar_count: self.source_jars.len(),
            class_count: self
                .jars
                .iter()
                .filter(|jar| jar.classifier.as_deref() != Some("sources"))
                .map(|jar| jar.classes.len())
                .sum(),
            unique_class_count: self.class_locations.len(),
            artifact_count: self.artifacts.len(),
        }
    }

    /// Deterministic estimate of the heap retained by this index: string payloads plus fixed
    /// per-container overheads. It is a budgeting figure, not an allocator measurement.
    pub fn estimated_bytes(&self) -> u64 {
        use std::mem::size_of;
        // Header plus a typical hash/B-tree node share for one map or set element.
        const NODE_OVERHEAD: usize = 32;
        // Allocator header and size-class rounding of one heap string.
        const ALLOCATION_OVERHEAD: usize = 16;
        let text = |value: &str| size_of::<String>() + value.len() + ALLOCATION_OVERHEAD;
        let mut total = size_of::<Self>() + self.root.as_os_str().len();
        for jar in &self.jars {
            total += size_of::<JarRecord>()
                + jar.key.group_id.len()
                + jar.key.artifact_id.len()
                + jar.key.version.len()
                + jar.classifier.as_deref().map_or(0, str::len)
                + jar.relative_path.len()
                + jar.path.as_os_str().len();
            total += jar.classes.iter().map(|name| text(name)).sum::<usize>();
            total += jar.entries.iter().map(|name| text(name)).sum::<usize>();
            total += jar
                .class_entries
                .iter()
                .map(|(logical, physical)| text(logical) + text(physical) + NODE_OVERHEAD)
                .sum::<usize>();
            total += jar
                .fact_strings
                .iter()
                .map(|value| {
                    size_of::<Arc<str>>()
                        + value.len()
                        + 2 * size_of::<usize>()
                        + ALLOCATION_OVERHEAD
                })
                .sum::<usize>();
            total += jar
                .class_facts
                .iter()
                .map(|facts| {
                    size_of::<IndexedClassFacts>()
                        + facts.parents.len() * size_of::<IndexedTypeParent>()
                        + facts.references.len() * size_of::<IndexedReference>()
                })
                .sum::<usize>();
            total += jar
                .providers
                .iter()
                .map(|provider| {
                    size_of::<IndexedProvider>()
                        + provider.service.len()
                        + provider.provider.as_deref().map_or(0, str::len)
                        + provider.entry.len()
                })
                .sum::<usize>();
        }
        total += self.source_jars.len() * (size_of::<ArtifactKey>() + NODE_OVERHEAD);
        total += self
            .class_locations
            .iter()
            .map(|(name, jars)| text(name) + jars.len() * size_of::<usize>() + NODE_OVERHEAD)
            .sum::<usize>();
        total += self
            .children_by_parent
            .iter()
            .map(|(parent, edges)| {
                text(parent)
                    + size_of::<Vec<HierarchyEdge>>()
                    + edges.len() * size_of::<HierarchyEdge>()
                    + NODE_OVERHEAD
            })
            .sum::<usize>();
        total += self
            .artifacts
            .iter()
            .map(|((group, artifact), versions)| {
                text(group)
                    + text(artifact)
                    + versions
                        .iter()
                        .map(|version| text(version) + NODE_OVERHEAD)
                        .sum::<usize>()
                    + NODE_OVERHEAD
            })
            .sum::<usize>();
        total as u64
    }

    pub fn search_classes(&self, query: &str, limit: Option<usize>) -> Vec<ClassLocation> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let limit = self.limit(limit);
        let mut names = self
            .class_locations
            .keys()
            .filter(|name| name.contains(&query))
            .collect::<Vec<_>>();
        names.sort_by_key(|name| {
            (
                if name.as_str() == query { 0 } else { 1 },
                name.len(),
                *name,
            )
        });

        let mut matches = Vec::new();
        for name in names {
            for jar_index in &self.class_locations[name] {
                let jar = &self.jars[*jar_index];
                let class_name = jar
                    .classes
                    .iter()
                    .find(|candidate| candidate.eq_ignore_ascii_case(name))
                    .cloned()
                    .unwrap_or_else(|| name.clone());
                matches.push(ClassLocation {
                    class_name,
                    jar: self.summary(jar),
                });
                if matches.len() == limit {
                    return matches;
                }
            }
        }
        matches
    }

    pub fn search_jars(&self, query: &str, limit: Option<usize>) -> Vec<JarSummary> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let limit = self.limit(limit);
        self.jars
            .iter()
            .filter(|jar| {
                jar.coordinate().to_lowercase().contains(&query)
                    || jar.relative_path.to_lowercase().contains(&query)
            })
            .take(limit)
            .map(|jar| self.summary(jar))
            .collect()
    }

    pub fn search_entries(
        &self,
        query: &str,
        jar_selector: Option<&str>,
        limit: Option<usize>,
    ) -> Vec<EntryMatch> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let limit = self.limit(limit);
        let mut matches = Vec::new();
        for jar in self.selected_jars(jar_selector) {
            for entry in &jar.entries {
                if entry.to_lowercase().contains(&query) {
                    matches.push(EntryMatch {
                        entry: entry.clone(),
                        jar: self.summary(jar),
                    });
                    if matches.len() == limit {
                        return matches;
                    }
                }
            }
        }
        matches
    }

    pub fn list_classes(
        &self,
        jar_selector: &str,
        offset: usize,
        limit: Option<usize>,
    ) -> Vec<ClassList> {
        let limit = self.limit(limit);
        self.selected_jars(Some(jar_selector))
            .into_iter()
            .filter(|jar| jar.classifier.as_deref() != Some("sources"))
            .map(|jar| ClassList {
                jar: self.summary(jar),
                total: jar.classes.len(),
                offset,
                classes: jar
                    .classes
                    .iter()
                    .skip(offset)
                    .take(limit)
                    .cloned()
                    .collect(),
            })
            .collect()
    }

    pub fn artifact_versions(
        &self,
        artifact_id: &str,
        group_id: Option<&str>,
    ) -> BTreeMap<String, Vec<String>> {
        let artifact_id = artifact_id.trim();
        self.artifacts
            .iter()
            .filter(|((group, artifact), _)| {
                artifact.eq_ignore_ascii_case(artifact_id)
                    && group_id.is_none_or(|expected| group.eq_ignore_ascii_case(expected.trim()))
            })
            .map(|((group, artifact), versions)| {
                (
                    format!("{group}:{artifact}"),
                    versions.iter().cloned().collect(),
                )
            })
            .collect()
    }

    pub fn class_source(
        &self,
        class_name: &str,
        jar_selector: Option<&str>,
        version: Option<&str>,
    ) -> Result<Vec<SourceResult>> {
        let normalized = normalize_class_query(class_name);
        let Some(locations) = self.class_locations.get(&normalized.to_lowercase()) else {
            return Ok(Vec::new());
        };
        let mut results = Vec::new();
        for jar_index in locations {
            let binary = &self.jars[*jar_index];
            if jar_selector.is_some_and(|selector| !matches_selector(binary, selector))
                || version.is_some_and(|expected| binary.key.version != expected)
            {
                continue;
            }
            let Some(source_index) = self.source_jars.get(&binary.key) else {
                continue;
            };
            let source_jar = &self.jars[*source_index];
            if let Some((entry, source, truncated)) =
                read_source(&source_jar.path, &normalized, self.max_source_bytes)?
            {
                results.push(SourceResult {
                    class_name: normalized.clone(),
                    source_jar: self.summary(source_jar),
                    entry,
                    source,
                    truncated,
                });
            }
        }
        Ok(results)
    }

    pub fn jar_entry(&self, jar_selector: &str, entry_path: &str) -> Result<Vec<JarEntryContent>> {
        let entry_path = entry_path.trim().replace('\\', "/");
        if entry_path.is_empty() {
            return Ok(Vec::new());
        }

        self.selected_jars(Some(jar_selector))
            .into_iter()
            .filter_map(
                |jar| match read_jar_entry(&jar.path, &entry_path, self.max_source_bytes) {
                    Ok(Some(content)) => Some(Ok(JarEntryContent {
                        jar: self.summary(jar),
                        entry: entry_path.clone(),
                        content_kind: content.kind,
                        text: content.text,
                        bytes: content.bytes,
                        original_size: content.original_size,
                        truncated: content.truncated,
                    })),
                    Ok(None) => None,
                    Err(error) => Some(Err(error)),
                },
            )
            .collect()
    }

    pub fn pom_descriptor(&self, coordinate: &str) -> Result<PomDescriptorLookup> {
        Self::pom_descriptor_at(&self.root, coordinate)
    }

    pub fn pom_descriptor_at(root: &Path, coordinate: &str) -> Result<PomDescriptorLookup> {
        let key = parse_exact_coordinate(coordinate)?;
        let coordinate = format!("{}:{}:{}", key.group_id, key.artifact_id, key.version);
        let pom_name = format!("{}-{}.pom", key.artifact_id, key.version);
        let Some(path) = contained_artifact_path(root, &key, Some(&pom_name))? else {
            return Ok(PomDescriptorLookup {
                coordinate,
                found: false,
                descriptor: None,
            });
        };

        let xml = crate::config::read_bounded_xml(&path).map_err(|error| {
            let operation = if error.is::<quick_xml::Error>() {
                "parse"
            } else {
                "read"
            };
            anyhow::anyhow!("cannot {operation} POM for {coordinate}: {error:#}")
        })?;
        let raw: RawPomProject = quick_xml::de::from_str(&xml)
            .with_context(|| format!("cannot parse POM for {coordinate}"))?;
        let descriptor = PomDescriptor::from_raw(coordinate.clone(), raw);
        Ok(PomDescriptorLookup {
            coordinate,
            found: true,
            descriptor: Some(descriptor),
        })
    }

    pub fn describe_class(
        &self,
        class_name: &str,
        jar_selector: Option<&str>,
        version: Option<&str>,
        public_only: bool,
    ) -> Result<Vec<ClassDescription>> {
        let normalized = normalize_class_query(class_name);
        let Some(locations) = self.class_locations.get(&normalized.to_lowercase()) else {
            return Ok(Vec::new());
        };

        locations
            .iter()
            .filter_map(|jar_index| {
                let jar = &self.jars[*jar_index];
                if jar_selector.is_some_and(|selector| !matches_selector(jar, selector))
                    || version.is_some_and(|expected| jar.key.version != expected)
                {
                    return None;
                }
                Some(read_class_description(
                    jar,
                    self.summary(jar),
                    &normalized,
                    self.max_source_bytes,
                    public_only,
                ))
            })
            .collect()
    }

    pub fn artifact_health(&self, coordinate: &str) -> Result<ArtifactHealth> {
        Self::artifact_health_at(&self.root, coordinate)
    }

    pub fn artifact_health_at(root: &Path, coordinate: &str) -> Result<ArtifactHealth> {
        let key = parse_exact_coordinate(coordinate)?;
        let coordinate = format!("{}:{}:{}", key.group_id, key.artifact_id, key.version);
        let directory = contained_artifact_path(root, &key, None)?.filter(|path| path.is_dir());
        let Some(directory) = directory else {
            return Ok(ArtifactHealth {
                coordinate,
                found: false,
                snapshot: key.version.ends_with("-SNAPSHOT"),
                files: Vec::new(),
                checksums: Vec::new(),
                last_updated_markers: Vec::new(),
                repository_ids: Vec::new(),
            });
        };

        let mut files = Vec::new();
        let mut checksums = Vec::new();
        let mut last_updated_markers = Vec::new();
        let mut repository_ids = BTreeSet::new();
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("cannot inspect local artifact {coordinate}"))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let file_name = entry.file_name().to_string_lossy().into_owned();
            if is_checksum_file(&file_name) {
                checksums.push(file_name);
            } else if file_name.ends_with(".lastUpdated") {
                last_updated_markers.push(file_name);
            } else if file_name == "_remote.repositories" {
                for line in std::fs::read_to_string(entry.path())?.lines() {
                    if line.trim_start().starts_with('#') {
                        continue;
                    }
                    if let Some((_, repository)) = line.split_once('>')
                        && let Some((repository_id, _)) = repository.split_once('=')
                        && !repository_id.trim().is_empty()
                    {
                        repository_ids.insert(repository_id.trim().to_owned());
                    }
                }
            } else if let Some(status) = artifact_file_status(&key, &file_name, &entry.path()) {
                files.push(status);
            }
        }
        files.sort();
        checksums.sort();
        last_updated_markers.sort();
        Ok(ArtifactHealth {
            coordinate,
            found: true,
            snapshot: key.version.ends_with("-SNAPSHOT"),
            files,
            checksums,
            last_updated_markers,
            repository_ids: repository_ids.into_iter().collect(),
        })
    }

    pub fn search_class_members(
        &self,
        query: &str,
        jar_selector: Option<&str>,
        limit: Option<usize>,
    ) -> Vec<ClassMemberMatch> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let limit = self.limit(limit);
        let mut matches = Vec::new();
        for jar in self
            .selected_jars(jar_selector)
            .into_iter()
            .filter(|jar| jar.classifier.as_deref() != Some("sources"))
        {
            if jar.classes.is_empty() {
                continue;
            }
            let mut archive = match ClassArchive::open(&jar.path) {
                Ok(archive) => archive,
                Err(error) => {
                    tracing::warn!(
                        coordinate = %jar.coordinate(),
                        %error,
                        "skipping unreadable class archive"
                    );
                    continue;
                }
            };
            for class_name in &jar.classes {
                let description = match read_class_description_from(
                    &mut archive,
                    jar,
                    self.summary(jar),
                    class_name,
                    self.max_source_bytes,
                    false,
                ) {
                    Ok(description) => description,
                    Err(error) => {
                        tracing::warn!(
                            coordinate = %jar.coordinate(),
                            class_name,
                            %error,
                            "skipping unreadable class metadata"
                        );
                        continue;
                    }
                };
                collect_member_matches(&description, &query, &mut matches);
            }
        }
        matches.sort_by(|left, right| {
            (
                &left.jar.coordinate,
                &left.class_name,
                &left.kind,
                &left.name,
                &left.signature,
            )
                .cmp(&(
                    &right.jar.coordinate,
                    &right.class_name,
                    &right.kind,
                    &right.name,
                    &right.signature,
                ))
        });
        matches.truncate(limit);
        matches
    }

    /// Compares the public and protected class API of two exact binary JAR versions read
    /// directly from the repository `root`. Only those two files are opened, so neither
    /// version needs to be on a project classpath or in any index.
    pub fn compare_artifact_api_at(
        root: &Path,
        max_source_bytes: usize,
        group_id: &str,
        artifact_id: &str,
        previous_version: &str,
        current_version: &str,
    ) -> Result<ArtifactApiDiff> {
        let previous_coordinate = format!("{group_id}:{artifact_id}:{previous_version}");
        let current_coordinate = format!("{group_id}:{artifact_id}:{current_version}");
        let previous_key = parse_exact_coordinate(&previous_coordinate)?;
        let current_key = parse_exact_coordinate(&current_coordinate)?;
        let previous_path = exact_binary_jar_path(root, &previous_key)?;
        let current_path = exact_binary_jar_path(root, &current_key)?;
        let mut budget = ArchiveBudget::configured();
        let previous_api = public_class_api(
            root,
            &previous_path,
            previous_key,
            &mut budget,
            max_source_bytes,
        )?;
        let current_api = public_class_api(
            root,
            &current_path,
            current_key,
            &mut budget,
            max_source_bytes,
        )?;

        let added_classes = current_api
            .keys()
            .filter(|class_name| !previous_api.contains_key(*class_name))
            .cloned()
            .collect::<Vec<_>>();
        let removed_classes = previous_api
            .keys()
            .filter(|class_name| !current_api.contains_key(*class_name))
            .cloned()
            .collect::<Vec<_>>();
        let mut changed_classes = Vec::new();
        for (class_name, previous_class) in &previous_api {
            let Some(current_class) = current_api.get(class_name) else {
                continue;
            };
            let previous_members = api_members(previous_class);
            let current_members = api_members(current_class);
            let added_members = current_members
                .difference(&previous_members)
                .cloned()
                .collect::<Vec<_>>();
            let removed_members = previous_members
                .difference(&current_members)
                .cloned()
                .collect::<Vec<_>>();
            let previous_interfaces = previous_class
                .interfaces
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            let current_interfaces = current_class
                .interfaces
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>();
            let added_interfaces = current_interfaces
                .difference(&previous_interfaces)
                .cloned()
                .collect::<Vec<_>>();
            let removed_interfaces = previous_interfaces
                .difference(&current_interfaces)
                .cloned()
                .collect::<Vec<_>>();
            if added_members.is_empty()
                && removed_members.is_empty()
                && added_interfaces.is_empty()
                && removed_interfaces.is_empty()
                && previous_class.super_class == current_class.super_class
            {
                continue;
            }
            changed_classes.push(ClassApiChange {
                class_name: class_name.clone(),
                added_members,
                removed_members,
                previous_super_class: previous_class.super_class.clone(),
                current_super_class: current_class.super_class.clone(),
                added_interfaces,
                removed_interfaces,
            });
        }

        Ok(ArtifactApiDiff {
            group_id: group_id.to_owned(),
            artifact_id: artifact_id.to_owned(),
            previous_version: previous_version.to_owned(),
            current_version: current_version.to_owned(),
            added_classes,
            removed_classes,
            changed_classes,
        })
    }

    pub fn search_jar_content(
        &self,
        query: &str,
        jar_selector: Option<&str>,
        limit: Option<usize>,
    ) -> Result<JarContentSearch> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Ok(JarContentSearch {
                results: Vec::new(),
                incomplete: false,
                scanned_bytes: 0,
            });
        }
        let limit = self.limit(limit);
        let total_budget = self.max_source_bytes.saturating_mul(self.max_results);
        let mut results = Vec::new();
        let mut scanned_bytes = 0usize;
        let mut incomplete = false;
        'jars: for jar in self
            .selected_jars(jar_selector)
            .into_iter()
            .filter(|jar| jar.classifier.as_deref() != Some("sources"))
        {
            let mut archive = ClassArchive::open(&jar.path)?;
            for entry_path in jar.entries.iter().filter(|entry| is_text_resource(entry)) {
                crate::blocking::checkpoint()?;
                let mut entry = archive.indexed_entry(entry_path)?;
                let declared_size = usize::try_from(entry.size()).unwrap_or(usize::MAX);
                if declared_size > self.max_source_bytes
                    || scanned_bytes.saturating_add(declared_size) > total_budget
                {
                    incomplete = true;
                    if scanned_bytes.saturating_add(declared_size) > total_budget {
                        break 'jars;
                    }
                    continue;
                }
                let mut bytes = Vec::with_capacity(declared_size);
                entry.read_to_end(&mut bytes)?;
                scanned_bytes = scanned_bytes.saturating_add(bytes.len());
                let Ok(text) = String::from_utf8(bytes) else {
                    continue;
                };
                if let Some((line, context)) = matching_context(&text, &query) {
                    results.push(JarContentMatch {
                        jar: self.summary(jar),
                        entry: entry_path.clone(),
                        line,
                        context,
                    });
                    if results.len() == limit {
                        incomplete = true;
                        break 'jars;
                    }
                }
            }
        }
        Ok(JarContentSearch {
            results,
            incomplete,
            scanned_bytes,
        })
    }

    pub fn search_type_hierarchy(
        &self,
        type_name: &str,
        transitive: bool,
        jar_selector: Option<&str>,
        limit: Option<usize>,
    ) -> Vec<TypeHierarchyMatch> {
        let root = normalize_class_query(type_name);
        if root.is_empty() {
            return Vec::new();
        }
        let limit = self.limit(limit);
        let mut queue = VecDeque::from([(root.clone(), vec![root.clone()], 0usize)]);
        let mut visited_edges = BTreeSet::new();
        let mut matches = Vec::new();
        while let Some((parent, path, depth)) = queue.pop_front() {
            for edge in self.children_of(&parent) {
                #[cfg(test)]
                HIERARCHY_EDGES_EXAMINED.with(|examined| examined.set(examined.get() + 1));
                let jar = &self.jars[edge.jar];
                let class = &jar.class_facts[edge.class];
                let fact = &class.parents[edge.parent];
                let child = jar.fact_string(class.source_class);
                let fact_parent = jar.fact_string(fact.parent);
                let edge_key = (
                    child.to_lowercase(),
                    fact_parent.to_lowercase(),
                    jar.coordinate(),
                );
                if !visited_edges.insert(edge_key) || path.iter().any(|node| node == child) {
                    continue;
                }
                let mut child_path = path.clone();
                child_path.push(child.to_owned());
                if jar_selector.is_none_or(|selector| matches_selector(jar, selector)) {
                    matches.push(TypeHierarchyMatch {
                        type_name: child.to_owned(),
                        jar: self.summary(jar),
                        relation: fact.relation,
                        depth: depth + 1,
                        path: child_path.clone(),
                    });
                    if matches.len() == limit {
                        return matches;
                    }
                }
                if transitive {
                    queue.push_back((child.to_owned(), child_path, depth + 1));
                }
            }
            if !transitive {
                break;
            }
        }
        matches
    }

    fn children_of(&self, parent: &str) -> &[HierarchyEdge] {
        self.children_by_parent
            .get(&parent.to_ascii_lowercase())
            .map_or(&[], Vec::as_slice)
    }

    pub fn search_source(
        &self,
        query: &str,
        regex: bool,
        jar_selector: Option<&str>,
        context_lines: usize,
        limit: Option<usize>,
    ) -> Result<SourceSearch> {
        let query = query.trim();
        if query.is_empty() {
            bail!("source query must not be empty");
        }
        let pattern = regex.then(|| Regex::new(query)).transpose()?;
        let context_lines = context_lines.min(10);
        let limit = self.limit(limit);
        let total_budget = self.max_source_bytes.saturating_mul(self.max_results);
        let mut results = Vec::new();
        let mut scanned_bytes = 0usize;
        let mut incomplete = false;
        'jars: for source_jar in self.jars.iter().filter(|jar| {
            jar.classifier.as_deref() == Some("sources")
                && self.source_matches_selector(jar, jar_selector)
        }) {
            let mut archive = ZipArchive::new(File::open(&source_jar.path)?)?;
            for entry_path in source_jar
                .entries
                .iter()
                .filter(|entry| entry.ends_with(".java") || entry.ends_with(".kt"))
            {
                crate::blocking::checkpoint()?;
                let mut entry = archive.by_name(entry_path)?;
                let declared_size = usize::try_from(entry.size()).unwrap_or(usize::MAX);
                if scanned_bytes.saturating_add(declared_size) > total_budget {
                    incomplete = true;
                    break 'jars;
                }
                let mut bytes = Vec::with_capacity(declared_size.min(self.max_source_bytes));
                entry
                    .by_ref()
                    .take(self.max_source_bytes.saturating_add(1) as u64)
                    .read_to_end(&mut bytes)?;
                let truncated = bytes.len() > self.max_source_bytes;
                incomplete |= truncated;
                bytes.truncate(self.max_source_bytes);
                if truncated {
                    bytes.truncate(complete_utf8_prefix_len(&bytes));
                }
                scanned_bytes = scanned_bytes.saturating_add(bytes.len());
                let text = String::from_utf8_lossy(&bytes);
                let lines = text.lines().collect::<Vec<_>>();
                for (line_index, line) in lines.iter().enumerate() {
                    let matched = pattern
                        .as_ref()
                        .map_or_else(|| line.contains(query), |pattern| pattern.is_match(line));
                    if !matched {
                        continue;
                    }
                    let start = line_index.saturating_sub(context_lines);
                    let end = (line_index + context_lines + 1).min(lines.len());
                    results.push(SourceMatch {
                        source_jar: self.summary(source_jar),
                        entry: entry_path.clone(),
                        line: line_index + 1,
                        context: lines[start..end].join("\n"),
                        truncated,
                    });
                    if results.len() == limit {
                        incomplete = true;
                        break 'jars;
                    }
                }
            }
        }
        Ok(SourceSearch {
            results,
            incomplete,
            scanned_bytes,
        })
    }

    pub fn get_declaration_source(
        &self,
        class_name: &str,
        member_name: Option<&str>,
        descriptor: Option<&str>,
        jar_selector: Option<&str>,
        version: Option<&str>,
    ) -> Result<DeclarationSourceLookup> {
        let class_name = normalize_class_query(class_name);
        let Some(locations) = self.class_locations.get(&class_name.to_lowercase()) else {
            return Ok(DeclarationSourceLookup {
                results: Vec::new(),
                ambiguous_candidates: Vec::new(),
                unresolved: Vec::new(),
            });
        };
        let member_name = member_name.map(normalize_member_name);
        let mut results = Vec::new();
        let mut ambiguous_candidates = BTreeSet::new();
        let mut unresolved = Vec::new();
        for jar_index in locations {
            let binary = &self.jars[*jar_index];
            if jar_selector.is_some_and(|selector| !matches_selector(binary, selector))
                || version.is_some_and(|expected| binary.key.version != expected)
            {
                continue;
            }
            let Some(source_index) = self.source_jars.get(&binary.key) else {
                continue;
            };
            let candidates = declaration_candidates(
                binary,
                &class_name,
                member_name.as_deref(),
                self.max_source_bytes,
            )?;
            let matching = candidates
                .into_iter()
                .filter(|declaration| {
                    descriptor.is_none_or(|expected| {
                        declaration.candidate.descriptor.as_deref() == Some(expected)
                    })
                })
                .collect::<Vec<_>>();
            if member_name.is_some() && descriptor.is_none() && matching.len() > 1 {
                ambiguous_candidates.extend(matching.into_iter().map(|value| value.candidate));
                continue;
            }
            let source_jar = &self.jars[*source_index];
            let Some((entry, source, truncated)) =
                read_source(&source_jar.path, &class_name, self.max_source_bytes)?
            else {
                continue;
            };
            let outline = SourceOutline::parse(&source, SourceLanguage::of_entry(&entry));
            for ClassfileDeclaration {
                candidate,
                is_compiler_generated,
            } in matching
            {
                let located = LocatedSource {
                    class_name: &class_name,
                    source_jar: self.summary(source_jar),
                    entry: &entry,
                    truncated,
                };
                match outline.locate(&class_name, &candidate, is_compiler_generated) {
                    SliceOutcome::Found(slice) => results.push(located.found(candidate, slice)),
                    SliceOutcome::Unresolved {
                        reason,
                        candidate_lines,
                    } => unresolved.push(located.unresolved(candidate, reason, candidate_lines)),
                }
            }
        }
        results.sort_by(|left, right| {
            (
                &left.source_jar.coordinate,
                &left.entry,
                left.start_line,
                &left.descriptor,
            )
                .cmp(&(
                    &right.source_jar.coordinate,
                    &right.entry,
                    right.start_line,
                    &right.descriptor,
                ))
        });
        unresolved.sort_by(|left, right| {
            (
                &left.source_jar.coordinate,
                &left.entry,
                left.kind,
                &left.name,
                &left.descriptor,
            )
                .cmp(&(
                    &right.source_jar.coordinate,
                    &right.entry,
                    right.kind,
                    &right.name,
                    &right.descriptor,
                ))
        });
        Ok(DeclarationSourceLookup {
            results,
            ambiguous_candidates: ambiguous_candidates.into_iter().collect(),
            unresolved,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn search_class_references(
        &self,
        class_name: &str,
        direction: ReferenceDirection,
        kind: Option<ClassReferenceKind>,
        member_name: Option<&str>,
        descriptor: Option<&str>,
        jar_selector: Option<&str>,
        limit: Option<usize>,
    ) -> Vec<ClassReference> {
        let class_name = normalize_class_query(class_name);
        if class_name.is_empty() {
            return Vec::new();
        }
        let mut results = self
            .jars
            .iter()
            .filter(|jar| {
                jar.classifier.as_deref() != Some("sources")
                    && jar_selector.is_none_or(|selector| matches_selector(jar, selector))
            })
            .flat_map(|jar| {
                jar.class_facts.iter().flat_map(|class| {
                    class
                        .references
                        .iter()
                        .filter(|reference| match direction {
                            ReferenceDirection::Inbound => jar
                                .fact_string(reference.target_owner)
                                .eq_ignore_ascii_case(&class_name),
                            ReferenceDirection::Outbound => jar
                                .fact_string(class.source_class)
                                .eq_ignore_ascii_case(&class_name),
                        })
                        .filter(|reference| kind.is_none_or(|expected| reference.kind == expected))
                        .filter(|reference| {
                            member_name.is_none_or(|expected| {
                                reference
                                    .target_name
                                    .is_some_and(|name| jar.fact_string(name) == expected)
                            })
                        })
                        .filter(|reference| {
                            descriptor.is_none_or(|expected| {
                                reference
                                    .target_descriptor
                                    .is_some_and(|value| jar.fact_string(value) == expected)
                            })
                        })
                        .map(|reference| ClassReference {
                            source_class: jar.fact_string(class.source_class).to_owned(),
                            source_jar: self.summary(jar),
                            target_owner: jar.fact_string(reference.target_owner).to_owned(),
                            target_name: reference
                                .target_name
                                .map(|value| jar.fact_string(value).to_owned()),
                            target_descriptor: reference
                                .target_descriptor
                                .map(|value| jar.fact_string(value).to_owned()),
                            kind: reference.kind,
                            target_artifacts: self
                                .target_artifacts(jar.fact_string(reference.target_owner)),
                        })
                })
            })
            .collect::<Vec<_>>();
        results.sort_by(|left, right| {
            (
                &left.source_jar.coordinate,
                &left.source_class,
                &left.target_owner,
                left.kind,
                &left.target_name,
                &left.target_descriptor,
            )
                .cmp(&(
                    &right.source_jar.coordinate,
                    &right.source_class,
                    &right.target_owner,
                    right.kind,
                    &right.target_name,
                    &right.target_descriptor,
                ))
        });
        results.truncate(self.limit(limit));
        results
    }

    pub fn search_providers(
        &self,
        service: Option<&str>,
        provider: Option<&str>,
        descriptor_kind: Option<ProviderDescriptorKind>,
        jar_selector: Option<&str>,
        limit: Option<usize>,
    ) -> Vec<ProviderFact> {
        let mut results = self
            .jars
            .iter()
            .filter(|jar| jar_selector.is_none_or(|selector| matches_selector(jar, selector)))
            .flat_map(|jar| {
                jar.providers
                    .iter()
                    .filter(|fact| {
                        service.is_none_or(|value| fact.service.eq_ignore_ascii_case(value.trim()))
                    })
                    .filter(|fact| {
                        provider.is_none_or(|value| {
                            fact.provider.as_deref().is_some_and(|candidate| {
                                candidate.eq_ignore_ascii_case(value.trim())
                            })
                        })
                    })
                    .filter(|fact| descriptor_kind.is_none_or(|kind| fact.descriptor_kind == kind))
                    .map(|fact| ProviderFact {
                        service: fact.service.clone(),
                        provider: fact.provider.clone(),
                        descriptor_kind: fact.descriptor_kind,
                        entry: fact.entry.clone(),
                        jar: self.summary(jar),
                    })
            })
            .collect::<Vec<_>>();
        results.sort_by(|left, right| {
            (
                &left.jar.coordinate,
                left.descriptor_kind,
                &left.service,
                &left.provider,
                &left.entry,
            )
                .cmp(&(
                    &right.jar.coordinate,
                    right.descriptor_kind,
                    &right.service,
                    &right.provider,
                    &right.entry,
                ))
        });
        results.dedup_by(|left, right| left == right);
        results.truncate(self.limit(limit));
        results
    }

    fn target_artifacts(&self, class_name: &str) -> Vec<String> {
        self.class_locations
            .get(&class_name.to_lowercase())
            .into_iter()
            .flatten()
            .map(|jar_index| self.jars[*jar_index].coordinate())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn source_matches_selector(&self, source_jar: &JarRecord, selector: Option<&str>) -> bool {
        selector.is_none_or(|selector| {
            matches_selector(source_jar, selector)
                || self.jars.iter().any(|jar| {
                    jar.key == source_jar.key
                        && jar.classifier.as_deref() != Some("sources")
                        && matches_selector(jar, selector)
                })
        })
    }

    fn selected_jars(&self, selector: Option<&str>) -> Vec<&JarRecord> {
        self.jars
            .iter()
            .filter(|jar| selector.is_none_or(|value| matches_selector(jar, value)))
            .collect()
    }

    fn limit(&self, requested: Option<usize>) -> usize {
        requested
            .unwrap_or(self.max_results)
            .clamp(1, self.max_results)
    }

    fn summary(&self, jar: &JarRecord) -> JarSummary {
        jar.summary()
    }
}

impl PomDescriptor {
    fn from_raw(coordinate: String, raw: RawPomProject) -> Self {
        let mut dependencies = convert_dependencies(raw.dependencies);
        let managed = convert_dependencies(raw.dependency_management.dependencies);
        let (mut bom_imports, mut dependency_management): (Vec<_>, Vec<_>) =
            managed.into_iter().partition(|dependency| {
                dependency.r#type.as_deref() == Some("pom")
                    && dependency.scope.as_deref() == Some("import")
            });
        dependencies.sort();
        dependency_management.sort();
        bom_imports.sort();

        Self {
            coordinate,
            group_id: raw.group_id,
            artifact_id: raw.artifact_id,
            version: raw.version,
            packaging: raw.packaging.unwrap_or_else(|| "jar".to_owned()),
            parent: raw.parent.map(|parent| PomCoordinate {
                group_id: Some(parent.group_id),
                artifact_id: parent.artifact_id,
                version: Some(parent.version),
            }),
            properties: crate::redaction::redact_properties(raw.properties),
            dependencies,
            dependency_management,
            bom_imports,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawPomProject {
    group_id: Option<String>,
    #[serde(default)]
    artifact_id: String,
    version: Option<String>,
    packaging: Option<String>,
    parent: Option<RawPomParent>,
    #[serde(default)]
    properties: BTreeMap<String, String>,
    #[serde(default)]
    dependencies: RawPomDependencies,
    #[serde(default)]
    dependency_management: RawPomDependencyManagement,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPomParent {
    group_id: String,
    artifact_id: String,
    version: String,
}

#[derive(Debug, Deserialize, Default)]
struct RawPomDependencies {
    #[serde(rename = "dependency", default)]
    dependencies: Vec<RawPomDependency>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct RawPomDependencyManagement {
    #[serde(default)]
    dependencies: RawPomDependencies,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPomDependency {
    group_id: String,
    artifact_id: String,
    version: Option<String>,
    #[serde(rename = "type")]
    dependency_type: Option<String>,
    classifier: Option<String>,
    scope: Option<String>,
    optional: Option<String>,
    #[serde(default)]
    exclusions: RawPomExclusions,
}

#[derive(Debug, Deserialize, Default)]
struct RawPomExclusions {
    #[serde(rename = "exclusion", default)]
    exclusions: Vec<RawPomExclusion>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPomExclusion {
    group_id: String,
    artifact_id: String,
}

fn convert_dependencies(raw: RawPomDependencies) -> Vec<PomDependency> {
    raw.dependencies
        .into_iter()
        .map(|dependency| {
            let mut exclusions = dependency
                .exclusions
                .exclusions
                .into_iter()
                .map(|exclusion| PomExclusion {
                    group_id: exclusion.group_id,
                    artifact_id: exclusion.artifact_id,
                })
                .collect::<Vec<_>>();
            exclusions.sort();
            PomDependency {
                group_id: dependency.group_id,
                artifact_id: dependency.artifact_id,
                version: dependency.version,
                r#type: dependency.dependency_type,
                classifier: dependency.classifier,
                scope: dependency.scope,
                optional: dependency.optional.as_deref() == Some("true"),
                exclusions,
            }
        })
        .collect()
}

/// Raised when an exact artifact path would resolve outside the configured repository.
#[derive(Debug)]
pub struct ArtifactOutsideRepository;

impl std::fmt::Display for ArtifactOutsideRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("artifact path resolves outside the configured Maven repository")
    }
}

impl std::error::Error for ArtifactOutsideRepository {}

fn parse_exact_coordinate(coordinate: &str) -> Result<ArtifactKey> {
    let parts = coordinate.trim().split(':').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || part.contains(['/', '\\', '\0'])
                || *part == "."
                || *part == ".."
                || part.contains("..")
        })
        || parts[0].split('.').any(str::is_empty)
    {
        bail!("coordinate must use exact groupId:artifactId:version form");
    }
    Ok(ArtifactKey {
        group_id: parts[0].to_owned(),
        artifact_id: parts[1].to_owned(),
        version: parts[2].to_owned(),
    })
}

/// Resolves an exact artifact directory (or file inside it) beneath the repository,
/// following every existing component and rejecting any that leaves the canonical
/// repository root. Returns `None` when the path does not exist.
fn contained_artifact_path(
    root: &Path,
    key: &ArtifactKey,
    file_name: Option<&str>,
) -> Result<Option<PathBuf>> {
    let canonical_root = match root.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("cannot access local Maven repository"),
    };
    let mut current = canonical_root.clone();
    let components = key
        .group_id
        .split('.')
        .chain([key.artifact_id.as_str(), key.version.as_str()])
        .chain(file_name);
    for component in components {
        let next = current.join(component);
        let metadata = match next.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => bail!("cannot inspect local artifact path"),
        };
        match next.canonicalize() {
            Ok(resolved) if resolved.starts_with(&canonical_root) => current = resolved,
            Ok(_) => return Err(ArtifactOutsideRepository.into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if metadata.file_type().is_symlink() {
                    ensure_dangling_symlink_contained(&next, &canonical_root)?;
                }
                return Ok(None);
            }
            Err(_) => bail!("cannot inspect local artifact path"),
        }
    }
    Ok(Some(current))
}

/// Maximum symlink hops followed for a dangling link, mirroring the usual OS limit.
const MAX_SYMLINK_HOPS: usize = 40;

/// Follows a dangling symlink chain to the deepest existing ancestor of its final
/// target and rejects it unless that ancestor resolves inside the repository, so an
/// outward link is never reported as an absent artifact.
fn ensure_dangling_symlink_contained(link: &Path, canonical_root: &Path) -> Result<()> {
    let mut link = link.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        let target = std::fs::read_link(&link)
            .map_err(|_| anyhow::anyhow!("cannot inspect local artifact path"))?;
        let target = link.parent().unwrap_or(canonical_root).join(target);
        let mut existing = target.as_path();
        let metadata = loop {
            match existing.symlink_metadata() {
                Ok(metadata) => break metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    existing = existing.parent().ok_or(ArtifactOutsideRepository)?;
                }
                Err(_) => bail!("cannot inspect local artifact path"),
            }
        };
        match existing.canonicalize() {
            Ok(resolved) if resolved.starts_with(canonical_root) => return Ok(()),
            Ok(_) => return Err(ArtifactOutsideRepository.into()),
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && metadata.file_type().is_symlink() =>
            {
                link = existing.to_path_buf();
            }
            Err(_) => bail!("cannot inspect local artifact path"),
        }
    }
    Err(ArtifactOutsideRepository.into())
}

pub fn validate_exact_coordinate(coordinate: &str) -> Result<()> {
    parse_exact_coordinate(coordinate).map(|_| ())
}

/// Resolves the unclassified binary JAR of one exact version below the repository,
/// applying the shared containment rule. A version without a regular JAR file is an
/// explicit error, never an empty API.
fn exact_binary_jar_path(root: &Path, key: &ArtifactKey) -> Result<PathBuf> {
    let jar_name = format!("{}-{}.jar", key.artifact_id, key.version);
    contained_artifact_path(root, key, Some(&jar_name))?
        .filter(|path| path.is_file())
        .with_context(|| {
            format!(
                "binary artifact version is not locally available: {}:{}:{}",
                key.group_id, key.artifact_id, key.version
            )
        })
}

/// Reads the public classes of one repository JAR through a single archive handle, under
/// the shared archive budget and the multi-release class selection of the index.
fn public_class_api(
    root: &Path,
    path: &Path,
    key: ArtifactKey,
    budget: &mut ArchiveBudget,
    max_source_bytes: usize,
) -> Result<BTreeMap<String, ClassDescription>> {
    // The ZIP reader accepts archives the preflight cannot verify (the indexer skips them),
    // so any preflight failure is an error: no archive is read outside the budget.
    preflight_archive(path, budget).map_err(|error| {
        if error.is::<ArchiveLimit>() {
            error
        } else {
            error.context("invalid ZIP/JAR")
        }
    })?;
    let mut archive = ClassArchive::open(path)?;
    let JarIndex {
        classes,
        entries,
        class_entries,
    } = read_jar_index_from(&mut archive)?;
    let jar = JarRecord {
        key,
        classifier: None,
        relative_path: relative_slash_path(root, path),
        path: path.to_owned(),
        classes,
        entries,
        class_entries,
        fact_strings: Vec::new(),
        class_facts: Vec::new(),
        providers: Vec::new(),
    };
    let mut api = BTreeMap::new();
    for class_name in &jar.classes {
        crate::blocking::checkpoint()?;
        let description = read_class_description_from(
            &mut archive,
            &jar,
            jar.summary(),
            class_name,
            max_source_bytes,
            false,
        )?;
        if description.visibility == "public" {
            api.insert(description.class_name.clone(), description);
        }
    }
    Ok(api)
}

fn load_jar_record(
    path: &Path,
    identity: JarIdentity,
    max_entry_bytes: usize,
) -> Option<JarRecord> {
    let JarIdentity {
        key,
        classifier,
        relative_path,
    } = identity;
    let JarIndex {
        classes,
        entries,
        class_entries,
    } = match read_jar_index(path) {
        Ok(content) => content,
        Err(error) => {
            tracing::warn!(file = %file_label(path), %error, "skipping unreadable jar");
            return None;
        }
    };
    let (class_facts, providers, fact_strings) = if classifier.as_deref() == Some("sources") {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        collect_jar_facts(path, &entries, &class_entries, max_entry_bytes)
    };
    Some(JarRecord {
        key,
        classifier,
        relative_path,
        path: path.to_owned(),
        classes,
        entries,
        class_entries,
        fact_strings,
        class_facts,
        providers,
    })
}

/// Logs and public errors name an archive by its file name only, never by its absolute host path.
fn file_label(path: &Path) -> std::borrow::Cow<'_, str> {
    path.file_name()
        .map_or(std::borrow::Cow::Borrowed("<unnamed>"), |name| {
            name.to_string_lossy()
        })
}

fn collect_repository_jars(root: &Path) -> Vec<PathBuf> {
    let mut jar_paths = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).into_iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(
                    error = %error.io_error().map_or_else(|| "symbolic link loop".to_owned(), ToString::to_string),
                    "unable to inspect repository entry"
                );
                continue;
            }
        };
        let path = entry.path();
        if !entry.file_type().is_file()
            || path.extension().and_then(|value| value.to_str()) != Some("jar")
        {
            continue;
        }
        jar_paths.push(path.to_owned());
    }
    jar_paths
}

fn repository_identity(root: &Path, path: &Path) -> Option<JarIdentity> {
    let Some((key, classifier)) = parse_coordinate(root, path) else {
        tracing::debug!(file = %file_label(path), "skipping jar outside Maven layout");
        return None;
    };
    Some(JarIdentity {
        key,
        classifier,
        relative_path: relative_slash_path(root, path),
    })
}

fn relative_slash_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn scoped_identity(
    repository_root: Option<&Path>,
    reactor: &ReactorLayout<'_>,
    path: &Path,
) -> Result<JarIdentity, UnindexedReason> {
    if let Some(repository_root) = repository_root
        && path.starts_with(repository_root)
    {
        return repository_identity(repository_root, path)
            .ok_or(UnindexedReason::NotMavenRepositoryLayout);
    }
    let Some(module_directory) = reactor.output_module(path) else {
        return Err(UnindexedReason::OutsideRepositoryAndReactor);
    };
    reactor_identity(reactor.root, module_directory, path)
        .ok_or(UnindexedReason::UnresolvedReactorCoordinate)
}

/// The project root and its reactor module directories, as discovered from the POMs.
#[derive(Debug, Clone, Copy)]
pub struct ReactorLayout<'a> {
    pub root: &'a Path,
    pub modules: &'a [PathBuf],
}

impl ReactorLayout<'_> {
    /// The innermost module whose `target` build directory contains `path`.
    fn output_module(&self, path: &Path) -> Option<&Path> {
        self.modules
            .iter()
            .filter(|module| {
                module.starts_with(self.root) && path.starts_with(module.join("target"))
            })
            .max_by_key(|module| module.components().count())
            .map(PathBuf::as_path)
    }
}

/// A project index together with the selected classpath entries it could not include.
#[derive(Debug)]
pub struct ScopedIndex {
    pub index: MavenIndex,
    pub unindexed: Vec<UnindexedClasspathEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnindexedClasspathEntry {
    pub path: PathBuf,
    pub reason: UnindexedReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum UnindexedReason {
    /// The selected file or directory does not exist.
    Missing,
    /// Below the local repository but not at an `group/artifact/version/file` location.
    NotMavenRepositoryLayout,
    /// Neither below the local repository nor below a reactor module build directory.
    OutsideRepositoryAndReactor,
    /// A reactor output whose module groupId, artifactId or version cannot be resolved.
    UnresolvedReactorCoordinate,
}

impl UnindexedReason {
    pub fn description(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::NotMavenRepositoryLayout => "not in Maven repository layout",
            Self::OutsideRepositoryAndReactor => {
                "outside the local repository and the reactor build directories"
            }
            Self::UnresolvedReactorCoordinate => {
                "reactor output without resolvable module coordinates"
            }
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ReactorPom {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
    parent: Option<ReactorPomParent>,
    #[serde(default)]
    properties: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ReactorPomParent {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
    relative_path: Option<String>,
}

const MAX_REACTOR_PARENT_DEPTH: usize = 16;
const MAX_INTERPOLATION_DEPTH: usize = 8;

fn read_reactor_pom(path: &Path) -> Option<ReactorPom> {
    let xml = crate::config::read_bounded_xml(path).ok()?;
    quick_xml::de::from_str(&xml).ok()
}

/// Properties visible to a module POM: its own over those of its parents inside the reactor.
fn inherited_properties(
    reactor_root: &Path,
    module_pom: &Path,
    pom: &ReactorPom,
) -> BTreeMap<String, String> {
    let mut chain = Vec::new();
    let mut current = module_pom.to_owned();
    let mut parent = pom
        .parent
        .as_ref()
        .map(|parent| parent.relative_path.clone());
    while let Some(relative_path) = parent
        && chain.len() < MAX_REACTOR_PARENT_DEPTH
    {
        let relative_path = relative_path.unwrap_or_else(|| "../pom.xml".to_owned());
        if relative_path.trim().is_empty() {
            break;
        }
        let Some(directory) = current.parent() else {
            break;
        };
        let mut candidate = directory.join(relative_path.trim());
        if candidate.is_dir() {
            candidate.push("pom.xml");
        }
        let Ok(candidate) = candidate.canonicalize() else {
            break;
        };
        if !candidate.starts_with(reactor_root) || candidate == current {
            break;
        }
        let Some(parent_pom) = read_reactor_pom(&candidate) else {
            break;
        };
        parent = parent_pom
            .parent
            .as_ref()
            .map(|parent| parent.relative_path.clone());
        chain.push(parent_pom.properties);
        current = candidate;
    }
    let mut properties = BTreeMap::new();
    for inherited in chain.into_iter().rev() {
        properties.extend(inherited);
    }
    properties.extend(pom.properties.clone());
    properties
}

/// `-Dname=value` user properties from `.mvn/maven.config`; they override POM properties.
fn maven_config_properties(reactor_root: &Path) -> BTreeMap<String, String> {
    let Ok(config) = fs::read_to_string(reactor_root.join(".mvn/maven.config")) else {
        return BTreeMap::new();
    };
    let mut properties = BTreeMap::new();
    let mut tokens = config.split_whitespace();
    while let Some(token) = tokens.next() {
        let definition = match token {
            "-D" | "--define" => tokens.next(),
            _ => token
                .strip_prefix("--define=")
                .or_else(|| token.strip_prefix("-D")),
        };
        if let Some((name, value)) = definition.and_then(|definition| definition.split_once('=')) {
            properties.insert(name.to_owned(), value.to_owned());
        }
    }
    properties
}

fn interpolate(value: &str, lookup: &BTreeMap<String, String>, depth: usize) -> Option<String> {
    let mut resolved = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        let end = rest[start..].find('}')? + start;
        let name = &rest[start + 2..end];
        if depth == 0 {
            return None;
        }
        let replacement = interpolate(lookup.get(name)?, lookup, depth - 1)?;
        resolved.push_str(&rest[..start]);
        resolved.push_str(&replacement);
        rest = &rest[end + 1..];
    }
    resolved.push_str(rest);
    Some(resolved)
}

/// Resolves a module's coordinate the way Maven interpolates it for CI-friendly versions:
/// inherited groupId/version, POM properties of the module and its in-reactor parents, and
/// `.mvn/maven.config` user properties.
fn reactor_module_coordinate(reactor_root: &Path, module_directory: &Path) -> Option<ArtifactKey> {
    let module_pom = module_directory.join("pom.xml");
    let pom = read_reactor_pom(&module_pom)?;
    let parent = pom.parent.as_ref();
    let raw_group = pom
        .group_id
        .clone()
        .or_else(|| parent.and_then(|parent| parent.group_id.clone()))?;
    let raw_version = pom
        .version
        .clone()
        .or_else(|| parent.and_then(|parent| parent.version.clone()))?;
    let raw_artifact = pom.artifact_id.clone()?;
    let mut lookup = inherited_properties(reactor_root, &module_pom, &pom);
    for (name, value) in [
        ("groupId", &raw_group),
        ("artifactId", &raw_artifact),
        ("version", &raw_version),
    ] {
        for prefix in ["project", "pom"] {
            lookup.insert(format!("{prefix}.{name}"), value.clone());
        }
    }
    if let Some(parent) = parent {
        for (name, value) in [
            ("groupId", &parent.group_id),
            ("artifactId", &parent.artifact_id),
            ("version", &parent.version),
        ] {
            if let Some(value) = value {
                lookup.insert(format!("project.parent.{name}"), value.clone());
                lookup.insert(format!("parent.{name}"), value.clone());
            }
        }
    }
    lookup.extend(maven_config_properties(reactor_root));
    let resolve = |value: &str| {
        interpolate(value.trim(), &lookup, MAX_INTERPOLATION_DEPTH)
            .filter(|value| !value.is_empty() && !value.contains("${"))
    };
    Some(ArtifactKey {
        group_id: resolve(&raw_group)?,
        artifact_id: resolve(&raw_artifact)?,
        version: resolve(&raw_version)?,
    })
}

/// Classifiers recognized at the end of a custom `finalName` archive.
const OUTPUT_CLASSIFIER_SUFFIXES: [&str; 3] = ["test-sources", "sources", "tests"];

/// Identifies a reactor module output: a JAR (conventionally or `finalName`-named) or a
/// class directory below the module build directory. `test-classes` maps to `tests`.
fn reactor_identity(
    reactor_root: &Path,
    module_directory: &Path,
    path: &Path,
) -> Option<JarIdentity> {
    let key = reactor_module_coordinate(reactor_root, module_directory)?;
    let file_name = path.file_name()?.to_str()?;
    let classifier = if path.is_dir() {
        (file_name == "test-classes").then(|| "tests".to_owned())
    } else {
        let stem = file_name.strip_suffix(".jar").filter(|_| path.is_file())?;
        let conventional = format!("{}-{}", key.artifact_id, key.version);
        match stem.strip_prefix(&conventional) {
            Some("") => None,
            Some(suffix) if suffix.starts_with('-') => Some(suffix[1..].to_owned()),
            _ => OUTPUT_CLASSIFIER_SUFFIXES
                .iter()
                .find(|classifier| stem.ends_with(&format!("-{classifier}")))
                .map(|classifier| (*classifier).to_owned()),
        }
    };
    Some(JarIdentity {
        key,
        classifier,
        relative_path: relative_slash_path(reactor_root, path),
    })
}

pub(crate) fn source_jar_path(binary_jar: &Path) -> Option<PathBuf> {
    let file_name = binary_jar.file_name()?.to_str()?;
    if file_name.ends_with("-sources.jar") {
        return None;
    }
    let stem = file_name.strip_suffix(".jar")?;
    Some(binary_jar.with_file_name(format!("{stem}-sources.jar")))
}

fn collect_jar_facts(
    path: &Path,
    entries: &[String],
    class_entries: &BTreeMap<String, String>,
    max_entry_bytes: usize,
) -> (Vec<IndexedClassFacts>, Vec<IndexedProvider>, Vec<Arc<str>>) {
    let Ok(mut archive) = ClassArchive::open(path) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    let mut class_facts = Vec::new();
    let mut providers = BTreeSet::new();
    let mut strings = FactStringPool::default();
    for entry_path in class_entries.values() {
        if crate::blocking::is_cancelled() {
            break;
        }
        let Ok(Some(mut entry)) = archive.by_name(entry_path) else {
            continue;
        };
        if entry.size() > max_entry_bytes as u64 {
            continue;
        }
        let mut bytes = Vec::with_capacity(
            usize::try_from(entry.size())
                .unwrap_or(max_entry_bytes)
                .min(max_entry_bytes),
        );
        if entry.read_to_end(&mut bytes).is_err() {
            continue;
        }
        drop(entry);
        let mut options = ParseOptions::default();
        options.parse_bytecode(false);
        let Ok(parsed) = cafebabe::parse_class_with_options(&bytes, &options) else {
            continue;
        };
        let source_class_name = binary_name(&parsed.this_class);
        let source_class = strings.intern(&source_class_name);
        if source_class_name == "module-info" {
            collect_module_providers(&parsed.attributes, entry_path, &mut providers);
            continue;
        }
        let mut parents = Vec::new();
        if let Some(super_class) = &parsed.super_class {
            parents.push(IndexedTypeParent {
                parent: strings.intern(&binary_name(super_class)),
                relation: TypeRelation::Extends,
            });
        }
        for interface in &parsed.interfaces {
            parents.push(IndexedTypeParent {
                parent: strings.intern(&binary_name(interface)),
                relation: TypeRelation::Implements,
            });
        }
        parents.sort_unstable();
        parents.dedup();
        let mut references = Vec::new();
        for item in parsed.constantpool_iter() {
            if let Some(reference) = indexed_reference(&source_class_name, item, &mut strings) {
                references.push(reference);
            }
        }
        references.sort_unstable();
        references.dedup();
        if !parents.is_empty() || !references.is_empty() {
            class_facts.push(IndexedClassFacts {
                source_class,
                parents,
                references,
            });
        }
    }
    for entry_path in entries.iter().filter(|entry| is_provider_descriptor(entry)) {
        let Ok(Some(mut entry)) = archive.by_name(entry_path) else {
            continue;
        };
        if entry.size() > max_entry_bytes as u64 {
            continue;
        }
        let mut text = String::new();
        if entry
            .by_ref()
            .take(max_entry_bytes as u64)
            .read_to_string(&mut text)
            .is_ok()
        {
            collect_text_providers(entry_path, &text, &mut providers);
        }
    }
    (
        class_facts,
        providers.into_iter().collect(),
        strings.into_values(),
    )
}

/// Selects the one effective classfile entry per logical class path.
///
/// Every classfile-based tool shares this rule. Base entries are always
/// candidates. When the manifest declares `Multi-Release: true`, entries under
/// `META-INF/versions/<N>/` with `N >= 9` also compete and the highest `N` wins;
/// the server does not target a specific Java runtime. Without the flag every
/// versioned entry is ignored, as the JVM does, so a class that exists only under
/// `META-INF/versions/` is not indexed. Entries whose version directory is not a
/// canonical decimal number (digits only, no sign or leading zero) or is `< 9`
/// are never selected, because the JVM never looks them up.
fn effective_class_entries(entries: &[String], multi_release: bool) -> BTreeMap<String, String> {
    let mut selected = BTreeMap::<String, (u32, String)>::new();
    for entry in entries.iter().filter(|entry| entry.ends_with(".class")) {
        let (version, logical) = if let Some(rest) = entry.strip_prefix(VERSIONED_ENTRY_PREFIX) {
            if !multi_release {
                continue;
            }
            let Some((version, logical)) = rest.split_once('/') else {
                continue;
            };
            if version.is_empty()
                || version.starts_with('0')
                || !version.bytes().all(|byte| byte.is_ascii_digit())
            {
                continue;
            }
            let Ok(version) = version.parse::<u32>() else {
                continue;
            };
            if version < MIN_MULTI_RELEASE_VERSION {
                continue;
            }
            (version, logical)
        } else {
            (0, entry.as_str())
        };
        let candidate = selected
            .entry(logical.to_owned())
            .or_insert_with(|| (version, entry.clone()));
        if version > candidate.0 {
            *candidate = (version, entry.clone());
        }
    }
    selected
        .into_iter()
        .map(|(logical, (_, entry))| (logical, entry))
        .collect()
}

fn manifest_lines(manifest: &str) -> impl Iterator<Item = &str> {
    let mut rest = manifest;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let Some(end) = rest.find(['\r', '\n']) else {
            return Some(std::mem::take(&mut rest));
        };
        let line = &rest[..end];
        let separator = if rest[end..].starts_with("\r\n") {
            2
        } else {
            1
        };
        rest = &rest[end + separator..];
        Some(line)
    })
}

const VERSIONED_ENTRY_PREFIX: &str = "META-INF/versions/";
const MIN_MULTI_RELEASE_VERSION: u32 = 9;
const MAX_MANIFEST_BYTES: u64 = 1_048_576;

/// Reads the main manifest section; lines end with CRLF, LF, or a CR not followed
/// by LF, matching the JAR specification and the JDK manifest parser.
fn manifest_declares_multi_release(manifest: &str) -> bool {
    let mut unfolded = Vec::<String>::new();
    for line in manifest_lines(manifest) {
        if line.is_empty() {
            break;
        }
        match (line.strip_prefix(' '), unfolded.last_mut()) {
            (Some(continuation), Some(previous)) => previous.push_str(continuation),
            _ => unfolded.push(line.to_owned()),
        }
    }
    unfolded.iter().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("Multi-Release")
                && value.trim().eq_ignore_ascii_case("true")
        })
    })
}

fn indexed_reference(
    source_class: &str,
    item: ConstantPoolItem<'_>,
    strings: &mut FactStringPool,
) -> Option<IndexedReference> {
    let (target_owner, target_name, target_descriptor, kind) = match item {
        ConstantPoolItem::ClassInfo(class_name) => (
            normalize_constant_class(&class_name)?,
            None,
            None,
            ClassReferenceKind::Class,
        ),
        ConstantPoolItem::FieldRef(reference) => (
            binary_name(&reference.class_name),
            Some(reference.name_and_type.name.into_owned()),
            Some(reference.name_and_type.descriptor.into_owned()),
            ClassReferenceKind::Field,
        ),
        ConstantPoolItem::MethodRef(reference) => (
            binary_name(&reference.class_name),
            Some(reference.name_and_type.name.into_owned()),
            Some(reference.name_and_type.descriptor.into_owned()),
            ClassReferenceKind::Method,
        ),
        ConstantPoolItem::InterfaceMethodRef(reference) => (
            binary_name(&reference.class_name),
            Some(reference.name_and_type.name.into_owned()),
            Some(reference.name_and_type.descriptor.into_owned()),
            ClassReferenceKind::InterfaceMethod,
        ),
        _ => return None,
    };
    if kind == ClassReferenceKind::Class && target_owner == source_class {
        return None;
    }
    Some(IndexedReference {
        target_owner: strings.intern(&target_owner),
        target_name: target_name.map(|name| strings.intern(&name)),
        target_descriptor: target_descriptor.map(|descriptor| strings.intern(&descriptor)),
        kind,
    })
}

fn normalize_constant_class(class_name: &str) -> Option<String> {
    if class_name.starts_with('[') {
        let class = class_name
            .trim_start_matches('[')
            .strip_prefix('L')?
            .strip_suffix(';')?;
        Some(binary_name(class))
    } else {
        Some(binary_name(class_name))
    }
}

fn collect_module_providers(
    attributes: &[AttributeInfo<'_>],
    entry: &str,
    providers: &mut BTreeSet<IndexedProvider>,
) {
    for module in attributes
        .iter()
        .filter_map(|attribute| match &attribute.data {
            AttributeData::Module(module) => Some(module),
            _ => None,
        })
    {
        for service in &module.uses {
            providers.insert(IndexedProvider {
                service: binary_name(service),
                provider: None,
                descriptor_kind: ProviderDescriptorKind::ModuleUses,
                entry: entry.to_owned(),
            });
        }
        for provided in &module.provides {
            for provider in &provided.provides_with {
                providers.insert(IndexedProvider {
                    service: binary_name(&provided.service_interface_name),
                    provider: Some(binary_name(provider)),
                    descriptor_kind: ProviderDescriptorKind::ModuleProvides,
                    entry: entry.to_owned(),
                });
            }
        }
    }
}

fn is_provider_descriptor(entry: &str) -> bool {
    entry.starts_with("META-INF/services/")
        || entry == "META-INF/spring.factories"
        || (entry.starts_with("META-INF/spring/") && entry.ends_with(".imports"))
}

fn collect_text_providers(entry: &str, text: &str, providers: &mut BTreeSet<IndexedProvider>) {
    if let Some(service) = entry.strip_prefix("META-INF/services/") {
        collect_line_providers(
            service,
            text,
            ProviderDescriptorKind::ServiceLoader,
            entry,
            providers,
        );
    } else if entry == "META-INF/spring.factories" {
        collect_spring_factories(text, entry, providers);
    } else if let Some(service) = entry
        .strip_prefix("META-INF/spring/")
        .and_then(|name| name.strip_suffix(".imports"))
    {
        collect_line_providers(
            service,
            text,
            ProviderDescriptorKind::SpringImports,
            entry,
            providers,
        );
    }
}

fn collect_line_providers(
    service: &str,
    text: &str,
    descriptor_kind: ProviderDescriptorKind,
    entry: &str,
    providers: &mut BTreeSet<IndexedProvider>,
) {
    for provider in text.lines().filter_map(clean_descriptor_line) {
        providers.insert(IndexedProvider {
            service: service.to_owned(),
            provider: Some(provider.to_owned()),
            descriptor_kind,
            entry: entry.to_owned(),
        });
    }
}

fn clean_descriptor_line(line: &str) -> Option<&str> {
    let value = line.split('#').next()?.trim();
    (!value.is_empty() && !value.chars().any(char::is_whitespace)).then_some(value)
}

fn collect_spring_factories(text: &str, entry: &str, providers: &mut BTreeSet<IndexedProvider>) {
    let mut logical = String::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        logical.push_str(line.trim_end_matches('\\'));
        if line.ends_with('\\') {
            continue;
        }
        if let Some((service, values)) = logical.split_once('=') {
            let service = service.trim();
            for provider in values
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty() && !value.chars().any(char::is_whitespace))
            {
                providers.insert(IndexedProvider {
                    service: service.to_owned(),
                    provider: Some(provider.to_owned()),
                    descriptor_kind: ProviderDescriptorKind::SpringFactories,
                    entry: entry.to_owned(),
                });
            }
        }
        logical.clear();
    }
}

fn parse_coordinate(root: &Path, path: &Path) -> Option<(ArtifactKey, Option<String>)> {
    let relative = path.strip_prefix(root).ok()?;
    let components = relative
        .components()
        .map(|component| component.as_os_str().to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    if components.len() < 4 {
        return None;
    }
    let file_name = components.last()?.strip_suffix(".jar")?;
    let version = components.get(components.len() - 2)?.clone();
    let artifact_id = components.get(components.len() - 3)?.clone();
    let group_id = components[..components.len() - 3].join(".");
    let prefix = format!("{artifact_id}-{version}");
    let suffix = file_name.strip_prefix(&prefix)?;
    let classifier = if suffix.is_empty() {
        None
    } else {
        Some(suffix.strip_prefix('-')?.to_owned())
    };
    Some((
        ArtifactKey {
            group_id,
            artifact_id,
            version,
        },
        classifier,
    ))
}

#[derive(Debug)]
struct ArchiveLimit(&'static str);
impl std::fmt::Display for ArchiveLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "archive/index resource limit exceeded: {}", self.0)
    }
}
impl std::error::Error for ArchiveLimit {}

struct ArchiveBudget {
    entries: usize,
    names: usize,
    jar_entries: usize,
}
impl ArchiveBudget {
    fn configured() -> Self {
        Self {
            entries: crate::config::max_index_entries(),
            names: crate::config::max_index_name_bytes(),
            jar_entries: crate::config::max_jar_entries(),
        }
    }
    fn reserve(&mut self, name_bytes: usize) -> Result<()> {
        self.entries = self
            .entries
            .checked_sub(1)
            .ok_or(ArchiveLimit("MAX_INDEX_ENTRIES"))?;
        self.names = self
            .names
            .checked_sub(name_bytes)
            .ok_or(ArchiveLimit("MAX_INDEX_NAME_BYTES"))?;
        Ok(())
    }
}

const MAX_CENTRAL_DIRECTORY_BYTES: u64 = 67_108_864;

// Only a 64 KiB tail and one fixed-size central header are held in memory.
// Count actual headers too: a forged EOCD count cannot bypass the budget.
fn preflight_archive(path: &Path, budget: &mut ArchiveBudget) -> Result<()> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let tail_size = length.min(65_557) as usize;
    file.seek(SeekFrom::End(-(tail_size as i64)))?;
    let mut tail = vec![0; tail_size];
    file.read_exact(&mut tail)?;
    let end = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&i| {
            tail[i..i + 4] == [0x50, 0x4b, 0x05, 0x06]
                && i + 22 + usize::from(u16::from_le_bytes([tail[i + 20], tail[i + 21]]))
                    == tail.len()
        })
        .context("ZIP end of central directory missing")?;
    let u16_at = |i| u16::from_le_bytes([tail[i], tail[i + 1]]);
    let u32_at = |i| u32::from_le_bytes([tail[i], tail[i + 1], tail[i + 2], tail[i + 3]]);
    if u16_at(end + 4) != 0 || u16_at(end + 6) != 0 {
        bail!("multi-disk ZIP is unsupported");
    }
    let mut count = u64::from(u16_at(end + 10));
    let mut size = u64::from(u32_at(end + 12));
    let mut central_end = length - tail_size as u64 + end as u64;
    if count == 65_535 || size == u64::from(u32::MAX) || u32_at(end + 16) == u32::MAX {
        let locator_pos = central_end
            .checked_sub(20)
            .context("ZIP64 locator missing")?;
        file.seek(SeekFrom::Start(locator_pos))?;
        let mut locator = [0; 20];
        file.read_exact(&mut locator)?;
        if locator[..4] != [0x50, 0x4b, 0x06, 0x07] {
            bail!("invalid ZIP64 locator");
        }
        let zip64_offset = u64::from_le_bytes(locator[8..16].try_into()?);
        file.seek(SeekFrom::Start(zip64_offset))?;
        let mut header = [0; 56];
        file.read_exact(&mut header)?;
        if header[..4] != [0x50, 0x4b, 0x06, 0x06] {
            bail!("invalid ZIP64 end record");
        }
        let record_bytes = u64::from_le_bytes(header[4..12].try_into()?);
        if record_bytes > MAX_CENTRAL_DIRECTORY_BYTES {
            return Err(ArchiveLimit("MAX_CENTRAL_DIRECTORY_BYTES").into());
        }
        count = u64::from_le_bytes(header[32..40].try_into()?);
        size = u64::from_le_bytes(header[40..48].try_into()?);
        central_end = zip64_offset;
    }
    if count > budget.jar_entries as u64 {
        return Err(ArchiveLimit("MAX_JAR_ENTRIES").into());
    }
    if count > budget.entries as u64 {
        return Err(ArchiveLimit("MAX_INDEX_ENTRIES").into());
    }
    if size > MAX_CENTRAL_DIRECTORY_BYTES {
        return Err(ArchiveLimit("MAX_CENTRAL_DIRECTORY_BYTES").into());
    }
    let start = central_end
        .checked_sub(size)
        .context("invalid central directory size")?;
    file.seek(SeekFrom::Start(start))?;
    let (mut consumed, mut entries) = (0u64, 0usize);
    while consumed < size {
        if size - consumed < 46 {
            bail!("truncated central directory");
        }
        let mut header = [0; 46];
        file.read_exact(&mut header)?;
        if header[..4] != [0x50, 0x4b, 0x01, 0x02] {
            bail!("invalid central directory entry");
        }
        entries += 1;
        if entries > budget.jar_entries {
            return Err(ArchiveLimit("MAX_JAR_ENTRIES").into());
        }
        let field = |i| usize::from(u16::from_le_bytes([header[i], header[i + 1]]));
        let names = field(28);
        if names > crate::config::MAX_ENTRY_NAME_BYTES {
            return Err(ArchiveLimit("MAX_ENTRY_NAME_BYTES").into());
        }
        budget.reserve(names)?;
        let variable_bytes = names + field(30) + field(32);
        consumed += 46 + variable_bytes as u64;
        if consumed > size {
            bail!("central entry exceeds directory size");
        }
        file.seek(SeekFrom::Current(variable_bytes as i64))?;
    }
    if entries as u64 != count {
        bail!("central directory entry count mismatch");
    }
    Ok(())
}

/// Groups every child-to-parent fact by its ASCII-lowercased parent name. Each group is sorted
/// by child name, JAR path, and relation (stable, so equal keys keep JAR and class order),
/// which is the order a hierarchy search reports children in.
fn build_children_by_parent(jars: &[JarRecord]) -> HashMap<String, Vec<HierarchyEdge>> {
    let mut children: HashMap<String, Vec<HierarchyEdge>> = HashMap::new();
    for (jar_index, jar) in jars.iter().enumerate() {
        for (class_index, class) in jar.class_facts.iter().enumerate() {
            for (parent_index, fact) in class.parents.iter().enumerate() {
                children
                    .entry(jar.fact_string(fact.parent).to_ascii_lowercase())
                    .or_default()
                    .push(HierarchyEdge {
                        jar: jar_index,
                        class: class_index,
                        parent: parent_index,
                    });
            }
        }
    }
    let sort_key = |edge: &HierarchyEdge| {
        let jar = &jars[edge.jar];
        let class = &jar.class_facts[edge.class];
        (
            jar.fact_string(class.source_class),
            jar.relative_path.as_str(),
            class.parents[edge.parent].relation,
        )
    };
    for edges in children.values_mut() {
        edges.sort_by_key(sort_key);
    }
    children
}

/// A class archive: a JAR or a reactor output directory such as `target/classes`. Entry
/// names always use `/` and are relative to the archive root.
enum ClassArchive {
    Zip(ZipArchive<File>),
    Directory(PathBuf),
}

#[cfg(test)]
thread_local! {
    /// Number of `ClassArchive::open` calls on this thread, for open-count regressions.
    static ARCHIVE_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
thread_local! {
    /// Number of hierarchy edges a search examined on this thread, for scan-free regressions.
    static HIERARCHY_EDGES_EXAMINED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

enum ArchiveEntry<'a> {
    Zip(zip::read::ZipFile<'a, File>),
    File { file: File, size: u64 },
}

impl ArchiveEntry<'_> {
    fn size(&self) -> u64 {
        match self {
            Self::Zip(entry) => entry.size(),
            Self::File { size, .. } => *size,
        }
    }
}

impl Read for ArchiveEntry<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Zip(entry) => entry.read(buffer),
            Self::File { file, .. } => file.read(buffer),
        }
    }
}

impl ClassArchive {
    fn open(path: &Path) -> Result<Self> {
        #[cfg(test)]
        ARCHIVE_OPENS.with(|opens| opens.set(opens.get() + 1));
        if path.is_dir() {
            let root = path
                .canonicalize()
                .with_context(|| format!("cannot open class directory {}", file_label(path)))?;
            return Ok(Self::Directory(root));
        }
        let file = File::open(path).with_context(|| format!("cannot open {}", file_label(path)))?;
        let archive = ZipArchive::new(file)
            .with_context(|| format!("invalid ZIP/JAR: {}", file_label(path)))?;
        Ok(Self::Zip(archive))
    }

    /// Sorted names of the regular files in the archive. Directories and, for class
    /// directories, symbolic links are not entries.
    fn file_names(&mut self) -> Result<Vec<String>> {
        let mut names = match self {
            Self::Zip(archive) => {
                let mut names = Vec::with_capacity(archive.len());
                for index in 0..archive.len() {
                    let entry = archive.by_index_raw(index)?;
                    if entry.is_dir() {
                        continue;
                    }
                    let name = entry.name().context("cannot decode JAR entry name")?;
                    names.push(name.replace('\\', "/"));
                }
                names
            }
            Self::Directory(root) => directory_file_names(root)?,
        };
        names.sort();
        Ok(names)
    }

    /// Opens a regular file entry; `None` when the archive has no such file.
    fn by_name(&mut self, name: &str) -> Result<Option<ArchiveEntry<'_>>> {
        match self {
            Self::Zip(archive) => match archive.by_name(name) {
                Ok(entry) if entry.is_dir() => Ok(None),
                Ok(entry) => Ok(Some(ArchiveEntry::Zip(entry))),
                Err(zip::result::ZipError::FileNotFound) => Ok(None),
                Err(error) => Err(error.into()),
            },
            Self::Directory(root) => {
                let Some(path) = directory_entry_path(root, name) else {
                    return Ok(None);
                };
                let file = File::open(&path)
                    .with_context(|| format!("cannot open class directory entry {name}"))?;
                let size = file.metadata()?.len();
                Ok(Some(ArchiveEntry::File { file, size }))
            }
        }
    }

    /// Opens an entry the index recorded; its absence means the archive changed.
    fn indexed_entry(&mut self, name: &str) -> Result<ArchiveEntry<'_>> {
        self.by_name(name)?
            .with_context(|| format!("indexed entry {name} is no longer present"))
    }
}

fn directory_file_names(root: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in WalkDir::new(root).follow_links(false).min_depth(1) {
        let entry = entry.context("cannot read class directory")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .context("class directory entry escaped its root")?;
        let name = relative
            .to_str()
            .context("cannot decode class directory entry name")?;
        names.push(name.replace('\\', "/"));
    }
    Ok(names)
}

/// Resolves an entry name inside a class directory without leaving it: every component must
/// be a plain name, and the resolved file must still be a regular file below the root.
fn directory_entry_path(root: &Path, name: &str) -> Option<PathBuf> {
    let relative = Path::new(name);
    if name.is_empty()
        || !relative
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    let path = root.join(relative);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let canonical = path.canonicalize().ok()?;
    canonical.starts_with(root).then_some(canonical)
}

/// Applies the archive budget to a class directory before its names are collected.
fn preflight_directory(path: &Path, budget: &mut ArchiveBudget) -> Result<()> {
    let mut entries = 0usize;
    for entry in WalkDir::new(path).follow_links(false).min_depth(1) {
        let entry = entry.context("cannot read class directory")?;
        if !entry.file_type().is_file() {
            continue;
        }
        entries += 1;
        if entries > budget.jar_entries {
            return Err(ArchiveLimit("MAX_JAR_ENTRIES").into());
        }
        let name_bytes = entry
            .path()
            .strip_prefix(path)
            .map_or(0, |relative| relative.as_os_str().len());
        if name_bytes > crate::config::MAX_ENTRY_NAME_BYTES {
            return Err(ArchiveLimit("MAX_ENTRY_NAME_BYTES").into());
        }
        budget.reserve(name_bytes)?;
    }
    Ok(())
}

struct JarIndex {
    classes: Vec<String>,
    entries: Vec<String>,
    class_entries: BTreeMap<String, String>,
}

fn read_jar_index(path: &Path) -> Result<JarIndex> {
    read_jar_index_from(&mut ClassArchive::open(path)?)
}

fn read_jar_index_from(archive: &mut ClassArchive) -> Result<JarIndex> {
    let entries = archive.file_names()?;
    let multi_release = read_multi_release_flag(archive, &entries);
    let class_entries = effective_class_entries(&entries, multi_release);
    let classes = class_entries
        .keys()
        .filter_map(|logical| class_name_from_entry(logical))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(JarIndex {
        classes,
        entries,
        class_entries,
    })
}

fn read_multi_release_flag(archive: &mut ClassArchive, entries: &[String]) -> bool {
    let Some(manifest_path) = entries
        .iter()
        .find(|entry| entry.eq_ignore_ascii_case("META-INF/MANIFEST.MF"))
    else {
        return false;
    };
    let Ok(Some(entry)) = archive.by_name(manifest_path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if entry
        .take(MAX_MANIFEST_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return false;
    }
    manifest_declares_multi_release(&String::from_utf8_lossy(&bytes))
}

/// Logical class name of an effective (already version-resolved) entry path.
fn class_name_from_entry(entry: &str) -> Option<String> {
    let class = entry.strip_suffix(".class")?;
    if class == "module-info" || class.ends_with("/module-info") || class.ends_with("package-info")
    {
        return None;
    }
    Some(class.replace('/', "."))
}

fn normalize_class_query(value: &str) -> String {
    value
        .trim()
        .trim_end_matches(".class")
        .replace(['/', '\\'], ".")
}

fn matches_selector(jar: &JarRecord, selector: &str) -> bool {
    let selector = selector.trim();
    jar.coordinate().eq_ignore_ascii_case(selector)
        || jar.relative_path.eq_ignore_ascii_case(selector)
        || jar
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case(selector))
}

fn read_source(
    path: &Path,
    class_name: &str,
    max_bytes: usize,
) -> Result<Option<(String, String, bool)>> {
    let mut archive = ZipArchive::new(File::open(path)?)?;
    let outer_class = class_name.split('$').next().unwrap_or(class_name);
    let expected_java = format!("{}.java", outer_class.replace('.', "/"));
    let expected_kotlin = format!("{}.kt", outer_class.replace('.', "/"));
    let mut selected = None;
    let mut first_error = None;
    for index in 0..archive.len() {
        let entry = match archive.by_index_raw(index) {
            Ok(entry) => entry,
            Err(error) => {
                first_error.get_or_insert(error);
                continue;
            }
        };
        let name = entry
            .name()
            .context("cannot decode source JAR entry name")?;
        let name = name.replace('\\', "/");
        if name == expected_java || name == expected_kotlin {
            selected = Some(index);
            break;
        }
    }
    let Some(index) = selected else {
        // An unreadable entry may have been the requested source, so do not
        // report it as a missing result.
        return match first_error {
            Some(error) => Err(error).context("cannot read source JAR entry"),
            None => Ok(None),
        };
    };
    let mut entry = archive.by_index(index)?;
    let name = entry
        .name()
        .context("cannot decode source JAR entry name")?
        .into_owned();
    let declared_size = usize::try_from(entry.size()).unwrap_or(usize::MAX);
    let mut bytes = Vec::with_capacity(declared_size.min(max_bytes));
    entry
        .by_ref()
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max_bytes;
    bytes.truncate(max_bytes);
    Ok(Some((
        name,
        String::from_utf8_lossy(&bytes).into_owned(),
        truncated,
    )))
}

struct BoundedEntryContent {
    kind: JarEntryContentKind,
    text: Option<String>,
    bytes: Option<Vec<u8>>,
    original_size: u64,
    truncated: bool,
}

fn read_jar_entry(
    path: &Path,
    entry_path: &str,
    max_bytes: usize,
) -> Result<Option<BoundedEntryContent>> {
    let mut archive = ClassArchive::open(path).with_context(|| {
        format!(
            "indexed JAR is no longer a valid ZIP or class directory: {}",
            file_label(path)
        )
    })?;
    let Some(mut entry) = archive.by_name(entry_path)? else {
        return Ok(None);
    };

    let original_size = entry.size();
    let mut bytes = Vec::with_capacity(
        usize::try_from(original_size)
            .unwrap_or(usize::MAX)
            .min(max_bytes),
    );
    entry
        .by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read JAR entry {entry_path}"))?;
    let truncated = bytes.len() > max_bytes;
    bytes.truncate(max_bytes);

    let (kind, text, binary) = match String::from_utf8(bytes) {
        Ok(text) => (JarEntryContentKind::Text, Some(text), None),
        Err(error) if truncated && error.utf8_error().error_len().is_none() => {
            let valid_length = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid_length);
            let text = String::from_utf8(bytes).context("validated UTF-8 prefix became invalid")?;
            (JarEntryContentKind::Text, Some(text), None)
        }
        Err(error) => (JarEntryContentKind::Binary, None, Some(error.into_bytes())),
    };

    Ok(Some(BoundedEntryContent {
        kind,
        text,
        bytes: binary,
        original_size,
        truncated,
    }))
}

fn normalize_member_name(value: &str) -> String {
    let value = value.trim();
    value
        .rsplit_once('#')
        .or_else(|| value.rsplit_once('.'))
        .map_or(value, |(_, name)| name)
        .to_owned()
}

/// A classfile declaration with whether the compiler generated it (a bridge or synthetic
/// method), in which case it has no source declaration of its own.
struct ClassfileDeclaration {
    candidate: DeclarationCandidate,
    is_compiler_generated: bool,
}

fn declaration_candidates(
    jar: &JarRecord,
    class_name: &str,
    member_name: Option<&str>,
    max_bytes: usize,
) -> Result<Vec<ClassfileDeclaration>> {
    let Some(member_name) = member_name else {
        return Ok(vec![ClassfileDeclaration {
            candidate: DeclarationCandidate {
                kind: DeclarationKind::Class,
                name: class_name.to_owned(),
                descriptor: None,
            },
            is_compiler_generated: false,
        }]);
    };
    let mut archive = ClassArchive::open(&jar.path)?;
    let entry_path = select_class_entry(jar, class_name)
        .with_context(|| format!("indexed class entry is missing for {class_name}"))?;
    let mut entry = archive.indexed_entry(entry_path)?;
    if entry.size() > max_bytes as u64 {
        bail!("class entry exceeds configured MAX_SOURCE_BYTES limit");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(max_bytes));
    entry.read_to_end(&mut bytes)?;
    let mut options = ParseOptions::default();
    options.parse_bytecode(false);
    let parsed = cafebabe::parse_class_with_options(&bytes, &options)
        .with_context(|| format!("cannot parse classfile for {class_name}"))?;
    let simple_class_name = class_name.rsplit(['.', '$']).next().unwrap_or(class_name);
    let mut candidates = Vec::new();
    candidates.extend(
        parsed
            .fields
            .iter()
            .filter(|field| field.name.as_ref() == member_name)
            .map(|field| ClassfileDeclaration {
                candidate: DeclarationCandidate {
                    kind: DeclarationKind::Field,
                    name: member_name.to_owned(),
                    descriptor: Some(field.descriptor.to_string()),
                },
                is_compiler_generated: false,
            }),
    );
    candidates.extend(
        parsed
            .methods
            .iter()
            .filter(|method| {
                method.name.as_ref() == member_name
                    || (method.name.as_ref() == "<init>" && member_name == simple_class_name)
            })
            .map(|method| ClassfileDeclaration {
                candidate: DeclarationCandidate {
                    kind: DeclarationKind::Method,
                    name: if method.name.as_ref() == "<init>" {
                        simple_class_name.to_owned()
                    } else {
                        member_name.to_owned()
                    },
                    descriptor: Some(method.descriptor.to_string()),
                },
                is_compiler_generated: method
                    .access_flags
                    .intersects(MethodAccessFlags::BRIDGE | MethodAccessFlags::SYNTHETIC),
            }),
    );
    Ok(candidates)
}

/// The source file context shared by the declarations located in it.
struct LocatedSource<'a> {
    class_name: &'a str,
    source_jar: JarSummary,
    entry: &'a str,
    truncated: bool,
}

impl LocatedSource<'_> {
    fn found(
        self,
        candidate: DeclarationCandidate,
        slice: declaration_source::SourceSlice,
    ) -> DeclarationSource {
        DeclarationSource {
            class_name: self.class_name.to_owned(),
            kind: candidate.kind,
            name: candidate.name,
            descriptor: candidate.descriptor,
            source_jar: self.source_jar,
            entry: self.entry.to_owned(),
            start_line: slice.start_line,
            end_line: slice.end_line,
            source: slice.source,
            truncated: self.truncated,
        }
    }

    fn unresolved(
        self,
        candidate: DeclarationCandidate,
        reason: UnresolvedDeclarationReason,
        candidate_lines: Vec<usize>,
    ) -> UnresolvedDeclaration {
        UnresolvedDeclaration {
            class_name: self.class_name.to_owned(),
            kind: candidate.kind,
            name: candidate.name,
            descriptor: candidate.descriptor,
            source_jar: self.source_jar,
            entry: self.entry.to_owned(),
            reason,
            candidate_lines,
            truncated: self.truncated,
        }
    }
}

fn read_class_description(
    jar: &JarRecord,
    jar_summary: JarSummary,
    class_name: &str,
    max_bytes: usize,
    public_only: bool,
) -> Result<ClassDescription> {
    let mut archive = ClassArchive::open(&jar.path)?;
    read_class_description_from(
        &mut archive,
        jar,
        jar_summary,
        class_name,
        max_bytes,
        public_only,
    )
}

/// Describes one class through an already opened archive of `jar`, so a caller that visits
/// many classes of the same JAR opens it and reads its central directory only once.
fn read_class_description_from(
    archive: &mut ClassArchive,
    jar: &JarRecord,
    jar_summary: JarSummary,
    class_name: &str,
    max_bytes: usize,
    public_only: bool,
) -> Result<ClassDescription> {
    let entry_path = select_class_entry(jar, class_name)
        .with_context(|| format!("indexed class entry is missing for {class_name}"))?;
    let mut entry = archive.indexed_entry(entry_path)?;
    if entry.size() > max_bytes as u64 {
        bail!("class entry exceeds configured MAX_SOURCE_BYTES limit");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(entry.size()).unwrap_or(max_bytes));
    entry.read_to_end(&mut bytes)?;

    let mut options = ParseOptions::default();
    options.parse_bytecode(false);
    let parsed = cafebabe::parse_class_with_options(&bytes, &options)
        .with_context(|| format!("cannot parse classfile for {class_name}"))?;

    let mut interfaces = parsed
        .interfaces
        .iter()
        .map(|interface| binary_name(interface))
        .collect::<Vec<_>>();
    interfaces.sort();
    let mut constructors = Vec::new();
    let mut methods = Vec::new();
    for method in &parsed.methods {
        if method.name.as_ref() == "<clinit>"
            || (public_only && !method.access_flags.contains(MethodAccessFlags::PUBLIC))
        {
            continue;
        }
        let description = ClassMemberDescription {
            name: if method.name.as_ref() == "<init>" {
                binary_name(&parsed.this_class)
                    .rsplit('.')
                    .next()
                    .unwrap_or("<init>")
                    .to_owned()
            } else {
                method.name.to_string()
            },
            descriptor: method.descriptor.to_string(),
            generic_signature: generic_signature(&method.attributes),
            visibility: member_visibility(method.access_flags.bits()).to_owned(),
            modifiers: method_modifiers(method.access_flags),
            annotations: annotation_names(&method.attributes),
        };
        if method.name.as_ref() == "<init>" {
            constructors.push(description);
        } else {
            methods.push(description);
        }
    }
    let mut fields = parsed
        .fields
        .iter()
        .filter(|field| !public_only || field.access_flags.contains(FieldAccessFlags::PUBLIC))
        .map(|field| ClassMemberDescription {
            name: field.name.to_string(),
            descriptor: field.descriptor.to_string(),
            generic_signature: generic_signature(&field.attributes),
            visibility: member_visibility(field.access_flags.bits()).to_owned(),
            modifiers: field_modifiers(field.access_flags),
            annotations: annotation_names(&field.attributes),
        })
        .collect::<Vec<_>>();
    constructors.sort_by(|left, right| left.descriptor.cmp(&right.descriptor));
    methods.sort_by(|left, right| {
        (&left.name, &left.descriptor).cmp(&(&right.name, &right.descriptor))
    });
    fields.sort_by(|left, right| {
        (&left.name, &left.descriptor).cmp(&(&right.name, &right.descriptor))
    });

    Ok(ClassDescription {
        class_name: binary_name(&parsed.this_class),
        jar: jar_summary,
        class_file_version: parsed.major_version,
        visibility: if parsed.access_flags.contains(ClassAccessFlags::PUBLIC) {
            "public"
        } else {
            "package_private"
        }
        .to_owned(),
        modifiers: class_modifiers(parsed.access_flags),
        generic_signature: generic_signature(&parsed.attributes),
        super_class: parsed.super_class.as_ref().map(|class| binary_name(class)),
        interfaces,
        constructors,
        methods,
        fields,
        annotations: annotation_names(&parsed.attributes),
    })
}

fn select_class_entry<'a>(jar: &'a JarRecord, class_name: &str) -> Option<&'a str> {
    let logical = format!("{}.class", class_name.replace('.', "/"));
    jar.class_entries.get(&logical).map(String::as_str)
}

fn binary_name(value: &str) -> String {
    value.replace('/', ".")
}

fn member_visibility(bits: u16) -> &'static str {
    if bits & 0x0001 != 0 {
        "public"
    } else if bits & 0x0004 != 0 {
        "protected"
    } else if bits & 0x0002 != 0 {
        "private"
    } else {
        "package_private"
    }
}

fn class_modifiers(flags: ClassAccessFlags) -> Vec<String> {
    modifier_names(&[
        (flags.contains(ClassAccessFlags::FINAL), "final"),
        (flags.contains(ClassAccessFlags::INTERFACE), "interface"),
        (flags.contains(ClassAccessFlags::ABSTRACT), "abstract"),
        (flags.contains(ClassAccessFlags::SYNTHETIC), "synthetic"),
        (flags.contains(ClassAccessFlags::ANNOTATION), "annotation"),
        (flags.contains(ClassAccessFlags::ENUM), "enum"),
    ])
}

fn method_modifiers(flags: MethodAccessFlags) -> Vec<String> {
    modifier_names(&[
        (flags.contains(MethodAccessFlags::STATIC), "static"),
        (flags.contains(MethodAccessFlags::FINAL), "final"),
        (
            flags.contains(MethodAccessFlags::SYNCHRONIZED),
            "synchronized",
        ),
        (flags.contains(MethodAccessFlags::BRIDGE), "bridge"),
        (flags.contains(MethodAccessFlags::VARARGS), "varargs"),
        (flags.contains(MethodAccessFlags::NATIVE), "native"),
        (flags.contains(MethodAccessFlags::ABSTRACT), "abstract"),
        (flags.contains(MethodAccessFlags::STRICT), "strict"),
        (flags.contains(MethodAccessFlags::SYNTHETIC), "synthetic"),
    ])
}

fn field_modifiers(flags: FieldAccessFlags) -> Vec<String> {
    modifier_names(&[
        (flags.contains(FieldAccessFlags::STATIC), "static"),
        (flags.contains(FieldAccessFlags::FINAL), "final"),
        (flags.contains(FieldAccessFlags::VOLATILE), "volatile"),
        (flags.contains(FieldAccessFlags::TRANSIENT), "transient"),
        (flags.contains(FieldAccessFlags::SYNTHETIC), "synthetic"),
        (flags.contains(FieldAccessFlags::ENUM), "enum"),
    ])
}

fn modifier_names(values: &[(bool, &str)]) -> Vec<String> {
    values
        .iter()
        .filter(|(enabled, _)| *enabled)
        .map(|(_, name)| (*name).to_owned())
        .collect()
}

fn generic_signature(attributes: &[AttributeInfo<'_>]) -> Option<String> {
    attributes
        .iter()
        .find_map(|attribute| match &attribute.data {
            AttributeData::Signature(signature) => Some(signature.to_string()),
            _ => None,
        })
}

fn annotation_names(attributes: &[AttributeInfo<'_>]) -> Vec<String> {
    let mut names = attributes
        .iter()
        .flat_map(|attribute| match &attribute.data {
            AttributeData::RuntimeVisibleAnnotations(annotations)
            | AttributeData::RuntimeInvisibleAnnotations(annotations) => annotations.as_slice(),
            _ => &[],
        })
        .map(annotation_name)
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    names
}

fn annotation_name(annotation: &Annotation<'_>) -> String {
    annotation
        .type_descriptor
        .to_string()
        .trim_start_matches('L')
        .trim_end_matches(';')
        .replace('/', ".")
}

fn is_checksum_file(file_name: &str) -> bool {
    [".md5", ".sha1", ".sha256", ".sha512"]
        .iter()
        .any(|suffix| file_name.ends_with(suffix))
}

fn collect_member_matches(
    description: &ClassDescription,
    query: &str,
    matches: &mut Vec<ClassMemberMatch>,
) {
    for method in description
        .constructors
        .iter()
        .chain(description.methods.iter())
    {
        if method.name.to_lowercase().contains(query) {
            matches.push(ClassMemberMatch {
                class_name: description.class_name.clone(),
                kind: ClassMemberMatchKind::Method,
                name: method.name.clone(),
                signature: method.descriptor.clone(),
                jar: description.jar.clone(),
            });
        }
        collect_annotation_matches(
            description,
            &method.annotations,
            &format!("method:{}{}", method.name, method.descriptor),
            query,
            matches,
        );
    }
    for field in &description.fields {
        if field.name.to_lowercase().contains(query) {
            matches.push(ClassMemberMatch {
                class_name: description.class_name.clone(),
                kind: ClassMemberMatchKind::Field,
                name: field.name.clone(),
                signature: field.descriptor.clone(),
                jar: description.jar.clone(),
            });
        }
        collect_annotation_matches(
            description,
            &field.annotations,
            &format!("field:{}:{}", field.name, field.descriptor),
            query,
            matches,
        );
    }
    collect_annotation_matches(
        description,
        &description.annotations,
        "class",
        query,
        matches,
    );
}

fn collect_annotation_matches(
    description: &ClassDescription,
    annotations: &[String],
    target: &str,
    query: &str,
    matches: &mut Vec<ClassMemberMatch>,
) {
    matches.extend(
        annotations
            .iter()
            .filter(|annotation| annotation.to_lowercase().contains(query))
            .map(|annotation| ClassMemberMatch {
                class_name: description.class_name.clone(),
                kind: ClassMemberMatchKind::Annotation,
                name: annotation.clone(),
                signature: target.to_owned(),
                jar: description.jar.clone(),
            }),
    );
}

fn api_members(description: &ClassDescription) -> BTreeSet<String> {
    let methods = description
        .constructors
        .iter()
        .chain(description.methods.iter())
        .filter(|member| matches!(member.visibility.as_str(), "public" | "protected"))
        .map(|member| format!("method:{}{}", member.name, member.descriptor));
    let fields = description
        .fields
        .iter()
        .filter(|member| matches!(member.visibility.as_str(), "public" | "protected"))
        .map(|member| format!("field:{}:{}", member.name, member.descriptor));
    methods.chain(fields).collect()
}

fn is_text_resource(entry: &str) -> bool {
    if entry == "META-INF/MANIFEST.MF" || entry.starts_with("META-INF/services/") {
        return true;
    }
    let extension = entry.rsplit_once('.').map(|(_, extension)| extension);
    matches!(
        extension,
        Some(
            "conf"
                | "config"
                | "factories"
                | "imports"
                | "json"
                | "list"
                | "properties"
                | "txt"
                | "xml"
                | "yaml"
                | "yml"
        )
    )
}

/// Length of `bytes` without a trailing UTF-8 sequence cut off by truncation;
/// invalid bytes elsewhere are kept for lossy decoding.
fn complete_utf8_prefix_len(bytes: &[u8]) -> usize {
    let mut offset = 0;
    loop {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(_) => return bytes.len(),
            Err(error) => match error.error_len() {
                Some(invalid_len) => offset += error.valid_up_to() + invalid_len,
                None => return offset + error.valid_up_to(),
            },
        }
    }
}

fn matching_context(text: &str, query: &str) -> Option<(usize, String)> {
    text.lines().enumerate().find_map(|(index, line)| {
        line.to_lowercase().contains(query).then(|| {
            let context = line.chars().take(240).collect::<String>();
            (index + 1, context)
        })
    })
}

fn artifact_file_status(
    key: &ArtifactKey,
    file_name: &str,
    path: &Path,
) -> Option<ArtifactFileStatus> {
    let (stem, extension) = file_name.rsplit_once('.')?;
    if !matches!(extension, "jar" | "pom" | "module" | "xml") {
        return None;
    }
    let expected_stem = format!("{}-{}", key.artifact_id, key.version);
    let classifier = stem
        .strip_prefix(&expected_stem)
        .and_then(|suffix| suffix.strip_prefix('-'))
        .filter(|suffix| !suffix.is_empty())
        .map(str::to_owned)
        .or_else(|| (stem != expected_stem).then(|| "snapshot_variant".to_owned()));
    let readable = (extension == "jar").then(|| {
        File::open(path)
            .ok()
            .and_then(|file| ZipArchive::new(file).ok())
            .is_some()
    });
    Some(ArtifactFileStatus {
        file_name: file_name.to_owned(),
        kind: extension.to_owned(),
        classifier,
        readable,
    })
}

#[cfg(test)]
#[path = "../tests/unit/index.rs"]
mod tests;
