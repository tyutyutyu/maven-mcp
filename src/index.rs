use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use cafebabe::{
    ClassAccessFlags, FieldAccessFlags, MethodAccessFlags, ParseOptions,
    attributes::{Annotation, AttributeData, AttributeInfo},
    constant_pool::ConstantPoolItem,
};
use rayon::prelude::*;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;
use zip::ZipArchive;

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
struct ArtifactKey {
    group_id: String,
    artifact_id: String,
    version: String,
}

#[derive(Debug, Clone)]
struct JarRecord {
    key: ArtifactKey,
    classifier: Option<String>,
    relative_path: String,
    path: PathBuf,
    classes: Vec<String>,
    entries: Vec<String>,
    type_facts: Vec<TypeFact>,
    references: Vec<IndexedReference>,
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

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DeclarationSourceLookup {
    pub results: Vec<DeclarationSource>,
    pub ambiguous_candidates: Vec<DeclarationCandidate>,
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
struct TypeFact {
    child: String,
    parent: String,
    relation: TypeRelation,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct IndexedReference {
    source_class: String,
    target_owner: String,
    target_name: Option<String>,
    target_descriptor: Option<String>,
    kind: ClassReferenceKind,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct IndexedProvider {
    service: String,
    provider: Option<String>,
    descriptor_kind: ProviderDescriptorKind,
    entry: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
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
    artifacts: BTreeMap<(String, String), BTreeSet<String>>,
    max_results: usize,
    max_source_bytes: usize,
}

impl MavenIndex {
    pub fn build(root: &Path, max_results: usize, max_source_bytes: usize) -> Result<Self> {
        let mut index = Self {
            root: root.to_owned(),
            jars: Vec::new(),
            source_jars: HashMap::new(),
            class_locations: HashMap::new(),
            artifacts: BTreeMap::new(),
            max_results,
            max_source_bytes,
        };

        let mut jar_paths = Vec::new();
        for entry in WalkDir::new(root).follow_links(false).into_iter() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!(%error, "unable to inspect repository entry");
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

        let mut records = jar_paths
            .par_iter()
            .filter_map(|path| load_jar_record(root, path, max_source_bytes))
            .collect::<Vec<_>>();
        records.sort_unstable_by(|left, right| left.relative_path.cmp(&right.relative_path));

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
        let key = parse_exact_coordinate(coordinate)?;
        let coordinate = format!("{}:{}:{}", key.group_id, key.artifact_id, key.version);
        let path = self
            .root
            .join(key.group_id.replace('.', "/"))
            .join(&key.artifact_id)
            .join(&key.version)
            .join(format!("{}-{}.pom", key.artifact_id, key.version));
        if !path.exists() {
            return Ok(PomDescriptorLookup {
                coordinate,
                found: false,
                descriptor: None,
            });
        }

        let xml = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read POM for {coordinate}"))?;
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
        let key = parse_exact_coordinate(coordinate)?;
        let coordinate = format!("{}:{}:{}", key.group_id, key.artifact_id, key.version);
        let directory = self
            .root
            .join(key.group_id.replace('.', "/"))
            .join(&key.artifact_id)
            .join(&key.version);
        if !directory.is_dir() {
            return Ok(ArtifactHealth {
                coordinate,
                found: false,
                snapshot: key.version.ends_with("-SNAPSHOT"),
                files: Vec::new(),
                checksums: Vec::new(),
                last_updated_markers: Vec::new(),
                repository_ids: Vec::new(),
            });
        }

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
            for class_name in &jar.classes {
                let description = match read_class_description(
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

    pub fn compare_artifact_api(
        &self,
        group_id: &str,
        artifact_id: &str,
        previous_version: &str,
        current_version: &str,
    ) -> Result<ArtifactApiDiff> {
        let previous_coordinate = format!("{group_id}:{artifact_id}:{previous_version}");
        let current_coordinate = format!("{group_id}:{artifact_id}:{current_version}");
        let previous = self.exact_binary_jar(&previous_coordinate)?;
        let current = self.exact_binary_jar(&current_coordinate)?;
        let previous_api = self.public_class_api(previous)?;
        let current_api = self.public_class_api(current)?;

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

    fn exact_binary_jar(&self, coordinate: &str) -> Result<&JarRecord> {
        validate_exact_coordinate(coordinate)?;
        self.jars
            .iter()
            .find(|jar| jar.classifier.is_none() && jar.coordinate() == coordinate)
            .with_context(|| {
                format!("binary artifact version is not locally available: {coordinate}")
            })
    }

    fn public_class_api(&self, jar: &JarRecord) -> Result<BTreeMap<String, ClassDescription>> {
        let mut api = BTreeMap::new();
        for class_name in &jar.classes {
            let description = read_class_description(
                jar,
                self.summary(jar),
                class_name,
                self.max_source_bytes,
                false,
            )?;
            if description.visibility == "public" {
                api.insert(description.class_name.clone(), description);
            }
        }
        Ok(api)
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
            let mut archive = ZipArchive::new(File::open(&jar.path)?)?;
            for entry_path in jar.entries.iter().filter(|entry| is_text_resource(entry)) {
                let mut entry = archive.by_name(entry_path)?;
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
        let mut queue = VecDeque::from([(root.clone(), vec![root.clone()], 0usize)]);
        let mut visited_edges = BTreeSet::new();
        let mut matches = Vec::new();
        while let Some((parent, path, depth)) = queue.pop_front() {
            let mut edges = self
                .jars
                .iter()
                .enumerate()
                .flat_map(|(jar_index, jar)| {
                    jar.type_facts
                        .iter()
                        .filter(|fact| fact.parent.eq_ignore_ascii_case(&parent))
                        .map(move |fact| (jar_index, fact))
                })
                .collect::<Vec<_>>();
            edges.sort_by(|(left_jar, left), (right_jar, right)| {
                (
                    &left.child,
                    &self.jars[*left_jar].relative_path,
                    left.relation,
                )
                    .cmp(&(
                        &right.child,
                        &self.jars[*right_jar].relative_path,
                        right.relation,
                    ))
            });
            for (jar_index, fact) in edges {
                let jar = &self.jars[jar_index];
                let edge_key = (
                    fact.child.to_lowercase(),
                    fact.parent.to_lowercase(),
                    jar.coordinate(),
                );
                if !visited_edges.insert(edge_key) || path.iter().any(|node| node == &fact.child) {
                    continue;
                }
                let mut child_path = path.clone();
                child_path.push(fact.child.clone());
                if jar_selector.is_none_or(|selector| matches_selector(jar, selector)) {
                    matches.push(TypeHierarchyMatch {
                        type_name: fact.child.clone(),
                        jar: self.summary(jar),
                        relation: fact.relation,
                        depth: depth + 1,
                        path: child_path.clone(),
                    });
                    if matches.len() == self.limit(limit) {
                        return matches;
                    }
                }
                if transitive {
                    queue.push_back((fact.child.clone(), child_path, depth + 1));
                }
            }
            if !transitive {
                break;
            }
        }
        matches
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
                bytes.truncate(self.max_source_bytes);
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
            });
        };
        let member_name = member_name.map(normalize_member_name);
        let mut results = Vec::new();
        let mut ambiguous_candidates = BTreeSet::new();
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
                .iter()
                .filter(|candidate| {
                    descriptor
                        .is_none_or(|expected| candidate.descriptor.as_deref() == Some(expected))
                })
                .cloned()
                .collect::<Vec<_>>();
            if member_name.is_some() && descriptor.is_none() && matching.len() > 1 {
                ambiguous_candidates.extend(matching);
                continue;
            }
            let source_jar = &self.jars[*source_index];
            let Some((entry, source, truncated)) =
                read_source(&source_jar.path, &class_name, self.max_source_bytes)?
            else {
                continue;
            };
            for candidate in matching {
                let Some(slice) = slice_declaration(
                    &source,
                    &class_name,
                    &candidate,
                    candidates
                        .iter()
                        .filter(|value| {
                            value.kind == candidate.kind && value.name == candidate.name
                        })
                        .position(|value| value == &candidate)
                        .unwrap_or(0),
                ) else {
                    continue;
                };
                results.push(DeclarationSource {
                    class_name: class_name.clone(),
                    kind: candidate.kind,
                    name: candidate.name,
                    descriptor: candidate.descriptor,
                    source_jar: self.summary(source_jar),
                    entry: entry.clone(),
                    start_line: slice.start_line,
                    end_line: slice.end_line,
                    source: slice.source,
                    truncated,
                });
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
        Ok(DeclarationSourceLookup {
            results,
            ambiguous_candidates: ambiguous_candidates.into_iter().collect(),
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
                jar.references
                    .iter()
                    .filter(|reference| match direction {
                        ReferenceDirection::Inbound => {
                            reference.target_owner.eq_ignore_ascii_case(&class_name)
                        }
                        ReferenceDirection::Outbound => {
                            reference.source_class.eq_ignore_ascii_case(&class_name)
                        }
                    })
                    .filter(|reference| kind.is_none_or(|expected| reference.kind == expected))
                    .filter(|reference| {
                        member_name.is_none_or(|expected| {
                            reference
                                .target_name
                                .as_deref()
                                .is_some_and(|name| name == expected)
                        })
                    })
                    .filter(|reference| {
                        descriptor.is_none_or(|expected| {
                            reference.target_descriptor.as_deref() == Some(expected)
                        })
                    })
                    .map(|reference| ClassReference {
                        source_class: reference.source_class.clone(),
                        source_jar: self.summary(jar),
                        target_owner: reference.target_owner.clone(),
                        target_name: reference.target_name.clone(),
                        target_descriptor: reference.target_descriptor.clone(),
                        kind: reference.kind,
                        target_artifacts: self.target_artifacts(&reference.target_owner),
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
        JarSummary {
            coordinate: jar.coordinate(),
            group_id: jar.key.group_id.clone(),
            artifact_id: jar.key.artifact_id.clone(),
            version: jar.key.version.clone(),
            classifier: jar.classifier.clone(),
            path: jar.relative_path.clone(),
            class_count: jar.classes.len(),
            entry_count: jar.entries.len(),
        }
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
            properties: raw.properties,
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

fn parse_exact_coordinate(coordinate: &str) -> Result<ArtifactKey> {
    let parts = coordinate.trim().split(':').collect::<Vec<_>>();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || part.contains(['/', '\\'])
                || *part == "."
                || *part == ".."
                || part.contains("..")
        })
    {
        bail!("coordinate must use exact groupId:artifactId:version form");
    }
    Ok(ArtifactKey {
        group_id: parts[0].to_owned(),
        artifact_id: parts[1].to_owned(),
        version: parts[2].to_owned(),
    })
}

pub fn validate_exact_coordinate(coordinate: &str) -> Result<()> {
    parse_exact_coordinate(coordinate).map(|_| ())
}

fn load_jar_record(root: &Path, path: &Path, max_entry_bytes: usize) -> Option<JarRecord> {
    let Some((key, classifier)) = parse_coordinate(root, path) else {
        tracing::debug!(path = %path.display(), "skipping jar outside Maven layout");
        return None;
    };
    let relative_path = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let (classes, entries) = match read_jar_index(path) {
        Ok(content) => content,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "skipping unreadable jar");
            return None;
        }
    };
    let (type_facts, references, providers) = if classifier.as_deref() == Some("sources") {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        collect_jar_facts(path, &entries, max_entry_bytes)
    };
    Some(JarRecord {
        key,
        classifier,
        relative_path,
        path: path.to_owned(),
        classes,
        entries,
        type_facts,
        references,
        providers,
    })
}

fn collect_jar_facts(
    path: &Path,
    entries: &[String],
    max_entry_bytes: usize,
) -> (Vec<TypeFact>, Vec<IndexedReference>, Vec<IndexedProvider>) {
    let Ok(file) = File::open(path) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    let Ok(mut archive) = ZipArchive::new(file) else {
        return (Vec::new(), Vec::new(), Vec::new());
    };
    let mut type_facts = BTreeSet::new();
    let mut references = BTreeSet::new();
    let mut providers = BTreeSet::new();
    let class_entries = effective_class_entries(entries);
    for entry_path in class_entries {
        let Ok(mut entry) = archive.by_name(&entry_path) else {
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
        let source_class = binary_name(&parsed.this_class);
        if source_class == "module-info" {
            collect_module_providers(&parsed.attributes, &entry_path, &mut providers);
            continue;
        }
        if let Some(super_class) = &parsed.super_class {
            type_facts.insert(TypeFact {
                child: source_class.clone(),
                parent: binary_name(super_class),
                relation: TypeRelation::Extends,
            });
        }
        for interface in &parsed.interfaces {
            type_facts.insert(TypeFact {
                child: source_class.clone(),
                parent: binary_name(interface),
                relation: TypeRelation::Implements,
            });
        }
        for item in parsed.constantpool_iter() {
            if let Some(reference) = indexed_reference(&source_class, item) {
                references.insert(reference);
            }
        }
    }
    for entry_path in entries.iter().filter(|entry| is_provider_descriptor(entry)) {
        let Ok(mut entry) = archive.by_name(entry_path) else {
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
        type_facts.into_iter().collect(),
        references.into_iter().collect(),
        providers.into_iter().collect(),
    )
}

fn effective_class_entries(entries: &[String]) -> Vec<String> {
    let mut selected = BTreeMap::<String, (u32, String)>::new();
    for entry in entries.iter().filter(|entry| entry.ends_with(".class")) {
        let (version, logical) = if let Some(rest) = entry.strip_prefix("META-INF/versions/") {
            let Some((version, logical)) = rest.split_once('/') else {
                continue;
            };
            let Ok(version) = version.parse::<u32>() else {
                continue;
            };
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
    selected.into_values().map(|(_, entry)| entry).collect()
}

fn indexed_reference(source_class: &str, item: ConstantPoolItem<'_>) -> Option<IndexedReference> {
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
        source_class: source_class.to_owned(),
        target_owner,
        target_name,
        target_descriptor,
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

fn read_jar_index(path: &Path) -> Result<(Vec<String>, Vec<String>)> {
    let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut archive =
        ZipArchive::new(file).with_context(|| format!("invalid ZIP/JAR: {}", path.display()))?;
    let mut classes = BTreeSet::new();
    let mut entries = Vec::with_capacity(archive.len());
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        if let Some(class_name) = class_name_from_entry(&name) {
            classes.insert(class_name);
        }
        entries.push(name);
    }
    entries.sort();
    Ok((classes.into_iter().collect(), entries))
}

fn class_name_from_entry(entry: &str) -> Option<String> {
    let mut entry = entry;
    if let Some(rest) = entry.strip_prefix("META-INF/versions/") {
        let (_, versioned_entry) = rest.split_once('/')?;
        entry = versioned_entry;
    }
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
    let selected = (0..archive.len()).find(|index| {
        archive
            .by_index_raw(*index)
            .map(|entry| entry.name() == expected_java || entry.name() == expected_kotlin)
            .unwrap_or(false)
    });
    let Some(index) = selected else {
        return Ok(None);
    };
    let mut entry = archive.by_index(index)?;
    let name = entry.name().to_owned();
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
    let file =
        File::open(path).with_context(|| format!("cannot open indexed JAR {}", path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("indexed JAR is no longer a valid ZIP: {}", path.display()))?;
    let mut entry = match archive.by_name(entry_path) {
        Ok(entry) => entry,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if entry.is_dir() {
        return Ok(None);
    }

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

fn declaration_candidates(
    jar: &JarRecord,
    class_name: &str,
    member_name: Option<&str>,
    max_bytes: usize,
) -> Result<Vec<DeclarationCandidate>> {
    let Some(member_name) = member_name else {
        return Ok(vec![DeclarationCandidate {
            kind: DeclarationKind::Class,
            name: class_name.to_owned(),
            descriptor: None,
        }]);
    };
    let mut archive = ZipArchive::new(File::open(&jar.path)?)?;
    let entry_path = select_class_entry(&jar.entries, class_name)
        .with_context(|| format!("indexed class entry is missing for {class_name}"))?;
    let mut entry = archive.by_name(entry_path)?;
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
            .map(|field| DeclarationCandidate {
                kind: DeclarationKind::Field,
                name: member_name.to_owned(),
                descriptor: Some(field.descriptor.to_string()),
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
            .map(|method| DeclarationCandidate {
                kind: DeclarationKind::Method,
                name: if method.name.as_ref() == "<init>" {
                    simple_class_name.to_owned()
                } else {
                    member_name.to_owned()
                },
                descriptor: Some(method.descriptor.to_string()),
            }),
    );
    Ok(candidates)
}

struct SourceSlice {
    start_line: usize,
    end_line: usize,
    source: String,
}

fn slice_declaration(
    source: &str,
    class_name: &str,
    candidate: &DeclarationCandidate,
    overload_ordinal: usize,
) -> Option<SourceSlice> {
    let lines = source.lines().collect::<Vec<_>>();
    let start = match candidate.kind {
        DeclarationKind::Class => {
            let simple_name = class_name.rsplit(['.', '$']).next().unwrap_or(class_name);
            lines
                .iter()
                .position(|line| is_type_declaration(line, simple_name))?
        }
        DeclarationKind::Method => lines
            .iter()
            .enumerate()
            .filter(|(_, line)| is_method_declaration(line, &candidate.name))
            .nth(overload_ordinal)
            .map(|(index, _)| index)?,
        DeclarationKind::Field => lines
            .iter()
            .position(|line| is_field_declaration(line, &candidate.name))?,
    };
    let end = match candidate.kind {
        DeclarationKind::Field => (start..lines.len())
            .find(|index| lines[*index].contains(';'))
            .unwrap_or(start),
        DeclarationKind::Class | DeclarationKind::Method => declaration_end(&lines, start),
    };
    Some(SourceSlice {
        start_line: start + 1,
        end_line: end + 1,
        source: lines[start..=end].join("\n"),
    })
}

fn is_type_declaration(line: &str, name: &str) -> bool {
    ["class", "interface", "enum", "record", "object"]
        .iter()
        .any(|keyword| contains_token_sequence(line, keyword, name))
}

fn contains_token_sequence(line: &str, keyword: &str, name: &str) -> bool {
    let tokens = line
        .split(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '$'
        })
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    tokens
        .windows(2)
        .any(|tokens| tokens[0] == keyword && tokens[1] == name)
}

fn is_method_declaration(line: &str, name: &str) -> bool {
    let compact = line.split_whitespace().collect::<String>();
    compact.contains(&format!("{name}("))
        && !compact.starts_with("//")
        && !compact.contains(&format!(".{name}("))
}

fn is_field_declaration(line: &str, name: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with("//") || trimmed.contains('(') {
        return false;
    }
    trimmed
        .split(|character: char| {
            !character.is_alphanumeric() && character != '_' && character != '$'
        })
        .any(|token| token == name)
        && (trimmed.contains(';')
            || trimmed
                .split_whitespace()
                .any(|token| matches!(token, "val" | "var")))
}

fn declaration_end(lines: &[&str], start: usize) -> usize {
    let mut opened = false;
    let mut depth = 0isize;
    for (index, line) in lines.iter().enumerate().skip(start) {
        for character in line.chars() {
            match character {
                '{' => {
                    opened = true;
                    depth += 1;
                }
                '}' if opened => depth -= 1,
                _ => {}
            }
        }
        if (!opened && line.contains(';')) || (opened && depth <= 0) {
            return index;
        }
    }
    lines.len().saturating_sub(1)
}

fn read_class_description(
    jar: &JarRecord,
    jar_summary: JarSummary,
    class_name: &str,
    max_bytes: usize,
    public_only: bool,
) -> Result<ClassDescription> {
    let mut archive = ZipArchive::new(File::open(&jar.path)?)?;
    let entry_path = select_class_entry(&jar.entries, class_name)
        .with_context(|| format!("indexed class entry is missing for {class_name}"))?;
    let mut entry = archive.by_name(entry_path)?;
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

fn select_class_entry<'a>(entries: &'a [String], class_name: &str) -> Option<&'a str> {
    let exact = format!("{}.class", class_name.replace('.', "/"));
    if let Some(entry) = entries.iter().find(|entry| entry.as_str() == exact) {
        return Some(entry);
    }
    let suffix = format!("/{exact}");
    entries
        .iter()
        .filter_map(|entry| {
            entry
                .strip_prefix("META-INF/versions/")
                .and_then(|rest| rest.split_once('/'))
                .filter(|(_, versioned)| versioned.ends_with(&suffix[1..]))
                .and_then(|(version, _)| {
                    version.parse::<u32>().ok().map(|version| (version, entry))
                })
        })
        .max_by_key(|(version, _)| *version)
        .map(|(_, entry)| entry.as_str())
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
mod tests {
    use std::io::Write;

    use tempfile::TempDir;
    use zip::{ZipWriter, write::SimpleFileOptions};

    use super::*;

    fn write_jar(path: &Path, entries: &[(&str, &[u8])]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut writer = ZipWriter::new(File::create(path).unwrap());
        for (name, content) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap();
    }

    fn push_u16(bytes: &mut Vec<u8>, value: u16) {
        bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn push_u32(bytes: &mut Vec<u8>, value: u32) {
        bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn push_utf8(bytes: &mut Vec<u8>, value: &str) {
        bytes.push(1);
        push_u16(bytes, u16::try_from(value.len()).unwrap());
        bytes.extend_from_slice(value.as_bytes());
    }

    fn push_class(bytes: &mut Vec<u8>, name_index: u16) {
        bytes.push(7);
        push_u16(bytes, name_index);
    }

    fn inspection_class(class_name: &str) -> Vec<u8> {
        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 61);
        push_u16(&mut bytes, 19);
        push_utf8(&mut bytes, class_name); // #1
        push_class(&mut bytes, 1); // #2
        push_utf8(&mut bytes, "java/lang/Object"); // #3
        push_class(&mut bytes, 3); // #4
        push_utf8(&mut bytes, "java/io/Serializable"); // #5
        push_class(&mut bytes, 5); // #6
        push_utf8(&mut bytes, "<init>"); // #7
        push_utf8(&mut bytes, "()V"); // #8
        push_utf8(&mut bytes, "greet"); // #9
        push_utf8(&mut bytes, "(Ljava/lang/String;)Ljava/lang/String;"); // #10
        push_utf8(&mut bytes, "value"); // #11
        push_utf8(&mut bytes, "Ljava/lang/String;"); // #12
        push_utf8(&mut bytes, "Signature"); // #13
        push_utf8(&mut bytes, "<T:Ljava/lang/Object;>Ljava/lang/Object;"); // #14
        push_utf8(&mut bytes, "(TT;)TT;"); // #15
        push_utf8(&mut bytes, "RuntimeVisibleAnnotations"); // #16
        push_utf8(&mut bytes, "Ljava/lang/Deprecated;"); // #17
        push_utf8(&mut bytes, "secret"); // #18

        push_u16(&mut bytes, 0x0421); // public, super, abstract
        push_u16(&mut bytes, 2);
        push_u16(&mut bytes, 4);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 6);

        push_u16(&mut bytes, 2); // fields
        push_u16(&mut bytes, 0x0019); // public static final
        push_u16(&mut bytes, 11);
        push_u16(&mut bytes, 12);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 16);
        push_u32(&mut bytes, 6);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 17);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0x0002); // private
        push_u16(&mut bytes, 18);
        push_u16(&mut bytes, 12);
        push_u16(&mut bytes, 0);

        push_u16(&mut bytes, 3); // methods
        push_u16(&mut bytes, 0x0001); // public constructor
        push_u16(&mut bytes, 7);
        push_u16(&mut bytes, 8);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0x0401); // public abstract
        push_u16(&mut bytes, 9);
        push_u16(&mut bytes, 10);
        push_u16(&mut bytes, 2);
        push_u16(&mut bytes, 13);
        push_u32(&mut bytes, 2);
        push_u16(&mut bytes, 15);
        push_u16(&mut bytes, 16);
        push_u32(&mut bytes, 6);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 17);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0x0002); // private method
        push_u16(&mut bytes, 18);
        push_u16(&mut bytes, 8);
        push_u16(&mut bytes, 0);

        push_u16(&mut bytes, 2); // class attributes
        push_u16(&mut bytes, 13);
        push_u32(&mut bytes, 2);
        push_u16(&mut bytes, 14);
        push_u16(&mut bytes, 16);
        push_u32(&mut bytes, 6);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 17);
        push_u16(&mut bytes, 0);
        bytes
    }

    fn hierarchy_class(class_name: &str, super_class: &str, interfaces: &[&str]) -> Vec<u8> {
        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 61);
        push_u16(&mut bytes, u16::try_from(5 + interfaces.len() * 2).unwrap());
        push_utf8(&mut bytes, class_name);
        push_class(&mut bytes, 1);
        push_utf8(&mut bytes, super_class);
        push_class(&mut bytes, 3);
        for (index, interface) in interfaces.iter().enumerate() {
            push_utf8(&mut bytes, interface);
            push_class(&mut bytes, u16::try_from(5 + index * 2).unwrap());
        }
        push_u16(&mut bytes, 0x0021);
        push_u16(&mut bytes, 2);
        push_u16(&mut bytes, 4);
        push_u16(&mut bytes, u16::try_from(interfaces.len()).unwrap());
        for index in 0..interfaces.len() {
            push_u16(&mut bytes, u16::try_from(6 + index * 2).unwrap());
        }
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        bytes
    }

    fn referencing_class(class_name: &str, target: &str) -> Vec<u8> {
        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 61);
        push_u16(&mut bytes, 11);
        push_utf8(&mut bytes, class_name); // #1
        push_class(&mut bytes, 1); // #2
        push_utf8(&mut bytes, "java/lang/Object"); // #3
        push_class(&mut bytes, 3); // #4
        push_utf8(&mut bytes, target); // #5
        push_class(&mut bytes, 5); // #6
        push_utf8(&mut bytes, "call"); // #7
        push_utf8(&mut bytes, "()V"); // #8
        bytes.push(12); // #9 NameAndType
        push_u16(&mut bytes, 7);
        push_u16(&mut bytes, 8);
        bytes.push(10); // #10 MethodRef
        push_u16(&mut bytes, 6);
        push_u16(&mut bytes, 9);
        push_u16(&mut bytes, 0x0021);
        push_u16(&mut bytes, 2);
        push_u16(&mut bytes, 4);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        bytes
    }

    fn module_info_class() -> Vec<u8> {
        let mut bytes = vec![0xca, 0xfe, 0xba, 0xbe];
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 61);
        push_u16(&mut bytes, 12);
        push_utf8(&mut bytes, "module-info"); // #1
        push_class(&mut bytes, 1); // #2
        push_utf8(&mut bytes, "Module"); // #3
        push_utf8(&mut bytes, "example.module"); // #4
        bytes.push(19); // #5 ModuleInfo
        push_u16(&mut bytes, 4);
        push_utf8(&mut bytes, "example/Service"); // #6
        push_class(&mut bytes, 6); // #7
        push_utf8(&mut bytes, "example/Provider"); // #8
        push_class(&mut bytes, 8); // #9
        push_utf8(&mut bytes, "example/Used"); // #10
        push_class(&mut bytes, 10); // #11
        push_u16(&mut bytes, 0x8000);
        push_u16(&mut bytes, 2);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 3);
        push_u32(&mut bytes, 24);
        push_u16(&mut bytes, 5);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 0);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 11);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 7);
        push_u16(&mut bytes, 1);
        push_u16(&mut bytes, 9);
        bytes
    }

    fn replace_ascii(bytes: &mut [u8], previous: &[u8], current: &[u8]) {
        assert_eq!(previous.len(), current.len());
        let offset = bytes
            .windows(previous.len())
            .position(|window| window == previous)
            .expect("test classfile should contain replacement text");
        bytes[offset..offset + current.len()].copy_from_slice(current);
    }

    fn fixture() -> (TempDir, MavenIndex) {
        let root = TempDir::new().unwrap();
        let base = root.path().join("org/example/demo/1.2.0");
        write_jar(
            &base.join("demo-1.2.0.jar"),
            &[
                ("org/example/Foo.class", b"bytecode"),
                ("config/app.conf", b"x"),
            ],
        );
        write_jar(
            &base.join("demo-1.2.0-sources.jar"),
            &[(
                "org/example/Foo.java",
                b"package org.example; public class Foo {}",
            )],
        );
        let index = MavenIndex::build(root.path(), 50, 1024).unwrap();
        (root, index)
    }

    fn build_index(root: &TempDir, max_results: usize, max_source_bytes: usize) -> MavenIndex {
        MavenIndex::build(root.path(), max_results, max_source_bytes).unwrap()
    }

    #[test]
    fn indexes_classes_and_artifact_versions() {
        let (_root, index) = fixture();
        let matches = index.search_classes("Foo", None);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].jar.coordinate, "org.example:demo:1.2.0");
        assert_eq!(
            index.artifact_versions("demo", Some("org.example"))["org.example:demo"],
            vec!["1.2.0"]
        );
    }

    #[test]
    fn reads_source_and_searches_entries() {
        let (_root, index) = fixture();
        let source = index.class_source("org.example.Foo", None, None).unwrap();
        assert!(source[0].source.contains("class Foo"));
        assert_eq!(index.search_entries("app.conf", None, None).len(), 1);
    }

    #[test]
    fn reads_exact_text_and_binary_entries_without_extracting_them() {
        let root = TempDir::new().unwrap();
        let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        write_jar(
            &jar,
            &[
                ("META-INF/MANIFEST.MF", b"Manifest-Version: 1.0\n"),
                ("META-INF/services/example.Service", b"org.example.Foo\n"),
                ("native/image.bin", &[0, 159, 146, 150]),
            ],
        );
        let index = build_index(&root, 10, 1024);

        let manifest = index
            .jar_entry("org.example:demo:1.0", "META-INF/MANIFEST.MF")
            .unwrap();
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[0].content_kind, JarEntryContentKind::Text);
        assert_eq!(manifest[0].text.as_deref(), Some("Manifest-Version: 1.0\n"));
        assert_eq!(manifest[0].bytes, None);
        assert_eq!(manifest[0].original_size, 22);
        assert!(!manifest[0].truncated);

        let binary = index.jar_entry("demo-1.0.jar", "native/image.bin").unwrap();
        assert_eq!(binary[0].content_kind, JarEntryContentKind::Binary);
        assert_eq!(
            binary[0].bytes.as_deref(),
            Some([0, 159, 146, 150].as_slice())
        );
        assert_eq!(binary[0].text, None);
        assert!(
            index
                .jar_entry("demo-1.0.jar", "missing.txt")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn exact_entry_reading_applies_the_byte_limit_and_preserves_utf8() {
        let root = TempDir::new().unwrap();
        let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        write_jar(&jar, &[("utf8.txt", "árvíztűrő".as_bytes())]);
        let index = build_index(&root, 10, 3);

        let result = index.jar_entry("demo-1.0.jar", "utf8.txt").unwrap();
        assert_eq!(result[0].content_kind, JarEntryContentKind::Text);
        assert_eq!(result[0].text.as_deref(), Some("ár"));
        assert_eq!(result[0].original_size, 13);
        assert!(result[0].truncated);
    }

    #[test]
    fn exact_entry_reading_surfaces_changes_that_corrupt_an_indexed_jar() {
        let root = TempDir::new().unwrap();
        let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        write_jar(&jar, &[("config.txt", b"value")]);
        let index = build_index(&root, 10, 1024);
        std::fs::write(&jar, b"not a zip anymore").unwrap();

        let error = index
            .jar_entry("demo-1.0.jar", "config.txt")
            .expect_err("a real ZIP read error must not become an empty result");
        assert!(error.to_string().contains("no longer a valid ZIP"));
    }

    #[test]
    fn parses_structured_pom_metadata_and_sorts_declared_content() {
        let root = TempDir::new().unwrap();
        let base = root.path().join("org/example/demo/1.0");
        write_jar(&base.join("demo-1.0.jar"), &[("Demo.class", b"bytecode")]);
        std::fs::write(
            base.join("demo-1.0.pom"),
            r#"<project>
                <modelVersion>4.0.0</modelVersion>
                <parent>
                    <groupId>org.example.parent</groupId>
                    <artifactId>parent</artifactId>
                    <version>3.0</version>
                </parent>
                <groupId>org.example</groupId>
                <artifactId>demo</artifactId>
                <version>1.0</version>
                <packaging>maven-plugin</packaging>
                <properties>
                    <library.version>2.4</library.version>
                    <java.version>21</java.version>
                </properties>
                <dependencyManagement><dependencies>
                    <dependency>
                        <groupId>org.platform</groupId><artifactId>bom</artifactId>
                        <version>5.0</version><type>pom</type><scope>import</scope>
                    </dependency>
                    <dependency>
                        <groupId>org.example</groupId><artifactId>managed</artifactId>
                        <version>${library.version}</version>
                    </dependency>
                </dependencies></dependencyManagement>
                <dependencies>
                    <dependency>
                        <groupId>org.example</groupId><artifactId>runtime</artifactId>
                        <version>${library.version}</version><scope>runtime</scope>
                        <optional>true</optional><classifier>linux</classifier><type>zip</type>
                        <exclusions>
                            <exclusion><groupId>z.group</groupId><artifactId>last</artifactId></exclusion>
                            <exclusion><groupId>a.group</groupId><artifactId>first</artifactId></exclusion>
                        </exclusions>
                    </dependency>
                </dependencies>
            </project>"#,
        )
        .unwrap();
        let index = build_index(&root, 10, 1024);

        let result = index.pom_descriptor("org.example:demo:1.0").unwrap();
        assert!(result.found);
        let descriptor = result.descriptor.unwrap();
        assert_eq!(descriptor.packaging, "maven-plugin");
        assert_eq!(descriptor.parent.unwrap().artifact_id, "parent");
        assert_eq!(descriptor.properties["java.version"], "21");
        assert_eq!(
            descriptor.dependencies[0].version.as_deref(),
            Some("${library.version}")
        );
        assert!(descriptor.dependencies[0].optional);
        assert_eq!(
            descriptor.dependencies[0].classifier.as_deref(),
            Some("linux")
        );
        assert_eq!(descriptor.dependencies[0].r#type.as_deref(), Some("zip"));
        assert_eq!(descriptor.dependencies[0].exclusions[0].group_id, "a.group");
        assert_eq!(descriptor.dependency_management.len(), 1);
        assert_eq!(descriptor.bom_imports.len(), 1);
        assert_eq!(descriptor.bom_imports[0].artifact_id, "bom");
    }

    #[test]
    fn pom_lookup_distinguishes_missing_invalid_and_malformed_descriptors() {
        let root = TempDir::new().unwrap();
        let base = root.path().join("org/example/demo/1.0");
        write_jar(&base.join("demo-1.0.jar"), &[("Demo.class", b"bytecode")]);
        let index = build_index(&root, 10, 1024);

        let missing = index.pom_descriptor("org.example:demo:1.0").unwrap();
        assert!(!missing.found);
        assert_eq!(missing.descriptor, None);
        assert!(index.pom_descriptor("../../outside:demo:1.0").is_err());

        std::fs::write(base.join("demo-1.0.pom"), "<project><broken></project>").unwrap();
        let error = index
            .pom_descriptor("org.example:demo:1.0")
            .expect_err("malformed POM XML must remain an error");
        assert!(error.to_string().contains("cannot parse POM"));
    }

    #[test]
    fn describes_classfile_api_without_a_sources_jar() {
        let root = TempDir::new().unwrap();
        let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        let class = inspection_class("org/example/Inspectable");
        write_jar(&jar, &[("org/example/Inspectable.class", &class)]);
        let index = build_index(&root, 10, 64 * 1024);

        let public = index
            .describe_class("org.example.Inspectable", None, None, true)
            .unwrap();
        assert_eq!(public.len(), 1);
        assert_eq!(public[0].class_name, "org.example.Inspectable");
        assert_eq!(public[0].visibility, "public");
        assert_eq!(public[0].super_class.as_deref(), Some("java.lang.Object"));
        assert_eq!(public[0].interfaces, vec!["java.io.Serializable"]);
        assert_eq!(
            public[0].generic_signature.as_deref(),
            Some("<T:Ljava/lang/Object;>Ljava/lang/Object;")
        );
        assert_eq!(public[0].annotations, vec!["java.lang.Deprecated"]);
        assert_eq!(public[0].constructors.len(), 1);
        assert_eq!(public[0].methods.len(), 1);
        assert_eq!(public[0].methods[0].name, "greet");
        assert_eq!(
            public[0].methods[0].generic_signature.as_deref(),
            Some("(TT;)TT;")
        );
        assert_eq!(public[0].fields.len(), 1);
        assert_eq!(
            public[0].fields[0].annotations,
            vec!["java.lang.Deprecated"]
        );

        let all = index
            .describe_class(
                "org.example.Inspectable",
                Some("demo-1.0.jar"),
                Some("1.0"),
                false,
            )
            .unwrap();
        assert_eq!(all[0].methods.len(), 2);
        assert_eq!(all[0].fields.len(), 2);
        assert!(
            index
                .describe_class("org.example.Missing", None, None, true)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn describes_inner_and_multi_release_classes_once() {
        let root = TempDir::new().unwrap();
        let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        let inner = inspection_class("org/example/Outer$Inner");
        let versioned = inspection_class("org/example/VersionOnly");
        write_jar(
            &jar,
            &[
                ("org/example/Outer$Inner.class", &inner),
                (
                    "META-INF/versions/17/org/example/VersionOnly.class",
                    &versioned,
                ),
            ],
        );
        let index = build_index(&root, 10, 64 * 1024);

        let inner_result = index
            .describe_class("org.example.Outer$Inner", None, None, true)
            .unwrap();
        assert_eq!(inner_result[0].class_name, "org.example.Outer$Inner");
        let versioned_result = index
            .describe_class("org.example.VersionOnly", None, None, true)
            .unwrap();
        assert_eq!(versioned_result.len(), 1);
    }

    #[test]
    fn diagnoses_complete_snapshot_and_corrupt_artifact_states_without_paths() {
        let root = TempDir::new().unwrap();
        let base = root.path().join("org/example/demo/1.0-SNAPSHOT");
        write_jar(
            &base.join("demo-1.0-SNAPSHOT.jar"),
            &[("Demo.class", b"bytecode")],
        );
        write_jar(
            &base.join("demo-1.0-SNAPSHOT-sources.jar"),
            &[("Demo.java", b"class Demo {}")],
        );
        std::fs::write(base.join("demo-1.0-SNAPSHOT-javadoc.jar"), b"corrupt").unwrap();
        std::fs::write(base.join("demo-1.0-SNAPSHOT.pom"), "<project/>").unwrap();
        std::fs::write(base.join("demo-1.0-SNAPSHOT.jar.sha256"), "checksum").unwrap();
        std::fs::write(base.join("demo-1.0-SNAPSHOT.pom.lastUpdated"), "failure").unwrap();
        std::fs::write(
            base.join("_remote.repositories"),
            "demo-1.0-SNAPSHOT.jar>central=\ndemo-1.0-SNAPSHOT.pom>private-repo=\n",
        )
        .unwrap();
        let index = build_index(&root, 10, 64 * 1024);

        let health = index
            .artifact_health("org.example:demo:1.0-SNAPSHOT")
            .unwrap();
        assert!(health.found);
        assert!(health.snapshot);
        assert_eq!(health.files.len(), 4);
        assert_eq!(health.checksums, vec!["demo-1.0-SNAPSHOT.jar.sha256"]);
        assert_eq!(
            health.last_updated_markers,
            vec!["demo-1.0-SNAPSHOT.pom.lastUpdated"]
        );
        assert_eq!(health.repository_ids, vec!["central", "private-repo"]);
        let corrupt = health
            .files
            .iter()
            .find(|file| file.classifier.as_deref() == Some("javadoc"))
            .unwrap();
        assert_eq!(corrupt.readable, Some(false));
        assert!(
            health
                .files
                .iter()
                .all(|file| !file.file_name.contains('/'))
        );

        let missing = index.artifact_health("org.example:missing:1.0").unwrap();
        assert!(!missing.found);
        assert!(missing.files.is_empty());
    }

    #[test]
    fn searches_class_members_and_annotations_deterministically_with_limits() {
        let root = TempDir::new().unwrap();
        for (artifact, class_name) in [
            ("first", "org/example/First"),
            ("second", "org/example/Second"),
        ] {
            let jar = root
                .path()
                .join("org/example")
                .join(artifact)
                .join("1.0")
                .join(format!("{artifact}-1.0.jar"));
            let class = inspection_class(class_name);
            let entry = format!("{class_name}.class");
            write_jar(&jar, &[(entry.as_str(), &class)]);
        }
        let index = build_index(&root, 10, 64 * 1024);

        let methods = index.search_class_members("GREET", None, None);
        assert_eq!(methods.len(), 2);
        assert_eq!(methods[0].kind, ClassMemberMatchKind::Method);
        assert_eq!(methods[0].jar.artifact_id, "first");
        assert_eq!(methods[1].jar.artifact_id, "second");
        let fields = index.search_class_members("value", Some("first-1.0.jar"), None);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].kind, ClassMemberMatchKind::Field);
        let annotations = index.search_class_members("deprecated", None, Some(2));
        assert_eq!(annotations.len(), 2);
        assert!(
            annotations
                .iter()
                .all(|item| item.kind == ClassMemberMatchKind::Annotation)
        );
        assert!(index.search_class_members("missing", None, None).is_empty());
    }

    #[test]
    fn compares_added_removed_and_changed_public_api_between_versions() {
        let root = TempDir::new().unwrap();
        let version_one = root.path().join("org/example/demo/1.0");
        let version_two = root.path().join("org/example/demo/2.0");
        let common_v1 = inspection_class("org/example/Common");
        let old_only = inspection_class("org/example/OldOnly");
        write_jar(
            &version_one.join("demo-1.0.jar"),
            &[
                ("org/example/Common.class", &common_v1),
                ("org/example/OldOnly.class", &old_only),
            ],
        );
        let mut common_v2 = inspection_class("org/example/Common");
        replace_ascii(&mut common_v2, b"java/lang/Object", b"java/lang/Number");
        replace_ascii(
            &mut common_v2,
            b"java/io/Serializable",
            b"java/lang/Comparable",
        );
        replace_ascii(&mut common_v2, b"value", b"other");
        let new_only = inspection_class("org/example/NewOnly");
        write_jar(
            &version_two.join("demo-2.0.jar"),
            &[
                ("org/example/Common.class", &common_v2),
                ("org/example/NewOnly.class", &new_only),
            ],
        );
        let index = build_index(&root, 20, 64 * 1024);

        let diff = index
            .compare_artifact_api("org.example", "demo", "1.0", "2.0")
            .unwrap();
        assert_eq!(diff.added_classes, vec!["org.example.NewOnly"]);
        assert_eq!(diff.removed_classes, vec!["org.example.OldOnly"]);
        assert_eq!(diff.changed_classes.len(), 1);
        let changed = &diff.changed_classes[0];
        assert_eq!(changed.class_name, "org.example.Common");
        assert_eq!(
            changed.added_members,
            vec!["field:other:Ljava/lang/String;"]
        );
        assert_eq!(
            changed.removed_members,
            vec!["field:value:Ljava/lang/String;"]
        );
        assert_eq!(
            changed.previous_super_class.as_deref(),
            Some("java.lang.Object")
        );
        assert_eq!(
            changed.current_super_class.as_deref(),
            Some("java.lang.Number")
        );
        assert_eq!(changed.added_interfaces, vec!["java.lang.Comparable"]);
        assert_eq!(changed.removed_interfaces, vec!["java.io.Serializable"]);

        let unchanged = index
            .compare_artifact_api("org.example", "demo", "2.0", "2.0")
            .unwrap();
        assert!(unchanged.added_classes.is_empty());
        assert!(unchanged.removed_classes.is_empty());
        assert!(unchanged.changed_classes.is_empty());
        assert!(
            index
                .compare_artifact_api("org.example", "demo", "1.0", "missing")
                .is_err()
        );
    }

    #[test]
    fn searches_supported_text_resources_with_context_and_budgets() {
        let root = TempDir::new().unwrap();
        let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        write_jar(
            &jar,
            &[
                (
                    "META-INF/MANIFEST.MF",
                    b"Manifest-Version: 1.0\nDemo-Key: Found\n",
                ),
                ("META-INF/services/example.Service", b"org.example.Foo\n"),
                ("config/application.properties", b"feature.name=Found\n"),
                (
                    "config/data.json",
                    b"this content is deliberately too large: Found",
                ),
                ("config/binary.properties", &[0xff, 0xfe, 0xfd]),
                ("image.bin", b"Found but unsupported"),
            ],
        );
        let index = build_index(&root, 10, 40);

        let service = index
            .search_jar_content("EXAMPLE.FOO", Some("demo-1.0.jar"), None)
            .unwrap();
        assert_eq!(service.results.len(), 1);
        assert_eq!(
            service.results[0].entry,
            "META-INF/services/example.Service"
        );
        assert_eq!(service.results[0].line, 1);
        assert_eq!(service.results[0].context, "org.example.Foo");
        let found = index.search_jar_content("found", None, None).unwrap();
        assert_eq!(found.results.len(), 2);
        assert!(found.incomplete);
        assert!(found.results.iter().all(|item| item.entry != "image.bin"));
        let limited = index.search_jar_content("found", None, Some(1)).unwrap();
        assert_eq!(limited.results.len(), 1);
        assert!(limited.incomplete);
        assert!(
            index
                .search_jar_content("missing", None, None)
                .unwrap()
                .results
                .is_empty()
        );
    }

    #[test]
    fn jar_content_search_surfaces_zip_errors_after_indexing() {
        let root = TempDir::new().unwrap();
        let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        write_jar(&jar, &[("config.txt", b"searchable")]);
        let index = build_index(&root, 10, 1024);
        std::fs::write(&jar, b"corrupt after startup").unwrap();

        assert!(index.search_jar_content("searchable", None, None).is_err());
    }

    #[test]
    fn recognizes_multi_release_classes_once() {
        assert_eq!(
            class_name_from_entry("META-INF/versions/17/org/example/Foo.class"),
            Some("org.example.Foo".to_owned())
        );
        assert_eq!(class_name_from_entry("module-info.class"), None);
    }

    #[test]
    fn class_search_is_case_insensitive_and_respects_result_limit() {
        let root = TempDir::new().unwrap();
        let first = root.path().join("org/example/first/1.0/first-1.0.jar");
        let second = root.path().join("org/example/second/1.0/second-1.0.jar");
        write_jar(&first, &[("org/example/Foo.class", b"one")]);
        write_jar(&second, &[("org/example/Foo.class", b"two")]);
        let index = build_index(&root, 1, 1024);

        let matches = index.search_classes("fOo", Some(20));
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].class_name, "org.example.Foo");
        assert!(index.search_classes("   ", None).is_empty());
    }

    #[test]
    fn jar_selection_supports_coordinate_filename_and_relative_path() {
        let (_root, index) = fixture();
        let coordinate = index.list_classes("org.example:demo:1.2.0", 0, None);
        let filename = index.list_classes("demo-1.2.0.jar", 0, None);
        let path = index.list_classes("org/example/demo/1.2.0/demo-1.2.0.jar", 0, None);

        assert_eq!(coordinate, filename);
        assert_eq!(filename, path);
        assert!(index.list_classes("missing.jar", 0, None).is_empty());
    }

    #[test]
    fn class_listing_paginates_and_entry_search_can_target_one_jar() {
        let root = TempDir::new().unwrap();
        let first = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        let second = root.path().join("org/example/other/1.0/other-1.0.jar");
        write_jar(
            &first,
            &[
                ("org/example/Alpha.class", b"a"),
                ("org/example/Beta.class", b"b"),
                ("config/shared.conf", b"first"),
            ],
        );
        write_jar(&second, &[("other/shared.conf", b"second")]);
        let index = build_index(&root, 10, 1024);

        let page = index.list_classes("demo-1.0.jar", 1, Some(1));
        assert_eq!(page[0].total, 2);
        assert_eq!(page[0].classes, vec!["org.example.Beta"]);
        let entries = index.search_entries("shared.conf", Some("demo-1.0.jar"), None);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].jar.artifact_id, "demo");
    }

    #[test]
    fn source_lookup_supports_inner_classes_filters_and_truncation() {
        let root = TempDir::new().unwrap();
        let version_one = root.path().join("org/example/demo/1.0");
        let version_two = root.path().join("org/example/demo/2.0");
        for base in [&version_one, &version_two] {
            let version = base.file_name().unwrap().to_str().unwrap();
            write_jar(
                &base.join(format!("demo-{version}.jar")),
                &[("org/example/Outer$Inner.class", b"bytecode")],
            );
            write_jar(
                &base.join(format!("demo-{version}-sources.jar")),
                &[(
                    "org/example/Outer.java",
                    b"public class Outer { class Inner {} }",
                )],
            );
        }
        let index = build_index(&root, 10, 12);

        let sources = index
            .class_source("org.example.Outer$Inner", None, Some("2.0"))
            .unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].source_jar.version, "2.0");
        assert!(sources[0].truncated);
        assert_eq!(sources[0].source.len(), 12);
        assert!(
            index
                .class_source("org.example.Outer$Inner", Some("demo-1.0.jar"), Some("2.0"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn artifact_versions_are_group_scoped_and_sorted() {
        let root = TempDir::new().unwrap();
        for (group, version) in [
            ("com/acme", "2.0"),
            ("com/acme", "1.0"),
            ("org/demo", "9.0"),
        ] {
            write_jar(
                &root
                    .path()
                    .join(group)
                    .join("shared")
                    .join(version)
                    .join(format!("shared-{version}.jar")),
                &[("Shared.class", b"x")],
            );
        }
        let index = build_index(&root, 10, 1024);

        assert_eq!(index.artifact_versions("shared", None).len(), 2);
        assert_eq!(
            index.artifact_versions("shared", Some("com.acme"))["com.acme:shared"],
            vec!["1.0", "2.0"]
        );
        assert!(index.artifact_versions("missing", None).is_empty());
    }

    #[test]
    fn classifiers_are_addressable_and_multi_release_classes_are_deduplicated() {
        let root = TempDir::new().unwrap();
        let path = root.path().join("org/example/demo/1.0/demo-1.0-tests.jar");
        write_jar(
            &path,
            &[
                ("org/example/Foo.class", b"base"),
                ("META-INF/versions/17/org/example/Foo.class", b"java17"),
                ("module-info.class", b"module"),
            ],
        );
        let index = build_index(&root, 10, 1024);

        let jars = index.search_jars("org.example:demo:1.0:tests", None);
        assert_eq!(jars.len(), 1);
        assert_eq!(jars[0].classifier.as_deref(), Some("tests"));
        assert_eq!(jars[0].class_count, 1);
    }

    #[test]
    fn searches_direct_and_transitive_type_hierarchy_deterministically() {
        let root = TempDir::new().unwrap();
        let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        let implementation = hierarchy_class(
            "org/example/Implementation",
            "java/lang/Object",
            &["org/example/Service"],
        );
        let child = hierarchy_class("org/example/Child", "org/example/Implementation", &[]);
        write_jar(
            &jar,
            &[
                ("org/example/Implementation.class", &implementation),
                ("org/example/Child.class", &child),
            ],
        );
        let index = build_index(&root, 10, 64 * 1024);

        let direct = index.search_type_hierarchy("org.example.Service", false, None, None);
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].relation, TypeRelation::Implements);
        assert_eq!(
            direct[0].path,
            vec!["org.example.Service", "org.example.Implementation"]
        );
        let transitive = index.search_type_hierarchy("org.example.Service", true, None, None);
        assert_eq!(transitive.len(), 2);
        assert_eq!(transitive[1].type_name, "org.example.Child");
        assert_eq!(transitive[1].depth, 2);
        assert!(
            index
                .search_type_hierarchy("org.example.Missing", true, None, None)
                .is_empty()
        );
    }

    #[test]
    fn searches_java_and_kotlin_sources_with_regex_context_and_limits() {
        let root = TempDir::new().unwrap();
        let base = root.path().join("org/example/demo/1.0");
        write_jar(
            &base.join("demo-1.0.jar"),
            &[("org/example/Demo.class", b"unparseable is still indexed")],
        );
        write_jar(
            &base.join("demo-1.0-sources.jar"),
            &[
                (
                    "org/example/Demo.java",
                    b"package org.example;\nclass Demo {\n  String needle = \"java\";\n}\n",
                ),
                (
                    "org/example/Other.kt",
                    b"package org.example\nclass Other { val needle = \"kotlin\" }\n",
                ),
            ],
        );
        let index = build_index(&root, 10, 64 * 1024);

        let found = index
            .search_source("needle\\s*=", true, Some("demo-1.0.jar"), 1, None)
            .unwrap();
        assert_eq!(found.results.len(), 2);
        assert_eq!(found.results[0].line, 3);
        assert!(found.results[0].context.contains("class Demo"));
        assert!(index.search_source("[", true, None, 0, None).is_err());
        assert!(index.search_source(" ", false, None, 0, None).is_err());
    }

    #[test]
    fn returns_class_and_method_declaration_source_slices() {
        let root = TempDir::new().unwrap();
        let base = root.path().join("org/example/demo/1.0");
        let class = inspection_class("org/example/Inspectable");
        write_jar(
            &base.join("demo-1.0.jar"),
            &[("org/example/Inspectable.class", &class)],
        );
        write_jar(
            &base.join("demo-1.0-sources.jar"),
            &[(
                "org/example/Inspectable.java",
                b"package org.example;\npublic abstract class Inspectable {\n  public abstract String greet(String value);\n}\n",
            )],
        );
        let index = build_index(&root, 10, 64 * 1024);

        let class_result = index
            .get_declaration_source("org.example.Inspectable", None, None, None, None)
            .unwrap();
        assert_eq!(class_result.results[0].start_line, 2);
        assert!(class_result.results[0].source.contains("greet"));
        let method = index
            .get_declaration_source(
                "org.example.Inspectable",
                Some("greet"),
                Some("(Ljava/lang/String;)Ljava/lang/String;"),
                None,
                None,
            )
            .unwrap();
        assert_eq!(method.results[0].kind, DeclarationKind::Method);
        assert_eq!(
            method.results[0].source.trim(),
            "public abstract String greet(String value);"
        );
    }

    #[test]
    fn indexes_and_filters_constant_pool_references() {
        let root = TempDir::new().unwrap();
        let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        let caller = referencing_class("org/example/Caller", "org/example/Target");
        let target = hierarchy_class("org/example/Target", "java/lang/Object", &[]);
        write_jar(
            &jar,
            &[
                ("org/example/Caller.class", &caller),
                ("org/example/Target.class", &target),
            ],
        );
        let index = build_index(&root, 10, 64 * 1024);

        let inbound = index.search_class_references(
            "org.example.Target",
            ReferenceDirection::Inbound,
            Some(ClassReferenceKind::Method),
            Some("call"),
            Some("()V"),
            None,
            None,
        );
        assert_eq!(inbound.len(), 1);
        assert_eq!(inbound[0].source_class, "org.example.Caller");
        assert_eq!(inbound[0].target_artifacts, vec!["org.example:demo:1.0"]);
        let outbound = index.search_class_references(
            "org.example.Caller",
            ReferenceDirection::Outbound,
            None,
            None,
            None,
            None,
            None,
        );
        assert!(outbound.iter().any(|reference| {
            reference.kind == ClassReferenceKind::Class
                && reference.target_owner == "org.example.Target"
        }));
        assert!(
            outbound
                .iter()
                .all(|reference| reference.target_owner != "org.example.Caller")
        );
    }

    #[test]
    fn indexes_service_module_and_spring_provider_facts() {
        let root = TempDir::new().unwrap();
        let jar = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        let module = module_info_class();
        write_jar(
            &jar,
            &[
                ("module-info.class", &module),
                (
                    "META-INF/services/example.Service",
                    b"# comment\nexample.Provider\ninvalid provider\nexample.Provider\n",
                ),
                (
                    "META-INF/spring.factories",
                    b"example.Factory=example.First,\\\n example.Second\nbroken\n",
                ),
                (
                    "META-INF/spring/example.ImportSelector.imports",
                    b"example.Imported\n",
                ),
            ],
        );
        let index = build_index(&root, 20, 64 * 1024);

        let service = index.search_providers(
            Some("example.Service"),
            None,
            Some(ProviderDescriptorKind::ServiceLoader),
            None,
            None,
        );
        assert_eq!(service.len(), 1);
        assert_eq!(service[0].provider.as_deref(), Some("example.Provider"));
        let module_provides = index.search_providers(
            Some("example.Service"),
            Some("example.Provider"),
            Some(ProviderDescriptorKind::ModuleProvides),
            None,
            None,
        );
        assert_eq!(module_provides.len(), 1);
        let uses = index.search_providers(
            Some("example.Used"),
            None,
            Some(ProviderDescriptorKind::ModuleUses),
            None,
            None,
        );
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].provider, None);
        assert_eq!(
            index
                .search_providers(
                    Some("example.Factory"),
                    None,
                    Some(ProviderDescriptorKind::SpringFactories),
                    None,
                    None,
                )
                .len(),
            2
        );
    }

    #[test]
    fn malformed_and_non_maven_jars_are_skipped() {
        let root = TempDir::new().unwrap();
        let malformed = root.path().join("org/example/demo/1.0/demo-1.0.jar");
        std::fs::create_dir_all(malformed.parent().unwrap()).unwrap();
        std::fs::write(malformed, b"not a zip").unwrap();
        write_jar(&root.path().join("standalone.jar"), &[("Foo.class", b"x")]);

        let index = build_index(&root, 10, 1024);
        assert_eq!(
            index.stats(),
            IndexStats {
                jar_count: 0,
                source_jar_count: 0,
                class_count: 0,
                unique_class_count: 0,
                artifact_count: 0,
            }
        );
    }
}
