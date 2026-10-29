use std::{
    cell::RefCell,
    collections::HashMap,
    fs::File,
    time::{Duration, SystemTime},
};

use tempfile::TempDir;

use super::*;
use crate::index::{
    MavenIndex, ReactorLayout, ReferenceDirection, ScopedIndex,
    tests::{
        MULTI_RELEASE_MANIFEST, hierarchy_class, module_info_class, referencing_class,
        write_class_directory, write_jar, write_module_pom,
    },
};

/// An in-memory stand-in for the on-disk cache.
#[derive(Default)]
struct MemoryRecords(RefCell<HashMap<PathBuf, Vec<u8>>>);

impl PersistedRecords for MemoryRecords {
    fn load(&self, path: &Path) -> Option<Vec<u8>> {
        self.0.borrow().get(path).cloned()
    }
}

impl MemoryRecords {
    fn store(&self, scoped: &ScopedIndex) {
        for record in &scoped.records.persistable {
            self.0
                .borrow_mut()
                .insert(record.path.clone(), record.payload.clone());
        }
    }
}

/// Moves every modification time below `root` an hour into the past.
fn settle(root: &Path) {
    let past = SystemTime::now() - Duration::from_secs(3600);
    for entry in WalkDir::new(root).contents_first(true) {
        let entry = entry.unwrap();
        File::open(entry.path())
            .unwrap()
            .set_modified(past)
            .unwrap();
    }
}

struct Project {
    _directory: TempDir,
    root: PathBuf,
    repository: PathBuf,
    modules: Vec<PathBuf>,
    classpath: Vec<PathBuf>,
}

impl Project {
    /// A reactor with one class-directory module and two repository JARs carrying hierarchy,
    /// reference, provider and multi-release facts, plus a sources JAR.
    fn new() -> Self {
        let directory = TempDir::new().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::write(
            root.join("pom.xml"),
            "<project><groupId>org.reactor</groupId><artifactId>parent</artifactId><version>3.0</version></project>",
        )
        .unwrap();
        let repository = root.join("repo");
        let api = repository.join("org/libs/api/1.0/api-1.0.jar");
        write_jar(
            &api,
            &[
                MULTI_RELEASE_MANIFEST,
                (
                    "org/libs/Base.class",
                    &hierarchy_class("org/libs/Base", "java/lang/Object", &["org/libs/Marker"]),
                ),
                (
                    "META-INF/versions/11/org/libs/Base.class",
                    &hierarchy_class("org/libs/Base", "java/lang/Object", &[]),
                ),
                (
                    "org/libs/Caller.class",
                    &referencing_class("org/libs/Caller", "org/libs/Base"),
                ),
                ("module-info.class", &module_info_class()),
                ("META-INF/services/org.libs.Spi", b"org.libs.SpiProvider\n"),
            ],
        );
        write_jar(
            &repository.join("org/libs/api/1.0/api-1.0-sources.jar"),
            &[("org/libs/Base.java", b"package org.libs; class Base {}")],
        );
        let util = repository.join("org/libs/util/1.0/util-1.0.jar");
        write_jar(
            &util,
            &[(
                "org/libs/Util.class",
                &hierarchy_class("org/libs/Util", "org/libs/Base", &[]),
            )],
        );
        let module = root.join("core");
        write_module_pom(&module, "core", "3.0");
        let classes = module.join("target/classes");
        write_class_directory(
            &classes,
            &[(
                "org/reactor/Core.class",
                &hierarchy_class("org/reactor/Core", "org/libs/Base", &[]),
            )],
        );
        settle(&root);
        Self {
            modules: vec![module],
            classpath: vec![api, util, classes],
            _directory: directory,
            root,
            repository,
        }
    }

    fn build(
        &self,
        records: Option<&dyn PersistedRecords>,
        max_source_bytes: usize,
    ) -> ScopedIndex {
        MavenIndex::build_scoped_with(
            Some(&self.repository),
            &ReactorLayout {
                root: &self.root,
                modules: &self.modules,
            },
            &self.classpath,
            50,
            max_source_bytes,
            records,
        )
        .unwrap()
    }
}

/// Everything a search can observe, in a deterministic form.
fn observable(index: &MavenIndex) -> String {
    format!(
        "{:?}\n{:?}\n{:?}\n{:?}\n{:?}\n{:?}",
        index.jars,
        index.stats(),
        index.search_classes("", None),
        index.search_type_hierarchy("org.libs.Base", true, None, None),
        index.search_class_references(
            "org.libs.Base",
            ReferenceDirection::Inbound,
            None,
            None,
            None,
            None,
            None
        ),
        index.search_providers(None, None, None, None, None),
    )
}

#[test]
fn reused_records_reproduce_the_freshly_built_index() {
    let project = Project::new();
    let records = MemoryRecords::default();

    let fresh = project.build(Some(&records), 1024);
    assert!(fresh.records.reused.is_empty());
    assert_eq!(
        fresh.records.built, 4,
        "three classpath entries and a sources JAR"
    );
    assert_eq!(fresh.records.persistable.len(), 4);
    records.store(&fresh);

    let reused = project.build(Some(&records), 1024);
    assert_eq!(reused.records.reused.len(), 4);
    assert_eq!(reused.records.built, 0);
    assert!(reused.records.persistable.is_empty());
    assert_eq!(observable(&reused.index), observable(&fresh.index));
    assert_eq!(
        observable(&reused.index),
        observable(&project.build(None, 1024).index),
        "an index built without a cache is the same"
    );
}

#[test]
fn only_changed_artifacts_are_rebuilt() {
    let project = Project::new();
    let records = MemoryRecords::default();
    records.store(&project.build(Some(&records), 1024));

    write_jar(
        &project.classpath[1],
        &[(
            "org/libs/Replacement.class",
            &hierarchy_class("org/libs/Replacement", "java/lang/Object", &[]),
        )],
    );
    settle(&project.classpath[1]);
    let scoped = project.build(Some(&records), 1024);

    assert_eq!(scoped.records.reused.len(), 3);
    assert_eq!(scoped.records.built, 1);
    assert_eq!(scoped.records.persistable.len(), 1);
    let names = scoped
        .index
        .search_classes("org.libs.", None)
        .into_iter()
        .map(|class| class.class_name)
        .collect::<Vec<_>>();
    assert!(names.contains(&"org.libs.Replacement".to_owned()));
    assert!(
        !names.contains(&"org.libs.Util".to_owned()),
        "no stale class: {names:?}"
    );
}

#[test]
fn class_directory_records_track_added_and_deleted_files() {
    let project = Project::new();
    let records = MemoryRecords::default();
    records.store(&project.build(Some(&records), 1024));
    let classes = &project.classpath[2];

    write_class_directory(
        classes,
        &[(
            "org/reactor/Added.class",
            &hierarchy_class("org/reactor/Added", "java/lang/Object", &[]),
        )],
    );
    settle(classes);
    let added = project.build(Some(&records), 1024);
    assert_eq!(added.records.built, 1);
    assert_eq!(added.index.search_classes("org.reactor.", None).len(), 2);
    records.store(&added);

    std::fs::remove_file(classes.join("org/reactor/Core.class")).unwrap();
    settle(classes);
    let deleted = project.build(Some(&records), 1024);
    assert_eq!(deleted.records.built, 1);
    let remaining = deleted.index.search_classes("org.reactor.", None);
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].class_name, "org.reactor.Added");
}

#[test]
fn recently_modified_artifacts_are_indexed_but_not_persisted() {
    let project = Project::new();
    write_jar(
        &project.classpath[1],
        &[(
            "org/libs/Util.class",
            &hierarchy_class("org/libs/Util", "java/lang/Object", &[]),
        )],
    );

    let scoped = project.build(Some(&MemoryRecords::default()), 1024);

    assert_eq!(scoped.records.built, 4);
    assert_eq!(scoped.records.persistable.len(), 3);
    assert!(
        scoped
            .records
            .persistable
            .iter()
            .all(|record| record.path != project.classpath[1])
    );
}

#[test]
fn damaged_foreign_or_differently_limited_records_are_rebuilt() {
    let project = Project::new();
    let records = MemoryRecords::default();
    records.store(&project.build(Some(&records), 1024));

    // A different MAX_SOURCE_BYTES derives different facts, so nothing is reused.
    assert!(
        project
            .build(Some(&records), 2048)
            .records
            .reused
            .is_empty()
    );

    // A record stored under another artifact's path describes another artifact.
    let api = records.load(&project.classpath[0]).unwrap();
    records
        .0
        .borrow_mut()
        .insert(project.classpath[1].clone(), api);
    // A flipped payload byte fails the zlib checksum or the JSON shape.
    let mut damaged = records.load(&project.classpath[2]).unwrap();
    let middle = damaged.len() / 2;
    damaged[middle] ^= 0xff;
    records
        .0
        .borrow_mut()
        .insert(project.classpath[2].clone(), damaged);
    let scoped = project.build(Some(&records), 1024);

    assert_eq!(scoped.records.reused.len(), 2);
    assert_eq!(scoped.records.built, 2);
    assert_eq!(
        observable(&scoped.index),
        observable(&project.build(None, 1024).index)
    );
}

#[test]
fn records_with_dangling_fact_strings_are_rejected() {
    let project = Project::new();
    let records = MemoryRecords::default();
    records.store(&project.build(Some(&records), 1024));
    let mut stored =
        decode_payload::<StoredRecord>(&records.load(&project.classpath[0]).unwrap()).unwrap();
    assert!(!stored.class_facts.is_empty());

    stored.fact_strings.clear();

    assert!(validate(stored).is_err());
}

#[test]
fn reused_records_are_charged_against_the_index_wide_budget() {
    let charge = PreflightCharge {
        entries: 3,
        name_bytes: 30,
    };
    let budget = |entries, names, jar_entries| ArchiveBudget {
        entries,
        names,
        jar_entries,
    };
    let limit = |result: Result<()>| result.unwrap_err().to_string();

    let mut fits = budget(3, 30, 3);
    charge.charge(&mut fits).unwrap();
    assert_eq!((fits.entries, fits.names), (0, 0));
    assert!(limit(charge.charge(&mut budget(3, 30, 2))).contains("MAX_JAR_ENTRIES"));
    assert!(limit(charge.charge(&mut budget(2, 30, 3))).contains("MAX_INDEX_ENTRIES"));
    assert!(limit(charge.charge(&mut budget(3, 29, 3))).contains("MAX_INDEX_NAME_BYTES"));
}

#[test]
fn settled_stamps_are_at_least_the_settling_age_old() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    assert!(is_settled(now - 2 * SETTLED_AGE.as_nanos()));
    assert!(!is_settled(now));
    assert!(!is_settled(now + SETTLED_AGE.as_nanos()), "a future stamp");
}
