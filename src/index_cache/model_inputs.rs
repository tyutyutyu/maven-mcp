//! Parent and imported BOM POMs that take part in Maven's model building.
//!
//! The reactor POMs are stamped by the project fingerprint and the POM of every repository
//! artifact by the classpath inputs, but neither covers what those POMs inherit from or
//! import: a parent outside the reactor or in the repository, or a BOM imported through
//! `dependencyManagement`. Editing, adding or deleting one changes which artifacts Maven
//! selects, so every such POM is a classpath input too, followed transitively.
//!
//! A reference with a literal version names one repository POM. A version Maven decides
//! (a property, a range, a SNAPSHOT) names every version of that artifact, so its whole
//! repository directory is stamped and every version's POM is followed. A reference whose
//! groupId or artifactId cannot be read without interpolating arbitrary properties, a POM
//! that cannot be parsed, or a walk beyond the XML budget declines persistence instead.

use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::config::XmlBudget;

/// Upper bound of the version directories of one artifact that are followed.
const MAX_FOLLOWED_VERSIONS: usize = 256;
const DEFAULT_RELATIVE_PATH: &str = "../pom.xml";

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ModelPom {
    group_id: Option<String>,
    artifact_id: Option<String>,
    parent: Option<ParentReference>,
    dependency_management: DependencyManagement,
    profiles: Profiles,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ParentReference {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
    relative_path: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct DependencyManagement {
    dependencies: Dependencies,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Dependencies {
    #[serde(rename = "dependency")]
    entries: Vec<ManagedDependency>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ManagedDependency {
    group_id: Option<String>,
    artifact_id: Option<String>,
    version: Option<String>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Profiles {
    #[serde(rename = "profile")]
    entries: Vec<Profile>,
}

/// Profile activation is not evaluated: the imports of every profile are inputs.
#[derive(Debug, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Profile {
    dependency_management: DependencyManagement,
}

impl ModelPom {
    fn imports(&self) -> impl Iterator<Item = &ManagedDependency> {
        std::iter::once(&self.dependency_management)
            .chain(
                self.profiles
                    .entries
                    .iter()
                    .map(|profile| &profile.dependency_management),
            )
            .flat_map(|management| &management.dependencies.entries)
            .filter(|dependency| dependency.scope.as_deref().map(str::trim) == Some("import"))
    }

    fn group_id(&self) -> Option<&str> {
        self.group_id
            .as_deref()
            .or_else(|| self.parent.as_ref()?.group_id.as_deref())
    }

    /// `value` with the model's own coordinates substituted; `None` when another expression
    /// remains, because its value may come from a profile, settings or the command line.
    fn interpolate(&self, value: &str) -> Option<String> {
        let parent = self.parent.as_ref();
        let known = [
            ("project.groupId", self.group_id()),
            ("pom.groupId", self.group_id()),
            ("groupId", self.group_id()),
            ("project.artifactId", self.artifact_id.as_deref()),
            ("pom.artifactId", self.artifact_id.as_deref()),
            ("artifactId", self.artifact_id.as_deref()),
            (
                "project.parent.groupId",
                parent.and_then(|parent| parent.group_id.as_deref()),
            ),
            (
                "project.parent.artifactId",
                parent.and_then(|parent| parent.artifact_id.as_deref()),
            ),
        ];
        let mut resolved = value.trim().to_owned();
        for (name, replacement) in known {
            let placeholder = format!("${{{name}}}");
            if resolved.contains(&placeholder) {
                resolved = resolved.replace(&placeholder, replacement?.trim());
            }
        }
        (!resolved.contains("${")).then_some(resolved)
    }

    /// Repository coordinates of a parent or import reference.
    fn coordinate(
        &self,
        group_id: Option<&str>,
        artifact_id: Option<&str>,
        version: Option<&str>,
    ) -> Option<Coordinate> {
        let group_id = self.interpolate(group_id?)?;
        let artifact_id = self.interpolate(artifact_id?)?;
        if !group_id.split('.').all(is_path_segment) || !is_path_segment(&artifact_id) {
            return None;
        }
        let version = version
            .map(str::trim)
            .filter(|version| {
                is_path_segment(version)
                    && !version.contains("${")
                    && !version.contains(['[', '(', ','])
                    && !version.ends_with("-SNAPSHOT")
            })
            .map(str::to_owned);
        Some(Coordinate {
            group_id,
            artifact_id,
            version,
        })
    }
}

/// A coordinate part that names exactly one directory below the repository.
fn is_path_segment(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && segment != ".." && !segment.contains(['/', '\\'])
}

/// A repository POM reference; `version` is `None` when Maven decides the version.
struct Coordinate {
    group_id: String,
    artifact_id: String,
    version: Option<String>,
}

struct ModelWalk<'a> {
    repository: Option<&'a Path>,
    queue: VecDeque<(PathBuf, bool)>,
    visited: BTreeSet<PathBuf>,
    inputs: BTreeSet<PathBuf>,
    budget: XmlBudget,
}

/// The parent and imported POM files and repository artifact directories that
/// `reactor_poms` (whose relative parent paths count) and `repository_poms` inherit from or
/// import, transitively. `None` when one of them cannot be identified.
pub(super) fn model_inputs(
    reactor_poms: &[PathBuf],
    repository_poms: &[PathBuf],
    repository: Option<&Path>,
) -> Option<Vec<PathBuf>> {
    let mut walk = ModelWalk {
        repository,
        queue: VecDeque::new(),
        visited: BTreeSet::new(),
        inputs: BTreeSet::new(),
        budget: XmlBudget::default(),
    };
    for pom in reactor_poms {
        walk.enqueue(pom.clone(), true);
    }
    for pom in repository_poms {
        walk.enqueue(pom.clone(), false);
    }
    while let Some((pom, in_reactor)) = walk.queue.pop_front() {
        walk.visit(&pom, in_reactor)?;
    }
    Some(walk.inputs.into_iter().collect())
}

impl ModelWalk<'_> {
    fn enqueue(&mut self, pom: PathBuf, in_reactor: bool) {
        if self.visited.insert(pom.clone()) {
            self.queue.push_back((pom, in_reactor));
        }
    }

    /// Follows the parent and imports of one POM; a missing POM is stamped as missing.
    fn visit(&mut self, pom: &Path, in_reactor: bool) -> Option<()> {
        if !pom.is_file() {
            return Some(());
        }
        let xml = self.budget.read(pom).ok()?;
        let model: ModelPom = quick_xml::de::from_str(&xml).ok()?;
        if let Some(parent) = &model.parent {
            if in_reactor {
                self.relative_parent(pom, parent.relative_path.as_deref());
            }
            let coordinate = model.coordinate(
                parent.group_id.as_deref(),
                parent.artifact_id.as_deref(),
                parent.version.as_deref(),
            )?;
            self.repository_pom(&coordinate)?;
        }
        for import in model.imports() {
            let coordinate = model.coordinate(
                import.group_id.as_deref(),
                import.artifact_id.as_deref(),
                import.version.as_deref(),
            )?;
            self.repository_pom(&coordinate)?;
        }
        Some(())
    }

    /// The local parent POM Maven tries before the repository; an empty path disables it.
    fn relative_parent(&mut self, pom: &Path, relative_path: Option<&str>) {
        let relative = relative_path.map_or(DEFAULT_RELATIVE_PATH, str::trim);
        let Some(directory) = pom.parent().filter(|_| !relative.is_empty()) else {
            return;
        };
        let target = directory.join(relative);
        let target = if target.is_dir() {
            target.join("pom.xml")
        } else {
            target
        };
        let target = target.canonicalize().unwrap_or(target);
        self.inputs.insert(target.clone());
        self.enqueue(target, true);
    }

    fn repository_pom(&mut self, coordinate: &Coordinate) -> Option<()> {
        let artifact_directory = self
            .repository?
            .join(coordinate.group_id.replace('.', "/"))
            .join(&coordinate.artifact_id);
        if let Some(version) = &coordinate.version {
            let pom = version_pom(&artifact_directory.join(version), &coordinate.artifact_id)?;
            self.inputs.insert(pom.clone());
            self.enqueue(pom, false);
            return Some(());
        }
        self.inputs.insert(artifact_directory.clone());
        let Ok(versions) = fs::read_dir(&artifact_directory) else {
            return Some(());
        };
        let versions = versions
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .take(MAX_FOLLOWED_VERSIONS + 1)
            .collect::<Vec<_>>();
        if versions.len() > MAX_FOLLOWED_VERSIONS {
            return None;
        }
        for version in versions {
            if let Some(pom) = version_pom(&version, &coordinate.artifact_id) {
                self.enqueue(pom, false);
            }
        }
        Some(())
    }
}

/// `<artifactId>-<version>.pom` in a repository version directory.
fn version_pom(version_directory: &Path, artifact_id: &str) -> Option<PathBuf> {
    let version = version_directory.file_name()?.to_str()?;
    Some(version_directory.join(format!("{artifact_id}-{version}.pom")))
}

#[cfg(test)]
#[path = "../../tests/unit/index_cache_model_inputs.rs"]
mod tests;
